//! PHAROH — per-user main session actor.
//!
//! The pinch point for all channels. Responsibilities:
//!   - Route inbound HeraldPayloads to the primary session or a named session
//!   - Maintain a lightweight SessionRegistry (name → summary only)
//!   - Spawn/end named sessions on demand
//!   - Emit SessionState events on state changes

pub mod session;
mod compaction;

use base64::Engine;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use kameo::{actor::ActorRef, request::MessageSend, Actor};
use maat_core::{
    CapabilityRegistry, ChatMessage, ChatReply, HeraldPayload, MaatError, ModelRegistry,
    ModelRouteRule, ModelRouteScope, ModelSpec, ParsedCommand, ResourceBudget, RetryPolicy,
    SessionId, SessionName, SessionState, SessionSummary, StatusEvent, StatusKind, StepId,
    ToolRegistry, TraceId, UserId,
};
use maat_config::{
    default_skill_dirs, install_skill, load_installed_skills, search_clawhub, InstallSource,
    prompts::PromptLibrary, MaatConfig, SecretResolver,
};
use maat_llm::LlmClient;
use maat_memory::{
    window::{build_window, total_history_tokens, window_keep_count},
    ContextConfig, MemoryStore, SessionMeta, StoredMessage,
};
use maat_vizier::{Dispatch, Vizier, VizierTask};
use session::{GetStatusLine, GetSummary, NamedSession, PurgeSession, SessionChat, SetModel};
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
    model: ModelSpec,
    model_registry: Arc<ModelRegistry>,
    route_rules: Arc<Vec<ModelRouteRule>>,
    capability_registry: Arc<CapabilityRegistry>,
    prompts: PromptLibrary,
    config: Arc<MaatConfig>,
    resolver: Arc<SecretResolver>,
    status_tx: broadcast::Sender<StatusEvent>,
}

impl Pharoh {
    pub async fn new(
        user_id: UserId,
        session_id: SessionId,
        system_prompt: impl Into<String>,
        vizier: ActorRef<Vizier>,
        llm: Arc<dyn LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        store: Arc<dyn MemoryStore>,
        ctx_config: ContextConfig,
        model: ModelSpec,
        model_registry: Arc<ModelRegistry>,
        route_rules: Arc<Vec<ModelRouteRule>>,
        capability_registry: Arc<CapabilityRegistry>,
        prompts: PromptLibrary,
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
        let _ = store.save_session_meta(&meta).await;

        // Restore history from DB.
        let history = store
            .load_history(&session_id.0.to_string())
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.to_chat())
            .collect();

