//! VIZIER — per-session orchestrator actor.
//!
//! Receives a task from PHAROH, wraps it in the envelope protocol,
//! spawns MINIONs to execute each step, handles retry, emits WorkflowState
//! events, and returns the final ResultEnvelope.
//!
//! Phase 4: single-step workflows only.
//! Phase 6: LLM-planned DAG workflows slot in here.

use std::sync::Arc;
use std::time::Duration;

use kameo::{request::MessageSend, Actor};
use maat_core::{
    CapabilityId, CapabilityKind, CapabilityRegistry, CapabilityTrust, ChatMessage,
    ComponentAddress, EnvelopeHeader, MaatError, ModelRegistry, ModelRouteRule, ModelRouteScope,
    ModelSelectionPolicy, ModelSpec, Priority, ResourceBudget, ResultEnvelope, RetryPolicy,
    Permission, Role, SessionId, StatusEvent, StatusKind, StepId, StepState, TaskEnvelope,
    TaskOutcome, TaskSpec, TraceId, ToolRegistry, UserId, WorkflowId, WorkflowState,
};
use maat_llm::{LlmClient, OpenAiCompatClient};
use maat_minions::{Minion, RunTask};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

// ─────────────────────────────────────────────
// Actor
// ─────────────────────────────────────────────

#[derive(Actor)]
pub struct Vizier {
    user_id: UserId,
    session_id: SessionId,
    tool_registry: Arc<ToolRegistry>,
    capability_registry: Arc<CapabilityRegistry>,
    model_registry: Arc<ModelRegistry>,
    route_rules: Arc<Vec<ModelRouteRule>>,
    capability_nudge_prompt: String,
    status_tx: broadcast::Sender<StatusEvent>,
}

impl Vizier {
    pub fn new(
        user_id: UserId,
        session_id: SessionId,
        _llm: Arc<dyn LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        capability_registry: Arc<CapabilityRegistry>,
        model_registry: Arc<ModelRegistry>,
        route_rules: Arc<Vec<ModelRouteRule>>,
        capability_nudge_prompt: String,
        status_tx: broadcast::Sender<StatusEvent>,
    ) -> Self {
        Self {
            user_id,
            session_id,
            tool_registry,
            capability_registry,
            model_registry,
            route_rules,
            capability_nudge_prompt,
            status_tx,
        }
    }

    fn emit(&self, trace_id: &TraceId, kind: StatusKind) {
        let source = ComponentAddress::Vizier(self.user_id.clone(), self.session_id.clone());
        let _ = self.status_tx.send(StatusEvent::new(source, trace_id.clone(), kind));
    }

    fn my_address(&self) -> ComponentAddress {
        ComponentAddress::Vizier(self.user_id.clone(), self.session_id.clone())
    }
}

// ─────────────────────────────────────────────
// Inbound message from PHAROH
// ─────────────────────────────────────────────

/// A task request from PHAROH — everything needed to build a single-step workflow.
pub struct VizierTask {
    pub trace_id: TraceId,
    pub description: String,
    pub messages: Vec<ChatMessage>,
    pub model: ModelSpec,
    pub model_policy: Option<ModelSelectionPolicy>,
    pub route_scope: ModelRouteScope,
    pub resource_budget: ResourceBudget,
    pub retry: RetryPolicy,
    /// Absolute unix-ms deadline; None = use MINION default (120s).
    pub deadline_ms: Option<u64>,
}

pub struct Dispatch(pub VizierTask);

impl kameo::message::Message<Dispatch> for Vizier {
    type Reply = Result<ResultEnvelope, MaatError>;

