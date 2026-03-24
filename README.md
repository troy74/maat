# MAAT

MAAT is a Rust agent runtime built around a small orchestration tree:

```text
Herald (TUI / Telegram) -> RA -> PHAROH -> VIZIER -> MINION -> tools / skills
```

It currently supports:
- a terminal UI
- a Telegram herald via bot polling
- a primary per-user session plus named sessions
- background runs backed by dedicated named sessions
- a separate runtime status lane in the TUI
- model routing via provider/profile/policy layers
- compiled talents for files, search, IMAP, Gmail, Calendar, and skill self-management
- installed third-party skills loaded from local folders
- local skill install plus provider-style install entry points for ClawHub and GitHub
- SQLite-backed persistence with context compaction, artifacts, automation runs, and background runs
- first-class artifacts with readable handles
- automations with interval/daily/weekly schedules and Telegram delivery
- registered Telegram sender gating for inbound control

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
- [crates/maat-heralds](/Users/troytravlos/maat/crates/maat-heralds) contains the TUI and Telegram heralds.

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

Telegram can be enabled with config like:

```toml
[telegram]
enabled = true
default_chat_id = 123456789
poll_seconds = 10
download_dir = "tmp/telegram"

[users.principals.owner]
display_name = "Troy"
role = "owner"
permissions = ["*"]

[[users.telegram]]
principal = "owner"
user_id = 123456789
allowed_chat_ids = [123456789]
can_instruct = true
```

Then provide the bot token either as:
- secret `maat/telegram/bot_token`
- env var `TELEGRAM_BOT_TOKEN`

Current Telegram ingress rules:
- if `[[users.telegram]]` entries exist, only those registered sender user IDs can instruct the bot
- `allowed_chat_ids` inside each identity can further restrict which chats that sender may use
- top-level `telegram.allowed_chat_ids` is now the legacy fallback when no registered Telegram identities exist yet

Current outbound truth:
- email and Telegram delivery both exist
- Telegram can return generated or imported artifacts back into the chat
- outbound authorization is still lighter than inbound authorization, and that is a known next-phase gap

Automations can now deliver directly to Telegram on completion via a TOML section like:

```toml
[delivery]
kind = "telegram"
chat_id = 123456789
```

If `chat_id` is omitted, MAAT will use `telegram.default_chat_id` as the single-user "me" target.

## Current Commands

Common slash-command surface:
- `/help`
- `/tools`
- `/skills`
- `/skills search <query>`
- `/skills install <path>`
- `/skills reload`
- `/skills install clawhub:<slug>`
- `/skills install github:<owner/repo>:<path>`
- internal `skill_manage` tool for scaffolding command-mode local skills and fetching GitHub assets into them
- `/config`
- `/config set <key> <value>`
- `/secret list`
- `/secret set <key> <value>`
- `/secret delete <key>`
- `/session new <name>: <description>`
- `/session list`
- `/session end <name>`
- `/status`
- `/automations`
- `/automation show <name>`
- `/automation run <name>`
- `/automation create <name> | <schedule> | <prompt>`
- `/runs`
- `/run start <title>: <prompt>`
- `/run show <handle>`
- `/run open <handle>`
- `/run cancel <handle>`
- `/artifacts`
- `/artifacts show <handle>`

Telegram first pass supports:
- plain chat
- slash commands
- photo and document uploads flowing into the artifact path
- one named session per registered Telegram principal and chat (`telegram-<principal>-<chat_id>`)

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

There is also now a first-class self-extension path:
- `skill_manage` can scaffold a command-mode skill directly into the local skills directory
- it can fetch exact GitHub repo assets like binaries and ONNX models into that skill folder
- it can install an existing local skill directory through the same local installer seam
- `/skills reload` can hot-load newly added skills into the running registries

Current limitation:
- hot reload is aimed at making newly added skills available without restart; it is not yet a full uninstall/removal reconciler for skills that disappear from disk mid-run
- explicit natural-language requests that name a skill can still overlap with model-routing intent, so this area is in transition

ClawHub and GitHub installs default to review-level trust unless explicitly promoted.

## Docs

- [ARCHITECTURE.md](/Users/troytravlos/maat/ARCHITECTURE.md)
- [PLAN.md](/Users/troytravlos/maat/PLAN.md)
- [MODEL_CAPABILITY_FRAMEWORK.md](/Users/troytravlos/maat/MODEL_CAPABILITY_FRAMEWORK.md)
- [SPEC.md](/Users/troytravlos/maat/SPEC.md)

## Background Runs

Longer-running work can now be detached from the main chat as a background run.

Each run has:
- a readable handle like `quiet-thread-a1b2`
- a dedicated named session, usually `@<handle>`
- persisted status and summary in SQLite

This lets PHAROH stay light while the work continues in the background. You can list runs, inspect one, and jump your active session to the run session from the TUI.

Current control surface:
- start a run
- inspect it
- focus its run session
- request cancellation

Current limitation:
- cancellation now propagates into the active workflow loop and stops future model/tool rounds, but it still cannot preempt a single provider/tool call that is already in flight mid-request
- Telegram is intentionally thin for now and does not yet expose the richer live runtime/status lane the TUI has

## Direction

The current architecture is good enough to use, but the next phase is about making it:
- smoother under ambiguity
- more autonomous in recovery
- more robust around identity and outbound policy
- easier to inspect when routing or tool execution goes wrong

The intended split is:
- prompts/config decide more of the soft judgment
- code enforces model constraints, trust, permissions, and delivery policy
- explicit invocation affordances reduce ambiguity without forcing every overlap into a hard-coded branch
