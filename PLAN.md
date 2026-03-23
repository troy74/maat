# MAAT — Build Plan & Status

> Living document. Update as work completes or priorities shift.
> Spec detail lives in SPEC.md — this is about what we're building when.

---

## Current State

**Phase 1 — Skinny Vertical: COMPLETE (working)**

End-to-end conversation loop is live:
```
TUI input → bridge task → PHAROH (kameo actor) → maat-llm → OpenRouter (minimax/minimax-m2.7) → TUI display
```

Crates in place:
| Crate | Status | Notes |
|---|---|---|
| `maat-core` | ✅ working | ChatMessage, Role, ModelSpec, TuiEvent, basic types |
| `maat-llm` | ✅ working | LlmClient trait + OpenAiCompatClient (async-openai → OpenRouter) |
| `maat-pharoh` | ✅ working | Kameo actor, holds conversation context, calls LLM |
| `maat-heralds` | ✅ working | ratatui TUI — message pane + input bar + status line |
| `maat-ra` | ✅ working | Entry point, wires TUI ↔ PHAROH bridge |

Model: `minimax/minimax-m2.7` via OpenRouter (`OPENROUTER_API_KEY` env var)

---

## Known Bugs

| # | Description | Priority |
|---|---|---|
| 1 | **TUI input box text overwrite** — characters write over each other rather than appending cleanly | High — fix next |

---

## Build Roadmap

### Phase 2 — TUI Polish & Core Fixes
*Goal: solid, usable dev harness before building more width*

- [ ] **Fix input box overwrite bug** (cursor/buffer state issue in `tui.rs`)
- [ ] Markdown rendering in message pane (bold, code blocks at minimum)
- [ ] Scroll in message pane (currently clips on long responses)
- [ ] Status line: show token count, model, last latency
- [ ] Ctrl+C / quit handling cleanup
- [ ] `/help` command in TUI

---

### Phase 3 — Envelope & Control Plane ✅
*Goal: structured data flow between components, deterministic status without LLM polling*

- [x] `EnvelopeHeader` (id, trace_id, timestamps, sender, recipient, priority)
- [x] `HeraldEnvelope` — raw channel message in; `ParsedCommand` for @ / slash routing
- [x] `SessionEnvelope` — PHAROH ↔ Named Session (Dispatch / SummaryUpdate / UserMessage)
- [x] `TaskEnvelope` — VIZIER → MINION (capability refs, model spec, resource budget, deadline, retry)
- [x] `ResultEnvelope` — MINION → VIZIER (TaskOutcome, usage, latency, tool call log)
- [x] `ControlMessage` — Pause / Resume / Cancel / Kill / Restart / RenewLease / PurgeContext
- [x] `StatusEvent` + broadcast kinds (SessionState, WorkflowState, StepState, HeartBeat)
- [x] `SessionState`, `WorkflowState`, `StepState` typed state machines
- [x] `CapabilityCard`, `CapabilityKind`, `PluginMode`, `Permission`, `CostProfile`
- [x] `SessionSummary` — PHAROH's lightweight view per named session
- [x] Logs redirected to `maat.log` (TUI terminal now clean)

---

### Phase 4 — VIZIER + MINION Layer ✅
*Goal: PHAROH delegates to VIZIER which spawns MINIONs — full data-plane path*

- [x] `maat-vizier` crate — kameo actor, `Dispatch(VizierTask)` → `ResultEnvelope`
- [x] Single-step workflow (one MINION, sequential)
- [x] `maat-minions` crate — ephemeral kameo actor, single LLM call + return
- [x] Wire PHAROH → VIZIER → MINION → result back (full envelope protocol)
- [x] Deadline timeout per TaskEnvelope (clamped 2s–5min, default 120s)
- [x] Exponential backoff retry (retryable vs non-retryable error classification)
- [x] StatusEvents: StepState (Running/Completed/Failed/Retrying), WorkflowState
- [x] Status bus (broadcast) wired in RA; PHAROH subscribes (logs for now)

---

### Phase 5 — Named Sessions ✅
*Goal: PHAROH manages multiple long-running sessions, user can address by name*

- [x] `NamedSession` actor (`maat-pharoh/src/session.rs`) — own history + VIZIER, `SessionChat` + `GetSummary` messages
- [x] Session registry in PHAROH — `HashMap<SessionName, SessionEntry>` with `SessionSummary`
- [x] PHAROH routing: `HeraldPayload::Text` → primary, `Command` → dispatch table
- [x] Session summaries pulled via `GetSummary` after each turn (no LLM poll)
- [x] TUI parses `@name: msg` → `RouteToSession`, `/session new/list/end`, `/status`
- [x] Channel type updated `String` → `HeraldPayload` end-to-end
- [x] `/session new <name>: <desc>` — spawns new session with context-aware system prompt
- [x] `/session list` — lists all sessions with live summaries
- [x] `/session end <name>` — drops actor, removes from registry

---

### Phase 6 — Tool Calling & First Talent ✅
*Goal: MINION runs an agentic loop; IMAP talent gives it email access*

