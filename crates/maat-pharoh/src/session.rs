//! NamedSession — a persistent, user-addressable session.
//!
//! Sits between PHAROH (which owns the registry) and VIZIER (which orchestrates work).
//! PHAROH routes `@name: message` directly here; this session maintains its own
//! conversation history and reports a one-line summary back to PHAROH after each turn.

use base64::Engine;
use std::sync::Arc;

use kameo::{actor::ActorRef, request::MessageSend, Actor};
use maat_core::{
    CancellationRegistry, CapabilityRegistry, ChatImageInput, ChatMessage, ChatReply, MaatError, ModelRegistry, ModelRouteRule,
    ModelRouteScope, ModelSpec, ResourceBudget, RetryPolicy, SessionId, SessionName,
    SessionState, StatusEvent, StatusKind, StepId, SupportCapabilityRule, ToolRegistry,
    TraceId, UserId,
};
use maat_memory::{
    ArtifactRecord,
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
    route_rules: Arc<Vec<ModelRouteRule>>,
    support_rules: Arc<Vec<SupportCapabilityRule>>,
    intent_classifier_prompt: String,
    capability_nudge_prompt: String,
    compaction_prompt: String,
    status_tx: broadcast::Sender<StatusEvent>,
    cancel_registry: CancellationRegistry,
}

pub struct ReloadRuntime {
    pub tool_registry: Arc<ToolRegistry>,
    pub capability_registry: Arc<CapabilityRegistry>,
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
        support_rules: Arc<Vec<SupportCapabilityRule>>,
        intent_classifier_prompt: String,
        capability_nudge_prompt: String,
        compaction_prompt: String,
        system_prompt: impl Into<String>,
        status_tx: broadcast::Sender<StatusEvent>,
        cancel_registry: CancellationRegistry,
    ) -> Self {
        let vizier = kameo::spawn(Vizier::new(
            user_id.clone(),
            session_id.clone(),
            llm.clone(),
            tool_registry,
            capability_registry,
            model_registry.clone(),
            route_rules.clone(),
            store.clone(),
            support_rules.clone(),
            intent_classifier_prompt.clone(),
            capability_nudge_prompt.clone(),
            status_tx.clone(),
            cancel_registry.clone(),
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
            route_rules,
            support_rules,
            intent_classifier_prompt,
            capability_nudge_prompt,
            compaction_prompt,
            status_tx,
            cancel_registry,
        }
    }

    fn vizier_llm(&self) -> Arc<dyn LlmClient> { self.llm.clone() }

    fn spawn_vizier(
        &self,
        tool_registry: Arc<ToolRegistry>,
        capability_registry: Arc<CapabilityRegistry>,
    ) -> ActorRef<Vizier> {
        kameo::spawn(Vizier::new(
            self.user_id.clone(),
            self.session_id.clone(),
            self.llm.clone(),
            tool_registry,
            capability_registry,
            self.model_registry.clone(),
            self.route_rules.clone(),
            self.store.clone(),
            self.support_rules.clone(),
            self.intent_classifier_prompt.clone(),
            self.capability_nudge_prompt.clone(),
            self.status_tx.clone(),
            self.cancel_registry.clone(),
        ))
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

    async fn compact_history_if_needed(&mut self) {
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
    }

    async fn try_inline_reply(
        &self,
        context: Vec<ChatMessage>,
        text: &str,
    ) -> Result<Option<ChatReply>, MaatError> {
        if !should_answer_inline(text) {
            return Ok(None);
        }
        let response = self.llm.complete(context, &[]).await?;
        if matches!(response.stop_reason, maat_core::StopReason::ToolUse) {
            return Ok(None);
        }
        Ok(Some(ChatReply {
            content: response.content,
            usage: response.usage,
            latency_ms: response.latency_ms,
        }))
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
                    "[LATEST ARTIFACT] handle={} kind={} mime={} name={} summary={} Use the artifact handle with tools when possible; do not rely on raw storage paths unless a tool explicitly requires them.",
                    artifact.handle, artifact.kind, artifact.mime_type, artifact.display_name, artifact.summary
                )));
            }
        }
        context
    }

    async fn build_context_for_message(
        &self,
        text: &str,
        attached_artifacts: &[ArtifactRecord],
    ) -> Vec<ChatMessage> {
        let mut context = self.build_context_for_text(text).await;
        for artifact in attached_artifacts {
            context.push(ChatMessage::system(format!(
                "[ATTACHED ARTIFACT] handle={} kind={} mime={} name={} summary={} Use the artifact handle with tools when possible.",
                artifact.handle, artifact.kind, artifact.mime_type, artifact.display_name, artifact.summary
            )));
        }
        attach_image_inputs_to_current_turn(&mut context, text, attached_artifacts);
        context
    }
}

