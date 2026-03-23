//! IMAP Talent — three tools for reading a mailbox.
//!
//! Tools exposed:
//!   `email_list`   — list the N most-recent messages in a folder (subject + from + date + uid)
//!   `email_read`   — fetch full body of a message by UID
//!   `email_search` — run an IMAP SEARCH query and return matching UIDs + subjects
//!
//! Config (env vars):
//!   IMAP_HOST     — e.g. imap.gmail.com
//!   IMAP_PORT     — default 993
//!   IMAP_USERNAME — full email address
//!   IMAP_PASSWORD — app password
//!
//! The underlying `imap` crate is synchronous; all calls run inside
//! `tokio::task::spawn_blocking` so the async runtime is not blocked.

use std::sync::Arc;

use async_trait::async_trait;
use mailparse::MailHeaderMap;
use maat_core::{LlmToolDef, MaatError, Tool, ToolRegistry};
use serde_json::{json, Value};

// ─────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl ImapConfig {
    /// Load config from env vars. Returns Err if any are missing.
    pub fn from_env() -> Result<Self, MaatError> {
        let host = std::env::var("IMAP_HOST")
            .map_err(|_| MaatError::Config("IMAP_HOST not set".into()))?;
        let port = std::env::var("IMAP_PORT")
            .unwrap_or_else(|_| "993".into())
            .parse::<u16>()
            .map_err(|_| MaatError::Config("IMAP_PORT must be a number".into()))?;
        let username = std::env::var("IMAP_USERNAME")
            .map_err(|_| MaatError::Config("IMAP_USERNAME not set".into()))?;
        let password = std::env::var("IMAP_PASSWORD")
            .map_err(|_| MaatError::Config("IMAP_PASSWORD not set".into()))?;
        Ok(Self { host, port, username, password })
    }
}

// ─────────────────────────────────────────────
// ImapTalent — holds three sub-tools
// ─────────────────────────────────────────────

/// A bundle that registers three IMAP tools into a ToolRegistry.
pub struct ImapTalent {
    config: ImapConfig,
}

impl ImapTalent {
    pub fn new(config: ImapConfig) -> Self {
        Self { config }
    }

    /// Register all three tools into the given registry.
    pub fn register_all(self, registry: &mut ToolRegistry) {
        let cfg = Arc::new(self.config);

        registry.register(Arc::new(EmailList { config: cfg.clone() }));
        registry.register(Arc::new(EmailRead { config: cfg.clone() }));
        registry.register(Arc::new(EmailSearch { config: cfg }));
    }
}

// ─────────────────────────────────────────────
// Shared: open TLS session
// ─────────────────────────────────────────────

fn connect(cfg: &ImapConfig) -> Result<imap::Session<native_tls::TlsStream<std::net::TcpStream>>, MaatError> {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| MaatError::Tool(format!("TLS init: {e}")))?;

    let client = imap::connect(
        format!("{}:{}", cfg.host, cfg.port),
        &cfg.host,
        &tls,
    )
    .map_err(|e| MaatError::Tool(format!("IMAP connect: {e}")))?;

    client
        .login(&cfg.username, &cfg.password)
        .map_err(|(e, _)| MaatError::Tool(format!("IMAP login: {e}")))
}

// ─────────────────────────────────────────────
// Tool 1 — email_list
// ─────────────────────────────────────────────

struct EmailList {
    config: Arc<ImapConfig>,
}

#[async_trait]
impl Tool for EmailList {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "email_list".into(),
            description: "List recent email messages. Returns subject, from, date and uid for each.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "folder": {
                        "type": "string",
                        "description": "Mailbox folder name (default: INBOX)"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of messages to return, most-recent first (default: 10, max: 50)"
                    }
                },
                "required": []
            }),
        }
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let folder = input["folder"].as_str().unwrap_or("INBOX").to_string();
        let count = input["count"].as_u64().unwrap_or(10).min(50) as u32;
        let cfg = self.config.clone();

        tokio::task::spawn_blocking(move || {
            let mut session = connect(&cfg)?;
            let mailbox = session
                .select(&folder)
                .map_err(|e| MaatError::Tool(format!("SELECT {folder}: {e}")))?;

            let total = mailbox.exists;
            if total == 0 {
                let _ = session.logout();
                return Ok(json!({ "messages": [] }));
            }

            let start = if total >= count { total - count + 1 } else { 1 };
            let range = format!("{start}:{total}");

            let messages = session
                .fetch(&range, "(UID ENVELOPE)")
                .map_err(|e| MaatError::Tool(format!("FETCH: {e}")))?;

            let mut rows: Vec<Value> = messages
                .iter()
                .rev()
                .map(|m| {
                    let uid = m.uid.map(|u| u.to_string()).unwrap_or_default();
                    let env = m.envelope();
                    let subject = env
                        .and_then(|e| e.subject.as_ref())
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .unwrap_or("(no subject)")
                        .to_string();
                    let from = env
                        .and_then(|e| e.from.as_ref())
                        .and_then(|v| v.first())
                        .map(|a| {
                            let name = a.name.as_ref()
                                .and_then(|n| std::str::from_utf8(n).ok())
                                .unwrap_or("");
                            let mailbox = a.mailbox.as_ref()
                                .and_then(|m| std::str::from_utf8(m).ok())
                                .unwrap_or("");
                            let host = a.host.as_ref()
                                .and_then(|h| std::str::from_utf8(h).ok())
                                .unwrap_or("");
                            if name.is_empty() {
                                format!("{mailbox}@{host}")
                            } else {
                                format!("{name} <{mailbox}@{host}>")
                            }
                        })
                        .unwrap_or_default();
                    let date = env
                        .and_then(|e| e.date.as_ref())
                        .and_then(|d| std::str::from_utf8(d).ok())
                        .unwrap_or("")
                        .to_string();

                    json!({ "uid": uid, "subject": subject, "from": from, "date": date })
                })
                .collect();

            rows.truncate(count as usize);
            let _ = session.logout();
            Ok(json!({ "folder": folder, "messages": rows }))
        })
        .await
        .map_err(|e| MaatError::Tool(format!("spawn_blocking: {e}")))?
    }
}

