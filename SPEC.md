# MAAT — Agent Harness Specification

> *Ma'at: the ancient Egyptian concept of truth, balance, and cosmic order.*
> A Rust framework for orchestrating multi-user, multi-model, recursive AI agent sessions.

This spec now serves two jobs:
- describe the target architecture
- record the transitional decisions already shaping the codebase

Current truth:
- MAAT is no longer just a clean session-tree sketch
- it already has artifacts, automations, heralds, prompt-driven routing, installed skills, identity-gated Telegram ingress, and background runs
- the next phase is therefore about smoothing overlap and improving recovery, not inventing the first architecture from scratch

---

## Overview

MAAT is a Rust-based agent harness built around a clear three-tier session model per user:

1. **PHAROH** — one per user, always-on. The channel pinch point: all inbound messages from every herald arrive here, all outbound responses leave here. Maintains a lightweight registry of active named sessions with brief status summaries. Handles simple requests directly; routes complex or long-running work to named sessions.

2. **Named Sessions** — zero or more per user, long-lived, user-addressable by name. Each runs its own VIZIER orchestrator. Reports status summaries back to PHAROH. Can receive multi-turn messages directly from PHAROH (routed via herald `@name:` addressing). MINIONs live beneath these.

3. **MINIONs** — ephemeral workers spawned by a VIZIER. Each executes a bounded task (LLM call ± tool calls) then terminates. Assigned a specific model and a resolved set of capabilities from the **Capability Registry**.

Tools, skills, and workspace integrations are described by **Capability Cards** — structured records that VIZIER uses to match workflow steps to available capabilities at runtime.

Current runtime truth:
- PHAROH also contains a few pragmatic fast paths for direct answers and narrow direct invocation cases
- VIZIER is partly heuristic and partly prompt-driven
- installed skills are both semantic capabilities and executable tools
- artifacts and runs are now first-class durable references with human-readable handles

That is useful progress, but it means overlap resolution should now be treated as a first-class subsystem.

---

## Transitional Decisions

### 1. Explicit invocation should beat soft inference

Once the runtime contains:
- models
- skills
- artifacts
- channels
- automations

natural language alone is not always a sufficient control surface.

When the user is clearly naming a skill, model, artifact, or run, MAAT should have a stable explicit invocation layer rather than hoping semantic routing infers the same thing every time.

The exact syntax is still open. Good candidates include:
- qualifier-style references
- slash-command subcommands
- inline handles with autocomplete support

The important design choice is not the syntax itself. It is that explicit invocation should be recognized before soft intent routing.

### 2. Routing should be layered

The desired routing order is:
1. explicit invocation detection
2. prompt-driven intent classification
3. capability shortlist or nudge
4. final model resolution under hard policy
5. execution and recovery

This is better than pure keyword routing, pure prompt routing, or pure deterministic overrides.

### 3. Skills need cleaner runtime contracts

Installed skills should converge on:
- structured inputs
- structured outputs
- explicit artifact-in and artifact-out support
- explicit execution metadata

This is especially important for local deterministic tools such as image rectification, OCR, or scanning pipelines that do not need an LLM generation step.

### 4. Autonomy requires self-healing

The system should increasingly recover from routine failures itself, including:
- unloaded but installable skills
- stale local paths
- missing output directories
- artifact-handle resolution mistakes
- route/model mismatches

That does not mean hiding errors. It means:
- detect
- retry or repair if safe
- explain what happened
- surface the remaining hard blocker if recovery failed

### 5. Communication policy must become principal-aware

Inbound Telegram sender registration is already a deterministic gate.
The next extension of the spec is outbound policy:
- who can trigger external sends
- to which recipients
- through which channels
- when approval is required

That is a runtime authorization concern, not a prompt concern.

---

## Runtime Topology

```
RA  (gateway: user registry, config loader, herald bus)
│
├── User A
│   └── PHAROH_A  ◄─── all channels in/out (pinch point)
│         │
│         │  SessionRegistry (lightweight summaries only)
│         │  ┌─────────────────┬──────────────────┐
│         │  │  "coding"       │  "research"      │  ← Named Sessions
│         │  │  status: active │  status: waiting │
│         │  └────────┬────────┴────────┬─────────┘
│         │           │                 │
│         │      Session:coding    Session:research
│         │      (VIZIER inside)   (VIZIER inside)
│         │           │
│         │     Workflow: DAG of steps
│         │     ├── [Envelope] → MINION (capabilities: A, B)
│         │     └── [Envelope] → MINION (capabilities: C)
│         │                          └── sub-VIZIER (if depth allows)
│         │
│         └── simple requests answered directly by PHAROH (no session spawned)
│
├── User B
│   └── PHAROH_B  (isolated, same structure)
│
└── Shared (process-wide)
    ├── CapabilityRegistry  — all known capability cards
    ├── TALENTS             — intrinsic built-in tools
    ├── SKILLS              — plugin tools (CLI / HTTP / stdio / WASM)
    └── KAP                 — workspace providers (email, calendar …)
```

**Key invariants:**
- PHAROH sees **no sub-MINION detail** — only the named session summary
- Named sessions are the only level that receives **multi-turn conversation** from the user
- MINIONs are **always ephemeral** — they return a result and die
- VIZIER lives **inside sessions** (PHAROH and Named), not as a standalone tier
- All inter-component communication uses the **Envelope** protocol
- The tree is fully **async** (tokio); branches under a VIZIER run concurrently

---

## Components

---

### RA — Gateway & User Registry
*"Ra: the source from which all begins."*

The runtime entry point and multi-user registry.

**Responsibilities:**
- Parse system config from the `.md` bouquet at startup
- Maintain a registry of known users (`UserId → PharohHandle`)
- Receive inbound events from all HERALD channels; route to the correct PHAROH
- Spawn a new PHAROH for a user on first contact; resume existing session otherwise
- Lifecycle management: graceful shutdown, health checks

**Config bouquet** — a directory of `.md` files RA reads at boot:
```
config/
  system.md        # global system prompt / persona
  models.md        # available model providers + params
  users.md         # known users, auth, per-user overrides
  heralds.md       # channel credentials and settings
  kap.md           # workspace integration settings
  features.md      # feature flags
  capabilities.md  # capability card overrides / plugin registrations
```

```rust
struct Ra {
    user_registry: HashMap<UserId, PharohHandle>,
    config: MaatConfig,
    herald_bus: HeraldBus,              // fan-in mpsc from all heralds
    capability_registry: Arc<CapabilityRegistry>,
}

struct MaatConfig {
    system_prompt: String,
    models: Vec<ModelSpec>,
    herald_configs: Vec<HeraldConfig>,
    kap_config: KapConfig,
    features: FeatureFlags,
}
```

---

### PHAROH — Per-User Main Session & Channel Hub
*"Pharaoh: sovereign of a domain — all roads lead here."*

One PHAROH per user. The permanent, always-on session. Its primary roles are:
1. **Channel hub** — the single point all inbound messages arrive at and all outbound responses leave from, regardless of herald
2. **Session supervisor** — owns a registry of named sessions, each with a lightweight summary
3. **Router** — decides whether to handle a request directly or dispatch to a named session
4. **Compact orchestrator** — for simple requests it runs its own embedded VIZIER; for long-running or named work it delegates

