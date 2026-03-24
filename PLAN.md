# MAAT Plan

Living execution plan. This should track the codebase as it exists now.

## Current State

Working vertical slice:

```text
Herald (TUI / Telegram) -> RA -> PHAROH -> VIZIER -> MINION -> tools / skills
```

Implemented:
- primary session plus named sessions
- background runs with readable handles and dedicated run sessions
- shared herald backend request path plus a thin Telegram herald
- stable session persistence and restore
- single-step task execution with retry
- model routing via provider/profile/policy layers
- capability registry with tags, trust, provenance, and routing hints
- compiled talents for files, search, IMAP, Gmail, Calendar, and skill self-management
- installed-skill loading from local folders
- local skill install
- ClawHub-backed skill search/install bridge via local CLI
- GitHub-backed skill install via sparse clone
- internal skill self-scaffold/fetch/install path via `skill_manage`
- prompt externalization
- SQLite history and context-pointer compaction
- automation scheduler with TOML-backed specs and SQLite run history

Important truths:
- workflow orchestration is still effectively single-step
- capability ranking exists, but capability-nudge behavior is not yet fully used in planning
- installed skills are normalized well, but execution remains tool-oriented rather than true plugin execution
- memory is persistence plus compaction, not semantic retrieval

## Recently Completed

- fixed primary-session restore behavior
- made runtime config actually drive model and context settings
- fixed typed `/config set`
- fixed MINION step IDs in status events
- fixed TUI scroll/input issues
- moved SQLite off the async hot path
- added real `ModelRegistry` and route-scoped model selection
- added real `CapabilityRegistry` and capability-based routing
- externalized key prompts
- added trust/provenance-aware installed-skill ingestion
- added `/skills`, `/skills search`, and `/skills install`
- added `/skills reload` and live installed-skill hot reload into active registries
- added ClawHub and GitHub provider seams for skill installation
- added first-class artifact storage with readable handles
- added background runs and detached execution through named sessions
- added automation CRUD plus richer schedules
- upgraded the TUI composer with autocomplete, attachments, file picker, recent artifacts, and run/session shortcuts
- added a Telegram herald for chat, slash commands, and attachment ingress
- added Telegram artifact return and automation delivery-on-completion
- added Telegram sender registration and ingress allowlisting via `users.telegram`
- added a first-class `skill_manage` talent so MAAT can scaffold command-mode skills and fetch GitHub assets for self-extension

## Next Up

### 0. Routing, Invocation, and Recovery Review

Goal: reduce brittle overlap handling and make the system smoother and more intentional.

Architectural decisions to carry forward:
- keep hard model constraints, permission checks, and trust enforcement in code
- move more soft routing and capability selection judgment into prompt/config surfaces
- add explicit invocation affordances instead of relying only on natural-language inference
- prefer self-healing and recovery loops over silent refusal or vague fallback prose

Recommended next-phase steps:
- [ ] define an explicit invocation surface for skills, models, artifacts, and channels
- [ ] decide whether this should be slash-based, qualifier-based, or a lighter inline syntax with autocomplete support
- [ ] add a routing state machine with these layers:
  - [ ] explicit invocation detection
  - [ ] prompt-driven intent classification
  - [ ] capability shortlist / nudge
  - [ ] final model resolution
- [ ] reduce one-off deterministic patches where they are papering over missing invocation structure
- [ ] make installed skills expose cleaner structured input/output contracts
- [ ] add tool recovery loops for common failures like:
  - [ ] unknown artifact handle
  - [ ] missing output path
  - [ ] skill not loaded
  - [ ] stale local asset path
- [ ] improve structured logging around route choice, tool normalization, and recovery actions

Exit criteria:
- explicit skill/model requests feel reliable without over-hardcoding
- ambiguous requests degrade gracefully instead of bouncing between prose and tools
- failures are easier for both the user and the runtime to recover from

### 1. Run Control and Async UX

Goal: make background work a complete first-class async surface.

- [x] add run cancellation request semantics
- [x] add cooperative cancellation through the workflow loop
- [ ] add deeper preemption for in-flight provider/tool calls where possible
- [ ] add pause semantics if we still want them after stronger cancel support exists
- [ ] surface run progress updates more explicitly
- [ ] allow automations to target existing run sessions or spawn fresh ones by policy
- [ ] add better run/result linking for produced artifacts

Exit criteria:
- long tasks can be started, inspected, and controlled without blocking the main chat
- run/session/artifact relationships are easy to inspect

### 2. Capability-Nudge and Planner Bridge

Goal: make vague user intent resolve through a capability-shortlist and nudge step before execution.

- [ ] use `capability_nudge` prompt/model path to break ties between plausible capabilities
- [ ] emit clearer logs for why a capability was chosen
- [ ] distinguish direct-answer tasks from capability-driven tasks before MINION dispatch
- [ ] start shaping a real planner interface instead of inlining all logic in VIZIER

Exit criteria:
- vague requests can be narrowed to a capability shortlist in an explainable way
- routing decisions are inspectable and not just inferred from logs

### 3. Third-Party Skill Runtime Model

Goal: move installed skills from metadata/routing inputs into a safe execution model.

- [ ] define how installed skills execute:
  - [ ] stdio wrapper
  - [ ] script runner
  - [ ] plugin host
- [ ] add explicit executable metadata to installed-skill manifests
- [ ] separate "discoverable capability" from "executable skill" more clearly
- [ ] add approval or quarantine behavior for review/untrusted skills
- [ ] add trust promotion and demotion commands

Exit criteria:
- installed skills can be both routed and executed through one coherent contract
- trust level affects both planning and execution behavior

### 4. Multi-Step Planner

Goal: move from single-step dispatch to explicit workflows.

- [ ] classify requests into direct answer, tool task, or multi-step workflow
- [ ] add a workflow plan representation
- [ ] support sequential multi-step tasks first
- [ ] later support parallel branches
- [ ] allow model choice and capability choice per step

Exit criteria:
- VIZIER can generate and execute more than one step for a user request

### 5. Retrieval Memory

Goal: move from persistence to useful recall.

- [ ] add embeddings storage
- [ ] retrieve relevant history/context pointers by semantic similarity
- [ ] define retrieval policy at PHAROH, named session, and planner boundaries
- [ ] add better pointer metadata and observability

Exit criteria:
- old useful context can re-enter prompts without relying only on sliding windows

### 6. Docs and Developer UX

Goal: keep the project understandable as the architecture grows.

- [x] add `README.md`
- [x] add `ARCHITECTURE.md`
- [x] bring this plan back in sync
- [x] reconcile `SPEC.md` with implemented reality vs target architecture
- [ ] add developer-facing inspect commands for models, capabilities, and skills
- [ ] improve structured logs around route scope, chosen model, and chosen capabilities

## Later

- richer marketplace/download backends beyond local CLI bridges
- richer automation and deferred-action UX
- richer herald coverage beyond TUI + Telegram
- stronger multi-user identity and delivery policy beyond the current single-user core with registered Telegram ingress
- workspace providers beyond current email/calendar/search/files
- PHAROH-to-PHAROH coordination