// ─────────────────────────────────────────────
// Tool 2 — email_read
// ─────────────────────────────────────────────

struct EmailRead {
    config: Arc<ImapConfig>,
}

#[async_trait]
impl Tool for EmailRead {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "email_read".into(),
            description: "Fetch the full body of an email by its UID. Returns subject, from, date and plain-text body.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "uid": {
                        "type": "string",
                        "description": "The message UID (obtained from email_list or email_search)"
                    },
                    "folder": {
                        "type": "string",
                        "description": "Mailbox folder (default: INBOX)"
                    }
                },
                "required": ["uid"]
            }),
        }
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let uid = input["uid"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("email_read: uid required".into()))?
            .to_string();
        let folder = input["folder"].as_str().unwrap_or("INBOX").to_string();
        let cfg = self.config.clone();

        tokio::task::spawn_blocking(move || {
            let mut session = connect(&cfg)?;
            session
                .select(&folder)
                .map_err(|e| MaatError::Tool(format!("SELECT {folder}: {e}")))?;

            let messages = session
                .uid_fetch(&uid, "RFC822")
                .map_err(|e| MaatError::Tool(format!("UID FETCH {uid}: {e}")))?;

            let msg = messages
                .iter()
                .next()
                .ok_or_else(|| MaatError::Tool(format!("UID {uid} not found")))?;

            let raw = msg
                .body()
                .ok_or_else(|| MaatError::Tool("empty body".into()))?;

            let parsed = mailparse::parse_mail(raw)
                .map_err(|e| MaatError::Tool(format!("parse mail: {e}")))?;

            let subject = parsed.headers.get_first_value("Subject").unwrap_or_default();
            let from = parsed.headers.get_first_value("From").unwrap_or_default();
            let date = parsed.headers.get_first_value("Date").unwrap_or_default();

            let body = extract_text_body(&parsed);

            let _ = session.logout();
            Ok(json!({ "uid": uid, "subject": subject, "from": from, "date": date, "body": body }))
        })
        .await
        .map_err(|e| MaatError::Tool(format!("spawn_blocking: {e}")))?
    }
}

/// Walk a parsed mail tree and return the first text/plain body, decoded.
fn extract_text_body(mail: &mailparse::ParsedMail) -> String {
    if mail.subparts.is_empty() {
        if mail.ctype.mimetype.to_lowercase() == "text/plain" {
            return mail.get_body().unwrap_or_default();
        }
        return String::new();
    }
    for part in &mail.subparts {
        let body = extract_text_body(part);
        if !body.is_empty() {
            return body;
        }
    }
    String::new()
}

// ─────────────────────────────────────────────
// Tool 3 — email_search
// ─────────────────────────────────────────────

struct EmailSearch {
    config: Arc<ImapConfig>,
}

#[async_trait]
impl Tool for EmailSearch {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "email_search".into(),
            description: "Search a mailbox using an IMAP SEARCH query. Examples: 'FROM alice@example.com', 'SUBJECT invoice', 'UNSEEN', 'SINCE 01-Jan-2025'. Returns matching UIDs and subjects.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "IMAP SEARCH criteria string"
                    },
                    "folder": {
                        "type": "string",
                        "description": "Mailbox folder (default: INBOX)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let query = input["query"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("email_search: query required".into()))?
            .to_string();
        let folder = input["folder"].as_str().unwrap_or("INBOX").to_string();
        let cfg = self.config.clone();

        tokio::task::spawn_blocking(move || {
            let mut session = connect(&cfg)?;
            session
                .select(&folder)
                .map_err(|e| MaatError::Tool(format!("SELECT {folder}: {e}")))?;

            let uids = session
                .uid_search(&query)
                .map_err(|e| MaatError::Tool(format!("SEARCH '{query}': {e}")))?;

            if uids.is_empty() {
                let _ = session.logout();
                return Ok(json!({ "query": query, "matches": [] }));
            }

            // Sort descending (most-recent first), cap at 50.
            let mut uid_vec: Vec<u32> = uids.into_iter().collect();
            uid_vec.sort_unstable_by(|a, b| b.cmp(a));
            uid_vec.truncate(50);
            let range = uid_vec.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");

            let messages = session
                .uid_fetch(&range, "(UID ENVELOPE)")
                .map_err(|e| MaatError::Tool(format!("UID FETCH envelopes: {e}")))?;

            let matches: Vec<Value> = messages
                .iter()
                .map(|m| {
                    let uid = m.uid.map(|u| u.to_string()).unwrap_or_default();
                    let subject = m
                        .envelope()
                        .and_then(|e| e.subject.as_ref())
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .unwrap_or("(no subject)")
                        .to_string();
                    json!({ "uid": uid, "subject": subject })
                })
                .collect();

            let _ = session.logout();
            Ok(json!({ "query": query, "folder": folder, "matches": matches }))
        })
        .await
        .map_err(|e| MaatError::Tool(format!("spawn_blocking: {e}")))?
    }
}
