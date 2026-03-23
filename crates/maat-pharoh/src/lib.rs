//! PHAROH — per-user main session actor.
//!
//! The pinch point for all channels. Responsibilities:
//!   - Route inbound HeraldPayloads to the primary session or a named session
//!   - Maintain a lightweight SessionRegistry (name → summary only)
//!   - Spawn/end named sessions on demand
//!   - Emit SessionState events on state changes

pub mod session;
mod compaction;

use std::collections::HashMap;
use std::sync::Arc;

use kameo::{actor::ActorRef, request::MessageSend, Actor};
use maat_core::{
    ChatMessage, ChatReply, HeraldPayload, MaatError, ModelSpec, ParsedCommand,
    ResourceBudget, RetryPolicy, SessionId, SessionName, SessionState, SessionSummary,
    StatusEvent, StatusKind, StepId, ToolRegistry, TraceId, UserId,
};
use maat_config::{MaatConfig, SecretResolver};
use maat_llm::LlmClient;
use maat_memory::{
    window::{build_window, total_history_tokens, window_keep_count},
    ContextConfig, MemoryStore, SessionMeta, StoredMessage,
};
use maat_vizier::{Dispatch, Vizier, VizierTask};
use session::{GetSummary, NamedSession, SessionChat};
use tokio::sync::broadcast;
use tracing::info;

// ─────────────────────────────────────────────
// Session registry entry
// ─────────────────────────────────────────────

struct SessionEntry {
    actor: ActorRef<NamedSession>,
    summary: SessionSummary,
}

// ─────────────────────────────────────────────
// Actor
// ─────────────────────────────────────────────

#[derive(Actor)]
pub struct Pharoh {
    pub user_id: UserId,
    pub session_id: SessionId,
    system_prompt: String,
    history: Vec<ChatMessage>,
    pointer_cache: Vec<ChatMessage>,
    primary_vizier: ActorRef<Vizier>,
    sessions: HashMap<SessionName, SessionEntry>,
    llm: Arc<dyn LlmClient>,
    tool_registry: Arc<ToolRegistry>,
    store: Arc<dyn MemoryStore>,
    ctx_config: ContextConfig,
    config: Arc<MaatConfig>,
    resolver: Arc<SecretResolver>,
    status_tx: broadcast::Sender<StatusEvent>,
}

impl Pharoh {
    pub fn new(
        user_id: UserId,
        session_id: SessionId,
        system_prompt: impl Into<String>,
        vizier: ActorRef<Vizier>,
        llm: Arc<dyn LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        store: Arc<dyn MemoryStore>,
        config: Arc<MaatConfig>,
        resolver: Arc<SecretResolver>,
        status_tx: broadcast::Sender<StatusEvent>,
    ) -> Self {
        let system_prompt = system_prompt.into();
        // Persist session meta (upsert — safe to call multiple times).
        let meta = SessionMeta {
            session_id: session_id.0.to_string(),
            user_id: user_id.0.clone(),
            name: "primary".into(),
            system_prompt: system_prompt.clone(),
            created_at_ms: maat_core::now_ms(),
            last_active_ms: maat_core::now_ms(),
        };
        let _ = store.save_session_meta(&meta);

        // Restore history from DB.
        let history = store
            .load_history(&session_id.0.to_string())
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.to_chat())
            .collect();

