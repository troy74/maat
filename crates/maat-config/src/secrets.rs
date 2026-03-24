//! Secret resolution chain.
//!
//! Stores are tried in order; first non-None result wins.
//! `/secret set` writes to the first writable store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use tracing::{debug, warn};

use crate::config::ConfigError;

// ─────────────────────────────────────────────
// Trait
// ─────────────────────────────────────────────

pub trait SecretStore: Send + Sync {
    fn name(&self) -> &str;
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str) -> Result<(), ConfigError>;
    fn delete(&self, key: &str) -> Result<(), ConfigError>;
    /// Key names only — never values.
    fn list_keys(&self) -> Vec<String>;
    fn is_writable(&self) -> bool;
}

// ─────────────────────────────────────────────
// SecretResolver
// ─────────────────────────────────────────────

pub struct SecretResolver {
    stores: Vec<Arc<dyn SecretStore>>,
    cache: Mutex<HashMap<String, String>>,
}

impl SecretResolver {
    pub fn new(stores: Vec<Arc<dyn SecretStore>>) -> Self {
        Self {
            stores,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve a secret from the chain.
    pub fn get(&self, key: &str) -> Option<String> {
        if let Some(cached) = self
            .cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(key).cloned())
        {
            debug!(key, "secret resolved from cache");
            return Some(cached);
        }
        for store in &self.stores {
            if let Some(val) = store.get(key) {
                if let Ok(mut cache) = self.cache.lock() {
                    cache.insert(key.to_string(), val.clone());
                }
                debug!(key, store = store.name(), "secret resolved");
                return Some(val);
            }
        }
        None
    }

    /// Store a secret in the first writable store.
    pub fn set(&self, key: &str, value: &str) -> Result<(), ConfigError> {
        for store in &self.stores {
            if store.is_writable() {
                store.set(key, value)?;
                if let Ok(mut cache) = self.cache.lock() {
                    cache.insert(key.to_string(), value.to_string());
                }
                debug!(key, store = store.name(), "secret stored");
                return Ok(());
            }
        }
        Err(ConfigError::Secret("no writable secret store available".into()))
    }

    pub fn delete(&self, key: &str) -> Result<(), ConfigError> {
        for store in &self.stores {
            if store.is_writable() {
                let result = store.delete(key);
                if result.is_ok() {
                    if let Ok(mut cache) = self.cache.lock() {
                        cache.remove(key);
                    }
                }
                return result;
            }
        }
        Err(ConfigError::Secret("no writable secret store available".into()))
    }

    /// List all known keys across all stores (deduplicated, names only).
    pub fn list_keys(&self) -> Vec<String> {
        let mut keys: std::collections::BTreeSet<String> = Default::default();
        for store in &self.stores {
            for k in store.list_keys() {
                keys.insert(k);
            }
        }
        keys.into_iter().collect()
    }

    /// Display-safe summary of which stores are active.
    pub fn store_summary(&self) -> String {
        self.stores
            .iter()
            .map(|s| format!("  [{}] writable={}", s.name(), s.is_writable()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ─────────────────────────────────────────────
// 1. OS Keychain store
// ─────────────────────────────────────────────

pub struct KeychainStore;

impl SecretStore for KeychainStore {
    fn name(&self) -> &str { "os-keychain" }
    fn is_writable(&self) -> bool { true }

    fn get(&self, key: &str) -> Option<String> {
        keyring::Entry::new("maat", key)
            .ok()
            .and_then(|e| e.get_password().ok())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), ConfigError> {
        keyring::Entry::new("maat", key)
            .map_err(|e| ConfigError::Secret(e.to_string()))?
            .set_password(value)
            .map_err(|e| ConfigError::Secret(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), ConfigError> {
        keyring::Entry::new("maat", key)
            .map_err(|e| ConfigError::Secret(e.to_string()))?
            .delete_credential()
            .map_err(|e| ConfigError::Secret(e.to_string()))
    }

    fn list_keys(&self) -> Vec<String> {
        // keyring has no enumerate API — return empty; keys are tracked elsewhere.
        vec![]
    }
}

// ─────────────────────────────────────────────
// 2. 1Password CLI store (read-only for now)
// ─────────────────────────────────────────────

/// Reads secrets via `op read "op://<vault>/<item>/<field>"`.
/// Key format: `maat/{item}/{field}` → `op://<vault>/{item}/{field}`.
/// Read-only: write support requires more complex `op item edit` orchestration.
pub struct OnePasswordStore {
    vault: String,
}

impl OnePasswordStore {
    pub fn new(vault: impl Into<String>) -> Self {
        Self { vault: vault.into() }
    }

    fn op_path(&self, key: &str) -> String {
        // key: "maat/openrouter/api_key" → "op://Vault/openrouter/api_key"
        let stripped = key.strip_prefix("maat/").unwrap_or(key);
        format!("op://{}/{}", self.vault, stripped)
    }

    fn op_available() -> bool {
        std::process::Command::new("op")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

impl SecretStore for OnePasswordStore {
    fn name(&self) -> &str { "1password-cli" }
    fn is_writable(&self) -> bool { false }

    fn get(&self, key: &str) -> Option<String> {
        if !Self::op_available() {
            warn!("1password-cli: `op` not found in PATH");
            return None;
        }
        let path = self.op_path(key);
        let out = std::process::Command::new("op")
            .args(["read", &path, "--no-newline"])
            .output()
            .ok()?;
        if out.status.success() {
            String::from_utf8(out.stdout).ok()
        } else {
            None
        }
    }

    fn set(&self, _key: &str, _value: &str) -> Result<(), ConfigError> {
        Err(ConfigError::Secret("1Password CLI store is read-only; use `op item edit` manually".into()))
    }

    fn delete(&self, _key: &str) -> Result<(), ConfigError> {
        Err(ConfigError::Secret("1Password CLI store is read-only".into()))
    }

    fn list_keys(&self) -> Vec<String> { vec![] }
}

// ─────────────────────────────────────────────
// 3. Encrypted file store (AES-256-GCM)
// ─────────────────────────────────────────────
//
// File format: base64( nonce[12] || ciphertext ) of a JSON map.
// Key: 32 raw bytes from MAAT_SECRET_KEY (base64), or auto-generated on first use.

const KEY_ENV: &str = "MAAT_SECRET_KEY";
const DEFAULT_PATH: &str = "maat.secrets.enc";

pub struct EncryptedFileStore {
    path: String,
}

impl EncryptedFileStore {
    pub fn new(path: Option<&str>) -> Self {
        Self { path: path.unwrap_or(DEFAULT_PATH).to_string() }
    }

    fn key() -> Result<[u8; 32], ConfigError> {
        match std::env::var(KEY_ENV) {
            Ok(b64) => {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    b64.trim(),
                )
                .map_err(|e| ConfigError::Secret(format!("MAAT_SECRET_KEY decode: {e}")))?;
                if bytes.len() != 32 {
                    return Err(ConfigError::Secret(
                        "MAAT_SECRET_KEY must be 32 bytes (44 base64 chars)".into(),
                    ));
                }
                let mut k = [0u8; 32];
                k.copy_from_slice(&bytes);
                Ok(k)
            }
            Err(_) => {
                // Generate a new key, print it, and exit with instructions.
                let key: [u8; 32] = {
                    use aes_gcm::aead::rand_core::RngCore;
                    let mut k = [0u8; 32];
                    OsRng.fill_bytes(&mut k);
                    k
                };
                let b64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    key,
                );
                eprintln!(
                    "\n[maat] No MAAT_SECRET_KEY found. \
                    Add this to your .env or shell profile:\n\
                    \n    export MAAT_SECRET_KEY={b64}\n"
                );
                Err(ConfigError::Secret("MAAT_SECRET_KEY not set".into()))
            }
        }
    }

    fn load_map(&self) -> HashMap<String, String> {
        let Ok(raw) = std::fs::read_to_string(&self.path) else { return HashMap::new() };
        let Ok(key_bytes) = Self::key() else { return HashMap::new() };
        let Ok(enc) = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            raw.trim(),
        ) else { return HashMap::new() };

        if enc.len() < 12 { return HashMap::new(); }
        let (nonce_bytes, ct) = enc.split_at(12);
        let cipher = Aes256Gcm::new_from_slice(&key_bytes).expect("valid key size");
        let nonce = Nonce::from_slice(nonce_bytes);
        let Ok(plain) = cipher.decrypt(nonce, ct) else {
            warn!("encrypted file store: decryption failed (wrong key?)");
            return HashMap::new();
        };
        serde_json::from_slice(&plain).unwrap_or_default()
    }

    fn save_map(&self, map: &HashMap<String, String>) -> Result<(), ConfigError> {
        let key_bytes = Self::key()?;
        let plain = serde_json::to_vec(map)
            .map_err(|e| ConfigError::Secret(e.to_string()))?;
        let cipher = Aes256Gcm::new_from_slice(&key_bytes).expect("valid key size");
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let mut ct = cipher
            .encrypt(&nonce, plain.as_slice())
            .map_err(|e| ConfigError::Secret(e.to_string()))?;
        let mut blob = nonce.to_vec();
        blob.append(&mut ct);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, blob);
        std::fs::write(&self.path, b64).map_err(|e| ConfigError::Io(e))
    }
}

impl SecretStore for EncryptedFileStore {
    fn name(&self) -> &str { "encrypted-file" }
    fn is_writable(&self) -> bool { Self::key().is_ok() }

    fn get(&self, key: &str) -> Option<String> {
        self.load_map().remove(key)
    }

    fn set(&self, key: &str, value: &str) -> Result<(), ConfigError> {
        let mut map = self.load_map();
        map.insert(key.to_string(), value.to_string());
        self.save_map(&map)
    }

    fn delete(&self, key: &str) -> Result<(), ConfigError> {
        let mut map = self.load_map();
        map.remove(key);
        self.save_map(&map)
    }

    fn list_keys(&self) -> Vec<String> {
        self.load_map().into_keys().collect()
    }
}

// ─────────────────────────────────────────────
// 4. Env store (read-only)
// ─────────────────────────────────────────────

/// Maps canonical secret keys to env var names.
pub struct EnvStore {
    /// key → env var name, e.g. "maat/openrouter/api_key" → "OPENROUTER_API_KEY"
    mapping: HashMap<String, String>,
}

impl EnvStore {
    pub fn with_defaults() -> Self {
        let mut m = HashMap::new();
        m.insert("maat/openrouter/api_key".into(), "OPENROUTER_API_KEY".into());
        m.insert("maat/imap/password".into(),       "IMAP_PASSWORD".into());
        m.insert("maat/imap/username".into(),        "IMAP_USERNAME".into());
        m.insert("maat/imap/host".into(),            "IMAP_HOST".into());
        m.insert("maat/google/client_secret".into(), "GOOGLE_CLIENT_SECRET".into());
        m.insert("maat/google/client_id".into(),     "GOOGLE_CLIENT_ID".into());
        Self { mapping: m }
    }
}

impl SecretStore for EnvStore {
    fn name(&self) -> &str { "env" }
    fn is_writable(&self) -> bool { false }

    fn get(&self, key: &str) -> Option<String> {
        let env_var = self.mapping.get(key)?;
        std::env::var(env_var).ok()
    }

    fn set(&self, _: &str, _: &str) -> Result<(), ConfigError> {
        Err(ConfigError::Secret("env store is read-only".into()))
    }

    fn delete(&self, _: &str) -> Result<(), ConfigError> {
        Err(ConfigError::Secret("env store is read-only".into()))
    }

    fn list_keys(&self) -> Vec<String> {
        self.mapping
            .iter()
            .filter(|(_, env)| std::env::var(env).is_ok())
            .map(|(k, _)| k.clone())
            .collect()
    }
}

// ─────────────────────────────────────────────
// Builder
// ─────────────────────────────────────────────

/// Build the default resolver from config.
/// Call after loading `MaatConfig`.
pub fn build_resolver(
    onepassword_vault: Option<&str>,
    encrypted_file_path: Option<&str>,
) -> SecretResolver {
    let mut stores: Vec<Arc<dyn SecretStore>> = Vec::new();

    // 1. 1Password (if vault configured)
    if let Some(vault) = onepassword_vault {
        stores.push(Arc::new(OnePasswordStore::new(vault)));
    }

    // 2. OS Keychain
    stores.push(Arc::new(KeychainStore));

    // 3. Encrypted file (headless fallback — only added if key is available)
    let enc_store = EncryptedFileStore::new(encrypted_file_path);
    stores.push(Arc::new(enc_store));

    // 4. Env vars (always last, read-only)
    stores.push(Arc::new(EnvStore::with_defaults()));

    SecretResolver::new(stores)
}
