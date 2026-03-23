//! File system tools — read and write local files.
//!
//! Safety model:
//!   - Paths are resolved relative to `base_dir` (defaults to cwd).
//!   - `..` traversal is blocked — the resolved path must remain under base_dir.
//!   - file_read caps output at 200 KB to avoid flooding the context window.
//!   - file_write creates parent directories if they don't exist.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use maat_core::{LlmToolDef, MaatError, Tool, ToolRegistry};
use serde_json::{json, Value};
use tracing::debug;

const MAX_READ_BYTES: u64 = 200 * 1024; // 200 KB

// ─────────────────────────────────────────────
// FileTalent
// ─────────────────────────────────────────────

pub struct FileTalent {
    base_dir: PathBuf,
}

impl FileTalent {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn register_all(&self, registry: &mut ToolRegistry) {
        registry.register(Arc::new(FileRead { base_dir: self.base_dir.clone() }));
        registry.register(Arc::new(FileWrite { base_dir: self.base_dir.clone() }));
        registry.register(Arc::new(FileList { base_dir: self.base_dir.clone() }));
    }
}

// ─────────────────────────────────────────────
// Shared guard
// ─────────────────────────────────────────────

/// Resolve `user_path` relative to `base_dir`, rejecting any path that
/// escapes the base directory via `..` or symlinks.
fn safe_path(base_dir: &Path, user_path: &str) -> Result<PathBuf, MaatError> {
    // Strip leading / so the path is always treated as relative.
    let stripped = user_path.trim_start_matches('/');
    let candidate = base_dir.join(stripped);

    // Canonicalise the base (must exist).
    let canon_base = base_dir
        .canonicalize()
        .map_err(|e| MaatError::Tool(format!("base_dir canonicalise: {e}")))?;

    // For files that don't exist yet (write) we canonicalise the parent.
    let canon_candidate = if candidate.exists() {
        candidate
            .canonicalize()
            .map_err(|e| MaatError::Tool(format!("path canonicalise: {e}")))?
    } else {
        // Parent must exist and be safe.
        let parent = candidate
            .parent()
            .ok_or_else(|| MaatError::Tool("path has no parent".into()))?;
        let canon_parent = parent
            .canonicalize()
            .map_err(|e| MaatError::Tool(format!("parent canonicalise: {e}")))?;
        if !canon_parent.starts_with(&canon_base) {
            return Err(MaatError::Tool(format!(
                "path '{}' escapes the allowed directory",
                user_path
            )));
        }
        candidate
    };

    if !canon_candidate.starts_with(&canon_base) {
        return Err(MaatError::Tool(format!(
            "path '{}' escapes the allowed directory",
            user_path
        )));
    }

    Ok(canon_candidate)
}

// ─────────────────────────────────────────────
// FileRead
// ─────────────────────────────────────────────

pub struct FileRead {
    base_dir: PathBuf,
}

#[async_trait]
impl Tool for FileRead {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "file_read".into(),
            description: "Read the contents of a local file. Paths are relative to the working directory. Use when the user asks you to look at, summarise, or analyse a file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to the file, e.g. 'notes.txt' or 'src/main.rs'"
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "First line to read (1-based, default 1)"
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Last line to read inclusive (default: read to end)"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let user_path = input["path"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'path'".into()))?;

        let abs_path = safe_path(&self.base_dir, user_path)?;
        debug!(path = %abs_path.display(), "file_read");

        if !abs_path.exists() {
            return Err(MaatError::Tool(format!("file not found: {user_path}")));
        }
        if abs_path.is_dir() {
            return Err(MaatError::Tool(format!("'{user_path}' is a directory, use file_list to browse")));
        }

        let meta = std::fs::metadata(&abs_path)
            .map_err(|e| MaatError::Tool(format!("stat: {e}")))?;
        if meta.len() > MAX_READ_BYTES {
            return Err(MaatError::Tool(format!(
                "file is {:.1} KB — too large to read in full (limit 200 KB). Use start_line/end_line to read a section.",
                meta.len() as f64 / 1024.0
            )));
        }

        let content = std::fs::read_to_string(&abs_path)
            .map_err(|e| MaatError::Tool(format!("read: {e}")))?;

        let start_line = input["start_line"].as_u64().unwrap_or(1).max(1) as usize;
        let end_line = input["end_line"].as_u64().map(|n| n as usize);

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let from = (start_line - 1).min(total_lines);
        let to = end_line.map(|e| e.min(total_lines)).unwrap_or(total_lines);
        let slice = lines[from..to].join("\n");

        Ok(json!({
            "path": user_path,
            "total_lines": total_lines,
            "returned_lines": to - from,
            "start_line": from + 1,
            "end_line": to,
            "content": slice
        }))
    }
}

