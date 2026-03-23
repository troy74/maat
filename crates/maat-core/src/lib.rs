//! Shared types for all MAAT crates.
//! Pure data — no I/O, no side effects.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

// ─────────────────────────────────────────────
// Identifiers
// ─────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(pub String);

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionName(pub String);

impl std::fmt::Display for SessionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─────────────────────────────────────────────
// Conversation turn
// Named `ChatMessage` to avoid collision with kameo's `Message` trait.
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// For Role::Tool — the tool_call_id this result responds to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// For Role::Assistant when it made tool calls — JSON-serialised Vec<PendingToolCall>.
    /// Kept as a string so maat-core stays free of complex nested generics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls_json: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into(), tool_call_id: None, tool_calls_json: None }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into(), tool_call_id: None, tool_calls_json: None }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: content.into(), tool_call_id: None, tool_calls_json: None }
    }
    /// Inject an assistant turn that requested tool calls (content is empty per OpenAI spec).
    pub fn assistant_tool_request(tool_calls: &[PendingToolCall]) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls_json: Some(serde_json::to_string(tool_calls).unwrap_or_default()),
        }
    }
    /// Inject a tool result into the conversation.
    pub fn tool_result(tool_call_id: impl Into<String>, result: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: result.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls_json: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    /// Tool result — injected after the LLM makes a tool call.
    Tool,
}

// ─────────────────────────────────────────────
// LLM primitives
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

// ─────────────────────────────────────────────
// Model configuration
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Model ID as the provider expects.
    /// For OpenRouter use "provider/model", e.g. "minimax/minimax-m1".
    /// Override at runtime with the `MAAT_MODEL` env var.
    pub model_id: String,

    /// OpenAI-compatible base URL.
    pub base_url: String,

    /// Name of the env var that holds the API key.
    pub api_key_env: String,

    pub temperature: f32,
    pub max_tokens: u32,
}

impl ModelSpec {
    /// OpenRouter + MiniMax default.
    /// Set `MAAT_MODEL` to override the model ID.
    /// Set `OPENROUTER_API_KEY` for auth.
    pub fn openrouter_default() -> Self {
        Self {
            model_id: std::env::var("MAAT_MODEL")
                .unwrap_or_else(|_| "minimax/minimax-m1".to_string()),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
        }
    }
}

// ─────────────────────────────────────────────
// Chat reply (PHAROH → bridge → TUI)
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChatReply {
    pub content: String,
    pub usage: TokenUsage,
    pub latency_ms: u64,
}

// ─────────────────────────────────────────────
// Events flowing from the backend to the TUI
// ─────────────────────────────────────────────

#[derive(Debug)]
pub enum TuiEvent {
    /// Completed assistant turn, ready to display.
    AssistantMessage(ChatReply),
    /// Non-fatal error to surface to the user.
    Error(String),
}

// ═════════════════════════════════════════════
// Phase 3 — Envelope & Control Plane
// ═════════════════════════════════════════════

// ─────────────────────────────────────────────
// Typed identifiers
// ─────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EnvelopeId(pub Ulid);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TraceId(pub Ulid);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Ulid);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkflowId(pub Ulid);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StepId(pub Ulid);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CapabilityId(pub String);

impl EnvelopeId  { pub fn new() -> Self { Self(Ulid::new()) } }
impl TraceId     { pub fn new() -> Self { Self(Ulid::new()) } }
impl SessionId   { pub fn new() -> Self { Self(Ulid::new()) } }
impl WorkflowId  { pub fn new() -> Self { Self(Ulid::new()) } }
impl StepId      { pub fn new() -> Self { Self(Ulid::new()) } }

impl Default for EnvelopeId { fn default() -> Self { Self::new() } }
impl Default for TraceId    { fn default() -> Self { Self::new() } }
impl Default for SessionId  { fn default() -> Self { Self::new() } }

// ─────────────────────────────────────────────
// Component address — who sent / who receives
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ComponentAddress {
    Ra,
    Pharoh(UserId),
    Session(UserId, SessionId),
    Vizier(UserId, SessionId),
    Minion(UserId, SessionId, StepId),
}

