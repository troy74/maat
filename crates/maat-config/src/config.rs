//! Structured configuration — maat.toml + maat.workspace.toml.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tracing::{info, warn};

// ─────────────────────────────────────────────
// Error
// ─────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config parse error in {file}: {source}")]
    Parse { file: String, source: toml::de::Error },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("secret store error: {0}")]
    Secret(String),
}

// ─────────────────────────────────────────────
// Top-level config
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MaatConfig {
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub prompts: PromptConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub automations: AutomationsConfig,
    #[serde(default)]
    pub imap: Option<ImapConfig>,
    #[serde(default)]
    pub google: Option<GoogleConfig>,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub users: UsersConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Legacy single-model config. Still supported as the bootstrap default.
    #[serde(default = "LlmConfig::default_model")]
    pub model: String,
    /// Legacy single-provider base URL. Still supported as the bootstrap default.
    #[serde(default = "LlmConfig::default_base_url")]
    pub base_url: String,
    #[serde(default = "LlmConfig::default_token_budget")]
    pub token_budget: u32,
    #[serde(default = "LlmConfig::default_compaction_threshold")]
    pub compaction_threshold: u32,
    #[serde(default)]
    pub providers: BTreeMap<String, ModelProviderConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ModelProfileConfig>,
    #[serde(default)]
    pub routing: ModelRoutingConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: Self::default_model(),
            base_url: Self::default_base_url(),
            token_budget: Self::default_token_budget(),
            compaction_threshold: Self::default_compaction_threshold(),
            providers: BTreeMap::new(),
            profiles: BTreeMap::new(),
            routing: ModelRoutingConfig::default(),
        }
    }
}

impl LlmConfig {
    fn default_model() -> String { "openai/gpt-4o-mini".into() }
    fn default_base_url() -> String { "https://openrouter.ai/api/v1".into() }
    fn default_token_budget() -> u32 { 50_000 }
    fn default_compaction_threshold() -> u32 { 40_000 }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    pub api_style: String,
    pub base_url: String,
    pub api_key_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfileConfig {
    pub provider: String,
    pub model_id: String,
    #[serde(default = "ModelProfileConfig::default_temperature")]
    pub temperature: f32,
    #[serde(default = "ModelProfileConfig::default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl ModelProfileConfig {
    fn default_temperature() -> f32 { 0.7 }
    fn default_max_tokens() -> u32 { 4096 }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelRoutingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pharoh_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_classifier_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_nudge_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_default_profile: Option<String>,
    #[serde(default)]
    pub allow_profiles: Vec<String>,
    #[serde(default)]
    pub deny_profiles: Vec<String>,
    #[serde(default)]
    pub routes: BTreeMap<String, ModelRouteConfig>,
    #[serde(default)]
    pub support_rules: Vec<SupportCapabilityRuleConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelRouteConfig {
    #[serde(default)]
    pub prefer_profiles: Vec<String>,
    #[serde(default)]
    pub allow_profiles: Vec<String>,
    #[serde(default)]
    pub deny_profiles: Vec<String>,
    #[serde(default)]
    pub required_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SupportCapabilityRuleConfig {
    #[serde(default)]
    pub match_any_terms: Vec<String>,
    #[serde(default)]
    pub capability_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "MemoryConfig::default_db_path")]
    pub db_path: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { db_path: Self::default_db_path() }
    }
}

impl MemoryConfig {
    fn default_db_path() -> String { "maat.db".into() }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptConfig {
    #[serde(default = "PromptConfig::default_dir")]
    pub dir: String,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self { dir: Self::default_dir() }
    }
}

impl PromptConfig {
    fn default_dir() -> String { "prompts".into() }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    #[serde(default = "SkillsConfig::default_dirs")]
    pub dirs: Vec<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self { dirs: Self::default_dirs() }
    }
}