        let pointer_cache = store
            .load_context_pointers(&session_id.0.to_string())
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_chat())
            .collect();

        Self {
            user_id,
            session_id,
            system_prompt,
            history,
            pointer_cache,
            primary_vizier: vizier,
            sessions: HashMap::new(),
            llm,
            tool_registry,
            store,
            ctx_config: ContextConfig::default(),
            config,
            resolver,
            status_tx,
        }
    }

    fn emit(&self, trace_id: &TraceId, state: SessionState) {
        let source = maat_core::ComponentAddress::Pharoh(self.user_id.clone());
        let _ = self.status_tx.send(StatusEvent::new(
            source,
            trace_id.clone(),
            StatusKind::SessionState { session_id: self.session_id.clone(), state },
        ));
    }

    fn build_session_prompt(&self, description: &str) -> String {
        let defs = self.tool_registry.all_definitions();
        let base = if description.is_empty() {
            "You are MAAT, a helpful AI assistant.".to_string()
        } else {
            format!("You are MAAT, a helpful AI assistant. Context: {description}")
        };
        if defs.is_empty() {
            return base;
        }
        let tool_lines: Vec<String> = defs
            .iter()
            .map(|d| format!("  - {} — {}", d.name, d.description))
            .collect();
        format!(
            "{base}\n\nYou have access to the following tools and MUST use them when relevant \
             instead of saying you cannot perform a task:\n{}\n\n\
             When a user asks about something a tool can help with, call the tool \
             immediately rather than apologising or suggesting alternatives.",
            tool_lines.join("\n")
        )
    }

    fn build_context(&self) -> Vec<ChatMessage> {
        build_window(&self.system_prompt, &self.pointer_cache, &self.history, &self.ctx_config)
    }

    fn persist_message(&self, msg: &ChatMessage) {
        let stored = StoredMessage::from_chat(&self.session_id, msg);
        let _ = self.store.save_message(&stored);
    }

    // ── Primary session LLM call ───────────────────────────────────

    async fn handle_primary(&mut self, text: String) -> Result<ChatReply, MaatError> {
        let trace_id = TraceId::new();
        info!(user = %self.user_id, chars = text.len(), "primary session");

        self.emit(&trace_id, SessionState::Running { step_id: StepId::new() });
        let user_msg = ChatMessage::user(&text);
        self.persist_message(&user_msg);
        self.history.push(user_msg);

        let result_env = self
            .primary_vizier
            .ask(Dispatch(VizierTask {
                trace_id: trace_id.clone(),
                description: text,
                messages: self.build_context(),
                model: ModelSpec::openrouter_default(),
                resource_budget: ResourceBudget::default(),
                retry: RetryPolicy::default(),
                deadline_ms: None,
            }))
            .send()
            .await
            .map_err(|e| MaatError::Actor(e.to_string()))?;

        let content = match result_env.outcome {
            maat_core::TaskOutcome::Success { content, .. } => content,
            maat_core::TaskOutcome::Failed { error, .. } => {
                self.emit(&trace_id, SessionState::Failed { error: error.clone() });
                return Err(MaatError::Llm(error));
            }
            maat_core::TaskOutcome::TimedOut => {
                self.emit(&trace_id, SessionState::Failed { error: "timed out".into() });
                return Err(MaatError::Llm("timed out".into()));
            }
            maat_core::TaskOutcome::Cancelled => {
                self.emit(&trace_id, SessionState::Cancelled);
                return Err(MaatError::Llm("cancelled".into()));
            }
        };

        let asst_msg = ChatMessage::assistant(&content);
        self.persist_message(&asst_msg);
        self.history.push(asst_msg);

        // Compact if threshold exceeded.
        if total_history_tokens(&self.history) > self.ctx_config.compaction_threshold {
            let keep = window_keep_count(&self.history, &self.ctx_config);
            let compact_count = self.history.len().saturating_sub(keep);
            if compact_count > 0 {
                let to_compact = self.history[..compact_count].to_vec();
                let sid = self.session_id.0.to_string();
                match compaction::compact(&to_compact, &sid, self.llm.as_ref(), self.store.as_ref()).await {
                    Ok(ptr) => {
                        self.history.drain(..compact_count);
                        self.pointer_cache.push(ptr.to_chat());
                    }
                    Err(e) => tracing::warn!(error = %e, "primary session compaction failed"),
                }
            }
        }

        self.emit(&trace_id, SessionState::Idle);
        Ok(ChatReply { content, usage: result_env.usage, latency_ms: result_env.latency_ms })
    }

    // ── Command routing ────────────────────────────────────────────

    async fn handle_command(&mut self, cmd: ParsedCommand) -> Result<ChatReply, MaatError> {
        match cmd {
            ParsedCommand::RouteToSession { name, message } => {
                self.route_to_session(name, message).await
            }
            ParsedCommand::SessionNew { name, description } => {
                self.session_new(name, description).await
            }
            ParsedCommand::SessionList => self.session_list().await,
            ParsedCommand::SessionEnd { name } => self.session_end(name).await,
            ParsedCommand::StatusAll => self.status_all().await,
            ParsedCommand::ToolsList => self.tools_list().await,
            ParsedCommand::ConfigShow => self.config_show().await,
            ParsedCommand::ConfigSet { key, value } => self.config_set(key, value).await,
            ParsedCommand::SecretSet { key, value } => self.secret_set(key, value).await,
            ParsedCommand::SecretList => self.secret_list().await,
            ParsedCommand::SecretDelete { key } => self.secret_delete(key).await,
            ParsedCommand::AuthGoogle => self.handle_auth_google().await,
            _ => Ok(quick_reply("command not yet implemented")),
        }
    }

    async fn route_to_session(
        &mut self,
        name: SessionName,
        message: String,
    ) -> Result<ChatReply, MaatError> {
        match self.sessions.get(&name) {
            None => Ok(quick_reply(&format!(
                "No session '{}'. Create it with: /session new {}: <description>",
                name, name
            ))),
            Some(entry) => {
                let reply: ChatReply = entry
                    .actor
                    .ask(SessionChat(message))
                    .send()
                    .await
                    .map_err(|e| MaatError::Actor(e.to_string()))?;

                // Refresh summary after each turn.
                if let Some(entry) = self.sessions.get_mut(&name) {
                    if let Ok(text) = entry.actor.ask(GetSummary).send().await {
                        entry.summary.summary = text;
                        entry.summary.last_active_ms = maat_core::now_ms();
                    }
                }
                Ok(reply)
            }
        }
    }

    async fn session_new(
        &mut self,
        name: SessionName,
        description: String,
    ) -> Result<ChatReply, MaatError> {
        if self.sessions.contains_key(&name) {
            return Ok(quick_reply(&format!("Session '{}' already exists.", name)));
        }

        let session_id = SessionId::new();
        let system_prompt = self.build_session_prompt(&description);

        let actor = kameo::spawn(NamedSession::new(
            name.clone(),
            session_id.clone(),
            self.user_id.clone(),
            self.llm.clone(),
            self.tool_registry.clone(),
            self.store.clone(),
            system_prompt,
            self.status_tx.clone(),
        ));

        let summary = SessionSummary {
            session_id,
            name: name.clone(),
            state: SessionState::Idle,
            summary: format!("new session: {}", description),
            last_active_ms: maat_core::now_ms(),
        };

        self.sessions.insert(name.clone(), SessionEntry { actor, summary });
        info!(session = %name, "created named session");

        Ok(quick_reply(&format!(
            "Session '{}' created. Talk to it with: @{}: <message>",
            name, name
        )))
    }

    async fn session_end(&mut self, name: SessionName) -> Result<ChatReply, MaatError> {
        if self.sessions.remove(&name).is_some() {
            info!(session = %name, "ended named session");
            Ok(quick_reply(&format!("Session '{}' ended.", name)))
        } else {
            Ok(quick_reply(&format!("No session '{}' found.", name)))
        }
    }

    async fn session_list(&mut self) -> Result<ChatReply, MaatError> {
        if self.sessions.is_empty() {
            return Ok(quick_reply(
                "No named sessions. Create one with: /session new <name>: <description>",
            ));
        }

        // Refresh all summaries.
        for entry in self.sessions.values_mut() {
            if let Ok(text) = entry.actor.ask(GetSummary).send().await {
                entry.summary.summary = text;
                entry.summary.last_active_ms = maat_core::now_ms();
            }
        }

        let mut lines = vec!["Active sessions:".to_string()];
        for (name, entry) in &self.sessions {
            lines.push(format!("  @{}  —  {}", name, entry.summary.summary));
        }
        lines.push(String::new());
        lines.push("Route with: @<name>: <message>".to_string());
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn status_all(&mut self) -> Result<ChatReply, MaatError> {
        let mut lines = vec![format!("PHAROH — user: {}", self.user_id)];
        lines.push(format!("  Primary history: {} turns", self.history.len()));
        lines.push(format!("  Named sessions: {}", self.sessions.len()));
        for (name, entry) in &self.sessions {
            lines.push(format!("    @{}  {}", name, entry.summary.summary));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn tools_list(&self) -> Result<ChatReply, MaatError> {
        let defs = self.tool_registry.all_definitions();
        if defs.is_empty() {
            return Ok(quick_reply(
                "No talents/tools loaded.\n\
                 Set IMAP_HOST, IMAP_USERNAME, IMAP_PASSWORD to enable the IMAP talent.",
            ));
        }
        let mut lines = vec![format!("Loaded tools ({}):", defs.len())];
        for d in &defs {
            lines.push(format!("  {}  —  {}", d.name, d.description));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn config_show(&self) -> Result<ChatReply, MaatError> {
        let mut text = self.config.display_summary();
        text.push_str("\n\nSecret stores:\n");
        text.push_str(&self.resolver.store_summary());
        Ok(quick_reply(&text))
    }

    async fn config_set(&self, key: String, value: String) -> Result<ChatReply, MaatError> {
        // Write a workspace-local override to maat.workspace.toml.
        // We persist as a flat key path like "llm.model" → TOML table.
        let existing_text = std::fs::read_to_string("maat.workspace.toml").unwrap_or_default();
        let mut table: toml::Value = toml::from_str(&existing_text)
            .unwrap_or(toml::Value::Table(toml::map::Map::new()));

        // Navigate/create nested path.
        let parts: Vec<&str> = key.splitn(2, '.').collect();
        if let toml::Value::Table(ref mut root) = table {
            if parts.len() == 2 {
                let section = root
                    .entry(parts[0].to_string())
                    .or_insert(toml::Value::Table(toml::map::Map::new()));
                if let toml::Value::Table(ref mut sec) = section {
                    sec.insert(parts[1].to_string(), toml::Value::String(value.clone()));
                }
            } else {
                root.insert(key.clone(), toml::Value::String(value.clone()));
            }
        }

        let new_text = toml::to_string_pretty(&table)
            .unwrap_or_else(|_| existing_text.clone());
        if let Err(e) = std::fs::write("maat.workspace.toml", &new_text) {
            return Ok(quick_reply(&format!("Failed to write maat.workspace.toml: {e}")));
        }
        Ok(quick_reply(&format!("Config set: {key} = {value}\n(saved to maat.workspace.toml — takes effect on next restart)")))
    }

    async fn secret_set(&self, key: String, value: String) -> Result<ChatReply, MaatError> {
        match self.resolver.set(&key, &value) {
            Ok(()) => Ok(quick_reply(&format!("Secret stored: {key}"))),
            Err(e) => Ok(quick_reply(&format!("Failed to store secret: {e}"))),
        }
    }

    async fn secret_list(&self) -> Result<ChatReply, MaatError> {
        let keys = self.resolver.list_keys();
        if keys.is_empty() {
            return Ok(quick_reply("No secrets stored."));
        }
        let mut lines = vec![format!("Known secret keys ({}):", keys.len())];
        for k in &keys {
            lines.push(format!("  {k}"));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn secret_delete(&self, key: String) -> Result<ChatReply, MaatError> {
        match self.resolver.delete(&key) {
            Ok(()) => Ok(quick_reply(&format!("Secret deleted: {key}"))),
            Err(e) => Ok(quick_reply(&format!("Failed to delete secret: {e}"))),
        }
    }

    async fn handle_auth_google(&self) -> Result<ChatReply, MaatError> {
        let google_cfg = match self.config.google.as_ref() {
            Some(g) => g,
            None => return Ok(quick_reply(
                "No [google] section in config.\n\
                 Add the following to maat.toml or maat.workspace.toml:\n\n\
                 [google]\n\
                 client_id = \"<your-oauth-client-id>\"\n\n\
                 Then store the client secret with:\n\
                 /secret set maat/google/client_secret <your-client-secret>"
            )),
        };

        let client_id = match google_cfg.client_id.clone() {
            Some(id) => id,
            None => return Ok(quick_reply(
                "google.client_id is not set in config.\n\
                 Set it in maat.toml: client_id = \"<your-oauth-client-id>\""
            )),
        };

        let client_secret = match self.resolver.get(google_cfg.client_secret_key()) {
            Some(s) => s,
            None => return Ok(quick_reply(&format!(
                "Google client secret not found.\n\
                 Store it with: /secret set {} <your-client-secret>",
                google_cfg.client_secret_key()
            ))),
        };

        let url = maat_talents::google::auth::start_oauth_flow(
            self.resolver.clone(),
            self.config.clone(),
            client_id,
            client_secret,
        )
        .await
        .map_err(|e| MaatError::Config(e))?;

        // Try to open the browser automatically.
        #[cfg(target_os = "macos")]
        { let _ = std::process::Command::new("open").arg(&url).spawn(); }
        #[cfg(target_os = "linux")]
        { let _ = std::process::Command::new("xdg-open").arg(&url).spawn(); }

        Ok(quick_reply(&format!(
            "Opening browser for Google authentication…\n\n\
             If it did not open automatically, visit:\n{url}\n\n\
             MAAT is listening for the OAuth callback in the background.\n\
             Once authenticated, the gmail_send tool will be available."
        )))
    }
}

// ─────────────────────────────────────────────
// Inbound message from any herald
// ─────────────────────────────────────────────

pub struct Inbound(pub HeraldPayload);

impl kameo::message::Message<Inbound> for Pharoh {
    type Reply = Result<ChatReply, MaatError>;

    async fn handle(
        &mut self,
        Inbound(payload): Inbound,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        match payload {
            HeraldPayload::Text(text) => self.handle_primary(text).await,
            HeraldPayload::Command(cmd) => self.handle_command(cmd).await,
            HeraldPayload::Attachment { .. } => {
                Ok(quick_reply("Attachment handling coming soon."))
            }
        }
    }
}

// ── Keep `Chat` as a thin alias so maat-ra compiles cleanly ──────
pub use Inbound as Chat;

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

fn quick_reply(text: &str) -> ChatReply {
    ChatReply {
        content: text.to_string(),
        usage: maat_core::TokenUsage::default(),
        latency_ms: 0,
    }
}
