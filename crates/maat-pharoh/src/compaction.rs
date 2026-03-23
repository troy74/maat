//! Context compaction — summarise old messages into a ContextPointer.
//!
//! Called from Pharoh and NamedSession when total history tokens exceed
//! the compaction threshold. Makes a single cheap LLM call, persists the
//! summary, marks the source messages as compacted in SQLite, and returns
//! the new ContextPointer to insert into the in-memory pointer cache.

use maat_core::{ChatMessage, MaatError, Role};
use maat_llm::LlmClient;
use maat_memory::{ContextPointer, MemoryStore};
use tracing::{info, warn};

/// Compact `messages` into a single `ContextPointer`.
///
/// On success the caller should:
///   1. `self.history.drain(..compact_count)`
///   2. `self.pointer_cache.push(ptr.to_chat())`
pub async fn compact(
    messages: &[ChatMessage],
    session_id: &str,
    llm: &dyn LlmClient,
    store: &dyn MemoryStore,
) -> Result<ContextPointer, MaatError> {
    if messages.is_empty() {
        return Err(MaatError::Llm("compact: no messages to summarise".into()));
    }

    info!(session = %session_id, count = messages.len(), "compacting history");

    // Build a plain-text transcript for the summariser.
    let transcript: String = messages
        .iter()
        .filter_map(|m| match m.role {
            Role::User      => Some(format!("User: {}", m.content)),
            Role::Assistant => Some(format!("Assistant: {}", m.content)),
            _               => None, // skip tool / system turns
        })
        .collect::<Vec<_>>()
        .join("\n");

    let summary_messages = vec![
        ChatMessage::system(
            "You are a concise summariser. \
             Summarise the conversation below, preserving key facts, decisions, \
             user preferences, and any important context. Be brief — aim for \
             3-6 sentences. Do not editorialize.",
        ),
        ChatMessage::user(transcript),
    ];

    let resp = llm.complete(summary_messages, &[]).await?;
    let summary = resp.content.trim().to_string();

    let now = maat_core::now_ms();
    let ptr = ContextPointer {
        id: ulid::Ulid::new().to_string(),
        session_id: session_id.to_string(),
        summary: summary.clone(),
        covers_from_ms: 0,
        covers_to_ms: now,
        created_at_ms: now,
    };

    store.save_context_pointer(&ptr)?;
    if let Err(e) = store.mark_compacted_count(session_id, messages.len()) {
        warn!(session = %session_id, error = %e, "failed to mark messages compacted in DB");
    }

    info!(session = %session_id, summary_len = summary.len(), "compaction complete");
    Ok(ptr)
}