// ─────────────────────────────────────────────
// Envelope header — shared by all envelope types
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeHeader {
    pub id: EnvelopeId,
    /// Shared across all envelopes in one user request, for tracing.
    pub trace_id: TraceId,
    pub sender: ComponentAddress,
    pub recipient: ComponentAddress,
    pub created_at_ms: u64,   // unix ms, avoids chrono dep in core
    pub priority: Priority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority { Low, Normal, High, Critical }

impl Default for Priority { fn default() -> Self { Self::Normal } }

impl EnvelopeHeader {
    pub fn new(sender: ComponentAddress, recipient: ComponentAddress) -> Self {
        Self {
            id: EnvelopeId::new(),
            trace_id: TraceId::new(),
            sender,
            recipient,
            created_at_ms: now_ms(),
            priority: Priority::Normal,
        }
    }
    pub fn with_trace(mut self, trace_id: TraceId) -> Self {
        self.trace_id = trace_id;
        self
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─────────────────────────────────────────────
// HeraldEnvelope — channel → RA → PHAROH
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeraldEnvelope {
    pub header: EnvelopeHeader,
    pub channel: ChannelId,
    pub user_id: UserId,
    pub payload: HeraldPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub String);   // e.g. "tui", "telegram", "web"

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HeraldPayload {
    Text(String),
    Command(ParsedCommand),
    Attachment { mime_type: String, size_bytes: u64, pointer: String },
}

/// Parsed @ / slash commands from any herald — normalised form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParsedCommand {
    Help,
    RouteToSession { name: SessionName, message: String },
    SessionNew     { name: SessionName, description: String },
    SessionList,
    SessionEnd     { name: SessionName },
    StatusAll,
    StatusSession  { name: SessionName },
    WorkflowInspect { session: SessionName },
    TaskInspect     { step_id: StepId },
    ModelSwap { session: Option<SessionName>, model_id: String },
    Purge     { session: SessionName },
    /// List all registered tools/talents.
    ToolsList,
    /// Show current config (no secret values).
    ConfigShow,
    /// Set a config value (key, value).
    ConfigSet { key: String, value: String },
    /// Store a secret in the secret chain.
    SecretSet { key: String, value: String },
    /// List known secret keys (no values).
    SecretList,
    /// Delete a secret.
    SecretDelete { key: String },
    /// Start Google OAuth flow.
    AuthGoogle,
}

// ─────────────────────────────────────────────
// SessionEnvelope — PHAROH ↔ Named Session
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEnvelope {
    pub header: EnvelopeHeader,
    pub session_id: SessionId,
    pub payload: SessionPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionPayload {
    /// Delegate a task to the named session.
    Dispatch {
        text: String,
        context_excerpt: Vec<ChatMessage>,   // recent turns only, not full history
        model: ModelSpec,
    },
    /// Session reports its current summary back to PHAROH.
    SummaryUpdate { summary: String },
    /// User message forwarded directly to a named session.
    UserMessage(String),
}

// ─────────────────────────────────────────────
// TaskEnvelope — VIZIER → MINION
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEnvelope {
    pub header: EnvelopeHeader,
    pub step_id: StepId,
    pub workflow_id: WorkflowId,
    pub task: TaskSpec,
    pub resource_budget: ResourceBudget,
    pub deadline_ms: Option<u64>,   // unix ms; None = no hard deadline
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub description: String,
    pub messages: Vec<ChatMessage>,
    pub model: ModelSpec,
    /// IDs only — resolved from the process-global CapabilityRegistry.
    pub capability_refs: Vec<CapabilityId>,
    pub retry: RetryPolicy,
    pub allow_sub_vizier: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceBudget {
    pub max_input_tokens: u32,
    pub max_output_tokens: u32,
    pub max_tool_calls: u32,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self { max_input_tokens: 8192, max_output_tokens: 4096, max_tool_calls: 20 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_ms: u64,   // base; actual = backoff_ms * 2^(attempt-1)
}

impl Default for RetryPolicy {
    fn default() -> Self { Self { max_attempts: 3, backoff_ms: 500 } }
}

// ─────────────────────────────────────────────
// ResultEnvelope — MINION → VIZIER
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultEnvelope {
    pub header: EnvelopeHeader,
    pub step_id: StepId,
    pub workflow_id: WorkflowId,
    pub outcome: TaskOutcome,
    pub usage: TokenUsage,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskOutcome {
    Success { content: String, tool_calls_made: Vec<ToolCallRecord> },
    Failed  { error: String, retryable: bool },
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub capability_id: CapabilityId,
    pub input: serde_json::Value,
    pub output: serde_json::Value,
    pub latency_ms: u64,
}

// ─────────────────────────────────────────────
// ControlMessage — PHAROH/RA → any component
// Never passed to an LLM.
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlMessage {
    pub header: EnvelopeHeader,
    pub target: ComponentAddress,
    pub command: ControlCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlCommand {
    Pause,
    Resume,
    Cancel,
    Kill,
    Restart,
    /// Request a lease renewal for a long-running task.
    RenewLease { extra_ms: u64 },
    /// Force-purge context above this token count.
    PurgeContext { keep_tokens: u32 },
}

// ─────────────────────────────────────────────
// StatusEvent — typed state transitions
// Broadcast to PHAROH; never touches an LLM.
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusEvent {
    pub source: ComponentAddress,
    pub trace_id: TraceId,
    pub at_ms: u64,
    pub kind: StatusKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StatusKind {
    SessionState   { session_id: SessionId, state: SessionState },
    WorkflowState  { workflow_id: WorkflowId, state: WorkflowState },
    StepState      { workflow_id: WorkflowId, step_id: StepId, state: StepState },
    HeartBeat      { session_id: SessionId },
}

impl StatusEvent {
    pub fn new(source: ComponentAddress, trace_id: TraceId, kind: StatusKind) -> Self {
        Self { source, trace_id, at_ms: now_ms(), kind }
    }
}

// ─────────────────────────────────────────────
// State machines — deterministic, no LLM needed
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Running   { step_id: StepId },
    Blocked   { reason: BlockReason },
    AwaitingUser,
    Completed,
    Failed    { error: String },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkflowState {
    Pending,
    Running  { completed: u32, total: u32 },
    Paused,
    Completed,
    Failed   { error: String },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Completed,
    Failed { error: String, retryable: bool },
    Retrying { attempt: u32 },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BlockReason {
    RateLimit { retry_after_ms: u64 },
    AwaitingDependency { step_id: StepId },
    ResourceExhausted,
    UserApprovalRequired,
}

// ─────────────────────────────────────────────
// Capability Cards
// Cards live in the process-global CapabilityRegistry.
// Envelopes carry CapabilityId refs only — never full cards.
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityCard {
    pub id: CapabilityId,
    pub name: String,
    /// Natural-language description for VIZIER's LLM to use when planning.
    pub semantic_description: String,
    pub kind: CapabilityKind,
    pub input_schema: serde_json::Value,   // JSON Schema
    pub output_schema: serde_json::Value,
    pub cost_profile: CostProfile,
    pub tags: Vec<String>,
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapabilityKind {
    Talent,                      // compiled-in, fully trusted
    Skill(PluginMode),           // sandboxed plugin
    Workspace(String),           // KAP provider operation
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginMode {
    Wasm { path: String },
    Stdio { command: String },
    Http { endpoint: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CostProfile {
    pub avg_latency_ms: u64,
    pub estimated_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Permission {
    Network,
    FileRead,
    FileWrite,
    ProcessSpawn,
    Email,
    Calendar,
}

// ─────────────────────────────────────────────
// SessionSummary — PHAROH's lightweight view
// of each named session (no sub-MINION detail)
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub name: SessionName,
    pub state: SessionState,
    pub summary: String,     // one-line, updated via StatusEvent
    pub last_active_ms: u64,
}

// ═════════════════════════════════════════════
// Tool / Skill infrastructure
// ═════════════════════════════════════════════

// ─────────────────────────────────────────────
// LLM tool definition (sent to the model)
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub parameters: serde_json::Value,
}

// ─────────────────────────────────────────────
// In-flight tool call (returned by the LLM)
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolCall {
    /// The tool_call_id assigned by the provider (needed to inject the result).
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

// ─────────────────────────────────────────────
// Tool trait — implemented by Talents and Skills
// ─────────────────────────────────────────────

#[async_trait]
pub trait Tool: Send + Sync {
    fn llm_definition(&self) -> LlmToolDef;
    async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value, MaatError>;
}

// ─────────────────────────────────────────────
// Tool registry — process-global, built once at startup
// ─────────────────────────────────────────────

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self { Self::default() }

    /// Register a tool. Key is the tool's LLM function name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.llm_definition().name;
        self.tools.insert(name, tool);
    }

    pub fn get_by_name(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn all_definitions(&self) -> Vec<LlmToolDef> {
        self.tools.values().map(|t| t.llm_definition()).collect()
    }

    pub fn is_empty(&self) -> bool { self.tools.is_empty() }

    /// Call a tool by its LLM function name.
    pub async fn call_by_name(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, MaatError> {
        match self.tools.get(name) {
            Some(tool) => tool.call(input).await,
            None => Err(MaatError::Tool(format!("unknown tool: {name}"))),
        }
    }
}

// ─────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum MaatError {
    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Actor error: {0}")]
    Actor(String),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Storage error: {0}")]
    Storage(String),
}