// ─────────────────────────────────────────────
// FileWrite
// ─────────────────────────────────────────────

pub struct FileWrite {
    base_dir: PathBuf,
}

#[async_trait]
impl Tool for FileWrite {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "file_write".into(),
            description: "Write or overwrite a local file with new content. Paths are relative to the working directory. Creates parent directories if needed. Use when the user asks you to save, create, or update a file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to write, e.g. 'output/report.md'"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full content to write to the file"
                    },
                    "append": {
                        "type": "boolean",
                        "description": "If true, append to the file instead of overwriting (default false)"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let user_path = input["path"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'path'".into()))?;
        let content = input["content"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'content'".into()))?;
        let append = input["append"].as_bool().unwrap_or(false);

        // For write we need the parent to be safe, not necessarily the file itself.
        let stripped = user_path.trim_start_matches('/');
        let candidate = self.base_dir.join(stripped);

        let canon_base = self.base_dir
            .canonicalize()
            .map_err(|e| MaatError::Tool(format!("base_dir: {e}")))?;

        // Ensure parent directory is safe and exists (create if needed).
        let parent = candidate
            .parent()
            .ok_or_else(|| MaatError::Tool("path has no parent".into()))?;

        if parent != Path::new("") {
            std::fs::create_dir_all(parent)
                .map_err(|e| MaatError::Tool(format!("mkdir: {e}")))?;
        }

        let canon_parent = parent
            .canonicalize()
            .map_err(|e| MaatError::Tool(format!("parent canonicalise: {e}")))?;

        if !canon_parent.starts_with(&canon_base) {
            return Err(MaatError::Tool(format!(
                "path '{}' escapes the allowed directory",
                user_path
            )));
        }

        debug!(path = %candidate.display(), append, "file_write");

        let bytes_written = if append {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&candidate)
                .map_err(|e| MaatError::Tool(format!("open: {e}")))?;
            f.write_all(content.as_bytes())
                .map_err(|e| MaatError::Tool(format!("write: {e}")))?;
            content.len()
        } else {
            std::fs::write(&candidate, content.as_bytes())
                .map_err(|e| MaatError::Tool(format!("write: {e}")))?;
            content.len()
        };

        Ok(json!({
            "status": if append { "appended" } else { "written" },
            "path": user_path,
            "bytes": bytes_written
        }))
    }
}

// ─────────────────────────────────────────────
// FileList
// ─────────────────────────────────────────────

pub struct FileList {
    base_dir: PathBuf,
}

#[async_trait]
impl Tool for FileList {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "file_list".into(),
            description: "List files and directories at a path. Paths are relative to the working directory. Use when the user asks what files exist, to browse a directory, or before reading a file you're unsure about.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list (default '.' for working directory)"
                    }
                },
                "required": []
            }),
        }
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let user_path = input["path"].as_str().unwrap_or(".");
        let abs_path = safe_path(&self.base_dir, user_path)?;

        if !abs_path.is_dir() {
            return Err(MaatError::Tool(format!("'{user_path}' is not a directory")));
        }

        let mut entries: Vec<Value> = std::fs::read_dir(&abs_path)
            .map_err(|e| MaatError::Tool(format!("readdir: {e}")))?
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let is_dir = e.path().is_dir();
                let size = e.metadata().ok().filter(|m| !is_dir).map(|m| m.len());
                json!({
                    "name": name,
                    "type": if is_dir { "dir" } else { "file" },
                    "size_bytes": size
                })
            })
            .collect();

        entries.sort_by(|a, b| {
            let a_dir = a["type"] == "dir";
            let b_dir = b["type"] == "dir";
            b_dir.cmp(&a_dir).then(a["name"].as_str().cmp(&b["name"].as_str()))
        });

        Ok(json!({
            "path": user_path,
            "count": entries.len(),
            "entries": entries
        }))
    }
}
