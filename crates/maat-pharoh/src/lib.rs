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
    BackgroundRunStatus, CancellationRegistry, CapabilityKind, CapabilityRegistry, ChatImageInput, ChatMessage, ChatReply,
    HeraldAttachment, HeraldPayload, MaatError, ModelRegistry, ModelRouteRule, ModelRouteScope,
    ModelSpec, ParsedCommand, ResourceBudget, RetryPolicy, SessionId, SessionName, SessionState,
    SessionSummary, StatusEvent, StatusKind, StepId, SupportCapabilityRule, ToolRegistry,
    TraceId, UserId,
};
use maat_config::{
    delete_automation, describe_schedule, find_automation, parse_schedule_expr,
    slugify_automation_id, upsert_automation, default_skill_dirs, install_skill,
    load_automations, load_installed_skills, prompts::PromptLibrary, search_clawhub,
    set_automation_status, AutomationSpec, AutomationStatus, InstallSource, MaatConfig,
    SecretResolver,
};
use maat_llm::LlmClient;
use maat_memory::{
    ArtifactRecord, AutomationRunRecord, BackgroundRunRecord,
    window::{build_window, total_history_tokens, window_keep_count},
    ContextConfig, MemoryStore, SessionMeta, StoredMessage,
};
use maat_vizier::{Dispatch, Vizier, VizierTask};
use serde_json::{json, Value};
use session::{GetStatusLine, GetSummary, NamedSession, PurgeSession, ReloadRuntime, SessionChat, SetModel};
use tokio::sync::broadcast;
use tokio::task::{self, AbortHandle};
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
    active_run_tasks: HashMap<String, AbortHandle>,
    llm: Arc<dyn LlmClient>,
    tool_registry: Arc<ToolRegistry>,
    store: Arc<dyn MemoryStore>,
    ctx_config: ContextConfig,
    model: ModelSpec,
    model_registry: Arc<ModelRegistry>,
    route_rules: Arc<Vec<ModelRouteRule>>,
    support_rules: Arc<Vec<SupportCapabilityRule>>,
    capability_registry: Arc<CapabilityRegistry>,
    prompts: PromptLibrary,
    config: Arc<MaatConfig>,
    resolver: Arc<SecretResolver>,
    status_tx: broadcast::Sender<StatusEvent>,
    cancel_registry: CancellationRegistry,
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
        support_rules: Arc<Vec<SupportCapabilityRule>>,
        capability_registry: Arc<CapabilityRegistry>,
        prompts: PromptLibrary,
        config: Arc<MaatConfig>,
        resolver: Arc<SecretResolver>,
        status_tx: broadcast::Sender<StatusEvent>,
        cancel_registry: CancellationRegistry,
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
            active_run_tasks: HashMap::new(),
            llm,
            tool_registry,
            store,
            ctx_config,
            model,
            model_registry,
            route_rules,
            support_rules,
            capability_registry,
            prompts,
            config,
            resolver,
            status_tx,
            cancel_registry,
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

    fn spawn_primary_vizier(&self) -> ActorRef<Vizier> {
        kameo::spawn(Vizier::new(
            self.user_id.clone(),
            self.session_id.clone(),
            self.llm.clone(),
            self.tool_registry.clone(),
            self.capability_registry.clone(),
            self.model_registry.clone(),
            self.route_rules.clone(),
            self.store.clone(),
            self.support_rules.clone(),
            self.prompts.intent_classifier.clone(),
            self.prompts.capability_nudge.clone(),
            self.status_tx.clone(),
            self.cancel_registry.clone(),
        ))
    }

    async fn skills_reload(&mut self) -> Result<ChatReply, MaatError> {
        let skill_dirs = default_skill_dirs(&self.config.skills.dirs);
        let loaded = load_installed_skills(&skill_dirs);

        let mut registry = (*self.tool_registry).clone();
        loaded.register_tools(&mut registry);
        let tool_registry = Arc::new(registry);
        let capability_registry = Arc::new(tool_registry.capability_registry());

        self.tool_registry = tool_registry.clone();
        self.capability_registry = capability_registry.clone();
        self.rebuild_system_prompt();
        self.primary_vizier = self.spawn_primary_vizier();

        for entry in self.sessions.values_mut() {
            let _ = entry
                .actor
                .ask(ReloadRuntime {
                    tool_registry: tool_registry.clone(),
                    capability_registry: capability_registry.clone(),
                })
                .send()
                .await;
        }

        Ok(quick_reply(&format!(
            "Reloaded {} installed skills into the live registries. New skills are now available without restarting.",
            loaded.all().len()
        )))
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

    async fn resolve_artifact_handles(
        &self,
        handles: &[String],
    ) -> Result<Vec<ArtifactRecord>, MaatError> {
        let mut resolved = Vec::new();
        for handle in handles {
            if let Some(record) = self
                .store
                .get_artifact_by_handle(&self.user_id.0, handle)
                .await?
            {
                resolved.push(record);
            }
        }
        Ok(resolved)
    }

    async fn import_attachments(
        &self,
        attachments: &[HeraldAttachment],
    ) -> Result<Vec<ArtifactRecord>, MaatError> {
        let mut imported = Vec::new();
        for attachment in attachments {
            let record = self
                .store
                .import_artifact(
                    &self.user_id.0,
                    &self.session_id.0.to_string(),
                    &PathBuf::from(&attachment.pointer),
                )
                .await?;
            imported.push(record);
        }
        Ok(imported)
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

    async fn compact_history_if_needed(&mut self) {
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

    async fn handle_primary_with_attachments(
        &mut self,
        text: String,
        attachments: Vec<HeraldAttachment>,
        artifact_handles: Vec<String>,
    ) -> Result<ChatReply, MaatError> {
        let imported = self.import_attachments(&attachments).await?;
        let referenced = self.resolve_artifact_handles(&artifact_handles).await?;
        let mut all_attached = imported.clone();
        all_attached.extend(referenced.clone());
        let imported_lines = format_artifact_lines("Attached artifacts", &all_attached);
        let trace_id = TraceId::new();
        info!(user = %self.user_id, chars = text.len(), attachments = all_attached.len(), "primary session with attachments");

        self.emit(&trace_id, SessionState::Running { step_id: StepId::new() });
        let user_msg = ChatMessage::user(&text);
        self.persist_message(&user_msg).await;
        self.history.push(user_msg);
        let context = self.build_context_for_message(&text, &all_attached).await;

        if let Some(reply) = self
            .try_direct_skill_reply(&text, &all_attached)
            .await?
        {
            let content = merge_artifact_notice(imported_lines, reply.content.clone());
            let asst_msg = ChatMessage::assistant(&content);
            self.persist_message(&asst_msg).await;
            self.history.push(asst_msg);
            self.compact_history_if_needed().await;
            self.emit(&trace_id, SessionState::Idle);
            return Ok(ChatReply { content, usage: reply.usage, latency_ms: reply.latency_ms });
        }

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
                cancel_key: None,
            }))
            .send()
            .await
            .map_err(|e| MaatError::Actor(e.to_string()))?;

        let content = merge_artifact_notice(
            imported_lines,
            self.outcome_to_content(&trace_id, result_env.outcome).await?,
        );
        let asst_msg = ChatMessage::assistant(&content);
        self.persist_message(&asst_msg).await;
        self.history.push(asst_msg);
        self.compact_history_if_needed().await;

        self.emit(&trace_id, SessionState::Idle);
        Ok(ChatReply { content, usage: result_env.usage, latency_ms: result_env.latency_ms })
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

        if let Some(reply) = self.try_direct_skill_reply(&text, &[]).await? {
            let asst_msg = ChatMessage::assistant(&reply.content);
            self.persist_message(&asst_msg).await;
            self.history.push(asst_msg);
            self.compact_history_if_needed().await;
            self.emit(&trace_id, SessionState::Idle);
            return Ok(reply);
        }

        if let Some(reply) = self.try_inline_reply(context.clone(), &text).await? {
            let asst_msg = ChatMessage::assistant(&reply.content);
            self.persist_message(&asst_msg).await;
            self.history.push(asst_msg);
            self.compact_history_if_needed().await;
            self.emit(&trace_id, SessionState::Idle);
            return Ok(reply);
        }

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
                cancel_key: None,
            }))
            .send()
            .await
            .map_err(|e| MaatError::Actor(e.to_string()))?;

        let content = self.outcome_to_content(&trace_id, result_env.outcome).await?;

        let asst_msg = ChatMessage::assistant(&content);
        self.persist_message(&asst_msg).await;
        self.history.push(asst_msg);
        self.compact_history_if_needed().await;

        self.emit(&trace_id, SessionState::Idle);
        Ok(ChatReply { content, usage: result_env.usage, latency_ms: result_env.latency_ms })
    }

    async fn try_direct_skill_reply(
        &self,
        text: &str,
        attached_artifacts: &[ArtifactRecord],
    ) -> Result<Option<ChatReply>, MaatError> {
        let Some(invocation) = self.extract_direct_skill_invocation(text, attached_artifacts).await? else {
            return Ok(None);
        };
        let result = self
            .tool_registry
            .call_by_name(&invocation.tool_name, invocation.input)
            .await?;
        Ok(Some(ChatReply {
            content: self.direct_skill_result_to_content(&invocation.tool_name, result).await?,
            usage: maat_core::TokenUsage::default(),
            latency_ms: 0,
        }))
    }

    async fn extract_direct_skill_invocation(
        &self,
        text: &str,
        attached_artifacts: &[ArtifactRecord],
    ) -> Result<Option<DirectSkillInvocation>, MaatError> {
        let lower = text.to_ascii_lowercase();
        if !["use ", "run ", "call "].iter().any(|needle| lower.starts_with(needle) || lower.contains(needle)) {
            return Ok(None);
        }

        let mut selected_skill: Option<(String, usize)> = None;
        for card in self.capability_registry.all() {
            if !matches!(card.kind, CapabilityKind::Skill(_)) {
                continue;
            }
            let id = card.id.0.to_ascii_lowercase();
            let name = card.name.to_ascii_lowercase();
            let mut best_match = 0usize;
            if lower.contains(&id) {
                best_match = best_match.max(id.len());
            }
            if lower.contains(&name) {
                best_match = best_match.max(name.len());
            }
            if best_match > 0 && selected_skill.as_ref().is_none_or(|(_, current)| best_match > *current) {
                selected_skill = Some((card.id.0, best_match));
            }
        }
        let Some((tool_name, _)) = selected_skill else {
            return Ok(None);
        };

        let mut input = json!({ "request": text });
        if let Some(handle) = extract_artifact_handle(text) {
            input["artifact_handle"] = Value::String(handle.to_string());
        } else if let Some(first) = attached_artifacts.first() {
            input["artifact_handle"] = Value::String(first.handle.clone());
        } else if let Some(input_path) = extract_input_path(text) {
            input["input_path"] = Value::String(input_path);
        }
        if let Some(output_path) = extract_output_path(text) {
            input["output_path"] = Value::String(output_path);
        }

        if let Some(handle) = input.get("artifact_handle").and_then(|value| value.as_str()) {
            let artifact = self
                .store
                .get_artifact_by_handle(&self.user_id.0, handle)
                .await?
                .ok_or_else(|| MaatError::Tool(format!("unknown artifact handle: {handle}")))?;
            if input.get("input_path").and_then(|value| value.as_str()).is_none() {
                input["input_path"] = Value::String(artifact.storage_path.clone());
            }
            if request_is_empty_or_mentions_artifact(input.get("request").and_then(|value| value.as_str())) {
                input["request"] = Value::String(artifact.storage_path);
            }
        }

        Ok(Some(DirectSkillInvocation { tool_name, input }))
    }

    async fn direct_skill_result_to_content(
        &self,
        tool_name: &str,
        result: Value,
    ) -> Result<String, MaatError> {
        let result_json = result.get("result").cloned().unwrap_or(Value::Null);
        let message = result_json
            .get("message")
            .and_then(|value| value.as_str())
            .or_else(|| result.get("message").and_then(|value| value.as_str()))
            .unwrap_or("skill completed");

        if let Some(output_path) = result_json
            .get("output_path")
            .and_then(|value| value.as_str())
            .or_else(|| result.get("output_path").and_then(|value| value.as_str()))
        {
            let output_path = PathBuf::from(output_path);
            if output_path.is_file() {
                let artifact = self
                    .store
                    .import_artifact(&self.user_id.0, &self.session_id.0.to_string(), &output_path)
                    .await?;
                return Ok(format!(
                    "{message}\n\nCreated artifacts:\n  - {}  {}  {}",
                    artifact.handle, artifact.mime_type, artifact.display_name
                ));
            }
        }

        Ok(format!("{tool_name} completed: {message}"))
    }

    async fn outcome_to_content(
        &mut self,
        trace_id: &TraceId,
        outcome: maat_core::TaskOutcome,
    ) -> Result<String, MaatError> {
        match outcome {
            maat_core::TaskOutcome::Success { content, generated_artifacts, .. } => {
                let artifact_lines = self.persist_generated_artifacts(&generated_artifacts).await?;
                Ok(if artifact_lines.is_empty() {
                    content
                } else if content.trim().is_empty() {
                    format!("Created artifacts:\n{}", artifact_lines.join("\n"))
                } else {
                    format!("{content}\n\nGenerated artifacts:\n{}", artifact_lines.join("\n"))
                })
            }
            maat_core::TaskOutcome::Failed { error, .. } => {
                self.emit(trace_id, SessionState::Failed { error: error.clone() });
                Err(MaatError::Llm(error))
            }
            maat_core::TaskOutcome::TimedOut => {
                self.emit(trace_id, SessionState::Failed { error: "timed out".into() });
                Err(MaatError::Llm("timed out".into()))
            }
            maat_core::TaskOutcome::Cancelled => {
                self.emit(trace_id, SessionState::Cancelled);
                Err(MaatError::Llm("cancelled".into()))
            }
        }
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
            ParsedCommand::SkillsReload => self.skills_reload().await,
            ParsedCommand::AutomationsList => self.automations_list().await,
            ParsedCommand::AutomationShow { name } => self.automation_show(name).await,
            ParsedCommand::AutomationRun { name } => self.automation_run(name).await,
            ParsedCommand::AutomationPause { name } => self.automation_pause(name).await,
            ParsedCommand::AutomationResume { name } => self.automation_resume(name).await,
            ParsedCommand::AutomationCreate { name, schedule, prompt } => {
                self.automation_create(name, schedule, prompt).await
            }
            ParsedCommand::AutomationEdit { name, schedule, prompt } => {
                self.automation_edit(name, schedule, prompt).await
            }
            ParsedCommand::AutomationDelete { name } => self.automation_delete(name).await,
            ParsedCommand::RunsList => self.runs_list().await,
            ParsedCommand::RunShow { handle } => self.run_show(handle).await,
            ParsedCommand::RunStart { title, prompt } => self.run_start(title, prompt).await,
            ParsedCommand::RunOpen { handle } => self.run_open(handle).await,
            ParsedCommand::RunCancel { handle } => self.run_cancel(handle).await,
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
                    .ask(SessionChat {
                        text: message,
                        attached_artifacts: Vec::new(),
                        cancel_key: None,
                    })
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
            self.support_rules.clone(),
            self.prompts.intent_classifier.clone(),
            self.prompts.capability_nudge.clone(),
            self.prompts.compaction.clone(),
            system_prompt,
            self.status_tx.clone(),
            self.cancel_registry.clone(),
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

    async fn route_to_session_with_attachments(
        &mut self,
        name: SessionName,
        message: String,
        attachments: Vec<HeraldAttachment>,
        artifact_handles: Vec<String>,
    ) -> Result<ChatReply, MaatError> {
        if !self.sessions.contains_key(&name) && should_autocreate_channel_session(&name) {
            let description = describe_channel_session(&name);
            let _ = self.session_new(name.clone(), description).await?;
        }
        if !self.sessions.contains_key(&name) {
            return Ok(quick_reply(&format!("No session '{}' found.", name)));
        }
        let imported = self.import_attachments(&attachments).await?;
        let referenced = self.resolve_artifact_handles(&artifact_handles).await?;
        let mut all_attached = imported;
        all_attached.extend(referenced);
        match self.sessions.get_mut(&name) {
            Some(entry) => {
                let reply = entry
                    .actor
                    .ask(SessionChat {
                        text: message,
                        attached_artifacts: all_attached,
                        cancel_key: None,
                    })
                    .send()
                    .await
                    .map_err(|e| MaatError::Actor(e.to_string()))?;
                entry.summary.last_active_ms = maat_core::now_ms();
                Ok(reply)
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

    async fn automations_list(&self) -> Result<ChatReply, MaatError> {
        let specs = load_automations(&self.config.automations.dir)
            .map_err(|e| MaatError::Config(e.to_string()))?;
        if specs.is_empty() {
            return Ok(quick_reply(
                "No automations configured yet. Add TOML specs under the automations directory.",
            ));
        }

        let mut lines = vec![format!("Automations ({}):", specs.len())];
        for spec in specs {
            let latest = self
                .store
                .latest_automation_run(&spec.id)
                .await?
                .map(|run| format!(" last:{} ", summarize_run_time(run.finished_at_ms)))
                .unwrap_or_default();
            lines.push(format!(
                "  {}  —  {:?} {}{}",
                spec.name, spec.status, describe_schedule(&spec.schedule), latest
            ));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn automation_show(&self, name: String) -> Result<ChatReply, MaatError> {
        let spec = find_automation(&self.config.automations.dir, &name)
            .map_err(|e| MaatError::Config(e.to_string()))?;
        let Some(spec) = spec else {
            return Ok(quick_reply(&format!("No automation '{name}' found.")));
        };
        let runs = self.store.list_automation_runs(&spec.id, 5).await?;
        let mut lines = vec![
            format!("Automation: {}", spec.name),
            format!("  id: {}", spec.id),
            format!("  status: {:?}", spec.status),
            format!("  schedule: {}", describe_schedule(&spec.schedule)),
            format!("  session: {}", spec.session.clone().unwrap_or_else(|| "primary".into())),
            format!("  delivery: {}", describe_automation_delivery(spec.delivery.as_ref())),
            format!("  prompt: {}", spec.prompt),
        ];
        if runs.is_empty() {
            lines.push("  runs: none yet".into());
        } else {
            lines.push("  recent runs:".into());
            for run in runs {
                lines.push(format!(
                    "    - {} {} {}",
                    summarize_run_time(run.finished_at_ms),
                    run.status,
                    run.summary
                ));
            }
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn automation_run(&mut self, name: String) -> Result<ChatReply, MaatError> {
        let spec = find_automation(&self.config.automations.dir, &name)
            .map_err(|e| MaatError::Config(e.to_string()))?;
        let Some(spec) = spec else {
            return Ok(quick_reply(&format!("No automation '{name}' found.")));
        };
        let run = self.execute_automation(&spec).await?;
        Ok(quick_reply(&format!(
            "Automation '{}' started as background run `{}`.\nSession: @{}\nStatus: {:?}",
            spec.name, run.handle, run.session_name, run.status
        )))
    }

    async fn automation_pause(&self, name: String) -> Result<ChatReply, MaatError> {
        let spec = set_automation_status(&self.config.automations.dir, &name, AutomationStatus::Paused)
            .map_err(|e| MaatError::Config(e.to_string()))?;
        match spec {
            Some(spec) => Ok(quick_reply(&format!("Paused automation '{}'.", spec.name))),
            None => Ok(quick_reply(&format!("No automation '{name}' found."))),
        }
    }

    async fn automation_resume(&self, name: String) -> Result<ChatReply, MaatError> {
        let spec = set_automation_status(&self.config.automations.dir, &name, AutomationStatus::Active)
            .map_err(|e| MaatError::Config(e.to_string()))?;
        match spec {
            Some(spec) => Ok(quick_reply(&format!("Resumed automation '{}'.", spec.name))),
            None => Ok(quick_reply(&format!("No automation '{name}' found."))),
        }
    }

    async fn automation_create(
        &self,
        name: String,
        schedule: String,
        prompt: String,
    ) -> Result<ChatReply, MaatError> {
        let schedule = parse_schedule_expr(&schedule)
            .map_err(MaatError::Config)?;
        let spec = AutomationSpec {
            id: slugify_automation_id(&name),
            name: name.clone(),
            prompt,
            status: AutomationStatus::Active,
            schedule,
            session: Some("automation".into()),
            delivery: None,
        };
        let _ = upsert_automation(&self.config.automations.dir, &spec)
            .map_err(|e| MaatError::Config(e.to_string()))?;
        Ok(quick_reply(&format!(
            "Created automation '{}'. Schedule: {}",
            spec.name,
            describe_schedule(&spec.schedule)
        )))
    }

    async fn automation_edit(
        &self,
        name: String,
        schedule: String,
        prompt: String,
    ) -> Result<ChatReply, MaatError> {
        let Some(mut spec) = find_automation(&self.config.automations.dir, &name)
            .map_err(|e| MaatError::Config(e.to_string()))? else {
            return Ok(quick_reply(&format!("No automation '{name}' found.")));
        };
        spec.schedule = parse_schedule_expr(&schedule).map_err(MaatError::Config)?;
        spec.prompt = prompt;
        let _ = upsert_automation(&self.config.automations.dir, &spec)
            .map_err(|e| MaatError::Config(e.to_string()))?;
        Ok(quick_reply(&format!(
            "Updated automation '{}'. Schedule: {}",
            spec.name,
            describe_schedule(&spec.schedule)
        )))
    }

    async fn automation_delete(&self, name: String) -> Result<ChatReply, MaatError> {
        match delete_automation(&self.config.automations.dir, &name)
            .map_err(|e| MaatError::Config(e.to_string()))? {
            Some(spec) => Ok(quick_reply(&format!("Deleted automation '{}'.", spec.name))),
            None => Ok(quick_reply(&format!("No automation '{name}' found."))),
        }
    }

    pub async fn execute_automation(&mut self, spec: &AutomationSpec) -> Result<BackgroundRunRecord, MaatError> {
        self.start_background_run(
            format!("automation {}", spec.name),
            format!("[AUTOMATION {}]\n{}", spec.name, spec.prompt),
            spec.session.clone().map(SessionName),
            Some(spec.id.clone()),
            Some(spec.name.clone()),
        )
        .await
    }

    async fn runs_list(&self) -> Result<ChatReply, MaatError> {
        let runs = self.store.list_background_runs(&self.user_id.0, 20).await?;
        if runs.is_empty() {
            return Ok(quick_reply("No background runs yet. Start one with: /run start <title>: <prompt>"));
        }
        let mut lines = vec![format!("Background runs ({} shown):", runs.len())];
        for run in runs {
            lines.push(format!(
                "  {}  —  {:?}  @{}  {}",
                run.handle, run.status, run.session_name, run.title
            ));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn run_show(&self, handle: String) -> Result<ChatReply, MaatError> {
        let Some(run) = self
            .store
            .get_background_run_by_handle(&self.user_id.0, &handle)
            .await? else {
            return Ok(quick_reply(&format!("No background run '{}' found.", handle)));
        };
        let mut lines = vec![
            format!("Background run `{}`", run.handle),
            format!("  title: {}", run.title),
            format!("  status: {:?}", run.status),
            format!("  session: @{}", run.session_name),
            format!("  created: {}", summarize_run_time(run.created_at_ms)),
            format!("  started: {}", summarize_run_time(run.started_at_ms)),
            format!("  summary: {}", run.summary),
        ];
        if let Some(finished_at_ms) = run.finished_at_ms {
            lines.push(format!("  finished: {}", summarize_run_time(finished_at_ms)));
        }
        if let Some(error) = run.error {
            lines.push(format!("  error: {}", error));
        }
        Ok(quick_reply(&lines.join("\n")))
    }

    async fn run_start(&mut self, title: String, prompt: String) -> Result<ChatReply, MaatError> {
        let run = self
            .start_background_run(
                title.clone(),
                prompt,
                None,
                None,
                None,
            )
            .await?;
        Ok(quick_reply(&format!(
            "Started background run `{}`.\nSession: @{}\nUse /run show {} or /session use {}",
            run.handle, run.session_name, run.handle, run.session_name
        )))
    }

    async fn run_open(&self, handle: String) -> Result<ChatReply, MaatError> {
        let Some(run) = self
            .store
            .get_background_run_by_handle(&self.user_id.0, &handle)
            .await? else {
            return Ok(quick_reply(&format!("No background run '{}' found.", handle)));
        };
        Ok(quick_reply(&format!(
            "Background run `{}` is attached to session @{}.\nIn the TUI use: /session use {}",
            run.handle, run.session_name, run.session_name
        )))
    }

    async fn run_cancel(&mut self, handle: String) -> Result<ChatReply, MaatError> {
        let Some(mut run) = self
            .store
            .get_background_run_by_handle(&self.user_id.0, &handle)
            .await? else {
            return Ok(quick_reply(&format!("No background run '{}' found.", handle)));
        };

        match run.status {
            BackgroundRunStatus::Completed => {
                return Ok(quick_reply(&format!("Background run '{}' is already completed.", handle)));
            }
            BackgroundRunStatus::Failed => {
                return Ok(quick_reply(&format!("Background run '{}' has already failed.", handle)));
            }
            BackgroundRunStatus::Cancelled => {
                return Ok(quick_reply(&format!("Background run '{}' is already marked cancelled.", handle)));
            }
            BackgroundRunStatus::Queued | BackgroundRunStatus::Running => {}
        }

        if let Some(abort_handle) = self.active_run_tasks.remove(&run.handle) {
            abort_handle.abort();
        }
        self.cancel_registry.request_cancel(run.handle.clone());
        run.status = BackgroundRunStatus::Cancelled;
        run.summary = "cancellation requested".into();
        run.error = Some("run cancellation requested".into());
        run.finished_at_ms = Some(maat_core::now_ms());
        self.store.save_background_run(&run).await?;

        Ok(quick_reply(&format!(
            "Cancellation requested for background run `{}`.\nSession: @{}\nNote: this stops MAAT's detached tracking immediately, but already-started model/tool work may still wind down in the session.",
            run.handle, run.session_name
        )))
    }

    async fn start_background_run(
        &mut self,
        title: String,
        prompt: String,
        session_name: Option<SessionName>,
        automation_id: Option<String>,
        automation_name: Option<String>,
    ) -> Result<BackgroundRunRecord, MaatError> {
        let handle = self.store.allocate_background_run_handle(&title).await?;
        self.cancel_registry.clear(&handle);
        let session_name = session_name.unwrap_or_else(|| SessionName(handle.clone()));
        if !self.sessions.contains_key(&session_name) {
            let _ = self
                .session_new(
                    session_name.clone(),
                    format!("background run session for {}", title),
                )
                .await?;
        }
        let actor = self
            .sessions
            .get(&session_name)
            .map(|entry| entry.actor.clone())
            .ok_or_else(|| MaatError::Actor(format!("missing background session '{}'", session_name)))?;

        let now_ms = maat_core::now_ms();
        let run = BackgroundRunRecord {
            run_id: ulid::Ulid::new().to_string(),
            handle: handle.clone(),
            user_id: self.user_id.0.clone(),
            parent_session_id: self.session_id.0.to_string(),
            session_name: session_name.0.clone(),
            title: title.clone(),
            prompt: prompt.clone(),
            status: BackgroundRunStatus::Running,
            summary: "run started".into(),
            error: None,
            created_at_ms: now_ms,
            started_at_ms: now_ms,
            finished_at_ms: None,
        };
        self.store.save_background_run(&run).await?;

        let store = self.store.clone();
        let status_tx = self.status_tx.clone();
        let user_id = self.user_id.clone();
        let run_clone = run.clone();
        let trace_prompt = prompt.clone();
        let join = task::spawn(async move {
            let result = actor
                .ask(SessionChat {
                    text: trace_prompt,
                    attached_artifacts: Vec::new(),
                    cancel_key: Some(handle.clone()),
                })
                .send()
                .await;

            let mut updated = run_clone.clone();
            updated.finished_at_ms = Some(maat_core::now_ms());
            if let Ok(Some(existing)) = store
                .get_background_run_by_handle(&run_clone.user_id, &run_clone.handle)
                .await
            {
                if existing.status == BackgroundRunStatus::Cancelled {
                    return;
                }
            }
            match result {
                Ok(reply) => {
                    updated.status = BackgroundRunStatus::Completed;
                    updated.summary = reply
                        .content
                        .lines()
                        .next()
                        .unwrap_or("background run completed")
                        .to_string();
                    updated.error = None;
                }
                Err(error) => {
                    updated.status = BackgroundRunStatus::Failed;
                    updated.summary = "background run failed".into();
                    updated.error = Some(error.to_string());
                }
            }
            let _ = store.save_background_run(&updated).await;

            let _ = status_tx.send(StatusEvent::new(
                maat_core::ComponentAddress::Pharoh(user_id.clone()),
                maat_core::TraceId::new(),
                StatusKind::RunCompleted {
                    handle: updated.handle.clone(),
                    session_name: updated.session_name.clone(),
                    title: updated.title.clone(),
                    status: updated.status.clone(),
                    summary: updated.summary.clone(),
                    error: updated.error.clone(),
                    automation_id: automation_id.clone(),
                    started_at_ms: updated.started_at_ms,
                    finished_at_ms: updated.finished_at_ms.unwrap_or(updated.started_at_ms),
                },
            ));

            if let (Some(automation_id), Some(automation_name)) = (automation_id, automation_name) {
                let automation_run = AutomationRunRecord {
                    run_id: updated.run_id.clone(),
                    automation_id,
                    automation_name,
                    status: match updated.status {
                        BackgroundRunStatus::Completed => "ok".into(),
                        BackgroundRunStatus::Cancelled => "cancelled".into(),
                        BackgroundRunStatus::Queued => "queued".into(),
                        BackgroundRunStatus::Running => "running".into(),
                        BackgroundRunStatus::Failed => "failed".into(),
                    },
                    started_at_ms: updated.started_at_ms,
                    finished_at_ms: updated.finished_at_ms.unwrap_or(updated.started_at_ms),
                    summary: updated.summary.clone(),
                    error: updated.error.clone(),
                };
                let _ = store.save_automation_run(&automation_run).await;
            }
        });
        self.active_run_tasks
            .insert(run.handle.clone(), join.abort_handle());

        Ok(run)
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
                "No installed skills.\nInstall one with: /skills install <path-to-skill-directory>\nReload later with: /skills reload\nOr browse ClawHub with: /skills search <query>",
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

    async fn skill_install(&mut self, source: String) -> Result<ChatReply, MaatError> {
        let skill_dirs = default_skill_dirs(&self.config.skills.dirs);
        let dest_root = skill_dirs
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("skills"));
        let install_source = InstallSource::parse(&source);
        let source_label = source.clone();

        match tokio::task::spawn_blocking(move || install_skill(install_source, &dest_root)).await {
            Ok(Ok(skill)) => {
                let _ = self.skills_reload().await;
                Ok(quick_reply(&format!(
                    "Installed skill '{}' from {} into {}.\nWrote maat-skill.toml with trust/provenance metadata.\nReloaded live registries so it is available without restarting.",
                    skill.name,
                    source_label,
                    skill.path.display()
                )))
            }
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
             Once authenticated, the Gmail, Calendar, Drive, Docs, and Sheets Google scopes will be available.\n\
             If you previously authenticated with narrower scopes, run /auth google again to refresh the stored token."
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
            HeraldPayload::Message { text, attachments, artifact_handles, session } => {
                if let Some(name) = session {
                    self.route_to_session_with_attachments(name, text, attachments, artifact_handles).await
                } else if attachments.is_empty() && artifact_handles.is_empty() {
                    self.handle_primary(text).await
                } else {
                    self.handle_primary_with_attachments(text, attachments, artifact_handles).await
                }
            }
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

struct DirectSkillInvocation {
    tool_name: String,
    input: Value,
}

fn should_attach_recent_artifact(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_pronoun = [" it", "it ", "that", "this", "latest image", "latest artifact"]
        .iter()
        .any(|needle| lower.contains(needle));
    let has_action = ["email", "send", "attach", "share", "use"].iter().any(|needle| lower.contains(needle));
    has_pronoun && has_action
}

fn extract_artifact_handle(request: &str) -> Option<&str> {
    let lower = request.to_ascii_lowercase();
    let idx = lower.find("artifact ")?;
    let suffix = &request[idx + "artifact ".len()..];
    suffix
        .split_whitespace()
        .next()
        .map(|token| token.trim_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | '"' | '\'' | ')' | '(')))
        .filter(|token| !token.is_empty())
}

fn request_is_empty_or_mentions_artifact(request: Option<&str>) -> bool {
    let Some(request) = request else { return true };
    let trimmed = request.trim();
    trimmed.is_empty() || trimmed.to_ascii_lowercase().contains("artifact ")
}

fn extract_output_path(text: &str) -> Option<String> {
    const NEEDLES: &[&str] = &[
        "write the result to ",
        "write result to ",
        "write it to ",
        "save the result to ",
        "save result to ",
        "save it to ",
        "output to ",
    ];
    let lower = text.to_ascii_lowercase();
    for needle in NEEDLES {
        if let Some(idx) = lower.find(needle) {
            let suffix = &text[idx + needle.len()..];
            if let Some(path) = suffix.split_whitespace().next() {
                let path = path.trim_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | '"' | '\'' | ')' | '('));
                if !path.is_empty() {
                    return Some(path.to_string());
                }
            }
        }
    }
    None
}

fn extract_input_path(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let idx = lower.find(" on ")?;
    let suffix = text[idx + 4..].trim_start();
    if suffix.to_ascii_lowercase().starts_with("artifact ") {
        return None;
    }
    let token = suffix.split_whitespace().next()?;
    let token = token.trim_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | '"' | '\'' | ')' | '('));
    if token.starts_with('/') || token.starts_with("./") || token.starts_with("../") || token.contains('/') {
        return Some(token.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_direct_skill_artifact_request_shape() {
        let text = "Use image-rectify on artifact test12-png-p3rv and write the result to output/image-rectify/result.jpg";
        assert_eq!(extract_artifact_handle(text), Some("test12-png-p3rv"));
        assert_eq!(
            extract_output_path(text).as_deref(),
            Some("output/image-rectify/result.jpg")
        );
    }
}

fn should_autocreate_channel_session(name: &SessionName) -> bool {
    name.0.starts_with("telegram-")
}

fn describe_channel_session(name: &SessionName) -> String {
    if let Some(chat_id) = name.0.strip_prefix("telegram-") {
        return format!("telegram chat session for {}", chat_id);
    }
    format!("channel session for {}", name.0)
}

fn describe_automation_delivery(
    delivery: Option<&maat_config::AutomationDelivery>,
) -> String {
    match delivery {
        Some(maat_config::AutomationDelivery::Telegram { chat_id: Some(chat_id) }) => {
            format!("telegram:{}", chat_id)
        }
        Some(maat_config::AutomationDelivery::Telegram { chat_id: None }) => {
            "telegram:default".into()
        }
        None => "none".into(),
    }
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

fn summarize_run_time(ms: u64) -> String {
    let secs = (maat_core::now_ms().saturating_sub(ms)) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
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

fn parse_config_value(value: &str) -> toml::Value {
    let wrapped = format!("value = {value}");
    if let Ok(parsed) = toml::from_str::<toml::Value>(&wrapped) {
        if let Some(parsed_value) = parsed.get("value") {
            return parsed_value.clone();
        }
    }
    toml::Value::String(value.to_string())
}