    async fn handle(
        &mut self,
        Dispatch(task): Dispatch,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let workflow_id = WorkflowId::new();
        let step_id = StepId::new();
        let trace_id = task.trace_id.clone();

        info!(
            workflow = ?workflow_id,
            model = %task.model.model_id,
            "vizier dispatching single-step workflow"
        );

        let capability_refs = self
            .select_capability_refs(&task.description, &task.messages, &task.model)
            .await;
        let intent_scope = detect_intent_scope(&task.description);
        let (talent_count, skill_count) = capability_refs
            .iter()
            .filter_map(|capability_id| self.capability_registry.get(capability_id))
            .fold((0usize, 0usize), |(talents, skills), card| match card.kind {
                CapabilityKind::Talent => (talents + 1, skills),
                CapabilityKind::Skill(_) => (talents, skills + 1),
                CapabilityKind::Workspace(_) => (talents, skills),
            });

        let selected_model = self.resolve_model(
            &task.route_scope,
            intent_scope.as_ref(),
            task.model_policy.as_ref(),
            &capability_refs,
            &task.model,
        );
        info!(
            workflow = ?workflow_id,
            route_scope = ?task.route_scope,
            selected_model = %selected_model.model_id,
            selected_profile = ?selected_model.profile_id,
            candidate_capabilities = capability_refs.len(),
            candidate_talents = talent_count,
            candidate_skills = skill_count,
            "vizier resolved model for task"
        );

        // ── WorkflowState: Running(0/1) ────────────────────────────
        self.emit(
            &trace_id,
            StatusKind::WorkflowState {
                workflow_id: workflow_id.clone(),
                state: WorkflowState::Running { completed: 0, total: 1 },
            },
        );

        // ── Build TaskEnvelope ──────────────────────────────────────
        let envelope = TaskEnvelope {
            header: {
                let mut h = EnvelopeHeader::new(
                    self.my_address(),
                    ComponentAddress::Minion(
                        self.user_id.clone(),
                        self.session_id.clone(),
                        step_id.clone(),
                    ),
                );
                h.trace_id = trace_id.clone();
                h.priority = Priority::Normal;
                h
            },
            step_id: step_id.clone(),
            workflow_id: workflow_id.clone(),
            task: TaskSpec {
                description: task.description,
                messages: trim_messages_for_intent(&task.messages, intent_scope.as_ref()),
                model: selected_model.clone(),
                model_policy: task.model_policy.clone(),
                capability_refs,
                retry: task.retry.clone(),
                allow_sub_vizier: false,
            },
            resource_budget: task.resource_budget,
            deadline_ms: task.deadline_ms,
        };

        // ── Spawn MINION and execute with retry ─────────────────────
        let result = self.run_with_retry(envelope, &task.retry, &trace_id, &workflow_id).await;

        // ── WorkflowState: Completed or Failed ─────────────────────
        let wf_state = match &result {
            Ok(r) => match &r.outcome {
                TaskOutcome::Success { .. } => WorkflowState::Completed,
                TaskOutcome::Failed { error, .. } => {
                    WorkflowState::Failed { error: error.clone() }
                }
                TaskOutcome::TimedOut => {
                    WorkflowState::Failed { error: "timed out".into() }
                }
                TaskOutcome::Cancelled => WorkflowState::Cancelled,
            },
            Err(e) => WorkflowState::Failed { error: e.to_string() },
        };

        self.emit(
            &trace_id,
            StatusKind::WorkflowState { workflow_id, state: wf_state },
        );

        result
    }
}

// ─────────────────────────────────────────────
// Retry logic
// ─────────────────────────────────────────────

