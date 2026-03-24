//! maat-config — structured configuration and secret management.
//!
//! # Config
//! Loads `maat.toml` (committed defaults) merged with `maat.workspace.toml`
//! (gitignored personal overrides). Workspace values win.
//!
//! # Secrets
//! `SecretResolver` tries a chain of stores in priority order:
//!   1. 1Password CLI  (`op read`)  — if `MAAT_1P_VAULT` is set
//!   2. OS Keychain    (keyring)    — macOS/Linux/Windows
//!   3. Encrypted file (AES-256-GCM) — headless/Docker fallback
//!   4. Env vars / .env             — read-only, always last
//!
//! Secret keys follow the convention `maat/{provider}/{name}`, e.g.:
//!   `maat/openrouter/api_key`, `maat/imap/password`, `maat/google/client_secret`

pub mod config;
pub mod automations;
pub mod prompts;
pub mod secrets;
pub mod skills;

pub use config::{
    AutomationsConfig, ConfigError, GoogleConfig, ImapConfig, LlmConfig, MaatConfig,
    MemoryConfig, ModelProfileConfig, ModelProviderConfig, ModelRouteConfig,
    ModelRoutingConfig, PromptConfig, SkillsConfig, SupportCapabilityRuleConfig,
    TelegramConfig, TelegramIdentityConfig, PrincipalConfig, UsersConfig,
};
pub use automations::{
    delete_automation, describe_schedule, ensure_sample_automation, find_automation,
    is_schedule_due, load_automations, parse_schedule_expr, set_automation_status,
    slugify_automation_id, upsert_automation, AutomationDelivery, AutomationSchedule, AutomationSpec, AutomationStatus,
};
pub use prompts::{PromptAssetInfo, PromptLibrary, UpdatePolicy};
pub use secrets::{SecretResolver, SecretStore};
pub use skills::{
    default_skill_dirs, fetch_github_asset, install_skill, install_skill_from_dir,
    load_installed_skills, search_clawhub, InstallSource, InstalledSkill,
    InstalledSkillManifest, SkillRegistry, SkillSource,
};
