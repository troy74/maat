//! NamedSession — a persistent, user-addressable session.
//!
//! Sits between PHAROH (which owns the registry) and VIZIER (which orchestrates work).
//! PHAROH routes `@name: message` directly here; this session maintains its own
//! conversation history and reports a one-line summary back to PHAROH after each turn.

use base64::Engine;
use std::sync::Arc;

use kameo::{actor::ActorRef, request::MessageSend, Actor};
use maat_core::{
    CapabilityRegistry, ChatMessage, ChatReply, MaatError, ModelRegistry, ModelRouteRule,
    ModelRouteScope, ModelSpec, ResourceBudget, RetryPolicy, SessionId, SessionName,
    SessionState, StatusEvent, StatusKind, StepId, ToolRegistry, TraceId, UserId,
};
use maat_memory::{
    window::{build_window, total_history_tokens, window_keep_count},
    ContextConfig, MemoryStore, SessionMeta, StoredMessage,
};
use maat_llm::LlmClient;
use maat_vizier::{Dispatch, Vizier, VizierTask};
use tokio::sync::broadcast;
use tracing::info;

// ─────────────────────────────────────────────
// Actor
// ─────────────────────────────────────────────

#[derive(Actor)]
pub struct NamedSession {
    pub name: SessionName,
    pub session_id: SessionId,
    pub user_id: UserId,
    system_prompt: String,
    history: Vec<ChatMessage>,
    pointer_cache: Vec<ChatMessage>,
    last_summary: String,
    vizier: ActorRef<Vizier>,
    llm: Arc<dyn LlmClient>,
    store: Arc<dyn MemoryStore>,
    ctx_config: ContextConfig,
    model: ModelSpec,
    model_registry: Arc<ModelRegistry>,
    compaction_prompt: String,
    status_tx: broadcast::Sender<StatusEvent>,
}

impl NamedSession {
    pub async fn new(
        name: SessionName,
        session_id: SessionId,
        user_id: UserId,
        llm: Arc<dyn LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        capability_registry: Arc<CapabilityRegistry>,
        store: Arc<dyn MemoryStore>,
        ctx_config: ContextConfig,
        model: ModelSpec,
        model_registry: Arc<ModelRegistry>,
        route_rules: Arc<Vec<ModelRouteRule>>,
        capability_nudge_prompt: String,
        compaction_prompt: String,
        system_prompt: impl Into<String>,
        status_tx: broadcast::Sender<StatusEvent>,
    ) -> Self {
        let vizier = kameo::spawn(Vizier::new(
            user_id.clone(),
            session_id.clone(),
            llm.clone(),
            tool_registry,
            capability_registry,
            model_registry.clone(),
            route_rules.clone(),
            capability_nudge_prompt,
            status_tx.clone(),
        ));
        let system_prompt = system_prompt.into();
        let meta = SessionMeta {
            session_id: session_id.0.to_string(),
            user_id: user_id.0.clone(),
            name: name.0.clone(),
            system_prompt: system_prompt.clone(),
            created_at_ms: maat_core::now_ms(),
            last_active_ms: maat_core::now_ms(),
        };
        let _ = store.save_session_meta(&meta).await;

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
            name: name.clone(),
            session_id,
            user_id,
            system_prompt,
            history,
            pointer_cache,
            last_summary: format!("session '{}' idle", name),
            vizier,
            llm,
            store,
            ctx_config,
            model,
            model_registry,
            compaction_prompt,
            status_tx,
        }
    }

    fn vizier_llm(&self) -> Arc<dyn LlmClient> { self.llm.clone() }

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
                "session": self.name.0,
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

    fn emit(&self, trace_id: &TraceId, state: SessionState) {
        let source = maat_core::ComponentAddress::Session(
            self.user_id.clone(),
            self.session_id.clone(),
        );
        let _ = self.status_tx.send(StatusEvent::new(
            source,
            trace_id.clone(),
            StatusKind::SessionState { session_id: self.session_id.clone(), state },
        ));
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
}

// ─────────────────────────────────────────────
// SessionChat — one user turn
// ─────────────────────────────────────────────

pub struct SessionChat(pub String);