**PHAROH does NOT:**
- Know the internal state of sub-MINIONs
- See individual tool calls made inside named sessions
- Manage workflow DAGs itself (those belong to the named session's VIZIER)

```rust
struct Pharoh {
    user_id: UserId,
    context: ConversationContext,       // PHAROH's own conversation history (compact)
    memory: MemoryStore,                // long-term user memory
    session_registry: SessionRegistry,  // named sessions + their summaries
    vizier: Vizier,                     // PHAROH's own embedded orchestrator
    config: PharohConfig,
    outbound_tx: mpsc::Sender<OutboundMessage>,  // back to RA → herald
}

struct PharohConfig {
    default_model: ModelSpec,
    persona: String,                    // per-user system prompt overlay
    max_session_depth: usize,           // max VIZIER recursion depth in any session
    max_named_sessions: usize,          // cap on concurrent named sessions
    concurrency_limit: usize,           // max parallel MINIONs across all sessions
}

/// Lightweight registry — PHAROH only holds summaries, not full session state
struct SessionRegistry {
    sessions: IndexMap<SessionName, SessionEntry>,
}

struct SessionEntry {
    name: SessionName,
    handle: NamedSessionHandle,         // tokio task handle + mailbox tx
    summary: SessionSummary,            // updated by the session on state change
    created_at: DateTime<Utc>,
    last_active: DateTime<Utc>,
    status: SessionStatus,
}

struct SessionSummary {
    one_liner: String,                  // e.g. "Refactoring auth module, step 3/5"
    current_step: Option<String>,
    pending_tool_calls: u32,
    tokens_used: u64,
}

enum SessionStatus {
    Idle,
    Working,
    AwaitingUserInput,
    Completed,
    Failed(String),
}
```

**Routing logic (in PHAROH's event loop):**

```rust
enum RoutingDecision {
    AnswerDirectly,                     // simple Q&A, PHAROH handles with its own VIZIER
    RouteToSession(SessionName),        // addressed session exists → forward envelope
    SpawnSession(SessionName, Task),    // new named session needed
    ListSessions,                       // user asked "what are you working on?"
    EndSession(SessionName),            // user asks to stop a session
}
```

A message is routed to a named session when:
- The herald prefix `@name:` is present (e.g. Telegram: `@coding: explain this function`)
- PHAROH's LLM layer classifies the request as belonging to an ongoing named session
- The user explicitly spawns a session (`/session new coding: refactor auth`)

PHAROH summarises all active sessions to the user on demand:
```
> what are you working on?
PHAROH: Three active sessions:
  coding   [working]  Refactoring auth module — on step 3 of 5
  research [idle]     Literature review on RAG architectures — waiting for you
  email    [done]     Drafted reply to Alice, ready to send
```

---

### Named Session
*"The vizier's domain — a long-running thread of work with its own identity."*

A Named Session is a persistent, user-addressable agent context. It sits between PHAROH and MINIONs. It has its own conversation context, its own VIZIER, and its own tool grants.

**Responsibilities:**
- Maintain full conversation context for its domain of work
- Run its embedded VIZIER to decompose incoming tasks into workflows
- Report `SessionSummary` to PHAROH whenever its status changes
- Accept multi-turn messages forwarded from PHAROH
- Notify PHAROH when it completes, needs input, or fails

```rust
struct NamedSession {
    name: SessionName,
    user_id: UserId,
    context: ConversationContext,           // full history for this session
    vizier: Vizier,                         // this session's orchestrator
    capability_grants: Vec<CapabilityId>,   // what this session is allowed to use
    pharoh_tx: mpsc::Sender<PharohMessage>, // to report summaries / completion
    config: NamedSessionConfig,
}

struct NamedSessionConfig {
    model: ModelSpec,
    max_minion_depth: usize,
    concurrency_limit: usize,
    auto_summarise_interval: Duration,      // how often to push summary to PHAROH
}

enum NamedSessionMessage {
    UserMessage(Envelope),                  // forwarded from PHAROH
    VizierResult(Envelope),                 // completed workflow step
    Shutdown,
}
```

---

### VIZIER — Orchestrator
*"Vizier: the official who directs the work and guards its integrity."*

VIZIER is not a standalone service — it is **embedded inside every session** (PHAROH and Named Sessions). When a task arrives that requires sub-task decomposition, the session's VIZIER takes over.

**Responsibilities:**
- Receive a task (via Envelope) from its parent session
- Decompose the task into a **Workflow** (DAG of steps)
- For each step: consult the **CapabilityRegistry** to resolve required capabilities → select model → build Envelope
- Dispatch Envelopes to MINIONs concurrently or sequentially per DAG topology
- Validate and assemble results; detect errors; apply retry policy; escalate failures
- Return a result Envelope to the parent session

```rust
struct Vizier {
    session_id: SessionId,
    depth: usize,                           // current depth in the tree
    max_depth: usize,                       // inherited from session config
    capability_registry: Arc<CapabilityRegistry>,
    model_router: ModelRouter,
}

struct ModelRouter {
    rules: Vec<RoutingRule>,                // tag → ModelSpec mappings
    fallback: ModelSpec,
}

struct RoutingRule {
    capability_tags: Vec<String>,           // if step requires these tags…
    model: ModelSpec,                       // …use this model
}
```

---

### MINION — Ephemeral Worker
*"Minions: the workers who carry out bounded tasks."*

MINIONs are ephemeral — spawned for one task, they execute and terminate. Each MINION is instantiated with a **resolved capability set** drawn from the CapabilityRegistry by VIZIER.

**Responsibilities:**
- Accept an Envelope specifying the task + assigned capabilities
- Execute: LLM inference loop with tool calls until `EndTurn` or error
- Optionally spawn a sub-VIZIER for further decomposition (if depth allows)
- Return a result Envelope to the parent VIZIER

```rust
struct Minion {
    id: MinionId,
    task: TaskPayload,
    model: ModelSpec,
    capabilities: Vec<ResolvedCapability>,  // assigned by VIZIER from CapabilityRegistry
    sub_vizier: Option<Vizier>,             // only if allow_recursion && depth < max
}

struct ResolvedCapability {
    card: CapabilityCard,                   // the full card
    tool: Box<dyn Tool>,                    // the live callable
}
```

---

### Capability Cards & Registry
*"The scroll that describes what each worker can do."*

A **CapabilityCard** is a structured description of one tool, skill, or workspace operation. VIZIER reads cards to match workflow steps to available capabilities. MINIONs are instantiated with resolved capabilities — they never query the registry directly.

```rust
struct CapabilityCard {
    id: CapabilityId,                       // stable identifier e.g. "web.fetch"
    name: String,                           // human name e.g. "Web Fetch"
    description: String,                    // natural language — used by VIZIER for LLM-assisted matching
    kind: CapabilityKind,
    tags: Vec<String>,                      // e.g. ["web", "read", "network"]
    input_schema: JsonSchema,
    output_schema: JsonSchema,
    cost_profile: CostProfile,
    permissions: Vec<Permission>,           // e.g. [Network, FileRead]
    available_to: AvailabilityScope,        // All | NamedOnly | ExplicitGrant
}

enum CapabilityKind {
    Talent(TalentId),                       // built-in, compiled in
    Skill(PluginManifest),                  // external plugin
    Workspace(WorkspaceOp),                 // KAP operation (email.send, cal.create_event …)
}

struct CostProfile {
    latency_hint: LatencyHint,              // Fast | Medium | Slow
    token_cost: Option<f32>,               // rough tokens-per-call estimate
    monetary_cost: Option<f32>,            // rough $/call estimate
    rate_limit: Option<RateLimit>,
}

enum LatencyHint { Fast, Medium, Slow }    // used by VIZIER for scheduling hints

struct AvailabilityScope {
    all_sessions: bool,                     // available everywhere by default
    named_sessions_only: bool,              // only in named sessions, not PHAROH direct
    requires_explicit_grant: bool,          // must be explicitly granted per session
}

/// Process-wide registry, shared via Arc
struct CapabilityRegistry {
    cards: HashMap<CapabilityId, CapabilityCard>,
    tools: HashMap<CapabilityId, Box<dyn Tool>>,
}

impl CapabilityRegistry {
    /// VIZIER calls this to resolve a step's required capabilities
    fn resolve(
        &self,
        required_tags: &[String],
        grants: &[CapabilityId],
    ) -> Vec<ResolvedCapability>;

    /// Register a new capability at runtime (plugin load, KAP init)
    fn register(&mut self, card: CapabilityCard, tool: Box<dyn Tool>);
}
```

**How VIZIER uses cards to build a workflow step:**

```
WorkflowStep has:
  required_capabilities: ["web", "read"]   ← tags the step needs
  preferred_latency: Fast
  budget_tokens: 1000

VIZIER queries CapabilityRegistry:
  → finds: web.fetch (tags: web, read, network), web.search (tags: web, search)
  → filters by session grants
  → selects model via ModelRouter (Fast latency → haiku)
  → instantiates MINION with resolved capabilities + model
```

---

### Workflow — Task Graph

A Workflow is a DAG of steps that a VIZIER constructs before dispatching any MINIONs. Steps declare **what capabilities they need** — VIZIER resolves the actual capabilities at dispatch time.

```rust
struct Workflow {
    id: WorkflowId,
    name: String,
    steps: Vec<WorkflowStep>,
    edges: Vec<(StepId, StepId)>,           // dependency: A must complete before B starts
    execution: ExecutionMode,
}

enum ExecutionMode {
    Sequential,                             // steps in order
    Parallel,                               // all steps concurrently
    Dag,                                    // respects edge graph; parallel where possible
}

struct WorkflowStep {
    id: StepId,
    name: String,
    description: String,                    // natural language, used to build MINION prompt
    required_capability_tags: Vec<String>,  // e.g. ["web", "read"]
    preferred_capability_ids: Vec<CapabilityId>,  // optional explicit overrides
    model_hint: Option<ModelHint>,          // Fast | Capable | Reasoning
    allow_recursion: bool,                  // can this MINION spawn a sub-VIZIER?
    timeout_ms: u64,
    retry: RetryPolicy,
    depends_on: Vec<StepId>,
}

enum ModelHint {
    Fast,       // prefer cheap/quick (haiku-class)
    Capable,    // prefer mid-tier (sonnet-class)
    Reasoning,  // prefer frontier (opus-class)
}

struct RetryPolicy {
    max_attempts: u32,
    backoff_ms: u64,
    retry_on: Vec<RetryCondition>,
}

enum RetryCondition {
    Timeout, RateLimit, TransientNetwork, ModelOverload
}
```

**Workflow lifecycle:**

```
1. Session receives task (Envelope)
2. Session's VIZIER calls LLM to produce a Workflow (or uses a template)
3. VIZIER topologically sorts steps by dependency edges
4. Independent steps dispatched concurrently (JoinSet + Semaphore)
5. Each step: resolve capabilities → build Envelope → spawn MINION
6. MINION result arrives → feed output into dependent steps' context
7. All steps complete → assemble final result Envelope → return to session
8. Session updates SessionSummary → push to PHAROH
```

---

### Envelope Protocol

There is no single mega-envelope type. Each communication path has its own purpose-built envelope. All share a common `EnvelopeHeader`. The control plane (`StatusEvent`, `ControlMessage`) is entirely separate from the data plane and **never touches an LLM**.

---

#### Shared Header

```rust
/// Embedded in every envelope type
struct EnvelopeHeader {
    id: Ulid,                           // unique per envelope
    parent_id: Option<Ulid>,            // envelope that caused this one
    trace_id: Ulid,                     // constant across entire request tree
    sender: ComponentAddress,
    recipient: ComponentAddress,
    created_at: DateTime<Utc>,
    priority: Priority,
}

enum Priority { Low, Normal, High, Critical }

/// e.g. "pharoh:user_A", "named_session:user_A:coding", "minion:user_A:m_01J..."
type ComponentAddress = String;
```

---

#### Data-Plane Envelopes

**1. `HeraldEnvelope` — Herald ↔ RA ↔ PHAROH**

Raw inbound message from an external channel. Stays close to the wire format; no capability or workflow information.

```rust
struct HeraldEnvelope {
    header: EnvelopeHeader,
    channel: ChannelId,
    user_id: UserId,
    message: ChannelMessage,
}

enum ChannelMessage {
    Text(String),
    Media { mime_type: String, url: String, caption: Option<String> },
    Command { name: String, args: Vec<String> },   // e.g. /session new coding
    SessionAddress { target: SessionName, text: String }, // @coding: do X
}
```

**2. `SessionEnvelope` — PHAROH ↔ Named Session**

Task delegation from PHAROH to a named session, or a forwarded user message. Carries a context excerpt (not the full history — PHAROH does not own session history).

```rust
struct SessionEnvelope {
    header: EnvelopeHeader,
    session_name: SessionName,
    kind: SessionEnvelopeKind,
}

enum SessionEnvelopeKind {
    /// New task delegated by PHAROH
    Task {
        description: String,
        context_excerpt: Vec<Message>,  // relevant prior messages, PHAROH-selected
        reply_channel: ChannelId,       // where the eventual response should go
        deadline: Option<DateTime<Utc>>,
    },
    /// User message forwarded directly to the session (@name: ...)
    UserMessage {
        text: String,
        channel: ChannelId,
    },
    /// PHAROH requesting a fresh summary (e.g. user asked "what are you doing?")
    SummaryRequest,
}
```

**3. `TaskEnvelope` — VIZIER → MINION**

The workhorse. Carries the task, capability references (IDs only — never full cards), and model spec. MINIONs resolve capability IDs against the process-global `CapabilityRegistry`.

```rust
struct TaskEnvelope {
    header: EnvelopeHeader,
    workflow_id: WorkflowId,
    step_id: StepId,
    depth: usize,

    /// What the MINION must accomplish
    task: MinionTask,

    /// Capability IDs only — MINION resolves locally, no cards in the envelope
    capability_refs: Vec<CapabilityId>,

    /// Model and inference params for this specific step
    model: ModelSpec,

    /// Hard deadline; MINION cancels itself if exceeded
    deadline_ms: u64,
}

struct MinionTask {
    description: String,
    context_window: Vec<Message>,       // the messages this MINION sees
    output_schema: Option<JsonSchema>,  // if structured output is expected
    allow_recursion: bool,              // may this MINION spawn a sub-VIZIER?
}

struct ModelSpec {
    provider: Provider,                 // Anthropic | OpenAICompat | Local
    model_id: String,                   // e.g. "claude-opus-4-6"
    temperature: f32,
    max_tokens: u32,
    top_p: Option<f32>,
}

/// When the MINION makes its LLM API call it builds tool definitions from resolved
/// capability cards, but sends ONLY the minimal schema — name, description,
/// input_schema. Cost profiles, tags, and permissions stay local.
struct LlmToolDef {
    name: String,
    description: String,
    input_schema: JsonSchema,
}
```

**4. `ResultEnvelope` — MINION → VIZIER**

Structured result returned when a MINION completes. VIZIER uses this to feed downstream steps and assemble the final session response.

```rust
struct ResultEnvelope {
    header: EnvelopeHeader,
    workflow_id: WorkflowId,
    step_id: StepId,
    status: StepStatus,
    output: StepOutput,
    tool_calls: Vec<ToolCallRecord>,
    usage: TokenUsage,
    duration_ms: u64,
}

enum StepStatus {
    Ok,
    Error(MaatError),
    Timeout,
    Cancelled,
}

enum StepOutput {
    Text(String),
    Structured(serde_json::Value),      // when output_schema was provided
    Empty,
}

struct ToolCallRecord {
    capability_id: CapabilityId,
    tool_name: String,
    input: serde_json::Value,
    output: serde_json::Value,
    duration_ms: u64,
    success: bool,
}

struct TokenUsage {
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
}
```

---

#### Control-Plane Types

These are **never passed to an LLM**. They are handled deterministically by Rust pattern-matching in each component's event loop. This is how PHAROH maintains visibility and takes control actions without polling.

**`StatusEvent` — typed state broadcast**

Every session and workflow emits `StatusEvent` on every state transition. PHAROH subscribes via a `broadcast::Receiver` and updates its `SessionRegistry` summaries in real time.

```rust
struct StatusEvent {
    emitted_at: DateTime<Utc>,
    source: ComponentAddress,
    user_id: UserId,
    kind: StatusKind,
}

enum StatusKind {
    SessionStateChanged {
        session_name: SessionName,
        state: SessionState,
    },
    WorkflowStateChanged {
        session_name: SessionName,
        workflow_id: WorkflowId,
        state: WorkflowState,
        steps_complete: u32,
        steps_total: u32,
    },
    MinionStarted {
        session_name: SessionName,
        minion_id: MinionId,
        step_id: StepId,
        capability_refs: Vec<CapabilityId>,
    },
    MinionCompleted {
        minion_id: MinionId,
        step_id: StepId,
        status: StepStatus,
        usage: TokenUsage,
    },
    SummaryUpdated {
        session_name: SessionName,
        summary: SessionSummary,
    },
}

/// Typed state machine for a named session
enum SessionState {
    Idle,
    Running { step_id: StepId, started_at: DateTime<Utc> },
    Blocked { reason: BlockReason },
    AwaitingUser { prompt: String },
    Completed { at: DateTime<Utc> },
    Failed { error: String, at: DateTime<Utc> },
    Cancelled,
}

enum BlockReason {
    RateLimit { retry_after: Duration },
    CapabilityUnavailable(CapabilityId),
    UserApprovalRequired { action: String },
}

/// Typed state machine for a workflow
enum WorkflowState {
    Pending,
    Running { completed: u32, total: u32 },
    Paused,
    Completed,
    Failed(String),
}
```

**`ControlMessage` — PHAROH / RA → any session or workflow**

PHAROH sends these based on typed state (e.g. `SessionState::Failed` → send `Restart`). No LLM involved.

```rust
struct ControlMessage {
    header: EnvelopeHeader,
    target: ComponentAddress,
    command: ControlCommand,
}

enum ControlCommand {
    Pause,
    Resume,
    Cancel,
    Kill,                               // hard stop, no cleanup
    Restart { preserve_context: bool }, // restart session from current state
    SetPriority(Priority),
    RequestSummary,
}
```

**PHAROH control logic (pure Rust, no LLM):**

```rust
// Called whenever a StatusEvent arrives in PHAROH's event loop
fn handle_status_event(&mut self, event: StatusEvent) {
    match event.kind {
        StatusKind::SessionStateChanged { session_name, state } => {
            self.session_registry.update_state(&session_name, &state);

            match state {
                SessionState::Failed { .. } => {
                    if self.config.auto_restart_on_failure {
                        self.send_control(session_name, ControlCommand::Restart {
                            preserve_context: true,
                        });
                    } else {
                        self.notify_user_of_failure(&session_name);
                    }
                }
                SessionState::AwaitingUser { prompt } => {
                    self.forward_to_user(&session_name, &prompt);
                }
                SessionState::Completed { .. } => {
                    self.session_registry.mark_complete(&session_name);
                }
                _ => {}
            }
        }
        StatusKind::SummaryUpdated { session_name, summary } => {
            self.session_registry.update_summary(&session_name, summary);
        }
        _ => {}
    }
}
```

---

#### Capability Reference Rule

**The rule:** envelopes carry `CapabilityId` strings only. No card data, no schemas, no cost profiles travel in any envelope.

```
CapabilityRegistry (Arc, process-global)
        │
        │  resolved at MINION init
        ▼
  ResolvedCapability { card, tool }
        │
        │  only name + description + input_schema sent to LLM
        ▼
  LlmToolDef  (minimal — what the model needs to call the tool)
```

This means:
- Envelopes stay small regardless of how many capabilities exist
- The LLM never sees cost profiles, permissions, or tags — those are internal routing data
- Adding new capabilities to the registry does not change envelope schemas

---

### HERALDS — Channel Adapters
*"Heralds: messengers between the outer world and the court."*

Each HERALD is an adapter for one external communication channel. They all funnel inbound messages into RA's unified event bus and accept outbound responses from RA.

**Planned channels:**
- WhatsApp (via Business API / Baileys)
- Telegram Bot API
- Web (HTTP REST + WebSocket)
- TUI (terminal — `ratatui`)
- Email (IMAP/SMTP)

```rust
#[async_trait]
trait Herald: Send + Sync {
    fn channel_id(&self) -> ChannelId;
    async fn start(&self, tx: mpsc::Sender<InboundEvent>) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
}

struct InboundEvent {
    channel: ChannelId,
    user_id: UserId,
    message: ChannelMessage,
    received_at: DateTime<Utc>,
}

struct OutboundMessage {
    channel: ChannelId,
    user_id: UserId,
    content: MessageContent,
}

// Concrete impls
struct TelegramHerald { bot_token: String, /* … */ }
struct WhatsAppHerald { /* … */ }
struct WebHerald      { bind_addr: SocketAddr, /* … */ }
struct TuiHerald      { /* … */ }
struct EmailHerald    { imap: ImapConfig, smtp: SmtpConfig }
```

---

### Tool Trust Model

Tools fall into two trust tiers. The tier determines how the tool is executed — the `Tool` trait is identical for both. **Permission checking always happens at VIZIER dispatch time, before a MINION is instantiated.** A MINION that has been given a capability can call it freely; it will never receive a capability it hasn't been granted.

```rust
/// Universal tool interface — same for both trust tiers
#[async_trait]
trait Tool: Send + Sync {
    fn capability_id(&self) -> &CapabilityId;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &JsonSchema;
    fn output_schema(&self) -> &JsonSchema;
    fn trust_tier(&self) -> TrustTier;
    async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value>;
}

enum TrustTier {
    Trusted,    // TALENTS — compiled in, same process
    Sandboxed,  // SKILLS — isolated execution
}
```

---

### TALENTS — Trusted Intrinsic Tools

Compiled directly into the binary. Run in the same tokio runtime. Trust derives from compilation and code review — no additional sandboxing overhead. These are the tools MAAT ships with and maintains.

**Permission model:** Talents still declare the permissions they require (in their `CapabilityCard`). VIZIER checks these against session grants before allocation. But at execution time, enforcement is by Rust's type system and code review — not a runtime sandbox.

**Planned talents:**

| ID | Description |
|---|---|
| `web.fetch` | HTTP GET with HTML/markdown extraction |
| `web.search` | Search engine query, returns ranked results |
| `fs.read` | Read a file from the local filesystem |
| `fs.write` | Write a file to the local filesystem |
| `fs.list` | List directory contents |
| `code.run` | Execute code in a sandboxed subprocess (language-configurable) |
| `math.eval` | Arithmetic and symbolic computation |
| `datetime.now` | Current time, timezone conversion, date arithmetic |
| `json.transform` | jq-style JSON transformation |
| `text.extract` | Extract structured data from unstructured text |

```rust
trait Talent: Tool {}

/// Talents self-register at startup
struct TalentRegistry {
    talents: HashMap<CapabilityId, Box<dyn Talent>>,
}

impl TalentRegistry {
    fn register_defaults(&mut self) {
        self.register(WebFetchTalent::new());
        self.register(WebSearchTalent::new());
        // …
    }
}
```

---

### SKILLS — Sandboxed Plugin Tools

Externally provided tools that extend MAAT without recompilation. Because the code is not compiled into the binary and may be third-party, all skills run in an **isolation boundary**. The boundary mode is declared in the `PluginManifest` and enforced at load time.

#### Sandbox modes

**Mode 1: WASM (preferred for lightweight skills)**

Runs the plugin as a WASM module via `wasmtime`. Provides strong memory isolation and capability-based access control — the module can only access what is explicitly imported (no ambient authority). Fast startup, no container overhead. Suitable for: data processing, custom formatters, domain-specific logic, lightweight APIs.

```
Plugin WASM module
  │
  ├── can only call explicitly imported host functions
  ├── no filesystem access unless Permission::FileRead granted
  ├── no network access unless Permission::Network granted
  └── memory isolated from host process
```

**Mode 2: Subprocess with optional Docker isolation**

For plugins that need a specific language runtime, native system calls, or heavy dependencies. Communicates via MCP-compatible stdio (JSON-RPC over stdin/stdout) or HTTP sidecar.

- **Bare subprocess** — process isolation via OS; suitable for trusted third-party tools run locally
- **Docker container** — full isolation: seccomp profile, network namespace, image pinned by digest, read-only root filesystem, no capabilities. Suitable for untrusted or user-provided plugins.

```
Docker container
  ├── image: sha256:<pinned digest>
  ├── network: none | restricted (only declared endpoints)
  ├── filesystem: read-only root + explicit volume mounts
  ├── seccomp: default Docker profile
  └── communicates via: stdio (MCP) | HTTP on loopback
```

#### Plugin Manifest

```rust
struct PluginManifest {
    id: CapabilityId,
    name: String,
    version: String,
    description: String,
    tags: Vec<String>,
    sandbox: SandboxMode,
    permissions: Vec<Permission>,       // declared required permissions
    input_schema: JsonSchema,
    output_schema: JsonSchema,
    cost_profile: CostProfile,
}

enum SandboxMode {
    Wasm {
        module_path: PathBuf,           // local .wasm file
        allowed_imports: Vec<String>,   // host functions the module may call
    },
    Subprocess {
        command: String,
        args: Vec<String>,
        protocol: SubprocessProtocol,   // Mcp | HttpLocal
        timeout_ms: u64,
    },
    Docker {
        image_digest: String,           // sha256:<digest> — no tags
        command: Option<Vec<String>>,
        protocol: SubprocessProtocol,
        network_policy: DockerNetwork,
        volume_mounts: Vec<VolumeMount>,
        timeout_ms: u64,
    },
}

enum DockerNetwork {
    None,
    Loopback,
    Restricted { allowed_hosts: Vec<String> },
}

enum Permission {
    Network,
    FileRead,
    FileWrite,
    ProcessSpawn,
    EnvRead,
    WorkspaceEmail,
    WorkspaceCalendar,
    WorkspaceDrive,
}
```

#### VIZIER permission check at dispatch

```rust
impl Vizier {
    fn resolve_capabilities(
        &self,
        step: &WorkflowStep,
        session_grants: &[Permission],
    ) -> Result<Vec<ResolvedCapability>> {
        let candidates = self.capability_registry
            .resolve(&step.required_capability_tags, &step.preferred_capability_ids);

        for cap in &candidates {
            for required_perm in &cap.card.permissions {
                if !session_grants.contains(required_perm) {
                    return Err(MaatError::PermissionDenied {
                        capability: cap.card.id.clone(),
                        missing: required_perm.clone(),
                    });
                }
            }
        }

        Ok(candidates)
    }
}
```

Permission denied errors surface as a `StatusEvent::SessionStateChanged(Blocked(CapabilityUnavailable))` — PHAROH sees it immediately via the status bus without any LLM call.

---

### KAP — Workspace
*"Ka: the vital essence that connects to the living world."*

KAP provides a unified, per-user interface to external productivity services. It owns credential lifecycle, exposes every operation as a `Tool` callable by any MINION, and isolates provider-specific details behind a common trait.

---

#### Auth & Credential Management

Each user has a `CredentialStore` — a per-user encrypted store of OAuth2 tokens and API keys. KAP handles token refresh transparently before any provider call.

```rust
struct CredentialStore {
    user_id: UserId,
    backend: Box<dyn CredentialBackend>,  // keychain | encrypted file | secrets manager
}

#[async_trait]
trait CredentialBackend: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Secret>>;
    async fn set(&self, key: &str, secret: Secret) -> Result<()>;
    async fn delete(&self, key: &str) -> Result<()>;
}

struct OAuth2Token {
    access_token: Secret,
    refresh_token: Option<Secret>,
    expires_at: Option<DateTime<Utc>>,
    scopes: Vec<String>,
}

struct AuthConfig {
    provider: OAuthProvider,    // Google | Microsoft | Custom
    client_id: String,
    client_secret: Secret,
    redirect_uri: String,
    scopes: Vec<String>,
}
```

Token refresh is handled by a background `TokenRefresher` task that wakes 60s before expiry and silently rotates. All provider calls obtain a valid token via `CredentialStore::get_valid_token()` which refreshes inline if needed.

---

#### Provider Trait

```rust
#[async_trait]
trait WorkspaceProvider: Send + Sync {
    fn name(&self) -> &str;
    fn tool_prefix(&self) -> &str;          // e.g. "email", "cal"
    fn tools(&self) -> Vec<Box<dyn Tool>>;  // tools this provider exposes
    async fn health_check(&self) -> Result<ProviderStatus>;
    async fn authenticate(&self, store: &CredentialStore) -> Result<()>;
}

enum ProviderStatus { Ok, Degraded(String), Down(String) }

struct Kap {
    user_id: UserId,
    credentials: CredentialStore,
    providers: Vec<Box<dyn WorkspaceProvider>>,
}

impl Kap {
    // Called at PHAROH init — each provider registers its tools into the ToolRegistry
    async fn register_tools(&self, registry: &mut ToolRegistry) -> Result<()>;
    async fn health_check_all(&self) -> Vec<(String, ProviderStatus)>;
}
```

---

#### Email Provider

Protocol: IMAP (read) + SMTP (send). Supports Google, Outlook, and generic IMAP/SMTP.

```rust
struct EmailProvider {
    imap: ImapConfig,
    smtp: SmtpConfig,
    auth: AuthConfig,
}

struct ImapConfig {
    host: String,
    port: u16,
    tls: bool,
}

struct SmtpConfig {
    host: String,
    port: u16,
    tls: SmtpTls,   // None | StartTls | Tls
}
```

**Exposed tools:**

| Tool | Description |
|---|---|
| `email.list` | List messages in a folder with filters (from, to, subject, date range, unread) |
| `email.read` | Fetch a full message by ID, returns headers + body + attachments metadata |
| `email.search` | Full-text search across mailbox |
| `email.send` | Compose and send a new email |
| `email.reply` | Reply to a thread |
| `email.forward` | Forward a message |
| `email.label` | Apply / remove labels or move to folder |
| `email.delete` | Move to trash |
| `email.attachment.get` | Download a specific attachment by ID |

**Core types:**

```rust
struct EmailMessage {
    id: MessageId,
    thread_id: ThreadId,
    from: Mailbox,
    to: Vec<Mailbox>,
    cc: Vec<Mailbox>,
    subject: String,
    body_plain: Option<String>,
    body_html: Option<String>,
    attachments: Vec<AttachmentMeta>,
    labels: Vec<String>,
    date: DateTime<Utc>,
    read: bool,
}

struct AttachmentMeta {
    id: String,
    filename: String,
    mime_type: String,
    size_bytes: u64,
}

struct SendRequest {
    to: Vec<Mailbox>,
    cc: Vec<Mailbox>,
    bcc: Vec<Mailbox>,
    subject: String,
    body: String,
    reply_to_id: Option<MessageId>,
    attachments: Vec<AttachmentUpload>,
}
```

---

#### Calendar Provider

Protocol: CalDAV or provider-native API (Google Calendar, Microsoft Graph).

```rust
struct CalendarProvider {
    backend: CalendarBackend,   // CalDav | GoogleApi | MicrosoftGraph
    auth: AuthConfig,
    default_calendar_id: String,
}
```

**Exposed tools:**

| Tool | Description |
|---|---|
| `cal.list_events` | List events in a date range across one or more calendars |
| `cal.get_event` | Fetch a single event by ID |
| `cal.create_event` | Create a new event with attendees, location, description, recurrence |
| `cal.update_event` | Update fields of an existing event |
| `cal.delete_event` | Delete / cancel an event |
| `cal.find_free` | Find available time slots for a set of attendees |
| `cal.list_calendars` | List all calendars the user has access to |

**Core types:**

```rust
struct CalEvent {
    id: EventId,
    calendar_id: String,
    title: String,
    description: Option<String>,
    location: Option<String>,
    start: EventTime,
    end: EventTime,
    attendees: Vec<Attendee>,
    recurrence: Option<RecurrenceRule>,
    status: EventStatus,            // Confirmed | Tentative | Cancelled
    organizer: Mailbox,
}

enum EventTime {
    DateTime(DateTime<Utc>),
    AllDay(NaiveDate),
}

struct Attendee {
    email: String,
    name: Option<String>,
    rsvp: RsvpStatus,               // Accepted | Declined | Tentative | NeedsAction
}

struct FreeBusyQuery {
    attendees: Vec<String>,         // email addresses
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    duration_minutes: u32,          // desired meeting length
}

struct FreeSlot {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}
```

---

#### Contacts Provider

Protocol: CardDAV or Google People / Microsoft Graph.

**Exposed tools:**

| Tool | Description |
|---|---|
| `contacts.search` | Search contacts by name, email, phone |
| `contacts.get` | Fetch a full contact record |
| `contacts.create` | Create a new contact |
| `contacts.update` | Update contact fields |

---

#### Drive / Files Provider

Protocol: Google Drive API or Microsoft Graph (OneDrive).

**Exposed tools:**

| Tool | Description |
|---|---|
| `drive.list` | List files / folders with optional query |
| `drive.read` | Read file content (text files, export Google Docs as text/md) |
| `drive.search` | Full-text search across drive |
| `drive.upload` | Upload a new file or new version |
| `drive.share` | Create a sharing link or grant access |

---

#### KAP Config (in `kap.md` bouquet)

```md
# KAP — Workspace Config

## Email
provider: google          # google | microsoft | imap
imap_host: imap.gmail.com
smtp_host: smtp.gmail.com
oauth_client_id: ...

## Calendar
provider: google
default_calendar: primary

## Drive
provider: google
root_folder: My Drive

## Contacts
provider: google
```

---

## Async Architecture

MAAT is built entirely on **tokio**. Every component is a long-lived async task with a typed mailbox. Communication is via **channels**, not shared mutable state. This section specifies the channel topology, task lifecycle, back-pressure, cancellation, and streaming.

---

### Runtime Setup

```rust
// Single multi-threaded tokio runtime for the entire process
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let shutdown = CancellationToken::new();
    let ra = Ra::init(shutdown.clone()).await?;
    ra.run().await?;
    Ok(())
}
```

One `CancellationToken` is created at the root and **cloned down the entire tree**. Any component can observe `token.cancelled()` to begin graceful shutdown. RA triggers it on SIGINT / SIGTERM.

---

### Channel Topology

```
HERALDS ──(mpsc)──► RA HeraldBus ──(mpsc)──► PHAROH Mailbox
                                                    │
                                              (oneshot) task dispatch
                                                    │
                                                 VIZIER
                                              ┌────┴────────────┐
                                         (oneshot)          (oneshot)
                                              │                  │
                                           MINION_A          MINION_B
                                              │
                                         (oneshot, optional)
                                           sub-VIZIER
                                              │
                                           MINION_C
```

**Channel types used:**

| Channel | Type | Why |
|---|---|---|
| Herald → RA | `mpsc` bounded | Fan-in from N channels; bounded for back-pressure |
| RA → PHAROH | `mpsc` bounded | Each PHAROH has its own mailbox |
| PHAROH → VIZIER | `oneshot` | One task, one reply |
| VIZIER → MINION | `oneshot` per task | Scatter; VIZIER holds all handles |
| MINION → VIZIER | `oneshot` reply | Typed result or error |
| PHAROH → HERALD | `mpsc` bounded | Outbound response queue |
| Shutdown signal | `CancellationToken` | Broadcast cancel to all tasks |
| Streaming output | `mpsc` unbounded | LLM token stream to herald |

---

### PHAROH — Mailbox Event Loop

PHAROH runs as a single long-lived tokio task. Its event loop selects over three sources:

```rust
enum PharohMessage {
    Inbound(InboundEvent),          // from RA (user message via any herald)
    VizierResult(Envelope),         // completed sub-task
    Heartbeat,                      // periodic tick for memory flush, health
}

impl Pharoh {
    async fn run(mut self, mut rx: mpsc::Receiver<PharohMessage>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    self.shutdown().await;
                    break;
                }

                Some(msg) = rx.recv() => {
                    match msg {
                        PharohMessage::Inbound(event)     => self.handle_inbound(event).await,
                        PharohMessage::VizierResult(env)  => self.handle_result(env).await,
                        PharohMessage::Heartbeat          => self.heartbeat().await,
                    }
                }
            }
        }
    }
}
```

PHAROH decides synchronously whether to answer directly (simple pass-through) or dispatch to VIZIER (complex / multi-step). It **never blocks** — all I/O is awaited, all CPU work is `spawn_blocking`.

---

### VIZIER — Workflow Dispatch

VIZIER is not a persistent task — it is **invoked per orchestration request** and returns when the workflow completes. This keeps it stateless and composable.

```rust
impl Vizier {
    async fn execute(
        &self,
        workflow: Workflow,
        cancel: CancellationToken,
    ) -> Result<Vec<Envelope>> {

        match workflow.execution {
            ExecutionMode::Sequential => self.run_sequential(workflow.steps, cancel).await,
            ExecutionMode::Parallel   => self.run_parallel(workflow.steps, cancel).await,
            ExecutionMode::Dag        => self.run_dag(workflow.steps, workflow.edges, cancel).await,
        }
    }

    async fn run_parallel(
        &self,
        steps: Vec<WorkflowStep>,
        cancel: CancellationToken,
    ) -> Result<Vec<Envelope>> {
        // Spawn all MINIONs concurrently, bounded by semaphore
        let sem = Arc::new(Semaphore::new(self.concurrency_limit));
        let mut handles = JoinSet::new();

        for step in steps {
            let permit = sem.clone().acquire_owned().await?;
            let minion = self.build_minion(&step)?;
            let cancel = cancel.clone();

            handles.spawn(async move {
                let result = tokio::time::timeout(
                    Duration::from_millis(step.timeout_ms),
                    minion.run(cancel),
                ).await;
                drop(permit);
                result
            });
        }

        // Collect results; cancel all on first fatal error
        let mut results = Vec::new();
        while let Some(res) = handles.join_next().await {
            match res? {
                Ok(Ok(env))  => results.push(env),
                Ok(Err(e))   => { handles.abort_all(); return Err(e); }
                Err(_timeout) => { handles.abort_all(); return Err(MaatError::Timeout); }
            }
        }
        Ok(results)
    }
}
```

**DAG execution** uses `petgraph` to topologically sort steps and run independent nodes in parallel, gating each step on its dependency futures via `tokio::sync::Notify` or `watch`.

---

### MINION — Task Execution

Each MINION is a `tokio::spawn`ed task. It owns one LLM call (or tool call) and returns a result Envelope.

```rust
impl Minion {
    async fn run(self, cancel: CancellationToken) -> Result<Envelope> {
        // 1. Build context from envelope
        // 2. Execute: LLM call, tool call, or both in an agentic loop
        // 3. Optionally recurse via sub-VIZIER
        // 4. Return result Envelope

        tokio::select! {
            _ = cancel.cancelled() => Err(MaatError::Cancelled),
            result = self.execute_inner() => result,
        }
    }

    async fn execute_inner(&self) -> Result<Envelope> {
        let mut messages = self.build_messages();
        let mut tool_calls_made = vec![];

        loop {
            let response = self.llm_client
                .complete(&self.model, &messages, &self.tools)
                .await?;

            match response.stop_reason {
                StopReason::EndTurn => {
                    return Ok(self.build_result_envelope(response, tool_calls_made));
                }
                StopReason::ToolUse => {
                    let tool_results = self.dispatch_tools(&response.tool_calls).await?;
                    tool_calls_made.extend(response.tool_calls.clone());
                    messages.push(response.into_assistant_message());
                    messages.push(tool_results.into_tool_message());
                    // loop continues
                }
                StopReason::MaxTokens => return Err(MaatError::MaxTokensReached),
            }
        }
    }
}
```

---

### Streaming Responses

LLM responses are streamed token-by-token to HERALDs that support it (Telegram streaming, WebSocket, TUI).

```rust
// LLM client returns an async stream of events
trait LlmClient: Send + Sync {
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse>;

    // For streaming-capable HERALDs
    fn stream(&self, req: &CompletionRequest)
        -> impl Stream<Item = Result<StreamEvent>> + Send;
}

enum StreamEvent {
    TextDelta(String),              // partial token
    ToolCallStart(ToolCallId, String),
    ToolCallDelta(ToolCallId, String),
    ToolCallEnd(ToolCallId),
    Done(CompletionUsage),
    Error(LlmError),
}

// PHAROH wires the stream to the appropriate HERALD
async fn stream_to_herald(
    mut stream: impl Stream<Item = Result<StreamEvent>> + Unpin,
    tx: mpsc::Sender<OutboundMessage>,
    channel: ChannelId,
    user_id: UserId,
) {
    let mut buffer = String::new();

    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(s)) => {
                buffer.push_str(&s);
                // flush to herald on punctuation or every N chars
                if should_flush(&buffer) {
                    let _ = tx.send(OutboundMessage {
                        channel: channel.clone(),
                        user_id: user_id.clone(),
                        content: MessageContent::StreamChunk(buffer.clone()),
                    }).await;
                    buffer.clear();
                }
            }
            Ok(StreamEvent::Done(_)) => { /* final flush */ break; }
            Err(e) => { /* propagate error to herald */ break; }
            _ => {}
        }
    }
}
```

---

### Back-pressure

All `mpsc` channels are **bounded**. Senders that fill the buffer will `await` (or receive `TrySendError::Full` if using `try_send`), creating natural back-pressure from herald ingest all the way down to MINION dispatch.

```rust
const HERALD_BUS_CAPACITY:  usize = 256;
const PHAROH_MAILBOX_CAP:   usize = 64;
const OUTBOUND_QUEUE_CAP:   usize = 128;
```

If a PHAROH mailbox is full (user flooding), RA drops with a logged warning and optionally sends a "busy" reply. This prevents one user from starving others.

---

### Cancellation & Timeout

**Cancellation** flows top-down via `CancellationToken`:

```
RA token (root)
  └─ PHAROH token (child)
       └─ VIZIER token (child, per workflow)
            └─ MINION token (child, per task)
```

Cancelling a parent automatically cancels all children. A timed-out MINION cancels only its own subtree.

**Timeouts** are applied at three levels:

```rust
// 1. Per-envelope timeout (set in the Envelope itself)
tokio::time::timeout(envelope.task.deadline, minion.run()).await

// 2. Per-workflow timeout (set in WorkflowStep)
tokio::time::timeout(Duration::from_millis(step.timeout_ms), ...)

// 3. Per-tool-call timeout (set in ToolDef)
tokio::time::timeout(Duration::from_millis(tool.timeout_ms), tool.call(...))
```

---

### Retry Policy

Retries are handled at the VIZIER level, per workflow step, before escalating to the parent.

```rust
async fn run_with_retry(
    step: &WorkflowStep,
    minion: Minion,
    cancel: CancellationToken,
) -> Result<Envelope> {
    let policy = &step.retry;
    let mut attempt = 0;

    loop {
        attempt += 1;
        match minion.clone().run(cancel.clone()).await {
            Ok(env) => return Ok(env),
            Err(e) if e.is_retryable() && attempt < policy.max_attempts => {
                let backoff = Duration::from_millis(
                    policy.backoff_ms * (2u64.pow(attempt - 1))  // exponential
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }
}
```

Retryable errors: rate-limit (429), transient network, model overload. Non-retryable: auth failure, bad request, max-tokens.

---

### Graceful Shutdown

```
1. RA receives SIGINT / SIGTERM
2. RA cancels root CancellationToken
3. All PHAROH loops observe cancel → flush memory → close connections
4. All VIZIER workflows observe cancel → abort JoinSet → propagate Cancelled envelope
5. All HERALDs observe cancel → drain outbound queue → disconnect
6. RA waits on all PHAROH handles (with a hard 30s timeout)
7. Process exits
```

---

### Observability

Every Envelope carries a `trace_id` that threads through the entire tree. OpenTelemetry spans are created at:
- RA: inbound event receipt
- PHAROH: task dispatch
- VIZIER: workflow step dispatch
- MINION: LLM call start/end, each tool call

```rust
struct TraceId(Ulid);
struct SpanContext { trace_id: TraceId, span_id: Ulid, parent_span_id: Option<Ulid> }
```

Logs emit structured JSON with `trace_id`, `user_id`, `depth`, `model`, `duration_ms`, `tokens`.

---

## Crate Structure

```
maat/
├── Cargo.toml                   # workspace root
├── config/                      # default .md bouquet
│   ├── system.md
│   ├── models.md
│   ├── users.md
│   ├── heralds.md
│   ├── kap.md
│   ├── capabilities.md
│   └── features.md
│
└── crates/
    ├── maat-core/               # all shared types: Envelope, Workflow, CapabilityCard,
    │                            #   ArtifactPointer, ContextItem, ParsedCommand,
    │                            #   StatusEvent, ControlMessage, ResourceBudget, etc.
    ├── maat-llm/                # LlmClient trait + OpenAiCompatClient + OllamaClient
    ├── maat-store/              # SQLite store: sessions, memory, artifacts, deferred actions
    ├── maat-ra/                 # gateway, user registry, herald bus, swarm broker
    ├── maat-pharoh/             # per-user main session + session registry + command dispatch
    ├── maat-session/            # named session + compaction agent
    ├── maat-vizier/             # orchestrator, capability resolver, workflow planner
    ├── maat-minions/            # ephemeral worker execution (kameo actors)
    ├── maat-heralds/            # channel adapters (TG, WA, Web, TUI, Email)
    ├── maat-kap/                # workspace providers (email, calendar, drive, contacts)
    ├── maat-talents/            # intrinsic built-in tools
    └── maat-skills/             # plugin skill runtime (WASM + stdio + Docker)
```

**Dependency flow** (no upward deps):
```
maat-core
  ↑
maat-llm   maat-store
  ↑              ↑
maat-talents   maat-skills   maat-kap   maat-heralds
  ↑                ↑             ↑             ↑
maat-minions ──────────────────────────────────┘
  ↑
maat-vizier
  ↑
maat-session   (named sessions + compaction agent)
  ↑
maat-pharoh
  ↑
maat-ra
```

---

## Architectural Decisions Log

| # | Question | Decision |
|---|---|---|
| 1 | LLM provider abstraction | Thin own `LlmClient` trait wrapper. Default backend: `async-openai` crate (OpenAI-compat). Covers Anthropic compat endpoint, Groq, Together, Azure, Ollama. Anthropic native SDK added if compat endpoint proves limiting. |
| 2 | Workflow generation | VIZIER uses LLM to decompose tasks into workflows. Capabilities publish a `semantic_description` for LLM consumption. Template shapes (sequential, fan-out, chain) available as fast-path for common patterns. |
| 3 | Session persistence | SQLite via `sqlx`. Session context = last full context window sent to LLM. Abstracted behind a `Store` trait for future upgrade. |
| 4 | Identity across channels | Mapped by natural identifier per channel (phone for WhatsApp/TG/SMS, email for email/Discord, username for Discord/Slack). OAuth2 for web herald. All map to a common `UserId`. Flexible — no single auth provider required. |
| 5 | Memory architecture | Short-term (context window management) + long-term (embeddings in SQLite via `sqlite-vec`). Compaction handled by a dedicated system agent. Full content replaced with `ContextPointer` (summary + embedding ID). Pointers retrievable on demand. |
| 6 | Actor model | Use `kameo`. Each PHAROH, Named Session, and VIZIER is a kameo actor with a typed message enum. MINIONs are ephemeral kameo actors. Supervision trees give restart semantics for free. |
| 7 | Rate limiting | Constraints propagate down the tree in a `ResourceBudget`. Enforced at RA (global), PHAROH (per-user), and session level. Per-model/provider limits via token-bucket, windowed by time bucket. |
| 8 | Command/routing syntax | `@name: message` for session routing. `/command args` for control. Abstracted into `ParsedCommand` enum; each herald parses its own syntax into that type. PHAROH handles all control commands without LLM involvement. |
| 9 | Session lease / timeout | `TaskEnvelope` carries deadline. MINIONs can send `LeaseRenewRequest` to VIZIER. VIZIER can force-purge. Regular context cleaning on idle. |
| 10 | Hot-reload of Skills | Supported. Manifest watcher triggers reload. Prompt contamination prevented by only re-injecting capability descriptions at the start of a new context window, never mid-conversation. |
| 11 | Observability | Deferred. `trace_id` chain in place; exporter TBD. |
| 12 | Docker sandboxing | Deferred. Treated as an orchestration option in `SandboxMode::Docker`. |
| 13 | PHAROH swarm | PHAROHs can discover and message each other via RA's swarm broker. Used for multi-user task delegation and MINION swarming. Inter-PHAROH envelope type defined. |

---

## LLM Provider Abstraction

MAAT owns a thin `LlmClient` trait. All MINIONs and VIZIER planning calls go through it. The default backend is `async-openai`, which speaks to any OpenAI-compatible endpoint.

```rust
#[async_trait]
trait LlmClient: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse>;
    fn stream(&self, req: CompletionRequest)
        -> impl Stream<Item = Result<StreamEvent>> + Send;
    fn model_id(&self) -> &str;
    fn provider(&self) -> Provider;
}

struct CompletionRequest {
    messages: Vec<Message>,
    tools: Vec<LlmToolDef>,     // minimal schema only — name + description + input_schema
    max_tokens: u32,
    temperature: f32,
    top_p: Option<f32>,
    budget: ResourceBudget,     // enforced before the call is made
}

struct CompletionResponse {
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
    usage: TokenUsage,
}

enum StopReason { EndTurn, ToolUse, MaxTokens, Timeout }

enum ContentBlock {
    Text(String),
    ToolCall { id: String, name: String, input: serde_json::Value },
}
```

**Backend implementations:**

```rust
/// Default: any OpenAI-compatible endpoint
/// Covers: Anthropic (compat), OpenAI, Groq, Together, Azure OpenAI, Ollama
struct OpenAiCompatClient {
    inner: async_openai::Client<async_openai::config::OpenAIConfig>,
    model_id: String,
    provider: Provider,
}

/// Ollama — also OpenAI-compat but needs base_url override and model name handling
struct OllamaClient(OpenAiCompatClient);

enum Provider {
    Anthropic,
    OpenAi,
    Groq,
    Together,
    AzureOpenAi { deployment: String },
    Ollama,
    Custom { name: String, base_url: Url },
}
```

**Model registry** (in `models.md` bouquet):

```toml
[[model]]
id          = "primary"
provider    = "anthropic"
model_id    = "claude-opus-4-6"
base_url    = "https://api.anthropic.com/v1"   # compat endpoint
api_key_env = "ANTHROPIC_API_KEY"
hint        = "Reasoning"

[[model]]
id          = "fast"
provider    = "groq"
model_id    = "llama-3.3-70b-versatile"
base_url    = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
hint        = "Fast"

[[model]]
id          = "local"
provider    = "ollama"
model_id    = "llama3.2"
base_url    = "http://localhost:11434/v1"
hint        = "Fast"
```

---

## Memory Architecture & Compaction

Two distinct memory layers. Both backed by SQLite (`sqlite-vec` extension for embeddings).

### Short-term: Context Window Management

The "session context" is the last complete message array sent to the LLM. PHAROH and Named Sessions maintain a `ContextWindow` that manages what goes into each inference call.

```rust
struct ContextWindow {
    system_prompt: String,
    items: Vec<ContextItem>,
    token_count: u32,
    max_tokens: u32,               // hard cap — never exceeded
    soft_limit: u32,               // triggers compaction agent when crossed
}

enum ContextItem {
    Full(Message),
    Pointer(ContextPointer),       // compressed; retrievable on demand
}

struct ContextPointer {
    id: PointerId,
    summary: String,               // 1–3 sentence human-readable summary
    embedding_id: EmbeddingId,     // stored in sqlite-vec for semantic retrieval
    original_token_count: u32,
    compressed_at: DateTime<Utc>,
    kind: PointerKind,
}

enum PointerKind {
    Messages { count: u32 },
    Artifact(ArtifactId),
    WorkflowResult { workflow_id: WorkflowId, step_count: u32 },
}
```

**Pruning rule** — PHAROH can drop any `ContextItem` from the live window and replace it with a `ContextPointer`. The full content is never deleted — it stays in the SQLite store. The pointer is enough to retrieve it if a future inference step needs it (VIZIER can request pointer expansion before dispatching a MINION).

### Long-term: Embedding Store

Facts, preferences, and prior work summaries are stored as embeddings in `sqlite-vec` and retrievable by semantic similarity.

```rust
struct LongTermMemory {
    db: SqlitePool,               // sqlite-vec enabled
}

struct MemoryEntry {
    id: MemoryId,
    user_id: UserId,
    kind: MemoryKind,
    content: String,              // the raw text
    embedding: Vec<f32>,          // stored in sqlite-vec
    created_at: DateTime<Utc>,
    last_accessed: DateTime<Utc>,
    relevance_score: f32,         // decays over time, boosted on access
}

enum MemoryKind {
    UserFact,                     // "user prefers Python over JS"
    SessionSummary { session_name: SessionName },
    ArtifactSummary { artifact_id: ArtifactId },
    WorkflowOutcome { workflow_id: WorkflowId },
}
```

### Compaction Agent

A system-level background named session that runs on a schedule or when `ContextWindow.token_count > soft_limit`. It is NOT a general LLM session — it has a narrow, deterministic job.

```rust
struct CompactionAgent {
    user_id: UserId,
    db: SqlitePool,
    llm: Box<dyn LlmClient>,      // uses a Fast model — cheap summarisation
    embedding_model: EmbeddingModel,
}

impl CompactionAgent {
    /// Compress the oldest N messages in a context window into a ContextPointer
    async fn compact(&self, window: &mut ContextWindow, target_tokens: u32) -> Result<()>;

    /// Embed a summary and store in long-term memory
    async fn memorise(&self, content: &str, kind: MemoryKind) -> Result<MemoryId>;

    /// Retrieve top-K relevant memories for a given query
    async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>>;
}
```

**Prompt contamination prevention (hot-reload):** Capability descriptions are only injected at the start of a fresh context window. When a skill is hot-reloaded, the new description takes effect at the next context window boundary — never mid-conversation.

---

## Artifact Store

Artifacts (images, PDFs, documents, audio, etc.) are never kept as raw binary in a context window. They are stored in an artifact DB; context carries only `ArtifactPointer`s.

```rust
struct ArtifactStore {
    db: SqlitePool,
    blob_dir: PathBuf,            // local filesystem; swap for S3/GCS later
}

struct Artifact {
    id: ArtifactId,               // ULID
    user_id: UserId,
    session_id: Option<SessionId>,
    kind: ArtifactKind,
    filename: Option<String>,
    mime_type: String,
    size_bytes: u64,
    summary: Option<String>,      // AI-generated; None if too large or skipped
    storage_path: PathBuf,        // where the binary lives
    created_at: DateTime<Utc>,
    direction: ArtifactDirection,
}

enum ArtifactKind {
    Image, Pdf, Document, Spreadsheet,
    Audio, Video, Code, Archive, Unknown,
}

enum ArtifactDirection {
    Inbound,    // received from user via a herald
    Outbound,   // generated by a session/minion and delivered to user
}

/// What travels in context and envelopes — never the binary
struct ArtifactPointer {
    id: ArtifactId,
    kind: ArtifactKind,
    filename: Option<String>,
    mime_type: String,
    size_bytes: u64,
    summary: String,              // always present once processed
}
```

**Inbound processing pipeline:**

```
Herald receives file attachment
  │
  ├── Store binary to blob_dir → Artifact record created
  │
  ├── size_bytes < SUMMARY_THRESHOLD (default: 10MB)?
  │     Yes → LLM summarisation call (Fast model) → summary stored
  │     No  → summary = "Large file: <filename>, <size>, <mime_type>"
  │
  └── ArtifactPointer injected into context window (binary never enters context)
```

**Retrieval:** If a MINION needs the actual binary (e.g. vision call with image), VIZIER expands the pointer — fetches from blob store and injects into that specific `TaskEnvelope.context_window` only.

---

## Command & Routing Syntax

All heralds parse their channel-native input into a `ParsedCommand`. PHAROH handles all commands deterministically — no LLM involved in command dispatch.

### Syntax

| Pattern | Meaning |
|---|---|
| `@name: message` | Route message to named session |
| `/session new <name>: description` | Spawn a new named session |
| `/session list` | List all active sessions with summaries |
| `/session end <name>` | Gracefully end a named session |
| `/session purge <name>` | Force-end and clear context |
| `/status` | All session states (from SessionRegistry — no LLM) |
| `/status <name>` | Status of one session |
| `/workflow <name>` | Inspect current workflow DAG for a session |
| `/task <id>` | Inspect a specific task envelope |
| `/model <name> [session]` | Swap model for PHAROH or a named session |
| `/memory recall <query>` | Semantic search over long-term memory |
| `/defer <id>` | Defer a pending interaction to later |
| `/remind <id> <time>` | Schedule a reminder for a deferred action |

### Parsed Command Type

```rust
enum ParsedCommand {
    // Routing
    RouteToSession { name: SessionName, message: String },

    // Session lifecycle
    SessionNew     { name: SessionName, description: String },
    SessionList,
    SessionEnd     { name: SessionName },
    SessionPurge   { name: SessionName },

    // Inspection (all answered from typed state — no LLM)
    StatusAll,
    StatusSession  { name: SessionName },
    WorkflowInspect{ name: SessionName },
    TaskInspect    { task_id: TaskId },

    // Control
    ModelSwap      { session: Option<SessionName>, model_id: String },
    PauseSession   { name: SessionName },
    ResumeSession  { name: SessionName },

    // Memory
    MemoryRecall   { query: String },

    // User interaction responses
    SelectOption   { interaction_id: InteractionId, choice: String },
    Defer          { interaction_id: InteractionId },
    Remind         { interaction_id: InteractionId, at: DateTime<Utc> },

    // Passthrough — not a command, route as normal message
    PlainMessage   { text: String },
}
```

Each herald implements its own parser that produces a `ParsedCommand`. Web uses query params + request body; Telegram uses `/` prefix and inline `@` notation; TUI has a command bar.

---

## Rate Limiting & Resource Budgets

Constraints propagate **down the tree** — a child can never exceed what its parent has been allocated. Enforced at call time in the `LlmClient` wrapper; violations surface as `StatusEvent::SessionStateChanged(Blocked(RateLimit))`.

```rust
/// Attached to every TaskEnvelope and LLM call — enforced before the call
struct ResourceBudget {
    max_tokens_this_call: u32,
    remaining_session_tokens: Option<u64>,      // None = unlimited
    remaining_user_daily_tokens: Option<u64>,
    provider_rpm_remaining: Option<u32>,        // requests per minute
    provider_tpm_remaining: Option<u32>,        // tokens per minute
}

/// Configuration — set in models.md / users.md
struct RateLimitConfig {
    // RA level (global hard caps)
    global_tpm: u32,
    global_rpm: u32,

    // Per user / PHAROH
    user_daily_token_budget: Option<u64>,
    user_rpm: u32,

    // Per model/provider (from provider's actual limits)
    provider_limits: HashMap<Provider, ProviderLimits>,

    // Per named session
    session_token_budget: Option<u64>,
}

struct ProviderLimits {
    rpm: u32,       // requests per minute
    tpm: u32,       // tokens per minute
    rpd: u32,       // requests per day
    tpd: u64,       // tokens per day
}

/// Token bucket implementation — one per (user, provider) pair, held in RA
struct TokenBucket {
    capacity: u32,
    remaining: u32,
    refill_rate: f32,               // tokens per second
    last_refill: Instant,
}
```

**Enforcement chain:**

```
RA checks global bucket
  → PHAROH checks user bucket
    → VIZIER checks session budget (from ResourceBudget in TaskEnvelope)
      → LlmClient checks provider bucket before making API call
        → on 429 from provider: emit StatusEvent::Blocked(RateLimit{retry_after})
```

---

## User Interaction Model

PHAROH can pause and request input from the user. Interactions are structured — options can be selected by label, deferred, or scheduled for a reminder.

```rust
struct InteractionRequest {
    id: InteractionId,
    session: Option<SessionName>,       // which session is asking
    prompt: String,                     // what PHAROH presents to the user
    options: Vec<SuggestedAction>,      // offered choices (may be empty)
    allow_free_text: bool,              // can user respond with anything?
    defer_allowed: bool,
    expires_at: Option<DateTime<Utc>>,  // auto-defer if no response by this time
}

struct SuggestedAction {
    label: String,                      // short selector: "A", "1", "yes", "send"
    description: String,                // what this does
    risk: ActionRisk,
}

enum ActionRisk { Safe, Caution, Destructive }

struct DeferredAction {
    id: DeferredId,
    user_id: UserId,
    description: String,
    payload: serde_json::Value,         // enough to reconstruct and replay
    created_at: DateTime<Utc>,
    remind_at: Option<DateTime<Utc>>,
    status: DeferredStatus,
}

enum DeferredStatus {
    Pending,
    RemindScheduled(DateTime<Utc>),
    Completed,
    Cancelled,
}
```

**Example interaction (Telegram):**

```
PHAROH: The research session has finished the literature review.
        Ready to draft the report?

  [A] Draft report now       — start immediately
  [B] Review sources first   — show me the source list
  [C] Defer                  — remind me in 1 hour
  [D] Cancel                 — discard this session

> A
```

User response is parsed into `ParsedCommand::SelectOption` by the herald. PHAROH dispatches the action without any LLM call.

---

## PHAROH Swarm

PHAROHs can discover each other and delegate work across user boundaries. RA acts as the swarm broker — it holds a registry of active PHAROHs and routes inter-PHAROH envelopes.

**Use cases:**
- User A's PHAROH delegates a sub-task to User B's PHAROH (shared team workflows)
- MINION swarming: a complex task is split and dispatched to multiple PHAROHs, each running MINIONs in parallel, results brokered back

```rust
/// RA maintains this for all active PHAROHs
struct SwarmRegistry {
    peers: HashMap<UserId, SwarmPeer>,
}

struct SwarmPeer {
    user_id: UserId,
    pharoh_addr: ComponentAddress,
    capabilities: Vec<CapabilityId>,    // what this PHAROH's sessions can offer
    load: SwarmLoad,
    last_seen: DateTime<Utc>,
}

struct SwarmLoad {
    active_sessions: u32,
    active_minions: u32,
    token_budget_remaining_pct: f32,
}

/// Inter-PHAROH envelope — routed by RA's swarm broker
struct SwarmEnvelope {
    header: EnvelopeHeader,
    from: UserId,
    to: UserId,
    kind: SwarmMessage,
}

enum SwarmMessage {
    /// PHAROH A asks PHAROH B to execute a task on its behalf
    DelegateTask {
        task: TaskEnvelope,
        callback: ComponentAddress,         // where to send the ResultEnvelope
        budget: ResourceBudget,             // A's budget for this delegation
    },
    /// Result of a delegated task
    TaskResult(ResultEnvelope),

    /// Broadcast: "I exist, here are my capabilities and load"
    Announce(SwarmPeer),

    /// Used to find peers with a specific capability
    CapabilityQuery { tags: Vec<String> },
    CapabilityResponse { peers: Vec<SwarmPeer> },
}
```

**Swarm task flow:**

```
PHAROH_A VIZIER: needs capability X, not available locally
  → query SwarmRegistry for peers with tag X
  → select PHAROH_B (lowest load, has capability X)
  → send SwarmEnvelope::DelegateTask to RA
  → RA routes to PHAROH_B
  → PHAROH_B's VIZIER instantiates MINION, runs task
  → PHAROH_B sends SwarmEnvelope::TaskResult back via RA
  → PHAROH_A VIZIER receives result, continues workflow
```

Trust between PHAROHs is explicit — delegation is only accepted from peers in the user's trusted peer list (configured in `users.md`).

---

## Open Questions

- [ ] **Observability** — OpenTelemetry exporter target TBD (Jaeger / OTLP cloud). `trace_id` chain already in place.
- [ ] **Docker sandbox orchestration** — `bollard` crate vs external orchestrator. Deferred to SKILLS implementation.
- [ ] **Multi-node PHAROH distribution** — swarm works within one process for now; sharding PHAROHs across machines deferred.
- [ ] **Embedding model** — which model for `sqlite-vec` embeddings? Local (e.g. `nomic-embed-text` via Ollama) vs API (OpenAI embeddings).

---

*Spec status: architecture complete — ready to begin implementation.*
