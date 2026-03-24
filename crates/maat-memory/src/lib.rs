//! maat-memory — session persistence and context window management.
//!
//! `MemoryStore` trait — implemented by `SqliteStore` (production) and
//! `InMemoryStore` (tests / no-persistence mode).
//!
//! `ContextWindow` — builds the bounded message slice sent to the LLM.

pub mod sqlite;
pub mod window;

use async_trait::async_trait;
use maat_core::{BackgroundRunStatus, ChatMessage, MaatError, Role, SessionId};
use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────
// StoredMessage
// ─────────────────────────────────────────────

/// A `ChatMessage` annotated with storage metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_calls_json: Option<String>,
    pub estimated_tokens: u32,
    pub created_at_ms: u64,
}

impl StoredMessage {
    pub fn from_chat(session_id: &SessionId, msg: &ChatMessage) -> Self {
        let role = match msg.role {
            Role::System    => "system",
            Role::User      => "user",
            Role::Assistant => "assistant",
            Role::Tool      => "tool",
        };
        Self {
            id: ulid::Ulid::new().to_string(),
            session_id: session_id.0.to_string(),
            role: role.to_string(),
            content: msg.content.clone(),
            tool_call_id: msg.tool_call_id.clone(),
            tool_calls_json: msg.tool_calls_json.clone(),
            estimated_tokens: estimate_tokens(&msg.content),
            created_at_ms: maat_core::now_ms(),
        }
    }

    pub fn to_chat(&self) -> ChatMessage {
        let role = match self.role.as_str() {
            "system"    => Role::System,
            "assistant" => Role::Assistant,
            "tool"      => Role::Tool,
            _           => Role::User,
        };
        ChatMessage {
            role,
            content: self.content.clone(),
            image_inputs: vec![],
            tool_call_id: self.tool_call_id.clone(),
            tool_calls_json: self.tool_calls_json.clone(),
        }
    }
}

// ─────────────────────────────────────────────
// SessionMeta
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub session_id: String,
    pub user_id: String,
    pub name: String,
    pub system_prompt: String,
    pub created_at_ms: u64,
    pub last_active_ms: u64,
}

// ─────────────────────────────────────────────
// ContextPointer
// ─────────────────────────────────────────────

/// A compressed summary replacing a span of pruned messages.
#[derive(Debug, Clone)]
pub struct ContextPointer {
    pub id: String,
    pub session_id: String,
    pub summary: String,
    pub covers_from_ms: u64,
    pub covers_to_ms: u64,
    pub created_at_ms: u64,
}

impl ContextPointer {
    /// Render as a cheap System message injected into the context window.
    pub fn to_chat(&self) -> ChatMessage {
        ChatMessage::system(format!("[CONTEXT SUMMARY] {}", self.summary))
    }
}

// ─────────────────────────────────────────────
// ArtifactRecord
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub artifact_id: String,
    pub handle: String,
    pub user_id: String,
    pub session_id: String,
    pub kind: String,
    pub mime_type: String,
    pub display_name: String,
    pub storage_path: String,
    pub byte_size: u64,
    pub source: String,
    pub summary: String,
    pub metadata_json: String,
    pub analysis_json: String,
    pub created_at_ms: u64,
}

impl ArtifactRecord {
    pub fn pointer_text(&self) -> String {
        format!(
            "[ARTIFACT {}] {} ({}, {}, {})",
            self.handle, self.summary, self.kind, self.mime_type, self.display_name
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationRunRecord {
    pub run_id: String,
    pub automation_id: String,
    pub automation_name: String,
    pub status: String,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub summary: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundRunRecord {
    pub run_id: String,
    pub handle: String,
    pub user_id: String,
    pub parent_session_id: String,
    pub session_name: String,
    pub title: String,
    pub prompt: String,
    pub status: BackgroundRunStatus,
    pub summary: String,
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

// ─────────────────────────────────────────────
// MemoryStore trait
// ─────────────────────────────────────────────

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn save_session_meta(&self, meta: &SessionMeta) -> Result<(), MaatError>;
    fn load_session_meta(&self, session_id: &str) -> Result<Option<SessionMeta>, MaatError>;
    async fn load_session_meta_by_user_and_name(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<SessionMeta>, MaatError>;
    async fn save_message(&self, msg: &StoredMessage) -> Result<(), MaatError>;
    async fn load_history(&self, session_id: &str) -> Result<Vec<StoredMessage>, MaatError>;
    async fn save_context_pointer(&self, ptr: &ContextPointer) -> Result<(), MaatError>;
    async fn load_context_pointers(&self, session_id: &str) -> Result<Vec<ContextPointer>, MaatError>;
    async fn import_artifact(
        &self,
        user_id: &str,
        session_id: &str,
        source_path: &std::path::Path,
    ) -> Result<ArtifactRecord, MaatError>;
    async fn save_generated_artifact(
        &self,
        user_id: &str,
        session_id: &str,
        display_name: &str,
        kind: &str,
        mime_type: &str,
        source: &str,
        summary: &str,
        metadata_json: &str,
        analysis_json: &str,
        bytes: &[u8],
    ) -> Result<ArtifactRecord, MaatError>;
    async fn list_artifacts(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<ArtifactRecord>, MaatError>;
    async fn get_artifact_by_handle(
        &self,
        user_id: &str,
        handle: &str,
    ) -> Result<Option<ArtifactRecord>, MaatError>;
    async fn latest_session_artifact(
        &self,
        session_id: &str,
    ) -> Result<Option<ArtifactRecord>, MaatError>;
    async fn save_automation_run(&self, run: &AutomationRunRecord) -> Result<(), MaatError>;
    async fn latest_automation_run(
        &self,
        automation_id: &str,
    ) -> Result<Option<AutomationRunRecord>, MaatError>;
    async fn list_automation_runs(
        &self,
        automation_id: &str,
        limit: usize,
    ) -> Result<Vec<AutomationRunRecord>, MaatError>;
    async fn save_background_run(&self, run: &BackgroundRunRecord) -> Result<(), MaatError>;
    async fn get_background_run_by_handle(
        &self,
        user_id: &str,
        handle: &str,
    ) -> Result<Option<BackgroundRunRecord>, MaatError>;
    async fn list_background_runs(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<BackgroundRunRecord>, MaatError>;
    async fn allocate_background_run_handle(&self, title: &str) -> Result<String, MaatError>;
    async fn mark_compacted(&self, session_id: &str, before_ms: u64) -> Result<(), MaatError>;
    /// Mark the oldest `count` uncompacted messages in a session as compacted.
    async fn mark_compacted_count(&self, session_id: &str, count: usize) -> Result<(), MaatError>;
    async fn purge_session(&self, session_id: &str) -> Result<(), MaatError>;
}

// ─────────────────────────────────────────────
// Token estimator
// ─────────────────────────────────────────────

/// Lightweight heuristic: ~4 chars per token.
pub fn estimate_tokens(text: &str) -> u32 {
    ((text.len() as f32) / 4.0).ceil() as u32
}

// ─────────────────────────────────────────────
// ContextConfig
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// Max tokens to send to LLM (excluding output headroom).
    pub token_budget: u32,
    /// Trigger compaction when total history exceeds this.
    pub compaction_threshold: u32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            token_budget: 50_000,
            compaction_threshold: 40_000,
        }
    }
}

impl ContextConfig {
    pub fn new(token_budget: u32, compaction_threshold: u32) -> Self {
        Self { token_budget, compaction_threshold }
    }
}
