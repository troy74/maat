//! Gmail tools — send email via the Gmail REST API.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use maat_config::{MaatConfig, SecretResolver};
use maat_core::{
    CapabilityCard, CapabilityId, CapabilityKind, CapabilityProvenance, CapabilityRoutingHints,
    CapabilityTrust, CostProfile, LlmToolDef, MaatError, ModelSelectionPolicy, ModelTrait,
    Permission, Tool,
};
use serde_json::{json, Value};
use tracing::debug;

use super::auth::{refresh_access_token, TokenSet};

// ─────────────────────────────────────────────
// GmailSend
// ─────────────────────────────────────────────

pub struct GmailSend {
    pub client_id: String,
    pub client_secret: String,
    pub resolver: Arc<SecretResolver>,
    pub config: Arc<MaatConfig>,
    pub base_dir: PathBuf,
}

#[async_trait]
impl Tool for GmailSend {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "gmail_send".into(),
            description: "Send an email via Gmail. Use this when the user asks to send an email to someone, including when a local file or stored artifact should be attached.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient email address"
                    },
                    "subject": {
                        "type": "string",
                        "description": "Email subject line"
                    },
                    "body": {
                        "type": "string",
                        "description": "Plain-text email body"
                    },
                    "attachments": {
                        "type": "array",
                        "description": "Optional list of local file paths to attach. Prefer artifact_handles for stored MAAT artifacts; use attachments for raw workspace files, for example ['output/pdf/report.pdf']",
                        "items": {
                            "type": "string"
                        }
                    },
                    "artifact_handles": {
                        "type": "array",
                        "description": "Optional list of stored MAAT artifact handles to attach. Prefer this for artifacts already tracked by MAAT, for example ['bright-canvas-a1b2']",
                        "items": {
                            "type": "string"
                        }
                    }
                },
                "required": ["to", "subject", "body"]
            }),
        }
    }

    fn capability_card(&self) -> Option<CapabilityCard> {
        let def = self.llm_definition();
        Some(CapabilityCard {
            id: CapabilityId(def.name.clone()),
            name: "Gmail Send".into(),
            semantic_description: def.description.clone(),
            kind: CapabilityKind::Talent,
            input_schema: def.parameters,
            output_schema: json!({ "type": "object" }),
            cost_profile: CostProfile { avg_latency_ms: 1800, estimated_tokens: 900 },
            tags: vec!["email".into(), "write".into(), "gmail".into()],
            semantic_terms: Vec::new(),
            trust: CapabilityTrust::Core,
            provenance: CapabilityProvenance {
                source: "compiled_talent".into(),
                path: None,
                reference: None,
            },
            permissions: vec![Permission::Email, Permission::Network],
            routing_hints: Some(CapabilityRoutingHints {
                preferred_tags: vec!["email".into(), "write".into()],
                avoids_tags: vec![],
                model_policy: Some(ModelSelectionPolicy {
                    preferred_profiles: vec![],
                    allow_profiles: vec![],
                    deny_profiles: vec![],
                    required_traits: vec![ModelTrait::ToolCalling, ModelTrait::StructuredOutput],
                    max_cost_tier: None,
                    max_latency_tier: None,
                    min_reasoning_tier: None,
                    require_tool_calling: Some(true),
                }),
            }),
        })
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let to = input["to"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'to'".into()))?
            .to_string();
        let subject = input["subject"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'subject'".into()))?
            .to_string();
        let body = input["body"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'body'".into()))?
            .to_string();
        let attachments = input
            .get("attachments")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(|path| path.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let access_token = self.valid_access_token().await?;
        send_message(&self.base_dir, &access_token, &to, &subject, &body, &attachments).await?;

        Ok(json!({ "status": "sent", "to": to, "subject": subject, "attachments": attachments }))
    }
}

impl GmailSend {
    /// Return a valid (non-expired) access token, refreshing if necessary.
    async fn valid_access_token(&self) -> Result<String, MaatError> {
        let token_key = self
            .config
            .google
            .as_ref()
            .map(|g| g.token_key().to_string())
            .unwrap_or_else(|| "maat/google/oauth_token".into());

        let raw = self.resolver.get(&token_key).ok_or_else(|| {
            MaatError::Config(
                "Google not authenticated. Run /auth google first.".into(),
            )
        })?;

        let token = TokenSet::from_json(&raw)
            .ok_or_else(|| MaatError::Config("Stored Google token is invalid.".into()))?;

        if !token.is_expired() {
            return Ok(token.access_token);
        }

        debug!("Google access token expired — refreshing");
        let rt = token.refresh_token.clone().ok_or_else(|| {
            MaatError::Config(
                "Google token expired and no refresh token stored. Run /auth google again.".into(),
            )
        })?;

        let refreshed = refresh_access_token(&self.client_id, &self.client_secret, &rt, token)
            .await
            .map_err(|e| MaatError::Tool(format!("token refresh failed: {e}")))?;

        // Persist the refreshed token.
        let _ = self.resolver.set(&token_key, &refreshed.to_json());

        Ok(refreshed.access_token)
    }
}

// ─────────────────────────────────────────────
// Gmail API call
// ─────────────────────────────────────────────

async fn send_message(
    base_dir: &Path,
    access_token: &str,
    to: &str,
    subject: &str,
    body: &str,
    attachments: &[String],
) -> Result<(), MaatError> {
    let raw = build_raw_message(base_dir, to, subject, body, attachments)?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes());

    let resp = reqwest::Client::new()
        .post("https://gmail.googleapis.com/gmail/v1/users/me/messages/send")
        .bearer_auth(access_token)
        .json(&json!({ "raw": encoded }))
        .send()
        .await
        .map_err(|e| MaatError::Tool(format!("Gmail API request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(MaatError::Tool(format!("Gmail API {status}: {text}")));
    }

    Ok(())
}

fn build_raw_message(
    base_dir: &Path,
    to: &str,
    subject: &str,
    body: &str,
    attachments: &[String],
) -> Result<String, MaatError> {
    if attachments.is_empty() {
        return Ok(format!(
            "To: {to}\r\nSubject: {subject}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}"
        ));
    }

    let boundary = format!("maat-boundary-{}", maat_core::now_ms());
    let mut raw = format!(
        "To: {to}\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    );

    for path in attachments {
        let attachment = load_attachment(base_dir, path)?;
        raw.push_str(&format!(
            "--{boundary}\r\nContent-Type: {}; name=\"{}\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"{}\"\r\n\r\n{}\r\n",
            attachment.content_type,
            attachment.filename,
            attachment.filename,
            wrap_base64(&attachment.base64_data),
        ));
    }

    raw.push_str(&format!("--{boundary}--\r\n"));
    Ok(raw)
}

struct AttachmentData {
    filename: String,
    content_type: String,
    base64_data: String,
}

fn load_attachment(base_dir: &Path, user_path: &str) -> Result<AttachmentData, MaatError> {
    let path = safe_attachment_path(base_dir, user_path)?;
    if !path.exists() {
        return Err(MaatError::Tool(format!("attachment not found: {user_path}")));
    }
    if path.is_dir() {
        return Err(MaatError::Tool(format!("attachment path is a directory: {user_path}")));
    }

    let bytes = std::fs::read(&path)
        .map_err(|e| MaatError::Tool(format!("failed to read attachment '{user_path}': {e}")))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment.bin")
        .replace('"', "_");

    Ok(AttachmentData {
        filename,
        content_type: detect_content_type(&path).to_string(),
        base64_data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn safe_attachment_path(base_dir: &Path, user_path: &str) -> Result<PathBuf, MaatError> {
    let canon_base = base_dir
        .canonicalize()
        .map_err(|e| MaatError::Tool(format!("base_dir canonicalise: {e}")))?;
    let candidate = {
        let raw = PathBuf::from(user_path);
        if raw.is_absolute() {
            raw
        } else {
            base_dir.join(raw)
        }
    };
    let canon_candidate = candidate
        .canonicalize()
        .map_err(|e| MaatError::Tool(format!("attachment canonicalise: {e}")))?;

    if !canon_candidate.starts_with(&canon_base) {
        return Err(MaatError::Tool(format!(
            "attachment path '{}' escapes the allowed directory",
            user_path
        )));
    }

    Ok(canon_candidate)
}

fn detect_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

fn wrap_base64(raw: &str) -> String {
    let mut wrapped = String::new();
    let mut start = 0usize;
    while start < raw.len() {
        let end = (start + 76).min(raw.len());
        wrapped.push_str(&raw[start..end]);
        wrapped.push_str("\r\n");
        start = end;
    }
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_message_includes_attachment_parts() {
        let base_dir = std::env::temp_dir().join(format!("maat-gmail-test-{}", maat_core::now_ms()));
        std::fs::create_dir_all(&base_dir).unwrap();
        let path = base_dir.join("sample.pdf");
        std::fs::write(&path, b"fake-pdf").unwrap();

        let raw = build_raw_message(
            &base_dir,
            "troy@example.com",
            "Report",
            "See attachment",
            &[String::from("sample.pdf")],
        )
        .unwrap();

        assert!(raw.contains("multipart/mixed"));
        assert!(raw.contains("filename=\"sample.pdf\""));
        assert!(raw.contains("application/pdf"));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(base_dir);
    }

    #[test]
    fn llm_definition_exposes_artifact_handles() {
        let tool = GmailSend {
            client_id: String::new(),
            client_secret: String::new(),
            resolver: std::sync::Arc::new(maat_config::secrets::build_resolver(None, None)),
            config: std::sync::Arc::new(maat_config::MaatConfig::default()),
            base_dir: std::env::temp_dir(),
        };
        let def = tool.llm_definition();
        assert!(def.parameters["properties"]["artifact_handles"].is_object());
    }

    #[test]
    fn safe_attachment_path_allows_absolute_paths_within_base_dir() {
        let base_dir = std::env::temp_dir().join(format!("maat-gmail-abs-{}", maat_core::now_ms()));
        std::fs::create_dir_all(&base_dir).unwrap();
        let path = base_dir.join("sample.pdf");
        std::fs::write(&path, b"fake-pdf").unwrap();

        let resolved = safe_attachment_path(&base_dir, path.to_str().unwrap()).unwrap();
        assert_eq!(resolved, path.canonicalize().unwrap());

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(base_dir);
    }
}
