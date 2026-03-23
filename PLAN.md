# MAAT Plan

Living execution plan. This should track the codebase as it exists now.

## Current State

Working vertical slice:

```text
TUI -> RA -> PHAROH -> VIZIER -> MINION -> tools / skills
```

Implemented:
- primary session plus named sessions
- stable session persistence and restore
- single-step task execution with retry
- model routing via provider/profile/policy layers
- capability registry with tags, trust, provenance, and routing hints
- compiled talents for files, search, IMAP, Gmail, and Calendar
- installed-skill loading from local folders
- local skill install
- ClawHub-backed skill search/install bridge via local CLI
- GitHub-backed skill install via sparse clone
- prompt externalization
- SQLite history and context-pointer compaction

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
- added ClawHub and GitHub provider seams for skill installation

## Next Up

### 1. Capability-Nudge and Planner Bridge

Goal: make vague user intent resolve through a capability-shortlist and nudge step before execution.

- [ ] use `capability_nudge` prompt/model path to break ties between plausible capabilities
- [ ] emit clearer logs for why a capability was chosen
- [ ] distinguish direct-answer tasks from capability-driven tasks before MINION dispatch
- [ ] start shaping a real planner interface instead of inlining all logic in VIZIER

Exit criteria:
- vague requests can be narrowed to a capability shortlist in an explainable way
- routing decisions are inspectable and not just inferred from logs

### 2. Third-Party Skill Runtime Model

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

### 3. Multi-Step Planner

Goal: move from single-step dispatch to explicit workflows.

- [ ] classify requests into direct answer, tool task, or multi-step workflow
- [ ] add a workflow plan representation
- [ ] support sequential multi-step tasks first
- [ ] later support parallel branches
- [ ] allow model choice and capability choice per step

Exit criteria:
- VIZIER can generate and execute more than one step for a user request

### 4. Retrieval Memory

Goal: move from persistence to useful recall.

- [ ] add embeddings storage
- [ ] retrieve relevant history/context pointers by semantic similarity
- [ ] define retrieval policy at PHAROH, named session, and planner boundaries
- [ ] add better pointer metadata and observability

Exit criteria:
- old useful context can re-enter prompts without relying only on sliding windows

### 5. Docs and Developer UX

Goal: keep the project understandable as the architecture grows.

- [x] add `README.md`
- [x] add `ARCHITECTURE.md`
- [x] bring this plan back in sync
- [ ] reconcile `SPEC.md` with implemented reality vs target architecture
- [ ] add developer-facing inspect commands for models, capabilities, and skills
- [ ] improve structured logs around route scope, chosen model, and chosen capabilities

## Later

- richer marketplace/download backends beyond local CLI bridges
- automation and deferred-action UX
- additional heralds beyond the TUI
- workspace providers beyond current email/calendar/search/files
- PHAROH-to-PHAROH coordination
