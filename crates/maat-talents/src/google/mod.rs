//! Google workspace talent — OAuth 2.0 + Gmail + Calendar.

pub mod auth;
pub mod calendar;
pub mod gmail;

use std::path::PathBuf;
use std::sync::Arc;

use maat_config::{MaatConfig, SecretResolver};
use maat_core::ToolRegistry;

use calendar::{CalendarCreate, CalendarList};
use gmail::GmailSend;

// ─────────────────────────────────────────────
// GoogleTalent — bundle that registers all Google tools
// ─────────────────────────────────────────────

pub struct GoogleTalent {
    client_id: String,
    client_secret: String,
    resolver: Arc<SecretResolver>,
    config: Arc<MaatConfig>,
    base_dir: PathBuf,
}

impl GoogleTalent {
    pub fn new(
        client_id: String,
        client_secret: String,
        resolver: Arc<SecretResolver>,
        config: Arc<MaatConfig>,
        base_dir: PathBuf,
    ) -> Self {
        Self { client_id, client_secret, resolver, config, base_dir }
    }

    /// Register all Google tools into the given registry.
    pub fn register_all(&self, registry: &mut ToolRegistry) {
        registry.register(Arc::new(GmailSend {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            resolver: self.resolver.clone(),
            config: self.config.clone(),
            base_dir: self.base_dir.clone(),
        }));
        registry.register(Arc::new(CalendarList {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            resolver: self.resolver.clone(),
            config: self.config.clone(),
        }));
        registry.register(Arc::new(CalendarCreate {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            resolver: self.resolver.clone(),
            config: self.config.clone(),
        }));
    }
}