impl SkillsConfig {
    fn default_dirs() -> Vec<String> { vec!["skills".into()] }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationsConfig {
    #[serde(default = "AutomationsConfig::default_dir")]
    pub dir: String,
    #[serde(default = "AutomationsConfig::default_poll_seconds")]
    pub poll_seconds: u64,
}

impl Default for AutomationsConfig {
    fn default() -> Self {
        Self {
            dir: Self::default_dir(),
            poll_seconds: Self::default_poll_seconds(),
        }
    }
}

impl AutomationsConfig {
    fn default_dir() -> String { "automations".into() }
    fn default_poll_seconds() -> u64 { 30 }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImapConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    /// Secret key to look up in the resolver.
    /// Defaults to `maat/imap/password`.
    pub password_secret: Option<String>,
}

impl ImapConfig {
    pub fn password_key(&self) -> &str {
        self.password_secret.as_deref().unwrap_or("maat/imap/password")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GoogleConfig {
    pub client_id: Option<String>,
    /// Secret key for client_secret. Defaults to `maat/google/client_secret`.
    pub client_secret_key: Option<String>,
    /// Secret key for OAuth token JSON. Defaults to `maat/google/oauth_token`.
    pub token_key: Option<String>,
}

impl GoogleConfig {
    pub fn client_secret_key(&self) -> &str {
        self.client_secret_key.as_deref().unwrap_or("maat/google/client_secret")
    }
    pub fn token_key(&self) -> &str {
        self.token_key.as_deref().unwrap_or("maat/google/oauth_token")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    pub bot_token_secret: Option<String>,
    pub bot_token_env: Option<String>,
    pub default_chat_id: Option<i64>,
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
    #[serde(default = "TelegramConfig::default_poll_seconds")]
    pub poll_seconds: u64,
    #[serde(default = "TelegramConfig::default_download_dir")]
    pub download_dir: String,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token_secret: None,
            bot_token_env: None,
            default_chat_id: None,
            allowed_chat_ids: Vec::new(),
            poll_seconds: Self::default_poll_seconds(),
            download_dir: Self::default_download_dir(),
        }
    }
}

impl TelegramConfig {
    fn default_poll_seconds() -> u64 { 10 }
    fn default_download_dir() -> String { "tmp/telegram".into() }

    pub fn token_key(&self) -> &str {
        self.bot_token_secret.as_deref().unwrap_or("maat/telegram/bot_token")
    }