impl Vizier {
    async fn select_capability_refs(
        &self,
        description: &str,
        messages: &[ChatMessage],
        fallback_model: &ModelSpec,
    ) -> Vec<CapabilityId> {
        let query_text = capability_query_text(description, messages);
        if matches!(
            detect_intent_scope(description),
            Some(ModelRouteScope::Intent(ref name))
                if name == "image_generate" || name == "image_edit"
        ) {
            return Vec::new();
        }
        let ranked = self.capability_registry.ranked_for_text(&query_text, 12);
        if ranked.is_empty() {
            return self.capability_registry.default_candidate_ids();
        }

        let mut selected = Vec::new();
        let mut selected_talents = 0usize;
        let mut selected_skills = 0usize;

        for (card, _) in ranked.iter() {
            match &card.kind {
                CapabilityKind::Talent | CapabilityKind::Workspace(_) => {
                    if selected_talents < 6 {
                        selected.push(card.id.clone());
                        selected_talents += 1;
                    }
                }
                CapabilityKind::Skill(_) => {
                    let skill_limit = match card.trust {
                        CapabilityTrust::Core | CapabilityTrust::Trusted => 4,
                        CapabilityTrust::Review => 2,
                        CapabilityTrust::Untrusted => 1,
                    };
                    if selected_skills < skill_limit {
                        selected.push(card.id.clone());
                        selected_skills += 1;
                    }
                }
            }
        }

        let support_ids = self.supporting_capability_refs(&query_text, &selected);

        if selected.is_empty() {
            self.capability_registry.default_candidate_ids()
        } else if self.should_nudge(&ranked) {
            merge_capability_ids(
                self.nudge_capability_refs(&query_text, &selected, fallback_model)
                .await
                .unwrap_or(selected),
                support_ids,
            )
        } else {
            merge_capability_ids(selected, support_ids)
        }
    }

    fn supporting_capability_refs(
        &self,
        query_text: &str,
        selected: &[CapabilityId],
    ) -> Vec<CapabilityId> {
        let mut support = Vec::new();
        let query_lower = query_text.to_ascii_lowercase();

        if text_has_any(&query_lower, &["mail", "email", "send", "attach", "attachment"]) {
            maybe_push_capability(&mut support, self.find_capability_by_id("gmail_send"));
        }
        if text_has_any(
            &query_lower,
            &["write", "save", "create", "draft", "generate", "export", "pdf", "attachment"],
        ) {
            maybe_push_capability(&mut support, self.find_capability_by_id("file_write"));
        }
        if text_has_any(
            &query_lower,
            &["read", "review", "inspect", "check", "open", "input", "source", "attachment"],
        ) {
            maybe_push_capability(&mut support, self.find_capability_by_id("file_read"));
        }
        if text_has_any(&query_lower, &["list", "browse", "folder", "directory", "files"]) {
            maybe_push_capability(&mut support, self.find_capability_by_id("file_list"));
        }

        for capability_id in selected {
            let Some(card) = self.capability_registry.get(capability_id) else {
                continue;
            };
            if !matches!(card.kind, CapabilityKind::Skill(_)) {
                continue;
            }
            for permission in &card.permissions {
                match permission {
                    Permission::FileRead => {
                        maybe_push_capability(&mut support, self.find_talent_for_permission(Permission::FileRead));
                    }
                    Permission::FileWrite => {
                        maybe_push_capability(&mut support, self.find_talent_for_permission(Permission::FileWrite));
                    }
                    Permission::Email => {
                        maybe_push_capability(&mut support, self.find_talent_for_permission(Permission::Email));
                    }
                    _ => {}
                }
            }
        }

        support
    }

    fn find_capability_by_id(&self, id: &str) -> Option<CapabilityId> {
        let capability_id = CapabilityId(id.to_string());
        self.capability_registry
            .get(&capability_id)
            .map(|_| capability_id)
    }

    fn find_talent_for_permission(&self, permission: Permission) -> Option<CapabilityId> {
        self.capability_registry
            .all()
            .into_iter()
            .find(|card| {
                matches!(card.kind, CapabilityKind::Talent)
                    && card.permissions.contains(&permission)
                    && card.trust == CapabilityTrust::Core
            })
            .map(|card| card.id)
    }

    fn should_nudge(&self, ranked: &[(maat_core::CapabilityCard, u32)]) -> bool {
        if ranked.len() < 2 {
            return false;
        }
        let top = ranked[0].1;
        let second = ranked[1].1;
        let close_scores = top.saturating_sub(second) <= 25;
        let multiple_meaningful = ranked.iter().take(4).filter(|(_, score)| *score > 0).count() >= 2;
        close_scores || multiple_meaningful
    }