- [x] `LlmToolDef`, `PendingToolCall`, `Tool` trait, `ToolRegistry` in maat-core
- [x] `ChatMessage` extended: `tool_call_id`, `tool_calls_json`, `Role::Tool`
- [x] `maat-llm`: `complete()` accepts `&[LlmToolDef]`, returns `tool_calls` in response
- [x] `maat-minions`: agentic loop — LLM → dispatch tools → inject results → repeat (max 10 rounds)
- [x] `maat-talents` crate: `ImapTalent` with `email_list`, `email_read`, `email_search`
- [x] `ToolRegistry` threaded through RA → VIZIER → MINION (and NamedSession path)
- [x] IMAP config from env vars: `IMAP_HOST`, `IMAP_PORT`, `IMAP_USERNAME`, `IMAP_PASSWORD`

---

### Phase 7 — Memory & Context Management
*Goal: sessions don't blow up context windows; long-term recall via embeddings*

- [ ] SQLite store (`rusqlite` or `sqlx`) — sessions, messages, embeddings
- [ ] Short-term: sliding context window with configurable token budget
- [ ] `ContextPointer` — compressed summary + embedding_id replaces pruned messages
- [ ] Compaction agent — background named session, triggered at context threshold
- [ ] Embeddings via LLM embedding endpoint (store in SQLite with `sqlite-vec`)
- [ ] PHAROH context pruning: drop junk, keep pointer for retrieval
- [ ] Semantic retrieval: search embeddings to reinject relevant history

---

### Phase 8 — Artifact Store
*Goal: binary content (images, PDFs) stays out of context; pointer-based passing*

- [ ] `ArtifactStore` — SQLite metadata + filesystem blob storage
- [ ] Inbound artifact pipeline: detect type → classify → summarise (if < size threshold) → store → inject `ArtifactPointer` into context
- [ ] Outbound artifact: store generated content, pass pointer to herald
- [ ] `ArtifactKind` enum: Image, Pdf, Document, Spreadsheet, Audio, Video, Code, Archive
- [ ] Herald renders pointer as preview/link, not raw binary

---

### Phase 9 — Rate Limiting & Resource Budgets
*Goal: per-model, per-session, per-user, global limits enforced deterministically*

- [ ] `RateLimitConfig` — token bucket per time window at each level
- [ ] RA-level global limits
- [ ] PHAROH-level per-user limits
- [ ] VIZIER-level per-session token budget (in TaskEnvelope `ResourceBudget`)
- [ ] Provider limits map (model → RPM + TPM)
- [ ] Back-pressure: PHAROH returns "busy" to herald on overflow rather than dropping

---

### Phase 10 — User Interaction Model
*Goal: PHAROH can offer options, handle deferred actions, ask for confirmation*

- [ ] `InteractionRequest` — prompt + `Vec<SuggestedAction>` + defer_allowed
- [ ] `DeferredAction` — stored with scheduled reminder
- [ ] TUI renders options as numbered list; user types number to select
- [ ] "remind me later" path: stores action with timestamp, resurfaces it
- [ ] PHAROH deferred queue (persistent in SQLite)

---

### Phase 11 — Additional Heralds
*Goal: same PHAROH tree reachable from multiple channels*

- [ ] Telegram herald (Bot API)
- [ ] WhatsApp herald (Business API)
- [ ] Web herald (HTTP + WebSocket)
- [ ] Email herald (IMAP/SMTP) — likely via KAP provider
- [ ] Identity unification: phone → UserId, email → UserId, TG username → UserId

---

### Phase 12 — KAP Workspace Providers
*Goal: email, calendar, contacts, drive as tools available to MINIONs*

- [ ] `maat-kap` crate
- [ ] Email provider (IMAP/SMTP + OAuth2)
- [ ] Calendar provider (Google Calendar / CalDAV)
- [ ] Contacts provider
- [ ] Drive provider
- [ ] Per-user `CredentialStore` (encrypted, with token refresh)

---

### Phase 13 — PHAROH Swarm
*Goal: PHAROHs can delegate to each other and broker MINION conversations*

- [ ] Inter-PHAROH `SwarmEnvelope`
- [ ] PHAROH peer registry in RA
- [ ] Task delegation: PHAROH A → PHAROH B runs MINION
- [ ] Swarming: coordinated multi-PHAROH workflows

---

### Deferred / Parking Lot

| Item | Decision |
|---|---|
| Docker sandboxing for Skills | Defer — orchestration option, design accommodates it |
| Observability / OpenTelemetry | Defer — trace_id in all envelopes already; wire OTel later |
| Hot reload of Skills/Prompts | Defer — design for it, implement when Skills crate lands |
| Upgrade from SQLite | Defer — revisit if scale demands it |

---

## Architecture Decisions (locked)

| # | Decision |
|---|---|
| Runtime | tokio multi-thread |
| Actor model | kameo |
| LLM client | maat-llm thin wrapper over async-openai (OpenAI-compat) → OpenRouter |
| Dev model | `minimax/minimax-m2.7` via OpenRouter |
| Persistence | SQLite to start (rusqlite or sqlx) |
| Status | Typed `StatusEvent` state machines — never LLM-polled |
| Capability passing | ID refs only in envelopes; cards resolved from process-global Arc registry |
| Skill trust | Talent (compiled-in, full trust) vs Skill (WASM sandboxed or subprocess) |
| Identity | Phone / email / username per channel → unified UserId |
| Memory | Short-term sliding window + long-term embeddings + compaction agent |
| Command syntax | `@session: msg`, `/command args` — parsed by herald, normalised to ParsedCommand |
| Envelope types | HeraldEnvelope / SessionEnvelope / TaskEnvelope / ResultEnvelope / StatusEvent / ControlMessage |
| Context pruning | PHAROH drops content, keeps ContextPointer (summary + embedding_id) |