    pub fn token_env(&self) -> &str {
        self.bot_token_env.as_deref().unwrap_or("TELEGRAM_BOT_TOKEN")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecretsConfig {
    /// 1Password vault name. If set, 1Password CLI is tried first.
    pub onepassword_vault: Option<String>,
    /// Path to encrypted secrets file (headless fallback).
    /// Defaults to `maat.secrets.enc` in the working directory.
    pub encrypted_file_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsersConfig {
    #[serde(default)]
    pub principals: BTreeMap<String, PrincipalConfig>,
    #[serde(default)]
    pub telegram: Vec<TelegramIdentityConfig>,
}

impl UsersConfig {
    pub fn has_telegram_identities(&self) -> bool {
        !self.telegram.is_empty()
    }

    pub fn resolve_telegram_identity(
        &self,
        telegram_user_id: i64,
        chat_id: i64,
    ) -> Option<&TelegramIdentityConfig> {
        self.telegram.iter().find(|identity| {
            identity.user_id == telegram_user_id
                && (identity.allowed_chat_ids.is_empty()
                    || identity.allowed_chat_ids.iter().any(|allowed| *allowed == chat_id))
        })
    }

    pub fn principal_display_name<'a>(&'a self, principal_id: &'a str) -> &'a str {
        self.principals
            .get(principal_id)
            .and_then(|principal| principal.display_name.as_deref())
            .unwrap_or(principal_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrincipalConfig {
    pub display_name: Option<String>,
    pub role: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TelegramIdentityConfig {
    pub principal: String,
    pub user_id: i64,
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
    #[serde(default = "TelegramIdentityConfig::default_can_instruct")]
    pub can_instruct: bool,
}

impl TelegramIdentityConfig {
    fn default_can_instruct() -> bool { true }
}

// ─────────────────────────────────────────────
// Loader
// ─────────────────────────────────────────────

impl MaatConfig {
    /// Load `maat.toml`, then merge `maat.workspace.toml` on top.
    ///
    /// Search order for each file:
    ///   1. `$MAAT_CONFIG_DIR` env var (if set)
    ///   2. Directory containing the running binary
    ///   3. Current working directory
    ///
    /// Missing files are silently skipped (returns defaults).
    pub fn load() -> Result<Self, ConfigError> {
        let dirs = config_search_dirs();
        info!(search_dirs = ?dirs, "config search dirs");
        let base = load_from_dirs("maat.toml", &dirs)?;
        let workspace = load_from_dirs("maat.workspace.toml", &dirs)?;
        Ok(merge(base, workspace))
    }

    /// Display-safe summary (no secret values).
    pub fn display_summary(&self) -> String {
        let mut lines = vec![
            format!("[llm]"),
            format!("  model              = {}", self.llm.model),
            format!("  base_url           = {}", self.llm.base_url),
            format!("  token_budget       = {}", self.llm.token_budget),
            format!("  compaction_threshold = {}", self.llm.compaction_threshold),
            format!("  providers          = {}", self.llm.providers.len()),
            format!("  profiles           = {}", self.llm.profiles.len()),
            format!("[memory]"),
            format!("  db_path            = {}", self.memory.db_path),
            format!("[prompts]"),
            format!("  dir                = {}", self.prompts.dir),
            format!("[skills]"),
            format!("  dirs               = {}", self.skills.dirs.join(", ")),
        ];
        if let Some(default_profile) = &self.llm.routing.default_profile {
            lines.push("[llm.routing]".into());
            lines.push(format!("  default_profile = {default_profile}"));
            if let Some(pharoh) = &self.llm.routing.pharoh_profile {
                lines.push(format!("  pharoh_profile  = {pharoh}"));
            }
            if let Some(planner) = &self.llm.routing.planner_profile {
                lines.push(format!("  planner_profile = {planner}"));
            }
            if let Some(capability_nudge) = &self.llm.routing.capability_nudge_profile {
                lines.push(format!("  capability_nudge_profile = {capability_nudge}"));
            }
            if let Some(session_default) = &self.llm.routing.session_default_profile {
                lines.push(format!("  session_default_profile = {session_default}"));
            }
        }
        if let Some(imap) = &self.imap {
            lines.push("[imap]".into());
            if let Some(h) = &imap.host     { lines.push(format!("  host     = {h}")); }
            if let Some(p) = &imap.port     { lines.push(format!("  port     = {p}")); }
            if let Some(u) = &imap.username { lines.push(format!("  username = {u}")); }
            lines.push(format!("  password → secret:{}", imap.password_key()));
        }
        if let Some(g) = &self.google {
            lines.push("[google]".into());
            if let Some(id) = &g.client_id { lines.push(format!("  client_id = {id}")); }
            lines.push(format!("  client_secret → secret:{}", g.client_secret_key()));
            lines.push(format!("  oauth_token   → secret:{}", g.token_key()));
        }
        if self.telegram.enabled {
            lines.push("[telegram]".into());
            lines.push("  enabled = true".into());
            lines.push(format!("  bot_token → secret:{}", self.telegram.token_key()));
            lines.push(format!("  poll_seconds = {}", self.telegram.poll_seconds));
            lines.push(format!("  download_dir = {}", self.telegram.download_dir));
            if let Some(chat_id) = self.telegram.default_chat_id {
                lines.push(format!("  default_chat_id = {}", chat_id));
            }
            if !self.telegram.allowed_chat_ids.is_empty() {
                lines.push(format!(
                    "  allowed_chat_ids = {}",
                    self.telegram
                        .allowed_chat_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        if let Some(vault) = &self.secrets.onepassword_vault {
            lines.push(format!("[secrets]"));
            lines.push(format!("  1password_vault = {vault}"));
        }
        if !self.users.principals.is_empty() || !self.users.telegram.is_empty() {
            lines.push("[users]".into());
            lines.push(format!("  principals = {}", self.users.principals.len()));
            lines.push(format!("  telegram_identities = {}", self.users.telegram.len()));
        }
        lines.join("\n")
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

/// Candidate directories to search for config files, in priority order.
fn config_search_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();

    // 1. Explicit env override.
    if let Ok(d) = std::env::var("MAAT_CONFIG_DIR") {
        dirs.push(std::path::PathBuf::from(d));
    }

    // 2. Directory containing the running binary.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.to_path_buf());
        }
    }

    // 3. Current working directory.
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }

    dirs
}

/// Find and load a TOML file by searching each directory in order.
fn load_from_dirs(filename: &str, dirs: &[std::path::PathBuf]) -> Result<toml::Value, ConfigError> {
    for dir in dirs {
        let path = dir.join(filename);
        if path.exists() {
            info!(path = %path.display(), "loading config file");
            let text = std::fs::read_to_string(&path)?;
            return toml::from_str(&text)
                .map_err(|e| ConfigError::Parse { file: path.display().to_string(), source: e });
        }
    }
    info!(filename, "config file not found in any search dir");
    Ok(toml::Value::Table(toml::map::Map::new()))
}

/// Deep-merge: workspace values override base values.
fn merge(base: toml::Value, workspace: toml::Value) -> MaatConfig {
    let merged = deep_merge(base, workspace);
    match merged.try_into() {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("config deserialisation failed, using defaults: {e}");
            MaatConfig::default()
        }
    }
}

fn deep_merge(base: toml::Value, overlay: toml::Value) -> toml::Value {
    match (base, overlay) {
        (toml::Value::Table(mut b), toml::Value::Table(o)) => {
            for (k, v) in o {
                let entry = b.remove(&k).unwrap_or(toml::Value::Table(toml::map::Map::new()));
                b.insert(k, deep_merge(entry, v));
            }
            toml::Value::Table(b)
        }
        (_, overlay) => overlay, // scalar / array: overlay wins
    }
}
