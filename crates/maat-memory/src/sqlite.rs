//! SQLite-backed MemoryStore.
//!
//! Uses `rusqlite` with WAL mode for concurrent reads.
//! All blocking DB calls are safe to wrap in `tokio::task::spawn_blocking`.

use std::path::Path;
use std::sync::Mutex;

use maat_core::MaatError;
use rusqlite::{params, Connection};

use crate::{ContextPointer, MemoryStore, SessionMeta, StoredMessage};

// ─────────────────────────────────────────────
// SqliteStore
// ─────────────────────────────────────────────

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(db_path: &Path) -> Result<Self, MaatError> {
        let conn = Connection::open(db_path)
            .map_err(|e| MaatError::Storage(format!("open DB: {e}")))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MaatError::Storage(format!("pragma: {e}")))?;

        conn.execute_batch(SCHEMA)
            .map_err(|e| MaatError::Storage(format!("schema: {e}")))?;

        Ok(Self { conn: Mutex::new(conn) })
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    session_id      TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL,
    name            TEXT NOT NULL,
    system_prompt   TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    last_active_ms  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id                TEXT PRIMARY KEY,
    session_id        TEXT NOT NULL,
    role              TEXT NOT NULL,
    content           TEXT NOT NULL,
    tool_call_id      TEXT,
    tool_calls_json   TEXT,
    estimated_tokens  INTEGER NOT NULL DEFAULT 0,
    created_at_ms     INTEGER NOT NULL,
    compacted         INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, created_at_ms);

CREATE TABLE IF NOT EXISTS context_pointers (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    summary         TEXT NOT NULL,
    covers_from_ms  INTEGER NOT NULL,
    covers_to_ms    INTEGER NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);
CREATE INDEX IF NOT EXISTS idx_pointers_session ON context_pointers(session_id, created_at_ms);
";

// ─────────────────────────────────────────────
// MemoryStore implementation
// ─────────────────────────────────────────────

impl MemoryStore for SqliteStore {
    fn save_session_meta(&self, meta: &SessionMeta) -> Result<(), MaatError> {
        let conn = lock(&self.conn)?;
        conn.execute(
            "INSERT INTO sessions (session_id, user_id, name, system_prompt, created_at_ms, last_active_ms)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(session_id) DO UPDATE SET
               last_active_ms = excluded.last_active_ms,
               system_prompt  = excluded.system_prompt",
            params![
                meta.session_id, meta.user_id, meta.name,
                meta.system_prompt, meta.created_at_ms, meta.last_active_ms
            ],
        )
        .map_err(|e| MaatError::Storage(format!("save_session_meta: {e}")))?;
        Ok(())
    }

    fn load_session_meta(&self, session_id: &str) -> Result<Option<SessionMeta>, MaatError> {
        let conn = lock(&self.conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, user_id, name, system_prompt, created_at_ms, last_active_ms
                 FROM sessions WHERE session_id = ?1",
            )
            .map_err(|e| MaatError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query_map(params![session_id], |row| {
                Ok(SessionMeta {
                    session_id:    row.get(0)?,
                    user_id:       row.get(1)?,
                    name:          row.get(2)?,
                    system_prompt: row.get(3)?,
                    created_at_ms: row.get(4)?,
                    last_active_ms: row.get(5)?,
                })
            })
            .map_err(|e| MaatError::Storage(e.to_string()))?;

        match rows.next() {
            Some(Ok(meta)) => Ok(Some(meta)),
            Some(Err(e))   => Err(MaatError::Storage(e.to_string())),
            None           => Ok(None),
        }
    }

    fn save_message(&self, msg: &StoredMessage) -> Result<(), MaatError> {
        let conn = lock(&self.conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO messages
             (id, session_id, role, content, tool_call_id, tool_calls_json, estimated_tokens, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                msg.id, msg.session_id, msg.role, msg.content,
                msg.tool_call_id, msg.tool_calls_json,
                msg.estimated_tokens, msg.created_at_ms
            ],
        )
        .map_err(|e| MaatError::Storage(format!("save_message: {e}")))?;
        Ok(())
    }

    fn load_history(&self, session_id: &str) -> Result<Vec<StoredMessage>, MaatError> {
        let conn = lock(&self.conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, role, content, tool_call_id, tool_calls_json,
                        estimated_tokens, created_at_ms
                 FROM messages
                 WHERE session_id = ?1 AND compacted = 0
                 ORDER BY created_at_ms ASC",
            )
            .map_err(|e| MaatError::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(StoredMessage {
                    id:               row.get(0)?,
                    session_id:       row.get(1)?,
                    role:             row.get(2)?,
                    content:          row.get(3)?,
                    tool_call_id:     row.get(4)?,
                    tool_calls_json:  row.get(5)?,
                    estimated_tokens: row.get(6)?,
                    created_at_ms:    row.get(7)?,
                })
            })
            .map_err(|e| MaatError::Storage(e.to_string()))?;

        rows.map(|r| r.map_err(|e| MaatError::Storage(e.to_string())))
            .collect()
    }

    fn save_context_pointer(&self, ptr: &ContextPointer) -> Result<(), MaatError> {
        let conn = lock(&self.conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO context_pointers
             (id, session_id, summary, covers_from_ms, covers_to_ms, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                ptr.id, ptr.session_id, ptr.summary,
                ptr.covers_from_ms, ptr.covers_to_ms, ptr.created_at_ms
            ],
        )
        .map_err(|e| MaatError::Storage(format!("save_context_pointer: {e}")))?;
        Ok(())
    }

    fn load_context_pointers(&self, session_id: &str) -> Result<Vec<ContextPointer>, MaatError> {
        let conn = lock(&self.conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, summary, covers_from_ms, covers_to_ms, created_at_ms
                 FROM context_pointers
                 WHERE session_id = ?1
                 ORDER BY created_at_ms ASC",
            )
            .map_err(|e| MaatError::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(ContextPointer {
                    id:             row.get(0)?,
                    session_id:     row.get(1)?,
                    summary:        row.get(2)?,
                    covers_from_ms: row.get(3)?,
                    covers_to_ms:   row.get(4)?,
                    created_at_ms:  row.get(5)?,
                })
            })
            .map_err(|e| MaatError::Storage(e.to_string()))?;

        rows.map(|r| r.map_err(|e| MaatError::Storage(e.to_string())))
            .collect()
    }

    fn mark_compacted(&self, session_id: &str, before_ms: u64) -> Result<(), MaatError> {
        let conn = lock(&self.conn)?;
        conn.execute(
            "UPDATE messages SET compacted = 1
             WHERE session_id = ?1 AND created_at_ms < ?2 AND compacted = 0",
            params![session_id, before_ms],
        )
        .map_err(|e| MaatError::Storage(format!("mark_compacted: {e}")))?;
        Ok(())
    }

    fn mark_compacted_count(&self, session_id: &str, count: usize) -> Result<(), MaatError> {
        let conn = lock(&self.conn)?;
        conn.execute(
            "UPDATE messages SET compacted = 1
             WHERE id IN (
                 SELECT id FROM messages
                 WHERE session_id = ?1 AND compacted = 0
                 ORDER BY created_at_ms ASC
                 LIMIT ?2
             )",
            params![session_id, count as i64],
        )
        .map_err(|e| MaatError::Storage(format!("mark_compacted_count: {e}")))?;
        Ok(())
    }
}

fn lock(m: &Mutex<Connection>) -> Result<std::sync::MutexGuard<'_, Connection>, MaatError> {
    m.lock().map_err(|e| MaatError::Storage(format!("DB lock poisoned: {e}")))
}
