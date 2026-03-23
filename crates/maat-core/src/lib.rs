//! Shared types for all MAAT crates.
//! Pure data — no I/O, no side effects.

pub mod commands;

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
    /// Optional logical profile ID used by MAAT's model registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,

    /// Optional provider ID used by MAAT's model registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,

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
            profile_id: Some("default".to_string()),
            provider_id: Some("openrouter".to_string()),
            model_id: std::env::var("MAAT_MODEL")
                .unwrap_or_else(|_| "minimax/minimax-m1".to_string()),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProviderApiStyle {
    OpenAiCompat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderSpec {
    pub id: String,
    pub api_style: ProviderApiStyle,
    pub base_url: String,
    pub api_key_env: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelCostTier {
    Cheap,
    Standard,
    Premium,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelLatencyTier {
    Fast,
    Balanced,
    Slow,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelReasoningTier {
    Light,
    Medium,
    Heavy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ModelTrait {
    ToolCalling,
    LongContext,
    StructuredOutput,
    Reasoning,
    Vision,
    FastResponse,
    LowCost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub cost_tier: ModelCostTier,
    pub latency_tier: ModelLatencyTier,
    pub reasoning_tier: ModelReasoningTier,
    pub context_window: u32,
    pub supports_tool_calling: bool,
    pub tags: Vec<String>,
    pub traits: Vec<ModelTrait>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelSelectionPolicy {
    /// Prefer these profiles first, in order.
    #[serde(default)]
    pub preferred_profiles: Vec<String>,
    /// Explicit allow-list. Empty means "any profile is allowed".
    #[serde(default)]
    pub allow_profiles: Vec<String>,
    /// Explicit deny-list.
    #[serde(default)]
    pub deny_profiles: Vec<String>,
    /// Prefer profiles that advertise these traits.
    #[serde(default)]
    pub required_traits: Vec<ModelTrait>,
    /// Upper bound for parsimony-sensitive routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_tier: Option<ModelCostTier>,
    /// Upper bound for latency-sensitive routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_latency_tier: Option<ModelLatencyTier>,
    /// Minimum reasoning tier for difficult routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_reasoning_tier: Option<ModelReasoningTier>,
    /// Whether tool-capable models are required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_tool_calling: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ModelRouteScope {
    Global,
    PharohPrimary,
    SessionDefault,
    SessionNamed(String),
    Planner,
    CapabilityNudge,
    Summarizer,
    Intent(String),
    Capability(CapabilityId),
    CapabilityTag(String),
    Talent(String),
    Skill(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRouteRule {
    pub scope: ModelRouteScope,
    pub policy: ModelSelectionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapabilityRoutingHints {
    #[serde(default)]
    pub preferred_tags: Vec<String>,
    #[serde(default)]
    pub avoids_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_policy: Option<ModelSelectionPolicy>,
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
    ModelList,
    ModelSwap { session: Option<SessionName>, model_id: String },
    Purge     { session: SessionName },
    /// List all registered tools/talents.
    ToolsList,
    /// List installed local skills.
    SkillsList,
    /// Search ClawHub for skills matching a query.
    SkillSearch { query: String },
    /// Install a skill from a local directory into the workspace skills folder.
    SkillInstall { source: String },
    ArtifactsList,
    ArtifactImport { path: String },
    ArtifactShow { handle: String },
    MemoryAdd { text: String },
    MistakeAdd { text: String },
    UserNoteAdd { user: Option<String>, text: String },
    PersonaAppend { text: String },
    PromptsList,
    PromptShow { name: String },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_policy: Option<ModelSelectionPolicy>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_policy: Option<ModelSelectionPolicy>,
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
    Success {
        content: String,
        tool_calls_made: Vec<ToolCallRecord>,
        generated_artifacts: Vec<GeneratedArtifact>,
    },
    Failed  { error: String, retryable: bool },
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedArtifact {
    pub kind: String,
    pub mime_type: String,
    pub suggested_name: String,
    pub summary: String,
    pub data_base64: String,
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
    #[serde(default)]
    pub semantic_terms: Vec<String>,
    #[serde(default)]
    pub trust: CapabilityTrust,
    #[serde(default)]
    pub provenance: CapabilityProvenance,
    pub permissions: Vec<Permission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_hints: Option<CapabilityRoutingHints>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum CapabilityTrust {
    Core,
    Trusted,
    Review,
    #[default]
    Untrusted,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapabilityProvenance {
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
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
    fn capability_card(&self) -> Option<CapabilityCard> {
        let def = self.llm_definition();
        Some(CapabilityCard {
            id: CapabilityId(def.name.clone()),
            name: def.name.clone(),
            semantic_description: def.description.clone(),
            kind: CapabilityKind::Talent,
            input_schema: def.parameters,
            output_schema: serde_json::json!({ "type": "object" }),
            cost_profile: CostProfile::default(),
            tags: Vec::new(),
            semantic_terms: Vec::new(),
            trust: CapabilityTrust::Core,
            provenance: CapabilityProvenance {
                source: "compiled_talent".into(),
                path: None,
                reference: None,
            },
            permissions: Vec::new(),
            routing_hints: None,
        })
    }
    async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value, MaatError>;
}

#[derive(Default)]
pub struct CapabilityRegistry {
    cards: HashMap<CapabilityId, CapabilityCard>,
}

impl CapabilityRegistry {
    pub fn new() -> Self { Self::default() }

    pub fn register(&mut self, card: CapabilityCard) {
        let card = normalize_capability_card(card);
        self.cards.insert(card.id.clone(), card);
    }

    pub fn get(&self, id: &CapabilityId) -> Option<&CapabilityCard> {
        self.cards.get(id)
    }

    pub fn all(&self) -> Vec<CapabilityCard> {
        self.cards.values().cloned().collect()
    }

    pub fn ids(&self) -> Vec<CapabilityId> {
        self.cards.keys().cloned().collect()
    }

    pub fn ranked_for_text(&self, text: &str, limit: usize) -> Vec<(CapabilityCard, u32)> {
        let query_terms = tokenize_terms(text);
        let mut scored: Vec<(CapabilityCard, u32)> = self.cards
            .values()
            .cloned()
            .map(|card| {
                let score: u32 = card.semantic_terms
                    .iter()
                    .map(|term| u32::from(query_terms.contains(term)))
                    .sum();
                let weighted_score = score.saturating_mul(100) + capability_priority(&card);
                (card, weighted_score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();

        scored.sort_by(|(a_card, a_score), (b_card, b_score)| {
            b_score.cmp(a_score).then_with(|| a_card.name.cmp(&b_card.name))
        });
        scored.truncate(limit);
        scored
    }

    pub fn default_candidate_ids(&self) -> Vec<CapabilityId> {
        let mut cards: Vec<_> = self.cards.values().cloned().collect();
        cards.sort_by(|a, b| {
            capability_priority(b)
                .cmp(&capability_priority(a))
                .then_with(|| a.name.cmp(&b.name))
        });
        let safe: Vec<_> = cards
            .iter()
            .filter(|card| card.trust != CapabilityTrust::Untrusted)
            .map(|card| card.id.clone())
            .collect();
        if safe.is_empty() {
            cards.into_iter().map(|card| card.id).collect()
        } else {
            safe
        }
    }
}

pub struct ModelRegistry {
    providers: HashMap<String, ModelProviderSpec>,
    profiles: HashMap<String, ModelProfile>,
    default_profile: Option<String>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self { providers: HashMap::new(), profiles: HashMap::new(), default_profile: None }
    }
}

impl ModelRegistry {
    pub fn new() -> Self { Self::default() }

    pub fn register_provider(&mut self, provider: ModelProviderSpec) {
        self.providers.insert(provider.id.clone(), provider);
    }

    pub fn register_profile(&mut self, profile: ModelProfile) {
        self.profiles.insert(profile.id.clone(), profile);
    }

    pub fn set_default_profile(&mut self, profile_id: impl Into<String>) {
        self.default_profile = Some(profile_id.into());
    }

    pub fn default_profile(&self) -> Option<&ModelProfile> {
        let id = self.default_profile.as_ref()?;
        self.profiles.get(id)
    }

    pub fn profile(&self, profile_id: &str) -> Option<&ModelProfile> {
        self.profiles.get(profile_id)
    }

    pub fn profiles(&self) -> Vec<&ModelProfile> {
        let mut profiles: Vec<_> = self.profiles.values().collect();
        profiles.sort_by(|a, b| a.id.cmp(&b.id));
        profiles
    }

    pub fn resolve_spec(&self, profile_id: &str) -> Option<ModelSpec> {
        let profile = self.profile(profile_id)?;
        let provider = self.providers.get(&profile.provider_id)?;
        Some(ModelSpec {
            profile_id: Some(profile.id.clone()),
            provider_id: Some(provider.id.clone()),
            model_id: profile.model_id.clone(),
            base_url: provider.base_url.clone(),
            api_key_env: provider.api_key_env.clone(),
            temperature: profile.temperature,
            max_tokens: profile.max_tokens,
        })
    }

    pub fn resolve_default_spec(&self) -> Option<ModelSpec> {
        let profile = self.default_profile()?;
        self.resolve_spec(&profile.id)
    }

    pub fn resolve_for_policies(
        &self,
        policies: &[ModelSelectionPolicy],
        fallback_profile: Option<&str>,
    ) -> Option<ModelSpec> {
        let merged = policies.iter().fold(ModelSelectionPolicy::default(), |acc, policy| {
            acc.merge(policy)
        });

        if let Some(profile_id) = merged
            .preferred_profiles
            .iter()
            .find(|profile_id| self.profile_matches_policy(profile_id, &merged))
        {
            return self.resolve_spec(profile_id);
        }

        let mut candidates: Vec<&ModelProfile> = self
            .profiles
            .values()
            .filter(|profile| self.profile_matches(profile, &merged))
            .collect();

        candidates.sort_by_key(|profile| {
            (
                profile.cost_tier,
                profile.latency_tier,
                profile.reasoning_tier,
                profile.id.clone(),
            )
        });

        candidates
            .first()
            .and_then(|profile| self.resolve_spec(&profile.id))
            .or_else(|| fallback_profile.and_then(|profile_id| self.resolve_spec(profile_id)))
            .or_else(|| self.resolve_default_spec())
    }

    fn profile_matches_policy(&self, profile_id: &str, policy: &ModelSelectionPolicy) -> bool {
        self.profile(profile_id)
            .map(|profile| self.profile_matches(profile, policy))
            .unwrap_or(false)
    }

    fn profile_matches(&self, profile: &ModelProfile, policy: &ModelSelectionPolicy) -> bool {
        if !policy.allow_profiles.is_empty() && !policy.allow_profiles.iter().any(|id| id == &profile.id) {
            return false;
        }
        if policy.deny_profiles.iter().any(|id| id == &profile.id) {
            return false;
        }
        if let Some(max_cost) = policy.max_cost_tier {
            if profile.cost_tier > max_cost {
                return false;
            }
        }
        if let Some(max_latency) = policy.max_latency_tier {
            if profile.latency_tier > max_latency {
                return false;
            }
        }
        if let Some(min_reasoning) = policy.min_reasoning_tier {
            if profile.reasoning_tier < min_reasoning {
                return false;
            }
        }
        if policy.require_tool_calling == Some(true) && !profile.supports_tool_calling {
            return false;
        }
        if !policy.required_traits.iter().all(|required| profile.traits.contains(required)) {
            return false;
        }
        true
    }
}

impl ModelSelectionPolicy {
    pub fn merge(&self, overlay: &ModelSelectionPolicy) -> ModelSelectionPolicy {
        ModelSelectionPolicy {
            preferred_profiles: merge_unique(&overlay.preferred_profiles, &self.preferred_profiles),
            allow_profiles: intersect_or_inherit(&self.allow_profiles, &overlay.allow_profiles),
            deny_profiles: merge_unique(&self.deny_profiles, &overlay.deny_profiles),
            required_traits: merge_unique(&self.required_traits, &overlay.required_traits),
            max_cost_tier: more_restrictive_min(self.max_cost_tier, overlay.max_cost_tier),
            max_latency_tier: more_restrictive_min(self.max_latency_tier, overlay.max_latency_tier),
            min_reasoning_tier: more_restrictive_max(self.min_reasoning_tier, overlay.min_reasoning_tier),
            require_tool_calling: match (self.require_tool_calling, overlay.require_tool_calling) {
                (Some(a), Some(b)) => Some(a || b),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            },
        }
    }
}

fn merge_unique<T: Clone + PartialEq>(a: &[T], b: &[T]) -> Vec<T> {
    let mut merged = Vec::new();
    for item in a.iter().chain(b.iter()) {
        if !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    merged
}

fn intersect_or_inherit<T: Clone + PartialEq>(base: &[T], overlay: &[T]) -> Vec<T> {
    match (base.is_empty(), overlay.is_empty()) {
        (true, true) => Vec::new(),
        (true, false) => overlay.to_vec(),
        (false, true) => base.to_vec(),
        (false, false) => base
            .iter()
            .filter(|item| overlay.contains(item))
            .cloned()
            .collect(),
    }
}

fn more_restrictive_min<T: Copy + Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(std::cmp::min(a, b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn more_restrictive_max<T: Copy + Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(std::cmp::max(a, b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn normalize_capability_card(mut card: CapabilityCard) -> CapabilityCard {
    let mut terms = tokenize_terms(&card.name);
    terms.extend(tokenize_terms(&card.semantic_description));
    for tag in &card.tags {
        terms.extend(tokenize_terms(tag));
    }
    terms.extend(schema_terms(&card.input_schema));
    terms.extend(schema_terms(&card.output_schema));
    for permission in &card.permissions {
        terms.extend(tokenize_terms(&format!("{permission:?}")));
    }
    terms.extend(tokenize_terms(&format!("{:?}", card.trust)));
    terms.extend(tokenize_terms(&card.provenance.source));
    if let Some(path) = &card.provenance.path {
        terms.extend(tokenize_terms(path));
    }
    if let Some(reference) = &card.provenance.reference {
        terms.extend(tokenize_terms(reference));
    }
    if let Some(hints) = &card.routing_hints {
        for tag in &hints.preferred_tags {
            terms.extend(tokenize_terms(tag));
        }
    }

    terms.sort();
    terms.dedup();

    if card.tags.is_empty() {
        card.tags = infer_tags(&terms);
    }
    card.semantic_terms = terms;
    card
}

fn schema_terms(schema: &serde_json::Value) -> Vec<String> {
    let mut terms = Vec::new();
    if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
        for (name, value) in properties {
            terms.extend(tokenize_terms(name));
            if let Some(description) = value.get("description").and_then(|d| d.as_str()) {
                terms.extend(tokenize_terms(description));
            }
        }
    }
    terms
}

fn infer_tags(terms: &[String]) -> Vec<String> {
    let known_tags = [
        "email", "calendar", "filesystem", "search", "web", "read", "write", "code",
        "browse", "analysis",
    ];
    known_tags
        .iter()
        .filter(|tag| terms.iter().any(|term| term == *tag))
        .map(|tag| (*tag).to_string())
        .collect()
}

fn capability_priority(card: &CapabilityCard) -> u32 {
    let trust_bonus = match card.trust {
        CapabilityTrust::Core => 60,
        CapabilityTrust::Trusted => 40,
        CapabilityTrust::Review => 20,
        CapabilityTrust::Untrusted => 5,
    };
    let kind_bonus = match card.kind {
        CapabilityKind::Talent => 20,
        CapabilityKind::Skill(_) => 10,
        CapabilityKind::Workspace(_) => 15,
    };
    trust_bonus + kind_bonus
}

fn tokenize_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| part.len() >= 3)
        .collect()
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

    pub fn definitions_for_names(&self, names: &[String]) -> Vec<LlmToolDef> {
        names.iter()
            .filter_map(|name| self.tools.get(name).map(|tool| tool.llm_definition()))
            .collect()
    }

    pub fn capability_registry(&self) -> CapabilityRegistry {
        let mut registry = CapabilityRegistry::new();
        for tool in self.tools.values() {
            if let Some(card) = tool.capability_card() {
                registry.register(card);
            }
        }
        registry
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
