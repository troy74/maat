//! Google OAuth 2.0 — local callback server flow.
//!
//! Flow:
//!   1. Bind a local HTTP server on port 8080-8099 (or OS-assigned fallback).
//!   2. Build the Google authorisation URL with the desired scopes.
//!   3. Return the URL to the caller; spawn a background task to wait for the callback.
//!   4. When the browser redirects with `?code=`, exchange the code for tokens.
//!   5. Store the resulting TokenSet via SecretResolver.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use maat_config::{MaatConfig, SecretResolver};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

// ─────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

const SCOPES: &str = concat!(
    "https://www.googleapis.com/auth/gmail.send ",
    "https://mail.google.com/ ",
    "https://www.googleapis.com/auth/calendar ",
    "https://www.googleapis.com/auth/drive ",
    "https://www.googleapis.com/auth/documents ",
    "https://www.googleapis.com/auth/spreadsheets ",
    "https://www.googleapis.com/auth/contacts.readonly"
);

// ─────────────────────────────────────────────
// TokenSet — persisted as JSON in SecretResolver
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix epoch milliseconds when the access token expires.
    pub expires_at_ms: u64,
}

impl TokenSet {
    /// Returns true if the token should be refreshed (expires within 5 minutes).
    pub fn is_expired(&self) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        now_ms + 300_000 >= self.expires_at_ms
    }

    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// ─────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────

/// Bind a local callback server, spawn the callback handler in the background,
/// and return the Google authorisation URL the user must visit.
pub async fn start_oauth_flow(
    resolver: Arc<SecretResolver>,
    config: Arc<MaatConfig>,
    client_id: String,
    client_secret: String,
) -> Result<String, String> {
    let (listener, port) = bind_callback_port()
        .await
        .map_err(|e| format!("could not bind callback port: {e}"))?;

    let redirect_uri = format!("http://localhost:{port}/callback");
    let auth_url = build_auth_url(&client_id, &redirect_uri);

    // Spawn background task — exchanges code and stores token.
    tokio::spawn(async move {
        match handle_callback(listener, &client_id, &client_secret, &redirect_uri).await {
            Ok(token) => {
                let token_key = config
                    .google
                    .as_ref()
                    .map(|g| g.token_key().to_string())
                    .unwrap_or_else(|| "maat/google/oauth_token".into());
                match resolver.set(&token_key, &token.to_json()) {
                    Ok(()) => info!(key = %token_key, "Google OAuth token stored"),
                    Err(e) => warn!("Failed to store Google token: {e}"),
                }
            }
            Err(e) => warn!("Google OAuth callback failed: {e}"),
        }
    });

    Ok(auth_url)
}

// ─────────────────────────────────────────────
// Port binding
// ─────────────────────────────────────────────

async fn bind_callback_port() -> std::io::Result<(TcpListener, u16)> {
    for port in 8080u16..8100 {
        if let Ok(listener) = TcpListener::bind(format!("127.0.0.1:{port}")).await {
            return Ok((listener, port));
        }
    }
    // Fallback: let the OS pick a port.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

// ─────────────────────────────────────────────
// Auth URL construction
// ─────────────────────────────────────────────

fn build_auth_url(client_id: &str, redirect_uri: &str) -> String {
    reqwest::Url::parse_with_params(
        AUTH_URL,
        &[
            ("client_id",     client_id),
            ("redirect_uri",  redirect_uri),
            ("response_type", "code"),
            ("scope",         SCOPES),
            ("access_type",   "offline"),
            ("prompt",        "consent"),
        ],
    )
    .expect("static base URL is valid")
    .to_string()
}

// ─────────────────────────────────────────────
// Callback handler
// ─────────────────────────────────────────────

async fn handle_callback(
    listener: TcpListener,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
) -> Result<TokenSet, String> {
    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|e| format!("accept: {e}"))?;

    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.map_err(|e| format!("read: {e}"))?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let code = extract_code(&request)
        .ok_or_else(|| "OAuth code not found in callback".to_string())?;

    // Acknowledge the browser.
    let html = concat!(
        "<html><body style='font-family:sans-serif;text-align:center;padding-top:60px'>",
        "<h2>MAAT — Authentication successful!</h2>",
        "<p>You can close this tab and return to the terminal.</p>",
        "</body></html>"
    );
    let _ = stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                html.len(),
                html
            )
            .as_bytes(),
        )
        .await;
    drop(stream);

    exchange_code(client_id, client_secret, &code, redirect_uri).await
}

fn extract_code(request: &str) -> Option<String> {
    // First line: "GET /callback?code=xxx&... HTTP/1.1"
    let line = request.lines().next()?;
    let path = line.split_whitespace().nth(1)?; // "/callback?code=xxx"
    let query = path.split('?').nth(1)?;        // "code=xxx&scope=..."
    for pair in query.split('&') {
        if let Some(code) = pair.strip_prefix("code=") {
            return Some(percent_decode(code));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b as char);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(if bytes[i] == b'+' { ' ' } else { bytes[i] as char });
        i += 1;
    }
    out
}

// ─────────────────────────────────────────────
// Token exchange / refresh
// ─────────────────────────────────────────────

async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<TokenSet, String> {
    let params = [
        ("code",          code),
        ("client_id",     client_id),
        ("client_secret", client_secret),
        ("redirect_uri",  redirect_uri),
        ("grant_type",    "authorization_code"),
    ];
    token_request(&params).await
}

pub async fn refresh_access_token(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
    existing: TokenSet,
) -> Result<TokenSet, String> {
    let params = [
        ("client_id",     client_id),
        ("client_secret", client_secret),
        ("refresh_token", refresh_token),
        ("grant_type",    "refresh_token"),
    ];
    let mut new_token = token_request(&params).await?;
    // Google may not return a new refresh_token on refresh — keep the old one.
    if new_token.refresh_token.is_none() {
        new_token.refresh_token = existing.refresh_token;
    }
    Ok(new_token)
}

async fn token_request(params: &[(&str, &str)]) -> Result<TokenSet, String> {
    let resp: serde_json::Value = reqwest::Client::new()
        .post(TOKEN_URL)
        .form(params)
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?
        .json()
        .await
        .map_err(|e| format!("JSON parse error: {e}"))?;

    if let Some(err) = resp["error"].as_str() {
        return Err(format!(
            "{err}: {}",
            resp["error_description"].as_str().unwrap_or("")
        ));
    }

    let access_token = resp["access_token"]
        .as_str()
        .ok_or("missing access_token")?
        .to_string();
    let refresh_token = resp["refresh_token"].as_str().map(String::from);
    let expires_in = resp["expires_in"].as_u64().unwrap_or(3600);
    let expires_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        + expires_in * 1000;

    Ok(TokenSet { access_token, refresh_token, expires_at_ms })
}
