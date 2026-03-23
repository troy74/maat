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
    ChatMessage, ComponentAddress, EnvelopeHeader, MaatError, ModelSpec, Priority,
    ResourceBudget, ResultEnvelope, RetryPolicy, SessionId, StatusEvent,
    StatusKind, StepId, StepState, TaskEnvelope, TaskOutcome, TaskSpec, TraceId,
    ToolRegistry, UserId, WorkflowId, WorkflowState,
};
use maat_llm::LlmClient;
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
    llm: Arc<dyn LlmClient>,
    tool_registry: Arc<ToolRegistry>,
    status_tx: broadcast::Sender<StatusEvent>,
}

impl Vizier {
    pub fn new(
        user_id: UserId,
        session_id: SessionId,
        llm: Arc<dyn LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        status_tx: broadcast::Sender<StatusEvent>,
    ) -> Self {
        Self { user_id, session_id, llm, tool_registry, status_tx }
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
                messages: task.messages,
                model: task.model,
                capability_refs: vec![],
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

            let minion = kameo::spawn(Minion::new(
                self.user_id.clone(),
                self.session_id.clone(),
                self.llm.clone(),
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