        let pointer_cache = store
            .load_context_pointers(&session_id.0.to_string())
            .await
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
            ctx_config,
            model,
            model_registry,
            route_rules,
            capability_registry,
            prompts,
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
        let tool_lines: Vec<String> = self.tool_registry.all_definitions()
            .iter()
            .map(|d| format!("  - {} — {}", d.name, d.description))
            .collect();
        let context = if description.is_empty() { "General session".to_string() } else { description.to_string() };
        self.prompts
            .render_named_session(&self.user_id.0, &context, &tool_lines.join("\n"))
    }

    fn rebuild_system_prompt(&mut self) {
        let tool_lines: Vec<String> = self.tool_registry.all_definitions()
            .iter()
            .map(|d| format!("  - {} — {}", d.name, d.description))
            .collect();
        self.system_prompt = self
            .prompts
            .render_primary_system(&self.user_id.0, &tool_lines.join("\n"));
    }

    fn build_context(&self) -> Vec<ChatMessage> {
        build_window(&self.system_prompt, &self.pointer_cache, &self.history, &self.ctx_config)
    }

    async fn build_context_for_text(&self, text: &str) -> Vec<ChatMessage> {
        let mut context = self.build_context();
        if should_attach_recent_artifact(text) {
            if let Ok(Some(artifact)) = self
                .store
                .latest_session_artifact(&self.session_id.0.to_string())
                .await
            {
                context.push(ChatMessage::system(format!(
                    "[LATEST ARTIFACT] handle={} kind={} mime={} path={} summary={}",
                    artifact.handle, artifact.kind, artifact.mime_type, artifact.storage_path, artifact.summary
                )));
            }
        }
        context
    }

    async fn persist_message(&self, msg: &ChatMessage) {
        let stored = StoredMessage::from_chat(&self.session_id, msg);
        let _ = self.store.save_message(&stored).await;
    }

    async fn persist_generated_artifacts(
        &self,
        generated: &[maat_core::GeneratedArtifact],
    ) -> Result<Vec<String>, MaatError> {
        let mut lines = Vec::new();
        for artifact in generated {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&artifact.data_base64)
                .map_err(|e| MaatError::Storage(format!("generated artifact decode: {e}")))?;
            let metadata = serde_json::json!({
                "encoding": "json-v1",
                "generated_by": "llm",
            });
            let analysis = serde_json::json!({
                "encoding": "json-v1",
                "status": "generated",
            });
            let record = self
                .store
                .save_generated_artifact(
                    &self.user_id.0,
                    &self.session_id.0.to_string(),
                    &artifact.suggested_name,
                    &artifact.kind,
                    &artifact.mime_type,
                    "generated",
                    &artifact.summary,
                    &metadata.to_string(),
                    &analysis.to_string(),
                    &bytes,
                )
                .await?;
            lines.push(format!("  - {}  {}  {}", record.handle, record.mime_type, record.display_name));
        }
        Ok(lines)
    }

    // ── Primary session LLM call ───────────────────────────────────

    async fn handle_primary(&mut self, text: String) -> Result<ChatReply, MaatError> {
        let trace_id = TraceId::new();
        info!(user = %self.user_id, chars = text.len(), "primary session");

        self.emit(&trace_id, SessionState::Running { step_id: StepId::new() });
        let user_msg = ChatMessage::user(&text);
        self.persist_message(&user_msg).await;
        self.history.push(user_msg);
        let context = self.build_context_for_text(&text).await;

        let result_env = self
            .primary_vizier
            .ask(Dispatch(VizierTask {
                trace_id: trace_id.clone(),
                description: text,
                messages: context,
                model: self.model.clone(),
                model_policy: None,
                route_scope: ModelRouteScope::PharohPrimary,
                resource_budget: ResourceBudget::default(),
                retry: RetryPolicy::default(),
                deadline_ms: None,
            }))
            .send()
            .await
            .map_err(|e| MaatError::Actor(e.to_string()))?;

        let content = match result_env.outcome {
            maat_core::TaskOutcome::Success { content, generated_artifacts, .. } => {
                let artifact_lines = self.persist_generated_artifacts(&generated_artifacts).await?;
                if artifact_lines.is_empty() {
                    content
                } else if content.trim().is_empty() {
                    format!("Created artifacts:\n{}", artifact_lines.join("\n"))
                } else {
                    format!("{content}\n\nGenerated artifacts:\n{}", artifact_lines.join("\n"))
                }
            }
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
        self.persist_message(&asst_msg).await;
        self.history.push(asst_msg);

        // Compact if threshold exceeded.
        if total_history_tokens(&self.history) > self.ctx_config.compaction_threshold {
            let keep = window_keep_count(&self.history, &self.ctx_config);
            let compact_count = self.history.len().saturating_sub(keep);
            if compact_count > 0 {
                let to_compact = self.history[..compact_count].to_vec();
                let sid = self.session_id.0.to_string();
                match compaction::compact(
                    &to_compact,
                    &sid,
                    &self.prompts.compaction,
                    self.llm.as_ref(),
                    self.store.as_ref(),
                ).await {
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
            ParsedCommand::StatusSession { name } => self.status_session(name).await,
            ParsedCommand::ModelList => self.model_list().await,
            ParsedCommand::ModelSwap { session, model_id } => self.model_swap(session, model_id).await,
            ParsedCommand::Purge { session } => self.purge(session).await,
            ParsedCommand::ToolsList => self.tools_list().await,
            ParsedCommand::SkillsList => self.skills_list().await,
            ParsedCommand::SkillSearch { query } => self.skill_search(query).await,
            ParsedCommand::SkillInstall { source } => self.skill_install(source).await,
            ParsedCommand::ArtifactsList => self.artifacts_list().await,
            ParsedCommand::ArtifactImport { path } => self.artifact_import(path).await,
            ParsedCommand::ArtifactShow { handle } => self.artifact_show(handle).await,
            ParsedCommand::MemoryAdd { text } => self.memory_add(text).await,
            ParsedCommand::MistakeAdd { text } => self.mistake_add(text).await,
            ParsedCommand::UserNoteAdd { user, text } => self.user_note_add(user, text).await,
            ParsedCommand::PersonaAppend { text } => self.persona_append(text).await,
            ParsedCommand::PromptsList => self.prompts_list().await,
            ParsedCommand::PromptShow { name } => self.prompt_show(name).await,
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
            self.capability_registry.clone(),
            self.store.clone(),
            self.ctx_config.clone(),
            self.model.clone(),
            self.model_registry.clone(),
            self.route_rules.clone(),
            self.prompts.capability_nudge.clone(),
            self.prompts.compaction.clone(),
            system_prompt,
            self.status_tx.clone(),
        ).await);

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
        lines.push(format!(
            "  Primary model: {}",
            self.model.profile_id.clone().unwrap_or_else(|| self.model.model_id.clone())
        ));
        lines.push(format!("  Primary history: {} turns", self.history.len()));
        lines.push(format!("  Named sessions: {}", self.sessions.len()));
        for (name, entry) in &self.sessions {
            lines.push(format!("    @{}  {}", name, entry.summary.summary));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn status_session(&mut self, name: SessionName) -> Result<ChatReply, MaatError> {
        if name.0 == "primary" {
            return Ok(quick_reply(&format!(
                "@primary  model:{}  turns:{}",
                self.model.profile_id.clone().unwrap_or_else(|| self.model.model_id.clone()),
                self.history.len()
            )));
        }
        match self.sessions.get_mut(&name) {
            Some(entry) => {
                let line = entry
                    .actor
                    .ask(GetStatusLine)
                    .send()
                    .await
                    .map_err(|e| MaatError::Actor(e.to_string()))?;
                Ok(quick_reply(&line))
            }
            None => Ok(quick_reply(&format!("No session '{}' found.", name))),
        }
    }

    async fn model_list(&self) -> Result<ChatReply, MaatError> {
        let profiles = self.model_registry.profiles();
        if profiles.is_empty() {
            return Ok(quick_reply("No model profiles registered."));
        }
        let mut lines = vec![format!("Model profiles ({}):", profiles.len())];
        for profile in profiles {
            lines.push(format!(
                "  {}  —  provider:{} model:{} tags:{}",
                profile.id,
                profile.provider_id,
                profile.model_id,
                if profile.tags.is_empty() { "-".into() } else { profile.tags.join(",") }
            ));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn model_swap(
        &mut self,
        session: Option<SessionName>,
        model_id: String,
    ) -> Result<ChatReply, MaatError> {
        match session {
            None => {
                if let Some(spec) = self.model_registry.resolve_spec(&model_id) {
                    self.model = spec;
                    Ok(quick_reply(&format!("Primary session model set to profile '{}'.", model_id)))
                } else {
                    self.model.model_id = model_id.clone();
                    self.model.profile_id = None;
                    Ok(quick_reply(&format!("Primary session model_id set to '{}'.", model_id)))
                }
            }
            Some(name) => match self.sessions.get_mut(&name) {
                Some(entry) => {
                    let reply = entry
                        .actor
                        .ask(SetModel(model_id))
                        .send()
                        .await
                        .map_err(|e| MaatError::Actor(e.to_string()))?;
                    Ok(quick_reply(&reply))
                }
                None => Ok(quick_reply(&format!("No session '{}' found.", name))),
            },
        }
    }

    async fn purge(&mut self, session: SessionName) -> Result<ChatReply, MaatError> {
        if session.0 == "primary" {
            self.store
                .purge_session(&self.session_id.0.to_string())
                .await?;
            self.history.clear();
            self.pointer_cache.clear();
            return Ok(quick_reply("Primary session history and context were purged."));
        }

        match self.sessions.get_mut(&session) {
            Some(entry) => {
                let reply = entry
                    .actor
                    .ask(PurgeSession)
                    .send()
                    .await
                    .map_err(|e| MaatError::Actor(e.to_string()))?;
                Ok(quick_reply(&reply))
            }
            None => Ok(quick_reply(&format!("No session '{}' found.", session))),
        }
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

    async fn skills_list(&self) -> Result<ChatReply, MaatError> {
        let skill_dirs = default_skill_dirs(&self.config.skills.dirs);
        let registry = load_installed_skills(&skill_dirs);
        if registry.all().is_empty() {
            return Ok(quick_reply(
                "No installed skills.\nInstall one with: /skills install <path-to-skill-directory>\nOr browse ClawHub with: /skills search <query>",
            ));
        }

        let mut lines = vec![format!("Installed skills ({}):", registry.all().len())];
        for skill in registry.all() {
            let perms = if skill.permissions.is_empty() {
                "none".to_string()
            } else {
                skill.permissions
                    .iter()
                    .map(|permission| format!("{permission:?}").to_ascii_lowercase())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            lines.push(format!(
                "  {}  —  {:?}  —  {}",
                skill.name,
                skill.trust,
                skill.path.display()
            ));
            lines.push(format!("     permissions: {perms}"));
            if let Some(reference) = &skill.reference {
                lines.push(format!("     source: {reference}"));
            }
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn skill_search(&self, query: String) -> Result<ChatReply, MaatError> {
        match tokio::task::spawn_blocking(move || search_clawhub(&query)).await {
            Ok(Ok(output)) if !output.is_empty() => Ok(quick_reply(&format!(
                "ClawHub search results:\n{output}"
            ))),
            Ok(Ok(_)) => Ok(quick_reply("ClawHub search returned no results.")),
            Ok(Err(error)) => Ok(quick_reply(&format!("ClawHub search failed: {error}"))),
            Err(error) => Ok(quick_reply(&format!("ClawHub search failed: {error}"))),
        }
    }

    async fn skill_install(&self, source: String) -> Result<ChatReply, MaatError> {
        let skill_dirs = default_skill_dirs(&self.config.skills.dirs);
        let dest_root = skill_dirs
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("skills"));
        let install_source = InstallSource::parse(&source);
        let source_label = source.clone();

        match tokio::task::spawn_blocking(move || install_skill(install_source, &dest_root)).await {
            Ok(Ok(skill)) => Ok(quick_reply(&format!(
                "Installed skill '{}' from {} into {}.\nWrote maat-skill.toml with trust/provenance metadata.\nRestart MAAT to pick it up in routing.",
                skill.name,
                source_label,
                skill.path.display()
            ))),
            Ok(Err(error)) => Ok(quick_reply(&format!("Skill install failed: {error}"))),
            Err(error) => Ok(quick_reply(&format!("Skill install failed: {error}"))),
        }
    }

    async fn artifacts_list(&self) -> Result<ChatReply, MaatError> {
        let artifacts = self.store.list_artifacts(&self.user_id.0, 20).await?;
        if artifacts.is_empty() {
            return Ok(quick_reply(
                "No artifacts stored yet. Import one with: /artifacts import <path>",
            ));
        }

        let mut lines = vec![format!("Stored artifacts ({} shown):", artifacts.len())];
        for artifact in artifacts {
            lines.push(format!(
                "  - {}  {}  {}  {}",
                artifact.handle, artifact.kind, artifact.display_name, artifact.summary
            ));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn artifact_import(&self, path: String) -> Result<ChatReply, MaatError> {
        let source_path = PathBuf::from(&path);
        let record = self
            .store
            .import_artifact(&self.user_id.0, &self.session_id.0.to_string(), &source_path)
            .await?;
        Ok(quick_reply(&format!(
            "Imported artifact `{}`.\nKind: {}\nPath: {}\nStored at: {}\nUse: /artifacts show {}",
            record.handle,
            record.kind,
            path,
            record.storage_path,
            record.handle
        )))
    }

    async fn artifact_show(&self, handle: String) -> Result<ChatReply, MaatError> {
        let Some(record) = self
            .store
            .get_artifact_by_handle(&self.user_id.0, &handle)
            .await?
        else {
            return Ok(quick_reply(&format!("No artifact '{}' found.", handle)));
        };

        let metadata = serde_json::from_str::<serde_json::Value>(&record.metadata_json)
            .unwrap_or_else(|_| serde_json::json!({}));
        let analysis = serde_json::from_str::<serde_json::Value>(&record.analysis_json)
            .unwrap_or_else(|_| serde_json::json!({}));
        let mut lines = vec![
            format!("Artifact `{}`", record.handle),
            format!("  id: {}", record.artifact_id),
            format!("  kind: {}", record.kind),
            format!("  mime: {}", record.mime_type),
            format!("  name: {}", record.display_name),
            format!("  stored: {}", record.storage_path),
            format!("  bytes: {}", record.byte_size),
            format!("  source: {}", record.source),
            format!("  summary: {}", record.summary),
        ];
        if !metadata.is_null() && metadata != serde_json::json!({}) {
            lines.push(format!("  metadata: {}", metadata));
        }
        if !analysis.is_null() && analysis != serde_json::json!({}) {
            lines.push(format!("  analysis: {}", analysis));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn memory_add(&mut self, text: String) -> Result<ChatReply, MaatError> {
        match self.prompts.append_memory(&text) {
            Ok(()) => {
                self.rebuild_system_prompt();
                Ok(quick_reply("Memory appended to prompts/bouquet/memory.md"))
            }
            Err(error) => Ok(quick_reply(&format!("Memory update failed: {error}"))),
        }
    }

    async fn mistake_add(&mut self, text: String) -> Result<ChatReply, MaatError> {
        match self.prompts.append_mistake(&text) {
            Ok(()) => {
                self.rebuild_system_prompt();
                Ok(quick_reply("Mistake appended to prompts/bouquet/mistakes.md"))
            }
            Err(error) => Ok(quick_reply(&format!("Mistake update failed: {error}"))),
        }
    }

    async fn user_note_add(
        &mut self,
        user: Option<String>,
        text: String,
    ) -> Result<ChatReply, MaatError> {
        let target_user = user.unwrap_or_else(|| self.user_id.0.clone());
        match self.prompts.append_user_note(&target_user, &text) {
            Ok(()) => {
                self.rebuild_system_prompt();
                Ok(quick_reply(&format!(
                    "User note appended for {} under prompts/bouquet/users/",
                    target_user
                )))
            }
            Err(error) => Ok(quick_reply(&format!("User note update failed: {error}"))),
        }
    }

    async fn persona_append(&mut self, text: String) -> Result<ChatReply, MaatError> {
        match self.prompts.append_persona(&text) {
            Ok(()) => {
                self.rebuild_system_prompt();
                Ok(quick_reply("Persona update appended to prompts/bouquet/persona.md"))
            }
            Err(error) => Ok(quick_reply(&format!("Persona update failed: {error}"))),
        }
    }

    async fn prompts_list(&self) -> Result<ChatReply, MaatError> {
        let assets = self.prompts.assets(&self.user_id.0);
        let mut lines = vec![format!("Prompt assets ({}):", assets.len())];
        for asset in assets {
            let policy = asset
                .policy
                .map(|policy| format!("{policy:?}").to_ascii_lowercase())
                .unwrap_or_else(|| "template".into());
            lines.push(format!("  {}  —  {}  —  {}", asset.name, policy, asset.path.display()));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn prompt_show(&self, name: String) -> Result<ChatReply, MaatError> {
        match self.prompts.show_asset(&self.user_id.0, &name) {
            Some(asset) => {
                let policy = asset
                    .policy
                    .map(|policy| format!("{policy:?}").to_ascii_lowercase())
                    .unwrap_or_else(|| "template".into());
                Ok(quick_reply(&format!(
                    "{}\npath: {}\npolicy: {}\n\n{}",
                    asset.name,
                    asset.path.display(),
                    policy,
                    asset.content
                )))
            }
            None => Ok(quick_reply(&format!("Unknown prompt asset '{}'.", name))),
        }
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
        let existing_text = tokio::fs::read_to_string("maat.workspace.toml").await.unwrap_or_default();
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
                    sec.insert(parts[1].to_string(), parse_config_value(&value));
                }
            } else {
                root.insert(key.clone(), parse_config_value(&value));
            }
        }

        let new_text = toml::to_string_pretty(&table)
            .unwrap_or_else(|_| existing_text.clone());
        if let Err(e) = tokio::fs::write("maat.workspace.toml", &new_text).await {
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
            HeraldPayload::Attachment { mime_type, size_bytes, pointer } => {
                match self
                    .store
                    .import_artifact(&self.user_id.0, &self.session_id.0.to_string(), &PathBuf::from(&pointer))
                    .await
                {
                    Ok(record) => Ok(quick_reply(&format!(
                        "Imported attachment `{}`.\nKind: {}\nMime: {}\nBytes: {}\nStored at: {}",
                        record.handle, record.kind, mime_type, size_bytes, record.storage_path
                    ))),
                    Err(error) => Ok(quick_reply(&format!("Attachment import failed: {error}"))),
                }
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

fn should_attach_recent_artifact(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_pronoun = [" it", "it ", "that", "this", "latest image", "latest artifact"]
        .iter()
        .any(|needle| lower.contains(needle));
    let has_action = ["email", "send", "attach", "share", "use"].iter().any(|needle| lower.contains(needle));
    has_pronoun && has_action
}

fn parse_config_value(value: &str) -> toml::Value {
    let wrapped = format!("value = {value}");
    if let Ok(parsed) = toml::from_str::<toml::Value>(&wrapped) {
        if let Some(parsed_value) = parsed.get("value") {
            return parsed_value.clone();
        }
    }
    toml::Value::String(value.to_string())
}