impl kameo::message::Message<SessionChat> for NamedSession {
    type Reply = Result<ChatReply, MaatError>;

    async fn handle(
        &mut self,
        SessionChat(text): SessionChat,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let trace_id = TraceId::new();

        info!(
            session = %self.name,
            chars = text.len(),
            "named session inbound"
        );

        self.emit(
            &trace_id,
            SessionState::Running { step_id: StepId::new() },
        );

        let user_msg = ChatMessage::user(&text);
        self.persist_message(&user_msg).await;
        self.history.push(user_msg);
        let context = self.build_context_for_text(&text).await;

        let result = self
            .vizier
            .ask(Dispatch(VizierTask {
                trace_id: trace_id.clone(),
                description: text,
                messages: context,
                model: self.model.clone(),
                model_policy: None,
                route_scope: ModelRouteScope::SessionDefault,
                resource_budget: ResourceBudget::default(),
                retry: RetryPolicy::default(),
                deadline_ms: None,
            }))
            .send()
            .await
            .map_err(|e| MaatError::Actor(e.to_string()))?;

        let content = match result.outcome {
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

        if total_history_tokens(&self.history) > self.ctx_config.compaction_threshold {
            let keep = window_keep_count(&self.history, &self.ctx_config);
            let compact_count = self.history.len().saturating_sub(keep);
            if compact_count > 0 {
                let to_compact = self.history[..compact_count].to_vec();
                let sid = self.session_id.0.to_string();
                match crate::compaction::compact(
                    &to_compact,
                    &sid,
                    &self.compaction_prompt,
                    self.vizier_llm().as_ref(),
                    self.store.as_ref(),
                ).await {
                    Ok(ptr) => {
                        self.history.drain(..compact_count);
                        self.pointer_cache.push(ptr.to_chat());
                    }
                    Err(e) => tracing::warn!(session = %self.name, error = %e, "compaction failed"),
                }
            }
        }

        // Keep a short summary — first 80 chars of last assistant turn.
        self.last_summary = content.chars().take(80).collect::<String>()
            + if content.len() > 80 { "…" } else { "" };

        self.emit(&trace_id, SessionState::Idle);

        Ok(ChatReply { content, usage: result.usage, latency_ms: result.latency_ms })
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

// ─────────────────────────────────────────────
// GetSummary — PHAROH pulls current state
// ─────────────────────────────────────────────

pub struct GetSummary;

impl kameo::message::Message<GetSummary> for NamedSession {
    /// Returns the one-line summary text. PHAROH reconstructs SessionSummary
    /// using its own stored session_id / name.
    type Reply = String;

    async fn handle(
        &mut self,
        _: GetSummary,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.last_summary.clone()
    }
}

pub struct GetStatusLine;

impl kameo::message::Message<GetStatusLine> for NamedSession {
    type Reply = String;

    async fn handle(
        &mut self,
        _: GetStatusLine,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        format!(
            "@{}  model:{}  turns:{}  summary:{}",
            self.name,
            self.model.profile_id.clone().unwrap_or_else(|| self.model.model_id.clone()),
            self.history.len(),
            self.last_summary
        )
    }
}

pub struct SetModel(pub String);

impl kameo::message::Message<SetModel> for NamedSession {
    type Reply = Result<String, MaatError>;

    async fn handle(
        &mut self,
        SetModel(model_id): SetModel,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        if let Some(spec) = self.model_registry.resolve_spec(&model_id) {
            self.model = spec;
            Ok(format!("Session '{}' model set to profile '{}'.", self.name, model_id))
        } else {
            self.model.model_id = model_id.clone();
            self.model.profile_id = None;
            Ok(format!("Session '{}' model_id set to '{}'.", self.name, model_id))
        }
    }
}

pub struct PurgeSession;

impl kameo::message::Message<PurgeSession> for NamedSession {
    type Reply = Result<String, MaatError>;

    async fn handle(
        &mut self,
        _: PurgeSession,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.store
            .purge_session(&self.session_id.0.to_string())
            .await?;
        self.history.clear();
        self.pointer_cache.clear();
        self.last_summary = format!("session '{}' purged", self.name);
        Ok(format!("Session '{}' history and context were purged.", self.name))
    }
}
