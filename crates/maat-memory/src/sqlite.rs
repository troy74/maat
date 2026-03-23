//! SQLite-backed MemoryStore.
//!
//! Uses `rusqlite` with WAL mode for concurrent reads.
//! Blocking DB work is pushed onto `spawn_blocking` so actor turns do not hold
//! up the async runtime.

use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use maat_core::MaatError;
use rusqlite::{params, Connection};
use tokio::task;

use crate::{ArtifactRecord, ContextPointer, MemoryStore, SessionMeta, StoredMessage};

pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    artifact_root: PathBuf,
}

impl SqliteStore {
    pub fn open(db_path: &Path) -> Result<Self, MaatError> {
        let conn = Connection::open(db_path)
            .map_err(|e| MaatError::Storage(format!("open DB: {e}")))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MaatError::Storage(format!("pragma: {e}")))?;

        conn.execute_batch(SCHEMA)
            .map_err(|e| MaatError::Storage(format!("schema: {e}")))?;

        let artifact_root = db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("artifacts");
        std::fs::create_dir_all(&artifact_root)
            .map_err(|e| MaatError::Storage(format!("artifact root: {e}")))?;

        Ok(Self { conn: Arc::new(Mutex::new(conn)), artifact_root })
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

CREATE TABLE IF NOT EXISTS artifacts (
    artifact_id      TEXT PRIMARY KEY,
    handle           TEXT NOT NULL UNIQUE,
    user_id          TEXT NOT NULL,
    session_id       TEXT NOT NULL,
    kind             TEXT NOT NULL,
    mime_type        TEXT NOT NULL,
    display_name     TEXT NOT NULL,
    storage_path     TEXT NOT NULL,
    byte_size        INTEGER NOT NULL,
    source           TEXT NOT NULL,
    summary          TEXT NOT NULL,
    metadata_json    TEXT NOT NULL,
    analysis_json    TEXT NOT NULL,
    created_at_ms    INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);
CREATE INDEX IF NOT EXISTS idx_artifacts_user_created ON artifacts(user_id, created_at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_artifacts_session_created ON artifacts(session_id, created_at_ms DESC);
";

#[async_trait]
impl MemoryStore for SqliteStore {
    async fn save_session_meta(&self, meta: &SessionMeta) -> Result<(), MaatError> {
        let conn = self.conn.clone();
        let meta = meta.clone();
        run_db(move || {
            let conn = lock(&conn)?;
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
        })
        .await
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
                    session_id: row.get(0)?,
                    user_id: row.get(1)?,
                    name: row.get(2)?,
                    system_prompt: row.get(3)?,
                    created_at_ms: row.get(4)?,
                    last_active_ms: row.get(5)?,
                })
            })
            .map_err(|e| MaatError::Storage(e.to_string()))?;

        match rows.next() {
            Some(Ok(meta)) => Ok(Some(meta)),
            Some(Err(e)) => Err(MaatError::Storage(e.to_string())),
            None => Ok(None),
        }
    }

    async fn load_session_meta_by_user_and_name(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<SessionMeta>, MaatError> {
        let conn = self.conn.clone();
        let user_id = user_id.to_string();
        let name = name.to_string();
        run_db(move || {
            let conn = lock(&conn)?;
            let mut stmt = conn
                .prepare(
                    "SELECT session_id, user_id, name, system_prompt, created_at_ms, last_active_ms
                     FROM sessions
                     WHERE user_id = ?1 AND name = ?2
                     ORDER BY last_active_ms DESC
                     LIMIT 1",
                )
                .map_err(|e| MaatError::Storage(e.to_string()))?;

            let mut rows = stmt
                .query_map(params![user_id, name], |row| {
                    Ok(SessionMeta {
                        session_id: row.get(0)?,
                        user_id: row.get(1)?,
                        name: row.get(2)?,
                        system_prompt: row.get(3)?,
                        created_at_ms: row.get(4)?,
                        last_active_ms: row.get(5)?,
                    })
                })
                .map_err(|e| MaatError::Storage(e.to_string()))?;

            match rows.next() {
                Some(Ok(meta)) => Ok(Some(meta)),
                Some(Err(e)) => Err(MaatError::Storage(e.to_string())),
                None => Ok(None),
            }
        })
        .await
    }

    async fn save_message(&self, msg: &StoredMessage) -> Result<(), MaatError> {
        let conn = self.conn.clone();
        let msg = msg.clone();
        run_db(move || {
            let conn = lock(&conn)?;
            conn.execute(
                "INSERT OR IGNORE INTO messages
                 (id, session_id, role, content, tool_call_id, tool_calls_json, estimated_tokens, created_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    msg.id, msg.session_id, msg.role, msg.content,
                    msg.tool_call_id, msg.tool_calls_json, msg.estimated_tokens, msg.created_at_ms
                ],
            )
            .map_err(|e| MaatError::Storage(format!("save_message: {e}")))?;
            Ok(())
        })
        .await
    }

    async fn load_history(&self, session_id: &str) -> Result<Vec<StoredMessage>, MaatError> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        run_db(move || {
            let conn = lock(&conn)?;
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
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        role: row.get(2)?,
                        content: row.get(3)?,
                        tool_call_id: row.get(4)?,
                        tool_calls_json: row.get(5)?,
                        estimated_tokens: row.get(6)?,
                        created_at_ms: row.get(7)?,
                    })
                })
                .map_err(|e| MaatError::Storage(e.to_string()))?;

            rows.map(|r| r.map_err(|e| MaatError::Storage(e.to_string())))
                .collect()
        })
        .await
    }

    async fn save_context_pointer(&self, ptr: &ContextPointer) -> Result<(), MaatError> {
        let conn = self.conn.clone();
        let ptr = ptr.clone();
        run_db(move || {
            let conn = lock(&conn)?;
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
        })
        .await
    }

    async fn load_context_pointers(&self, session_id: &str) -> Result<Vec<ContextPointer>, MaatError> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        run_db(move || {
            let conn = lock(&conn)?;
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
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        summary: row.get(2)?,
                        covers_from_ms: row.get(3)?,
                        covers_to_ms: row.get(4)?,
                        created_at_ms: row.get(5)?,
                    })
                })
                .map_err(|e| MaatError::Storage(e.to_string()))?;

            rows.map(|r| r.map_err(|e| MaatError::Storage(e.to_string())))
                .collect()
        })
        .await
    }

    async fn import_artifact(
        &self,
        user_id: &str,
        session_id: &str,
        source_path: &Path,
    ) -> Result<ArtifactRecord, MaatError> {
        let conn = self.conn.clone();
        let artifact_root = self.artifact_root.clone();
        let user_id = user_id.to_string();
        let session_id = session_id.to_string();
        let source_path = source_path.to_path_buf();
        run_db(move || {
            let canonical = source_path
                .canonicalize()
                .map_err(|e| MaatError::Storage(format!("artifact source canonicalise: {e}")))?;
            if !canonical.exists() {
                return Err(MaatError::Storage(format!(
                    "artifact source not found: {}",
                    source_path.display()
                )));
            }
            if canonical.is_dir() {
                return Err(MaatError::Storage(format!(
                    "artifact source is a directory: {}",
                    source_path.display()
                )));
            }

            let bytes = std::fs::read(&canonical)
                .map_err(|e| MaatError::Storage(format!("artifact source read: {e}")))?;
            let metadata = std::fs::metadata(&canonical)
                .map_err(|e| MaatError::Storage(format!("artifact source metadata: {e}")))?;

            let artifact_id = ulid::Ulid::new().to_string();
            let created_at_ms = maat_core::now_ms();
            let file_name = canonical
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("artifact.bin")
                .to_string();
            let kind = infer_artifact_kind(&file_name);
            let mime_type = infer_mime_type(&file_name).to_string();
            let handle = {
                let conn = lock(&conn)?;
                next_artifact_handle(&conn, &kind, &file_name)?
            };
            let storage_dir = artifact_root.join(&artifact_id);
            std::fs::create_dir_all(&storage_dir)
                .map_err(|e| MaatError::Storage(format!("artifact directory: {e}")))?;
            let stored_name = sanitize_file_name(&file_name);
            let stored_path = storage_dir.join(&stored_name);
            std::fs::write(&stored_path, &bytes)
                .map_err(|e| MaatError::Storage(format!("artifact write: {e}")))?;

            let summary = format!("{} artifact imported from {}", kind, file_name);
            let metadata_json = serde_json::json!({
                "encoding": "json-v1",
                "original_path": canonical.display().to_string(),
                "original_file_name": file_name,
            })
            .to_string();
            let analysis_json = serde_json::json!({
                "encoding": "json-v1",
                "status": "pending",
            })
            .to_string();
            let record = ArtifactRecord {
                artifact_id: artifact_id.clone(),
                handle: handle.clone(),
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                kind: kind.clone(),
                mime_type: mime_type.clone(),
                display_name: file_name.clone(),
                storage_path: stored_path.display().to_string(),
                byte_size: metadata.len(),
                source: "imported".into(),
                summary,
                metadata_json,
                analysis_json,
                created_at_ms,
            };

            let conn = lock(&conn)?;
            conn.execute(
                "INSERT INTO artifacts
                 (artifact_id, handle, user_id, session_id, kind, mime_type, display_name, storage_path, byte_size, source, summary, metadata_json, analysis_json, created_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    &record.artifact_id,
                    &record.handle,
                    &record.user_id,
                    &record.session_id,
                    &record.kind,
                    &record.mime_type,
                    &record.display_name,
                    &record.storage_path,
                    record.byte_size as i64,
                    &record.source,
                    &record.summary,
                    &record.metadata_json,
                    &record.analysis_json,
                    record.created_at_ms as i64,
                ],
            )
            .map_err(|e| MaatError::Storage(format!("save artifact: {e}")))?;

            Ok(record)
        })
        .await
    }

    async fn list_artifacts(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<ArtifactRecord>, MaatError> {
        let conn = self.conn.clone();
        let user_id = user_id.to_string();
        run_db(move || {
            let conn = lock(&conn)?;
            let mut stmt = conn
                .prepare(
                    "SELECT artifact_id, handle, user_id, session_id, kind, mime_type, display_name, storage_path, byte_size, source, summary, metadata_json, analysis_json, created_at_ms
                     FROM artifacts
                     WHERE user_id = ?1
                     ORDER BY created_at_ms DESC
                     LIMIT ?2",
                )
                .map_err(|e| MaatError::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(params![user_id, limit as i64], row_to_artifact)
                .map_err(|e| MaatError::Storage(e.to_string()))?;
            rows.map(|r| r.map_err(|e| MaatError::Storage(e.to_string())))
                .collect()
        })
        .await
    }

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
    ) -> Result<ArtifactRecord, MaatError> {
        let conn = self.conn.clone();
        let artifact_root = self.artifact_root.clone();
        let user_id = user_id.to_string();
        let session_id = session_id.to_string();
        let display_name = display_name.to_string();
        let kind = kind.to_string();
        let mime_type = mime_type.to_string();
        let source = source.to_string();
        let summary = summary.to_string();
        let metadata_json = metadata_json.to_string();
        let analysis_json = analysis_json.to_string();
        let bytes = bytes.to_vec();
        run_db(move || {
            let artifact_id = ulid::Ulid::new().to_string();
            let created_at_ms = maat_core::now_ms();
            let handle = {
                let conn = lock(&conn)?;
                next_artifact_handle(&conn, &kind, &display_name)?
            };
            let storage_dir = artifact_root.join(&artifact_id);
            std::fs::create_dir_all(&storage_dir)
                .map_err(|e| MaatError::Storage(format!("artifact directory: {e}")))?;
            let stored_name = sanitize_file_name(&display_name);
            let stored_path = storage_dir.join(&stored_name);
            std::fs::write(&stored_path, &bytes)
                .map_err(|e| MaatError::Storage(format!("artifact write: {e}")))?;

            let record = ArtifactRecord {
                artifact_id: artifact_id.clone(),
                handle: handle.clone(),
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                kind: kind.clone(),
                mime_type: mime_type.clone(),
                display_name: display_name.clone(),
                storage_path: stored_path.display().to_string(),
                byte_size: bytes.len() as u64,
                source: source.clone(),
                summary: summary.clone(),
                metadata_json: metadata_json.clone(),
                analysis_json: analysis_json.clone(),
                created_at_ms,
            };

            let conn = lock(&conn)?;
            conn.execute(
                "INSERT INTO artifacts
                 (artifact_id, handle, user_id, session_id, kind, mime_type, display_name, storage_path, byte_size, source, summary, metadata_json, analysis_json, created_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    &record.artifact_id,
                    &record.handle,
                    &record.user_id,
                    &record.session_id,
                    &record.kind,
                    &record.mime_type,
                    &record.display_name,
                    &record.storage_path,
                    record.byte_size as i64,
                    &record.source,
                    &record.summary,
                    &record.metadata_json,
                    &record.analysis_json,
                    record.created_at_ms as i64,
                ],
            )
            .map_err(|e| MaatError::Storage(format!("save artifact: {e}")))?;

            Ok(record)
        })
        .await
    }

    async fn get_artifact_by_handle(
        &self,
        user_id: &str,
        handle: &str,
    ) -> Result<Option<ArtifactRecord>, MaatError> {
        let conn = self.conn.clone();
        let user_id = user_id.to_string();
        let handle = handle.to_string();
        run_db(move || {
            let conn = lock(&conn)?;
            let mut stmt = conn
                .prepare(
                    "SELECT artifact_id, handle, user_id, session_id, kind, mime_type, display_name, storage_path, byte_size, source, summary, metadata_json, analysis_json, created_at_ms
                     FROM artifacts
                     WHERE user_id = ?1 AND handle = ?2
                     LIMIT 1",
                )
                .map_err(|e| MaatError::Storage(e.to_string()))?;
            let mut rows = stmt
                .query_map(params![user_id, handle], row_to_artifact)
                .map_err(|e| MaatError::Storage(e.to_string()))?;
            match rows.next() {
                Some(Ok(record)) => Ok(Some(record)),
                Some(Err(e)) => Err(MaatError::Storage(e.to_string())),
                None => Ok(None),
            }
        })
        .await
    }

    async fn latest_session_artifact(
        &self,
        session_id: &str,
    ) -> Result<Option<ArtifactRecord>, MaatError> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        run_db(move || {
            let conn = lock(&conn)?;
            let mut stmt = conn
                .prepare(
                    "SELECT artifact_id, handle, user_id, session_id, kind, mime_type, display_name, storage_path, byte_size, source, summary, metadata_json, analysis_json, created_at_ms
                     FROM artifacts
                     WHERE session_id = ?1
                     ORDER BY created_at_ms DESC
                     LIMIT 1",
                )
                .map_err(|e| MaatError::Storage(e.to_string()))?;
            let mut rows = stmt
                .query_map(params![session_id], row_to_artifact)
                .map_err(|e| MaatError::Storage(e.to_string()))?;
            match rows.next() {
                Some(Ok(record)) => Ok(Some(record)),
                Some(Err(e)) => Err(MaatError::Storage(e.to_string())),
                None => Ok(None),
            }
        })
        .await
    }

    async fn mark_compacted(&self, session_id: &str, before_ms: u64) -> Result<(), MaatError> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        run_db(move || {
            let conn = lock(&conn)?;
            conn.execute(
                "UPDATE messages SET compacted = 1
                 WHERE session_id = ?1 AND created_at_ms < ?2 AND compacted = 0",
                params![session_id, before_ms],
            )
            .map_err(|e| MaatError::Storage(format!("mark_compacted: {e}")))?;
            Ok(())
        })
        .await
    }

    async fn mark_compacted_count(&self, session_id: &str, count: usize) -> Result<(), MaatError> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        run_db(move || {
            let conn = lock(&conn)?;
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
        })
        .await
    }

    async fn purge_session(&self, session_id: &str) -> Result<(), MaatError> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        run_db(move || {
            let conn = lock(&conn)?;
            conn.execute(
                "DELETE FROM messages WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| MaatError::Storage(format!("purge_session messages: {e}")))?;
            conn.execute(
                "DELETE FROM context_pointers WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| MaatError::Storage(format!("purge_session pointers: {e}")))?;
            Ok(())
        })
        .await
    }
}

