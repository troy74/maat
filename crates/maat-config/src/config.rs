//! Structured configuration — maat.toml + maat.workspace.toml.

use serde::{Deserialize, Serialize};
use std::path::Path;
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
    pub imap: Option<ImapConfig>,
    #[serde(default)]
    pub google: Option<GoogleConfig>,
    #[serde(default)]
    pub secrets: SecretsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "LlmConfig::default_model")]
    pub model: String,
    #[serde(default = "LlmConfig::default_base_url")]
    pub base_url: String,
    #[serde(default = "LlmConfig::default_token_budget")]
    pub token_budget: u32,
    #[serde(default = "LlmConfig::default_compaction_threshold")]
    pub compaction_threshold: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: Self::default_model(),
            base_url: Self::default_base_url(),
            token_budget: Self::default_token_budget(),
            compaction_threshold: Self::default_compaction_threshold(),
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecretsConfig {
    /// 1Password vault name. If set, 1Password CLI is tried first.
    pub onepassword_vault: Option<String>,
    /// Path to encrypted secrets file (headless fallback).
    /// Defaults to `maat.secrets.enc` in the working directory.
    pub encrypted_file_path: Option<String>,
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
            format!("[memory]"),
            format!("  db_path            = {}", self.memory.db_path),
        ];
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
        if let Some(vault) = &self.secrets.onepassword_vault {
            lines.push(format!("[secrets]"));
            lines.push(format!("  1password_vault = {vault}"));
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

fn load_file(path: &str) -> Result<toml::Value, ConfigError> {
    if !Path::new(path).exists() {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }
    let text = std::fs::read_to_string(path)?;
    toml::from_str(&text).map_err(|e| ConfigError::Parse { file: path.into(), source: e })
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
