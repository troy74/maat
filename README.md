# MAAT

MAAT is a Rust agent runtime built around a small orchestration tree:

```text
TUI -> RA -> PHAROH -> VIZIER -> MINION -> tools / skills
```

It currently supports:
- a terminal UI
- a primary per-user session plus named sessions
- model routing via provider/profile/policy layers
- compiled talents for files, search, IMAP, Gmail, and Calendar
- installed third-party skills loaded from local folders
- local skill install plus provider-style install entry points for ClawHub and GitHub
- SQLite-backed persistence with context compaction

The codebase is still a work in progress, but the current slice is coherent and usable.

## Workspace Layout

- [crates/maat-ra](/Users/troytravlos/maat/crates/maat-ra) bootstraps the app and runtime.
- [crates/maat-pharoh](/Users/troytravlos/maat/crates/maat-pharoh) owns the primary session and command surface.
- [crates/maat-vizier](/Users/troytravlos/maat/crates/maat-vizier) resolves models and capability candidates per task.
- [crates/maat-minions](/Users/troytravlos/maat/crates/maat-minions) executes bounded tasks.
- [crates/maat-core](/Users/troytravlos/maat/crates/maat-core) holds shared runtime types.
- [crates/maat-config](/Users/troytravlos/maat/crates/maat-config) handles config, prompts, secrets, and installed-skill loading.
- [crates/maat-memory](/Users/troytravlos/maat/crates/maat-memory) provides SQLite-backed storage and context-window helpers.
- [crates/maat-talents](/Users/troytravlos/maat/crates/maat-talents) contains compiled-in tools.
- [crates/maat-heralds](/Users/troytravlos/maat/crates/maat-heralds) contains the TUI herald.

## Running

Prereqs:
- Rust toolchain
- an OpenRouter key, or a provider config that resolves to a usable model

Useful commands:

```bash
cargo check
cargo run -p maat-ra
```

The app reads:
- [maat.toml](/Users/troytravlos/maat/maat.toml)
- optional `maat.workspace.toml`
- prompt files under [prompts/](/Users/troytravlos/maat/prompts)

Secrets can be supplied through the secret resolver or environment variables.

## Current Commands

Inside the TUI:
- `/help`
- `/tools`
- `/skills`
- `/skills search <query>`
- `/skills install <path>`
- `/skills install clawhub:<slug>`
- `/skills install github:<owner/repo>:<path>`
- `/config`
- `/config set <key> <value>`
- `/secret list`
- `/secret set <key> <value>`
- `/secret delete <key>`
- `/session new <name>: <description>`
- `/session list`
- `/session end <name>`
- `/status`

## Installed Skills

Installed skills are normalized through [crates/maat-config/src/skills.rs](/Users/troytravlos/maat/crates/maat-config/src/skills.rs).

Each installed skill can carry:
- trust level
- permissions
- source kind
- reference
- local install path

That metadata is stamped into `maat-skill.toml` and then turned into a `CapabilityCard` for routing.

Supported install sources today:
- local directory
- `clawhub:<slug>` via local `clawhub` CLI
- `github:<owner/repo>:<path>` via sparse `git clone`

ClawHub and GitHub installs default to review-level trust unless explicitly promoted.

## Docs

- [ARCHITECTURE.md](/Users/troytravlos/maat/ARCHITECTURE.md)
- [PLAN.md](/Users/troytravlos/maat/PLAN.md)
- [MODEL_CAPABILITY_FRAMEWORK.md](/Users/troytravlos/maat/MODEL_CAPABILITY_FRAMEWORK.md)
- [SPEC.md](/Users/troytravlos/maat/SPEC.md)