impl kameo::message::Message<ReloadRuntime> for NamedSession {
    type Reply = Result<(), MaatError>;

    async fn handle(
        &mut self,
        msg: ReloadRuntime,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.vizier = self.spawn_vizier(msg.tool_registry, msg.capability_registry);
        Ok(())
    }
}

// ─────────────────────────────────────────────
// SessionChat — one user turn
// ─────────────────────────────────────────────

pub struct SessionChat {
    pub text: String,
    pub attached_artifacts: Vec<ArtifactRecord>,
    pub cancel_key: Option<String>,
}

impl kameo::message::Message<SessionChat> for NamedSession {
    type Reply = Result<ChatReply, MaatError>;

    async fn handle(
        &mut self,
        SessionChat { text, attached_artifacts, cancel_key }: SessionChat,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let trace_id = TraceId::new();
        if cancel_key.as_deref().is_some_and(|key| self.cancel_registry.is_cancelled(key)) {
            self.emit(&trace_id, SessionState::Cancelled);
            return Err(MaatError::Cancelled);
        }

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
        let context = if attached_artifacts.is_empty() {
            self.build_context_for_text(&text).await
        } else {
            self.build_context_for_message(&text, &attached_artifacts).await
        };
        let imported_lines = format_artifact_lines("Attached artifacts", &attached_artifacts);

        if attached_artifacts.is_empty() {
            if let Some(reply) = self.try_inline_reply(context.clone(), &text).await? {
                let asst_msg = ChatMessage::assistant(&reply.content);
                self.persist_message(&asst_msg).await;
                self.history.push(asst_msg);
                self.last_summary = reply.content.chars().take(80).collect::<String>()
                    + if reply.content.len() > 80 { "…" } else { "" };
                self.compact_history_if_needed().await;
                self.emit(&trace_id, SessionState::Idle);
                return Ok(reply);
            }
        }

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
                cancel_key,
            }))
            .send()
            .await
            .map_err(|e| MaatError::Actor(e.to_string()))?;

        let content = merge_artifact_notice(imported_lines, match result.outcome {
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
        });

        let asst_msg = ChatMessage::assistant(&content);
        self.persist_message(&asst_msg).await;
        self.history.push(asst_msg);
        self.compact_history_if_needed().await;

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

fn attach_image_inputs_to_current_turn(
    context: &mut Vec<ChatMessage>,
    text: &str,
    attached_artifacts: &[ArtifactRecord],
) {
    let image_inputs = attached_artifacts
        .iter()
        .filter(|artifact| artifact.mime_type.starts_with("image/"))
        .map(|artifact| ChatImageInput {
            mime_type: artifact.mime_type.clone(),
            label: artifact.handle.clone(),
            source_path: Some(artifact.storage_path.clone()),
            data_base64: None,
        })
        .collect::<Vec<_>>();
    if image_inputs.is_empty() {
        return;
    }

    if let Some(current_user) = context.iter_mut().rev().find(|message| {
        matches!(message.role, maat_core::Role::User) && message.content.trim() == text.trim()
    }) {
        current_user.image_inputs.extend(image_inputs);
    } else {
        context.push(ChatMessage::user_with_images(text.to_string(), image_inputs));
    }
}

fn format_artifact_lines(label: &str, artifacts: &[ArtifactRecord]) -> Option<String> {
    if artifacts.is_empty() {
        return None;
    }
    let lines = artifacts
        .iter()
        .map(|artifact| {
            format!(
                "  - {}  {}  {}",
                artifact.handle, artifact.mime_type, artifact.display_name
            )
        })
        .collect::<Vec<_>>();
    Some(format!("{label}:\n{}", lines.join("\n")))
}

fn merge_artifact_notice(notice: Option<String>, content: String) -> String {
    match (notice, content.trim().is_empty()) {
        (Some(notice), true) => notice,
        (Some(notice), false) => format!("{notice}\n\n{content}"),
        (None, _) => content,
    }
}

fn should_answer_inline(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() || lower.len() > 220 {
        return false;
    }

    let action_terms = [
        "email", "mail", "send", "attach", "calendar", "schedule", "search", "find",
        "read", "write", "save", "pdf", "image", "draw", "render", "edit ", "artifact",
        "upload", "import", "browse", "list files", "tool", "skill", "session", "model",
        "auth ", "secret ", "config ", "/",
    ];
    if action_terms.iter().any(|term| lower.contains(term)) {
        return false;
    }

    let time_sensitive = [
        "latest", "current", "today", "tomorrow", "yesterday", "news", "price", "weather",
        "stock", "score", "recent",
    ];
    if time_sensitive.iter().any(|term| lower.contains(term)) {
        return false;
    }

    true
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
