//! maat-talents — compiled-in Talents (full-trust tools).
//!
//! Each Talent implements maat_core::Tool.
//! Register instances into a ToolRegistry and pass the Arc to VIZIER/MINION.

pub mod files;
pub mod google;
pub mod imap;
pub mod search;

pub use files::FileTalent;
pub use google::GoogleTalent;
pub use imap::ImapTalent;
pub use search::SearchTalent;
