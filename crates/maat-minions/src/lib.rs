//! MINION — ephemeral worker actor.
//!
//! One MINION handles one task: run the agentic loop (LLM → tool dispatch →
//! inject result → repeat until EndTurn) and return a ResultEnvelope.
//! Spawned fresh per task by VIZIER; terminates when VIZIER drops the ref.
//!
//! Responsibilities:
//!   - Apply deadline timeout to the full agentic loop
//!   - Emit StepState transitions on the status bus
//!   - Return a typed ResultEnvelope (never panics on LLM failure)

use std::sync::Arc;
use std::time::Instant;

use kameo::Actor;
use maat_core::{
    ChatMessage, ComponentAddress, EnvelopeHeader, MaatError, ResultEnvelope, SessionId,
    StatusEvent, StatusKind, StepState, TaskEnvelope, TaskOutcome, TokenUsage, ToolRegistry,
    TraceId, UserId,
};
use maat_llm::LlmClient;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

const MAX_TOOL_ROUNDS: usize = 10;

// ─────────────────────────────────────────────
// Actor
// ─────────────────────────────────────────────

#[derive(Actor)]
pub struct Minion {
    user_id: UserId,
    session_id: SessionId,
    llm: Arc<dyn LlmClient>,
    tool_registry: Arc<ToolRegistry>,
    status_tx: broadcast::Sender<StatusEvent>,
}

impl Minion {
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
        let source = ComponentAddress::Minion(
            self.user_id.clone(),
            self.session_id.clone(),
            maat_core::StepId::new(),
        );
        let _ = self.status_tx.send(StatusEvent::new(source, trace_id.clone(), kind));
    }

    /// Drive the LLM → tool-call → inject loop until the model stops requesting tools
    /// or MAX_TOOL_ROUNDS is exhausted. Returns (final_content, cumulative_usage, latency_ms).
    async fn run_agentic_loop(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Result<(String, TokenUsage, u64), MaatError> {
        let tool_defs = self.tool_registry.all_definitions();
        let mut messages = messages;
        let mut total_usage = TokenUsage::default();
        let t0 = Instant::now();

        for round in 0..MAX_TOOL_ROUNDS {
            let resp = self.llm.complete(messages.clone(), &tool_defs).await?;
            total_usage.input_tokens += resp.usage.input_tokens;
            total_usage.output_tokens += resp.usage.output_tokens;

            if resp.tool_calls.is_empty() {
                let latency_ms = t0.elapsed().as_millis() as u64;
                debug!(round, "agentic loop done");
                return Ok((resp.content, total_usage, latency_ms));
            }

            debug!(round, tools = resp.tool_calls.len(), "dispatching tool calls");

            // Inject the assistant's tool-request turn.
            messages.push(ChatMessage::assistant_tool_request(&resp.tool_calls));

            // Execute each tool call and inject results.
            for tc in &resp.tool_calls {
                let result = self
                    .tool_registry
                    .call_by_name(&tc.name, tc.input.clone())
                    .await
                    .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));
                messages.push(ChatMessage::tool_result(tc.id.clone(), result.to_string()));
            }
        }

        Err(MaatError::Llm(format!("exceeded {MAX_TOOL_ROUNDS} tool rounds")))
    }
}

// ─────────────────────────────────────────────
// Message
// ─────────────────────────────────────────────

pub struct RunTask(pub TaskEnvelope);

impl kameo::message::Message<RunTask> for Minion {
    type Reply = Result<ResultEnvelope, MaatError>;

    async fn handle(
        &mut self,
        RunTask(env): RunTask,
        _ctx: kameo::message::Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let step_id = env.step_id.clone();
        let workflow_id = env.workflow_id.clone();
        let trace_id = env.header.trace_id.clone();

        info!(step = ?step_id, model = %env.task.model.model_id, "minion starting");

        // ── emit Running ────────────────────────────────────────────
        self.emit(
            &trace_id,
            StatusKind::StepState {
                workflow_id: workflow_id.clone(),
                step_id: step_id.clone(),
                state: StepState::Running,
            },
        );

        // ── agentic loop with timeout ───────────────────────────────
        let timeout_dur = deadline_to_duration(env.deadline_ms);

        let loop_result = tokio::time::timeout(
            timeout_dur,
            self.run_agentic_loop(env.task.messages.clone()),
        )
        .await;

        // ── map to TaskOutcome ──────────────────────────────────────
        let (outcome, usage, latency_ms) = match loop_result {
            Ok(Ok((content, usage, latency_ms))) => {
                debug!(
                    in_tok = usage.input_tokens,
                    out_tok = usage.output_tokens,
                    latency_ms,
                    "minion complete"
                );
                (TaskOutcome::Success { content, tool_calls_made: vec![] }, usage, latency_ms)
            }
            Ok(Err(e)) => {
                warn!(error = %e, "minion LLM error");
                (
                    TaskOutcome::Failed { error: e.to_string(), retryable: is_retryable(&e) },
                    TokenUsage::default(),
                    0,
                )
            }
            Err(_elapsed) => {
                warn!(step = ?step_id, "minion timed out");
                (TaskOutcome::TimedOut, TokenUsage::default(), 0)
            }
        };

        // ── emit final StepState ────────────────────────────────────
        let step_state = step_state_from_outcome(&outcome);
        self.emit(
            &trace_id,
            StatusKind::StepState {
                workflow_id: workflow_id.clone(),
                step_id: step_id.clone(),
                state: step_state,
            },
        );

        // ── build ResultEnvelope ────────────────────────────────────
        let result_header =
            EnvelopeHeader::new(env.header.recipient.clone(), env.header.sender.clone())
                .with_trace(trace_id);

        Ok(ResultEnvelope { header: result_header, step_id, workflow_id, outcome, usage, latency_ms })
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

fn deadline_to_duration(deadline_ms: Option<u64>) -> std::time::Duration {
    match deadline_ms {
        Some(dl) => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let remaining = dl.saturating_sub(now_ms).clamp(2_000, 300_000);
            std::time::Duration::from_millis(remaining)
        }
        None => std::time::Duration::from_secs(120),
    }
}

fn is_retryable(e: &MaatError) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("429") || s.contains("503") || s.contains("timeout") || s.contains("connection")
}

fn step_state_from_outcome(outcome: &TaskOutcome) -> StepState {
    match outcome {
        TaskOutcome::Success { .. } => StepState::Completed,
        TaskOutcome::Failed { error, retryable } => {
            StepState::Failed { error: error.clone(), retryable: *retryable }
        }
        TaskOutcome::TimedOut => StepState::Failed { error: "timed out".into(), retryable: true },
        TaskOutcome::Cancelled => StepState::Cancelled,
    }
}
