//! maat-memory — session persistence and context window management.
//!
//! `MemoryStore` trait — implemented by `SqliteStore` (production) and
//! `InMemoryStore` (tests / no-persistence mode).
//!
//! `ContextWindow` — builds the bounded message slice sent to the LLM.

pub mod sqlite;
pub mod window;

use maat_core::{ChatMessage, MaatError, Role, SessionId};
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
// MemoryStore trait
// ─────────────────────────────────────────────

pub trait MemoryStore: Send + Sync {
    fn save_session_meta(&self, meta: &SessionMeta) -> Result<(), MaatError>;
    fn load_session_meta(&self, session_id: &str) -> Result<Option<SessionMeta>, MaatError>;
    fn save_message(&self, msg: &StoredMessage) -> Result<(), MaatError>;
    fn load_history(&self, session_id: &str) -> Result<Vec<StoredMessage>, MaatError>;
    fn save_context_pointer(&self, ptr: &ContextPointer) -> Result<(), MaatError>;
    fn load_context_pointers(&self, session_id: &str) -> Result<Vec<ContextPointer>, MaatError>;
    fn mark_compacted(&self, session_id: &str, before_ms: u64) -> Result<(), MaatError>;
    /// Mark the oldest `count` uncompacted messages in a session as compacted.
    fn mark_compacted_count(&self, session_id: &str, count: usize) -> Result<(), MaatError>;
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