fn row_to_artifact(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRecord> {
    Ok(ArtifactRecord {
        artifact_id: row.get(0)?,
        handle: row.get(1)?,
        user_id: row.get(2)?,
        session_id: row.get(3)?,
        kind: row.get(4)?,
        mime_type: row.get(5)?,
        display_name: row.get(6)?,
        storage_path: row.get(7)?,
        byte_size: row.get::<_, i64>(8)? as u64,
        source: row.get(9)?,
        summary: row.get(10)?,
        metadata_json: row.get(11)?,
        analysis_json: row.get(12)?,
        created_at_ms: row.get::<_, i64>(13)? as u64,
    })
}

fn infer_artifact_kind(file_name: &str) -> String {
    match file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => "image".into(),
        "pdf" => "pdf".into(),
        "md" | "txt" | "json" => "document".into(),
        _ => "file".into(),
    }
}

fn infer_mime_type(file_name: &str) -> &'static str {
    match file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "md" => "text/markdown",
        "txt" => "text/plain",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

fn next_artifact_handle(conn: &Connection, kind: &str, file_name: &str) -> Result<String, MaatError> {
    let words = artifact_words(kind, file_name);
    for suffix_index in 0..256u32 {
        let suffix = short_code(suffix_index);
        let candidate = format!("{}-{}-{suffix}", words.0, words.1);
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM artifacts WHERE handle = ?1",
                params![candidate],
                |row| row.get(0),
            )
            .map_err(|e| MaatError::Storage(format!("artifact handle lookup: {e}")))?;
        if exists == 0 {
            return Ok(candidate);
        }
    }
    Err(MaatError::Storage("failed to allocate unique artifact handle".into()))
}