    async fn nudge_capability_refs(
        &self,
        description: &str,
        selected: &[CapabilityId],
        fallback_model: &ModelSpec,
    ) -> Option<Vec<CapabilityId>> {
        let candidates = selected
            .iter()
            .filter_map(|id| self.capability_registry.get(id))
            .cloned()
            .collect::<Vec<_>>();
        if candidates.len() < 2 {
            return None;
        }

        let nudge_model = self.resolve_model(
            &ModelRouteScope::CapabilityNudge,
            None,
            None,
            selected,
            fallback_model,
        );
        let llm = OpenAiCompatClient::from_spec(&nudge_model).ok()?;

        let candidate_block = candidates
            .iter()
            .map(|card| {
                format!(
                    "- id: {}\n  name: {}\n  kind: {:?}\n  trust: {:?}\n  tags: {}\n  permissions: {}\n  description: {}",
                    card.id.0,
                    card.name,
                    card.kind,
                    card.trust,
                    card.tags.join(", "),
                    card.permissions.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>().join(", "),
                    card.semantic_description,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "{}\n\nReturn JSON only in this shape:\n{{\"selected_ids\":[\"id1\"],\"reason\":\"short explanation\"}}\n\nUser request:\n{}\n\nCandidates:\n{}",
            self.capability_nudge_prompt,
            description,
            candidate_block
        );

        let response = llm.complete(
            vec![
                ChatMessage::system("You must return valid JSON only."),
                ChatMessage::user(prompt),
            ],
            &[],
        ).await.ok()?;

        let parsed: CapabilityNudgeDecision = serde_json::from_str(&response.content).ok()?;
        let mut nudged = parsed
            .selected_ids
            .into_iter()
            .filter(|id| selected.iter().any(|candidate| candidate.0 == *id))
            .map(CapabilityId)
            .collect::<Vec<_>>();
        nudged.dedup_by(|a, b| a.0 == b.0);
        if nudged.is_empty() {
            None
        } else {
            info!(selected_capabilities = ?nudged, reason = %parsed.reason, "vizier capability nudge selected shortlist");
            Some(nudged)
        }
    }

    fn resolve_model(
        &self,
        scope: &ModelRouteScope,
        intent_scope: Option<&ModelRouteScope>,
        task_policy: Option<&ModelSelectionPolicy>,
        capability_refs: &[maat_core::CapabilityId],
        fallback: &ModelSpec,
    ) -> ModelSpec {
        let mut policies = Vec::new();
        let mut fallback_profile = fallback.profile_id.as_deref();

        for rule in self.route_rules.iter() {
            if &rule.scope == scope
                || intent_scope.is_some_and(|intent| &rule.scope == intent)
                || matches!(rule.scope, ModelRouteScope::Global)
            {
                policies.push(rule.policy.clone());
                if fallback_profile.is_none() {
                    fallback_profile = rule.fallback_profile.as_deref();
                }
            }
        }

        for capability_id in capability_refs {
            if let Some(card) = self.capability_registry.get(capability_id) {
                if let Some(hints) = &card.routing_hints {
                    if let Some(policy) = &hints.model_policy {
                        policies.push(policy.clone());
                    }
                }

                for tag in &card.tags {
                    for rule in self.route_rules.iter() {
                        if let ModelRouteScope::CapabilityTag(rule_tag) = &rule.scope {
                            if rule_tag == tag {
                                policies.push(rule.policy.clone());
                                if fallback_profile.is_none() {
                                    fallback_profile = rule.fallback_profile.as_deref();
                                }
                            }
                        }
                    }
                }

                for rule in self.route_rules.iter() {
                    match &rule.scope {
                        ModelRouteScope::Talent(name) if *name == card.name || *name == card.id.0 => {
                            policies.push(rule.policy.clone());
                        }
                        ModelRouteScope::Skill(name) if *name == card.name || *name == card.id.0 => {
                            policies.push(rule.policy.clone());
                        }
                        _ => {}
                    }
                }
            }

            for rule in self.route_rules.iter() {
                if let ModelRouteScope::Capability(rule_id) = &rule.scope {
                    if rule_id == capability_id {
                        policies.push(rule.policy.clone());
                        if fallback_profile.is_none() {
                            fallback_profile = rule.fallback_profile.as_deref();
                        }
                    }
                }
            }
        }

        if let Some(task_policy) = task_policy {
            policies.push(task_policy.clone());
        }

        self.model_registry
            .resolve_for_policies(&policies, fallback_profile)
            .unwrap_or_else(|| fallback.clone())
    }

    async fn run_with_retry(
        &self,
        envelope: TaskEnvelope,
        retry: &RetryPolicy,
        trace_id: &TraceId,
        workflow_id: &WorkflowId,
    ) -> Result<ResultEnvelope, MaatError> {
        let step_id = envelope.step_id.clone();
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            debug!(attempt, step = ?step_id, "spawning minion");
            let task_llm: Arc<dyn LlmClient> = Arc::new(
                OpenAiCompatClient::from_spec(&envelope.task.model)
                    .map_err(|e| MaatError::Config(format!("failed to create task llm client: {e}")))?,
            );

            let minion = kameo::spawn(Minion::new(
                self.user_id.clone(),
                self.session_id.clone(),
                task_llm,
                self.tool_registry.clone(),
                self.status_tx.clone(),
            ));

            let result = minion.ask(RunTask(envelope.clone())).send().await;

            match result {
                // Kameo send error (actor dead before reply)
                Err(e) => {
                    return Err(MaatError::Actor(e.to_string()));
                }

                Ok(result_env) => {
                    let should_retry = match &result_env.outcome {
                        TaskOutcome::Failed { retryable: true, .. } => true,
                        TaskOutcome::TimedOut => true,
                        _ => false,
                    } && attempt < retry.max_attempts;

                    if !should_retry {
                        return Ok(result_env);
                    }

                    let backoff = retry.backoff_ms
                        .saturating_mul(2u64.saturating_pow(attempt - 1));
                    warn!(
                        attempt,
                        backoff_ms = backoff,
                        step = ?step_id,
                        "minion failed, retrying"
                    );
                    self.emit(
                        trace_id,
                        StatusKind::StepState {
                            workflow_id: workflow_id.clone(),
                            step_id: step_id.clone(),
                            state: StepState::Retrying { attempt },
                        },
                    );
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                }
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct CapabilityNudgeDecision {
    #[serde(default)]
    selected_ids: Vec<String>,
    #[serde(default)]
    reason: String,
}

fn capability_query_text(description: &str, messages: &[ChatMessage]) -> String {
    let mut parts = vec![description.trim().to_string()];
    for message in messages.iter().rev().take(8).rev() {
        match message.role {
            Role::System => {}
            Role::User | Role::Assistant | Role::Tool => {
                if !message.content.trim().is_empty() {
                    parts.push(message.content.trim().to_string());
                }
            }
        }
    }
    parts.join("\n")
}

fn merge_capability_ids(primary: Vec<CapabilityId>, support: Vec<CapabilityId>) -> Vec<CapabilityId> {
    let mut merged = primary;
    for capability_id in support {
        if !merged.iter().any(|existing| existing == &capability_id) {
            merged.push(capability_id);
        }
    }
    merged
}

fn maybe_push_capability(target: &mut Vec<CapabilityId>, candidate: Option<CapabilityId>) {
    if let Some(candidate) = candidate {
        if !target.iter().any(|existing| existing == &candidate) {
            target.push(candidate);
        }
    }
}

fn text_has_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn detect_intent_scope(query_text: &str) -> Option<ModelRouteScope> {
    let lower = query_text.to_ascii_lowercase();
    let image_terms = ["image", "picture", "photo", "illustration", "logo", "banner"];
    if !text_has_any(&lower, &image_terms) {
        return None;
    }

    if text_has_any(
        &lower,
        &["edit", "modify", "change", "remove background", "retouch", "outpaint", "inpaint"],
    ) {
        return Some(ModelRouteScope::Intent("image_edit".into()));
    }

    if text_has_any(
        &lower,
        &["create", "generate", "make", "draw", "render", "design"],
    ) {
        return Some(ModelRouteScope::Intent("image_generate".into()));
    }

    None
}

fn trim_messages_for_intent(
    messages: &[ChatMessage],
    intent_scope: Option<&ModelRouteScope>,
) -> Vec<ChatMessage> {
    match intent_scope {
        Some(ModelRouteScope::Intent(name))
            if name == "image_generate" || name == "image_edit" =>
        {
            let mut trimmed = Vec::new();
            if let Some(system) = messages.iter().find(|message| matches!(message.role, Role::System)) {
                trimmed.push(system.clone());
            }
            let mut tail = messages
                .iter()
                .filter(|message| !matches!(message.role, Role::System))
                .rev()
                .take(4)
                .cloned()
                .collect::<Vec<_>>();
            tail.reverse();
            trimmed.extend(tail);
            trimmed
        }
        _ => messages.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use maat_core::{
        CapabilityCard, CapabilityKind, CapabilityProvenance, CapabilityTrust, CostProfile,
        ModelCostTier, ModelLatencyTier, ModelProviderSpec, ModelReasoningTier, ModelTrait,
        ProviderApiStyle, StopReason, TokenUsage,
    };
    use maat_llm::{CompletionResponse, LlmClient};
    use serde_json::json;
    use tokio::sync::broadcast;

    struct DummyLlm;

    #[async_trait]
    impl LlmClient for DummyLlm {
        async fn complete(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: &[maat_core::LlmToolDef],
        ) -> Result<CompletionResponse, MaatError> {
            Ok(CompletionResponse {
                content: String::new(),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                latency_ms: 0,
                tool_calls: vec![],
                generated_artifacts: vec![],
            })
        }
    }

    #[test]
    fn capability_query_text_includes_recent_context() {
        let query = capability_query_text(
            "do it properly",
            &[
                ChatMessage::user("make it and send it"),
                ChatMessage::assistant("I'll create a PDF and email it."),
                ChatMessage::user("there was no attachment to the email"),
            ],
        );

        assert!(query.contains("make it and send it"));
        assert!(query.contains("attachment"));
        assert!(query.contains("do it properly"));
    }

    #[test]
    fn detects_image_generation_and_edit_intents() {
        assert_eq!(
            detect_intent_scope("create an image banner for the landing page"),
            Some(ModelRouteScope::Intent("image_generate".into()))
        );
        assert_eq!(
            detect_intent_scope("edit this image and remove the background"),
            Some(ModelRouteScope::Intent("image_edit".into()))
        );
        assert_eq!(detect_intent_scope("look at this image and summarize it"), None);
        assert_eq!(
            detect_intent_scope("send it to troy.travlos@gmail.com"),
            None
        );
    }

    #[tokio::test]
    async fn image_generation_intent_avoids_tool_capability_shortlist() {
        let vizier = Vizier::new(
            UserId("user".into()),
            SessionId::new(),
            Arc::new(DummyLlm),
            Arc::new(ToolRegistry::new()),
            Arc::new(CapabilityRegistry::new()),
            Arc::new(ModelRegistry::new()),
            Arc::new(vec![]),
            "return json".into(),
            broadcast::channel(8).0,
        );

        let refs = vizier
            .select_capability_refs(
                "create an image of a clown in art deco poster style",
                &[],
                &ModelSpec::openrouter_default(),
            )
            .await;

        assert!(refs.is_empty());
    }

    #[test]
    fn image_generation_intent_trims_context_window() {
        let messages = vec![
            ChatMessage::system("system"),
            ChatMessage::user("old 1"),
            ChatMessage::assistant("old 2"),
            ChatMessage::user("old 3"),
            ChatMessage::assistant("old 4"),
            ChatMessage::user("old 5"),
        ];

        let trimmed = trim_messages_for_intent(
            &messages,
            Some(&ModelRouteScope::Intent("image_generate".into())),
        );

        assert_eq!(trimmed.len(), 5);
        assert!(matches!(trimmed.first().map(|m| &m.role), Some(Role::System)));
        assert_eq!(trimmed.last().map(|m| m.content.as_str()), Some("old 5"));
        assert_eq!(trimmed[1].content, "old 2");
    }

    #[tokio::test]
    async fn support_capabilities_are_preserved_for_compound_requests() {
        let mut registry = CapabilityRegistry::new();
        registry.register(test_card("pdf", CapabilityKind::Skill(maat_core::PluginMode::Stdio { command: "pdf".into() }), vec![Permission::FileWrite], vec!["pdf"]));
        registry.register(test_card("file_write", CapabilityKind::Talent, vec![Permission::FileWrite], vec!["write", "file"]));
        registry.register(test_card("gmail_send", CapabilityKind::Talent, vec![Permission::Email], vec!["email", "send"]));

        let mut model_registry = ModelRegistry::new();
        model_registry.register_provider(ModelProviderSpec {
            id: "openrouter".into(),
            api_style: ProviderApiStyle::OpenAiCompat,
            base_url: "https://example.com".into(),
            api_key_env: "TEST_API_KEY".into(),
        });
        model_registry.register_profile(maat_core::ModelProfile {
            id: "default".into(),
            provider_id: "openrouter".into(),
            model_id: "openai/gpt-test".into(),
            temperature: 0.1,
            max_tokens: 1024,
            cost_tier: ModelCostTier::Cheap,
            latency_tier: ModelLatencyTier::Fast,
            reasoning_tier: ModelReasoningTier::Light,
            context_window: 8192,
            supports_tool_calling: true,
            tags: vec![],
            traits: vec![ModelTrait::ToolCalling, ModelTrait::StructuredOutput],
        });
        model_registry.set_default_profile("default");

        let (status_tx, _) = broadcast::channel(8);
        let vizier = Vizier::new(
            UserId("user".into()),
            SessionId::new(),
            Arc::new(DummyLlm),
            Arc::new(ToolRegistry::new()),
            Arc::new(registry),
            Arc::new(model_registry),
            Arc::new(vec![]),
            "return json".into(),
            status_tx,
        );

        let refs = vizier
            .select_capability_refs(
                "do it properly",
                &[
                    ChatMessage::user("make a PDF and email it to Troy"),
                    ChatMessage::assistant("I can use the pdf skill and gmail_send."),
                ],
                &ModelSpec::openrouter_default(),
            )
            .await;

        assert!(refs.contains(&CapabilityId("pdf".into())));
        assert!(refs.contains(&CapabilityId("file_write".into())));
        assert!(refs.contains(&CapabilityId("gmail_send".into())));
    }

    fn test_card(
        id: &str,
        kind: CapabilityKind,
        permissions: Vec<Permission>,
        tags: Vec<&str>,
    ) -> CapabilityCard {
        CapabilityCard {
            id: CapabilityId(id.into()),
            name: id.into(),
            semantic_description: id.into(),
            kind,
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            cost_profile: CostProfile::default(),
            tags: tags.into_iter().map(|tag| tag.to_string()).collect(),
            semantic_terms: Vec::new(),
            trust: CapabilityTrust::Core,
            provenance: CapabilityProvenance {
                source: "test".into(),
                path: None,
                reference: None,
            },
            permissions,
            routing_hints: None,
        }
    }
}
