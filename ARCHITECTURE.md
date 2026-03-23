# MAAT Architecture

This file describes the current runtime shape in the repository, not just the intended end state.

## Runtime Tree

```text
TUI Herald
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

## Main Components

### RA

Bootstrap and runtime wiring in [main.rs](/Users/troytravlos/maat/crates/maat-ra/src/main.rs).

Responsibilities:
- load config
- resolve secrets
- construct `ModelRegistry`
- build tool and capability registries
- load installed skills from configured skill directories
- construct the primary PHAROH actor and TUI bridge

### PHAROH

Primary per-user session in [lib.rs](/Users/troytravlos/maat/crates/maat-pharoh/src/lib.rs).

Responsibilities:
- own the primary session history
- persist messages
- expose command handling
- create and manage named sessions
- dispatch bounded tasks to VIZIER
- present tool and skill management commands

Important current behavior:
- PHAROH handles a single user in practice
- named sessions are persisted and restorable
- `/skills` commands are handled here

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

### Installed Skills

Managed by [skills.rs](/Users/troytravlos/maat/crates/maat-config/src/skills.rs).

Install sources currently supported:
- local directory
- ClawHub via local `clawhub` CLI
- GitHub via sparse `git clone`

Installed skills are normalized into `CapabilityCard`s and stamped with `maat-skill.toml`.

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

Config is defined in [config.rs](/Users/troytravlos/maat/crates/maat-config/src/config.rs) and loaded from:
- [maat.toml](/Users/troytravlos/maat/maat.toml)
- optional `maat.workspace.toml`

## Persistence

SQLite storage lives in [sqlite.rs](/Users/troytravlos/maat/crates/maat-memory/src/sqlite.rs).

Persisted data includes:
- session metadata
- chat history
- context pointers

Current memory model:
- sliding context window
- compaction to lightweight pointers
- no semantic retrieval yet

## Current Architectural Boundaries

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