fn artifact_words(kind: &str, file_name: &str) -> (String, String) {
    let tokens = file_name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 3)
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let first = tokens
        .first()
        .cloned()
        .unwrap_or_else(|| default_first_word(kind).to_string());
    let second = tokens
        .iter()
        .skip(1)
        .find(|token| *token != &first)
        .cloned()
        .unwrap_or_else(|| default_second_word(kind).to_string());
    (sanitize_slug(&first), sanitize_slug(&second))
}

fn default_first_word(kind: &str) -> &'static str {
    match kind {
        "image" => "bright",
        "pdf" => "paper",
        "document" => "noted",
        _ => "stored",
    }
}

fn default_second_word(kind: &str) -> &'static str {
    match kind {
        "image" => "canvas",
        "pdf" => "ledger",
        "document" => "brief",
        _ => "record",
    }
}

fn sanitize_slug(value: &str) -> String {
    let cleaned = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect::<String>();
    if cleaned.len() >= 3 {
        cleaned
    } else {
        format!("{cleaned}item")
    }
}

fn sanitize_file_name(value: &str) -> String {
    value.chars()
        .map(|c| match c {
            '/' | '\\' => '_',
            _ => c,
        })
        .collect()
}

fn short_code(index: u32) -> String {
    const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstvwxyz";
    let mut n = (maat_core::now_ms() as u32).wrapping_add(index);
    let mut out = String::new();
    for _ in 0..4 {
        out.push(ALPHABET[(n % ALPHABET.len() as u32) as usize] as char);
        n /= ALPHABET.len() as u32;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn imports_and_lists_artifacts_with_readable_handles() {
        let root = std::env::temp_dir().join(format!("maat-artifacts-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("test.db");
        let source_path = root.join("memory-notes.pdf");
        std::fs::write(&source_path, b"fake-pdf").unwrap();

        let store = SqliteStore::open(&db_path).unwrap();
        store
            .save_session_meta(&SessionMeta {
                session_id: "session".into(),
                user_id: "user".into(),
                name: "primary".into(),
                system_prompt: "test".into(),
                created_at_ms: maat_core::now_ms(),
                last_active_ms: maat_core::now_ms(),
            })
            .await
            .unwrap();
        let artifact = store
            .import_artifact("user", "session", &source_path)
            .await
            .unwrap();

        assert!(artifact.handle.starts_with("memory-notes-"));
        assert!(Path::new(&artifact.storage_path).exists());

        let listed = store.list_artifacts("user", 10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].handle, artifact.handle);

        let fetched = store
            .get_artifact_by_handle("user", &artifact.handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.display_name, "memory-notes.pdf");

        let _ = std::fs::remove_dir_all(root);
    }
}

async fn run_db<T, F>(f: F) -> Result<T, MaatError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, MaatError> + Send + 'static,
{
    task::spawn_blocking(f)
        .await
        .map_err(|e| MaatError::Storage(format!("DB task join: {e}")))?
}

fn lock(m: &Arc<Mutex<Connection>>) -> Result<std::sync::MutexGuard<'_, Connection>, MaatError> {
    m.lock().map_err(|e| MaatError::Storage(format!("DB lock poisoned: {e}")))
}
