# MAAT Architecture

This file describes the current runtime shape in the repository, not just the intended end state.

## Runtime Tree

```text
Herald (TUI / Telegram)
  -> RA
    -> PHAROH
      -> VIZIER
        -> MINION
          -> Tool loop / capability execution
```

Named sessions sit beside the primary PHAROH flow:

```text
PHAROH
  -> primary VIZIER
  -> named session registry
     -> NamedSession
        -> session VIZIER
           -> MINIONs
```

Background runs now sit on top of named sessions:

```text
PHAROH
  -> BackgroundRunRecord
  -> named session @<run-handle>
     -> session VIZIER
        -> MINIONs
```

## Main Components

### RA

Bootstrap and runtime wiring in [main.rs](/Users/troytravlos/maat/crates/maat-ra/src/main.rs).

Responsibilities:
- load config
- resolve secrets
- construct `ModelRegistry`
- build tool and capability registries
- load installed skills from configured skill directories
- construct the primary PHAROH actor and herald bridges

### PHAROH

Primary per-user session in [lib.rs](/Users/troytravlos/maat/crates/maat-pharoh/src/lib.rs).

Responsibilities:
- own the primary session history
- persist messages
- expose command handling
- create and manage named sessions
- create and track background runs
- dispatch bounded tasks to VIZIER
- present tool and skill management commands

Important current behavior:
- PHAROH handles a single user in practice
- named sessions are persisted and restorable
- `/skills` commands are handled here
- background runs are started here and executed through named sessions
- PHAROH also now contains a few pragmatic fast paths:
  - inline direct-answer replies
  - explicit direct skill invocation for narrow `use/run <skill>` requests
  - artifact attachment and artifact-return bridging

Important design note:
- those fast paths solve real UX failures, but they are not the whole destination
- the next layer should make explicit invocation and routing state more structured, so we do not accumulate one-off deterministic branches forever

### NamedSession

Defined in [session.rs](/Users/troytravlos/maat/crates/maat-pharoh/src/session.rs).

Responsibilities:
- hold its own conversation state
- use session-scoped prompts and compaction
- dispatch work through its own VIZIER route scope

### VIZIER

Defined in [lib.rs](/Users/troytravlos/maat/crates/maat-vizier/src/lib.rs).

Responsibilities:
- rank capabilities against task text
- select a candidate capability subset
- merge route-level, task-level, and capability-level model policies
- resolve the concrete `ModelSpec`
- spawn a MINION and manage retries

Current limitation:
- orchestration is still effectively single-step
- there is not yet a true explicit multi-step planner
- overlap between “route to specialist model”, “call a skill”, and “answer directly” is still not modeled richly enough

### MINION

Bounded worker in [lib.rs](/Users/troytravlos/maat/crates/maat-minions/src/lib.rs).

Responsibilities:
- run one task with one resolved model
- execute tool-calling loops
- emit status events
- return a `ResultEnvelope`

MINION intentionally does not pick its own model.

## Shared Registries

### ToolRegistry

Defined in [lib.rs](/Users/troytravlos/maat/crates/maat-core/src/lib.rs).

Execution-facing registry of compiled tools.

### CapabilityRegistry

Also in [lib.rs](/Users/troytravlos/maat/crates/maat-core/src/lib.rs).

Planning/routing-facing registry of normalized `CapabilityCard`s.

Capabilities now carry:
- kind
- tags
- semantic terms
- permissions
- trust
- provenance
- routing hints

Current truth:
- the capability registry is currently serving as both a semantic routing substrate and an execution affordance index
- that works surprisingly well, but overlap cases like `image_edit` vs `image-rectify` suggest we likely need a clearer invocation layer above it

### ModelRegistry

Also in [lib.rs](/Users/troytravlos/maat/crates/maat-core/src/lib.rs).

Separates:
- provider transport
- profile identity
- route policy

This is what enables cheap PHAROH defaults and stronger per-task MINION routing.

## Skills and Talents

### Talents

Compiled-in tools under [crates/maat-talents](/Users/troytravlos/maat/crates/maat-talents).

These are trusted `CapabilityKind::Talent` entries and currently include:
- file tools
- Tavily web search
- IMAP mail access
- Gmail send
- Google Calendar list/create
- skill self-management for scaffolding and installing local command-mode skills

### Installed Skills

Managed by [skills.rs](/Users/troytravlos/maat/crates/maat-config/src/skills.rs).

Install sources currently supported:
- local directory
- ClawHub via local `clawhub` CLI
- GitHub via sparse `git clone`

Installed skills are normalized into `CapabilityCard`s and stamped with `maat-skill.toml`.

There is now also a compiled self-extension seam:
- `skill_manage` can scaffold a command-mode skill directly into the configured local skills directory
- it can fetch exact GitHub repo assets into the skill folder
- it can install an existing local skill folder through the same local installer seam

This exists because generic file tools were not enough for MAAT to reliably build its own capabilities end to end.

