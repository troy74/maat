//! Context window builder.
//!
//! Builds the bounded message slice sent to the LLM, respecting a token budget.
//! Always includes the system prompt. Fills from newest message backwards.
//! ContextPointer summaries (Role::System with [CONTEXT SUMMARY] prefix) are
//! always included at low cost.

use maat_core::ChatMessage;

use crate::{estimate_tokens, ContextConfig};

/// Build a token-bounded context slice from history + pointers.
///
/// `system_prompt` — prepended as the first message.
/// `pointers`      — context summaries, inserted after system prompt, always included.
/// `history`       — conversation turns, trimmed from the oldest end.
///
/// Returns the final message list ready to pass to the LLM.
pub fn build_window(
    system_prompt: &str,
    pointers: &[ChatMessage],
    history: &[ChatMessage],
    config: &ContextConfig,
) -> Vec<ChatMessage> {
    let system_msg = ChatMessage::system(system_prompt);
    let system_tokens = estimate_tokens(system_prompt);

    // Pointers are always included — count their tokens.
    let pointer_tokens: u32 = pointers.iter().map(|m| estimate_tokens(&m.content)).sum();

    let available = config
        .token_budget
        .saturating_sub(system_tokens)
        .saturating_sub(pointer_tokens);

    // Walk history newest-first and collect until budget exhausted.
    let mut selected: Vec<&ChatMessage> = Vec::new();
    let mut used: u32 = 0;

    for msg in history.iter().rev() {
        let cost = estimate_tokens(&msg.content);
        if used + cost > available {
            break;
        }
        used += cost;
        selected.push(msg);
    }
    selected.reverse();

    let mut ctx = vec![system_msg];
    ctx.extend(pointers.iter().cloned());
    ctx.extend(selected.into_iter().cloned());
    ctx
}

/// Estimated total tokens for the entire history (used to trigger compaction).
pub fn total_history_tokens(history: &[ChatMessage]) -> u32 {
    history.iter().map(|m| estimate_tokens(&m.content)).sum()
}

/// How many messages from the END of history fit in the budget.
/// Messages before `history.len() - keep_count` are candidates for compaction.
pub fn window_keep_count(history: &[ChatMessage], config: &ContextConfig) -> usize {
    let mut used: u32 = 0;
    let mut keep = 0usize;
    for msg in history.iter().rev() {
        let cost = estimate_tokens(&msg.content);
        if used + cost > config.token_budget {
            break;
        }
        used += cost;
        keep += 1;
    }
    keep
}
