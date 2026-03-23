//! Gmail tools — send email via the Gmail REST API.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use maat_config::{MaatConfig, SecretResolver};
use maat_core::{LlmToolDef, MaatError, Tool};
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
}

#[async_trait]
impl Tool for GmailSend {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "gmail_send".into(),
            description: "Send an email via Gmail. Use this when the user asks to send an email to someone.".into(),
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
                    }
                },
                "required": ["to", "subject", "body"]
            }),
        }
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

        let access_token = self.valid_access_token().await?;
        send_message(&access_token, &to, &subject, &body).await?;

        Ok(json!({ "status": "sent", "to": to, "subject": subject }))
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
    access_token: &str,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), MaatError> {
    // Build a minimal RFC 822 message.
    let raw = format!(
        "To: {to}\r\nSubject: {subject}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}"
    );
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