Important current safety rule:
- local workspace installs are generally trusted
- ClawHub and GitHub installs default to review-level trust

## Prompt and Config Surface

Prompts are externalized under [prompts/](/Users/troytravlos/maat/prompts).

Key prompt files:
- [primary_system.md](/Users/troytravlos/maat/prompts/primary_system.md)
- [named_session.md](/Users/troytravlos/maat/prompts/named_session.md)
- [compaction.md](/Users/troytravlos/maat/prompts/compaction.md)
- [capability_nudge.md](/Users/troytravlos/maat/prompts/capability_nudge.md)
- [intent_classifier.md](/Users/troytravlos/maat/prompts/intent_classifier.md)

Config is defined in [config.rs](/Users/troytravlos/maat/crates/maat-config/src/config.rs) and loaded from:
- [maat.toml](/Users/troytravlos/maat/maat.toml)
- optional `maat.workspace.toml`

Design direction:
- prompts should increasingly own soft judgment like route labels and shortlist nudges
- config should declare policy and preferences
- code should enforce permissions, trust, delivery policy, and hard model constraints

## Persistence

SQLite storage lives in [sqlite.rs](/Users/troytravlos/maat/crates/maat-memory/src/sqlite.rs).

Persisted data includes:
- session metadata
- chat history
- context pointers
- artifacts
- automation runs
- background runs

Current memory model:
- sliding context window
- compaction to lightweight pointers
- no semantic retrieval yet
- artifacts are already a separate first-class persistence layer with readable handles

## Background Runs

Background runs are the first async execution layer above the existing actor tree.

Current shape:
- a user command or automation starts a `BackgroundRunRecord`
- the run gets a readable handle
- the run is attached to a named session, usually `@<handle>`
- PHAROH spawns detached work into that session
- completion updates the run record with final status and summary
- users can request cancellation of a run by handle

Why this shape:
- PHAROH stays responsive
- long jobs have a durable status surface
- the user has a natural place to route back to ongoing work
- automations and manual background work can share the same execution model

Current limitation:
- cancellation is now cooperative through the workflow loop, but not guaranteed hard preemption of an already-running provider/tool call
- runs are detached and inspectable, but not yet streamed back into the main chat as live progress updates
- status is event-driven, but still rendered as a simple textual stream rather than a richer structured dashboard

## Current Architectural Boundaries

## Heralds

### Shared Herald Boundary

Heralds now share a small backend request seam:
- a herald turns local input into `HeraldPayload`
- it sends a `BackendRequest` carrying a reply channel
- RA bridges that request to PHAROH
- replies return as `HeraldEvent`

That keeps the backend channel-agnostic even though the TUI still has richer local UX.

### TUI Herald

The TUI is still the richest herald today.

It additionally provides:
- local active-session targeting
- autocomplete and command suggestions
- draft file attachments and recent-artifact reuse
- a second runtime/status lane fed directly from `StatusEvent`s

### Telegram Herald

Telegram is currently a thin polling herald.

Current behavior:
- each Telegram request is resolved against a registered sender identity when `users.telegram` is configured
- each Telegram principal/chat pair is mapped to a named session `telegram-<principal>-<chat_id>`
- text and slash commands flow through the shared herald boundary
- uploaded documents and photos are downloaded locally and sent as normal herald attachments
- replies are sent back as plain Telegram messages, and image artifacts can be pushed back down the channel

Current limitation:
- Telegram does not yet expose the richer live runtime/status thread the TUI has
- it currently waits for the first assistant/error response per request rather than streaming updates

### Delivery Identity

MAAT is still fundamentally single-user internally.

Current practical identity model:
- the core runtime user is still one logical `UserId`
- TUI requests come from the local machine/user running MAAT
- Telegram ingress can now be locked to registered sender user IDs via `users.telegram`
- Telegram requests are isolated per registered principal/chat via named sessions like `telegram-<principal>-<chat_id>`
- automation delivery can target a Telegram chat explicitly, or fall back to `telegram.default_chat_id` as the current meaning of "me"

This is a good first ingress-auth layer, but it is not yet a full multi-user identity/permission model or outbound authorization framework.

## Review Focus For Next Phase

The next phase should optimize for:
- smarter routing under ambiguity
- smoother explicit invocation UX
- stronger autonomy and self-healing
- deterministic communication policy
- better observability

Recommended architectural decisions:
- keep final model resolution and hard permissions in code
- keep more route-label and shortlist judgment in prompts/config
- introduce an explicit invocation layer for skills, models, artifacts, and channels
- avoid solving every overlap by adding ad hoc deterministic branches
- prefer backend event/status buses over model-based polling for runtime visibility

What is already solid:
- per-task model resolution
- capability metadata as a real routing input
- installed-skill normalization with provenance/trust
- externalized prompts

What is still intentionally incomplete:
- multi-step planning
- semantic retrieval memory
- rich third-party capability manifests beyond current normalized fields
- runtime hot-reload of skills and prompts
- remote installer backends beyond shelling out to local tooling
