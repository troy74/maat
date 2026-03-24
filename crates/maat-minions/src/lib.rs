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
use std::collections::HashSet;

use kameo::Actor;
use maat_core::{
    CancellationRegistry, CapabilityId, ChatMessage, ComponentAddress, EnvelopeHeader, MaatError, ResultEnvelope,
    SessionId, StatusEvent, StatusKind, StepState, TaskEnvelope, TaskOutcome, TokenUsage,
    ToolRegistry, TraceId, UserId,
};
use maat_llm::LlmClient;
use maat_memory::MemoryStore;
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
    store: Arc<dyn MemoryStore>,
    status_tx: broadcast::Sender<StatusEvent>,
    cancel_registry: CancellationRegistry,
}

impl Minion {
    pub fn new(
        user_id: UserId,
        session_id: SessionId,
        llm: Arc<dyn LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        store: Arc<dyn MemoryStore>,
        status_tx: broadcast::Sender<StatusEvent>,
        cancel_registry: CancellationRegistry,
    ) -> Self {
        Self { user_id, session_id, llm, tool_registry, store, status_tx, cancel_registry }
    }

    fn emit(&self, trace_id: &TraceId, step_id: &maat_core::StepId, kind: StatusKind) {
        let source = ComponentAddress::Minion(
            self.user_id.clone(),
            self.session_id.clone(),
            step_id.clone(),
        );
        let _ = self.status_tx.send(StatusEvent::new(source, trace_id.clone(), kind));
    }

    /// Drive the LLM → tool-call → inject loop until the model stops requesting tools
    /// or MAX_TOOL_ROUNDS is exhausted. Returns final content plus any generated artifacts.
    async fn run_agentic_loop(
        &self,
        messages: Vec<ChatMessage>,
        capability_refs: &[CapabilityId],
        cancel_key: Option<&str>,
    ) -> Result<(String, Vec<maat_core::GeneratedArtifact>, TokenUsage, u64), MaatError> {
        let allowed_tool_names: Vec<String> = capability_refs.iter().map(|id| id.0.clone()).collect();
        let allowed_tool_set: HashSet<String> = allowed_tool_names.iter().cloned().collect();
        let tool_defs = self.tool_registry.definitions_for_names(&allowed_tool_names);
        let mut messages = messages;
        let mut total_usage = TokenUsage::default();
        let t0 = Instant::now();

        for round in 0..MAX_TOOL_ROUNDS {
            if cancel_key.is_some_and(|key| self.cancel_registry.is_cancelled(key)) {
                return Err(MaatError::Cancelled);
            }
            let resp = self.llm.complete(messages.clone(), &tool_defs).await?;
            total_usage.input_tokens += resp.usage.input_tokens;
            total_usage.output_tokens += resp.usage.output_tokens;

            if resp.tool_calls.is_empty() {
                let latency_ms = t0.elapsed().as_millis() as u64;
                debug!(round, generated_artifacts = resp.generated_artifacts.len(), "agentic loop done");
                return Ok((resp.content, resp.generated_artifacts, total_usage, latency_ms));
            }

            debug!(round, tools = resp.tool_calls.len(), "dispatching tool calls");

            // Inject the assistant's tool-request turn.
            messages.push(ChatMessage::assistant_tool_request(&resp.tool_calls));

            // Execute each tool call and inject results.
            for tc in &resp.tool_calls {
                if cancel_key.is_some_and(|key| self.cancel_registry.is_cancelled(key)) {
                    return Err(MaatError::Cancelled);
                }
                let result = if allowed_tool_set.contains(&tc.name) {
                    let input = self.prepare_tool_input(&tc.name, tc.input.clone()).await
                        .unwrap_or_else(|e| serde_json::json!({"__maat_error": e.to_string()}));
                    if let Some(error) = input.get("__maat_error").and_then(|v| v.as_str()) {
                        serde_json::json!({"error": error})
                    } else {
                    self.tool_registry
                        .call_by_name(&tc.name, input)
                        .await
                        .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))
                    }
                } else {
                    serde_json::json!({
                        "error": format!("tool '{}' is not available for this task", tc.name)
                    })
                };
                messages.push(ChatMessage::tool_result(tc.id.clone(), result.to_string()));
            }
        }

        Err(MaatError::Llm(format!("exceeded {MAX_TOOL_ROUNDS} tool rounds")))
    }

    async fn prepare_tool_input(
        &self,
        tool_name: &str,
        mut input: serde_json::Value,
    ) -> Result<serde_json::Value, MaatError> {
        if input.get("artifact_handle").and_then(|value| value.as_str()).is_none() {
            if let Some(request) = input.get("request").and_then(|value| value.as_str()) {
                if let Some(handle) = extract_artifact_handle(request) {
                    input["artifact_handle"] = serde_json::Value::String(handle.to_string());
                }
            }
        }
        if let Some(handle) = input.get("artifact_handle").and_then(|value| value.as_str()) {
            let artifact = self
                .store
                .get_artifact_by_handle(&self.user_id.0, handle)
                .await?
                .ok_or_else(|| MaatError::Tool(format!("unknown artifact handle: {handle}")))?;
            if input.get("input_path").and_then(|value| value.as_str()).is_none() {
                input["input_path"] = serde_json::Value::String(artifact.storage_path.clone());
            }
            if request_is_empty_or_mentions_artifact(input.get("request").and_then(|value| value.as_str())) {
                input["request"] = serde_json::Value::String(artifact.storage_path);
            }
        }
        if tool_name == "gmail_send" {
            let attachments = input
                .get("attachments")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            let mut merged = attachments
                .into_iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>();
            if let Some(handles) = input.get("artifact_handles").and_then(|value| value.as_array()) {
                for handle in handles.iter().filter_map(|item| item.as_str()) {
                    let artifact = self
                        .store
                        .get_artifact_by_handle(&self.user_id.0, handle)
                        .await?
                        .ok_or_else(|| MaatError::Tool(format!("unknown artifact handle: {handle}")))?;
                    merged.push(artifact.storage_path);
                }
            }
            input["attachments"] = serde_json::Value::Array(
                merged.into_iter().map(serde_json::Value::String).collect()
            );
        }
        Ok(input)
    }
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
            &step_id,
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
            self.run_agentic_loop(
                env.task.messages.clone(),
                &env.task.capability_refs,
                env.task.cancel_key.as_deref(),
            ),
        )
        .await;

        // ── map to TaskOutcome ──────────────────────────────────────
        let (outcome, usage, latency_ms) = match loop_result {
            Ok(Ok((content, generated_artifacts, usage, latency_ms))) => {
                debug!(
                    in_tok = usage.input_tokens,
                    out_tok = usage.output_tokens,
                    latency_ms,
                    "minion complete"
                );
                (
                    TaskOutcome::Success {
                        content,
                        tool_calls_made: vec![],
                        generated_artifacts,
                    },
                    usage,
                    latency_ms,
                )
            }
            Ok(Err(e)) => {
                warn!(error = %e, "minion LLM error");
                match e {
                    MaatError::Cancelled => (TaskOutcome::Cancelled, TokenUsage::default(), 0),
                    other => (
                        TaskOutcome::Failed { error: other.to_string(), retryable: is_retryable(&other) },
                        TokenUsage::default(),
                        0,
                    ),
                }
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
            &step_id,
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
