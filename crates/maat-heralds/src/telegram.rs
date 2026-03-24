use std::path::{Path, PathBuf};
use std::time::Duration;
use std::sync::Arc;

use anyhow::Context;
use maat_config::{TelegramConfig, UsersConfig};
use maat_core::commands::command_specs;
use maat_core::{BackendRequest, ChannelId, HeraldAttachment, HeraldEvent};
use maat_memory::{ArtifactRecord, MemoryStore};
use reqwest::Client;
use serde::Deserialize;
use reqwest::multipart::{Form, Part};
use tokio::fs;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::input::parse_input;

pub async fn run_telegram(
    backend_tx: mpsc::Sender<BackendRequest>,
    cfg: TelegramConfig,
    users: UsersConfig,
    bot_token: String,
    store: Arc<dyn MemoryStore>,
) -> anyhow::Result<()> {
    let client = Client::new();
    let download_dir = PathBuf::from(&cfg.download_dir);
    fs::create_dir_all(&download_dir).await?;
    let mut offset = 0_i64;

    info!(download_dir = %download_dir.display(), "telegram herald starting");

    loop {
        match get_updates(&client, &bot_token, offset, cfg.poll_seconds).await {
            Ok(updates) => {
                for update in updates {
                    offset = offset.max(update.update_id + 1);
                    if let Some(message) = update.message {
                        let Some(identity) = authorize_sender(&cfg, &users, &message) else {
                            continue;
                        };
                        if let Err(error) = handle_message(
                            &client,
                            &bot_token,
                            &backend_tx,
                            &download_dir,
                            store.clone(),
                            update.update_id,
                            identity,
                            message,
                        )
                        .await
                        {
                            error!(?error, "telegram message handling failed");
                        }
                    }
                }
            }
            Err(error) => {
                warn!(?error, "telegram polling failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

pub async fn send_telegram_delivery(
    bot_token: &str,
    chat_id: i64,
    text: &str,
    artifacts: &[ArtifactRecord],
) -> anyhow::Result<()> {
    let client = Client::new();
    send_message(&client, bot_token, chat_id, text).await?;
    for artifact in artifacts {
        send_artifact(&client, bot_token, chat_id, artifact).await?;
    }
    Ok(())
}

async fn handle_message(
    client: &Client,
    bot_token: &str,
    backend_tx: &mpsc::Sender<BackendRequest>,
    download_dir: &Path,
    store: Arc<dyn MemoryStore>,
    update_id: i64,
    identity: TelegramIngressIdentity<'_>,
    message: TelegramMessage,
) -> anyhow::Result<()> {
    let chat_id = message.chat.id;
    let session_name = format!("telegram-{}-{chat_id}", slugify_identity(identity.principal_id));
    let mut attachments = Vec::new();

    if let Some(document) = message.document {
        attachments.push(
            download_attachment(client, bot_token, download_dir, update_id, &document.file_id, document.file_name.as_deref(), document.mime_type.as_deref())
                .await
                .context("download telegram document")?,
        );
    }

    if let Some(photo_sizes) = message.photo {
        if let Some(photo) = photo_sizes.last() {
            attachments.push(
                download_attachment(
                    client,
                    bot_token,
                    download_dir,
                    update_id,
                    &photo.file_id,
                    Some("photo.jpg"),
                    Some("image/jpeg"),
                )
                .await
                .context("download telegram photo")?,
            );
        }
    }

    let original_text = message
        .text
        .or(message.caption)
        .unwrap_or_default();
    let mut text = original_text.clone();
    text.push_str(&format!(
        "\n\n[CHANNEL IDENTITY] The inbound Telegram sender is principal `{}` (display `{}`).",
        identity.principal_id,
        identity.display_name,
    ));
    if should_hint_return_to_chat(&text) {
        text.push_str(
            "\n\n[CHANNEL CONTEXT] The user is talking to you in Telegram. If they ask to send or return an artifact to \"me\" or \"back here\", deliver it in this Telegram chat instead of asking for email."
        );
    }
    let trimmed = text.trim();

    if trimmed.is_empty() && attachments.is_empty() {
        return Ok(());
    }

    if should_return_latest_artifact_request(&original_text) {
        if let Some(artifact) = store.latest_session_artifact(&session_name).await? {
            send_message(
                client,
                bot_token,
                chat_id,
                &format!("Sending `{}` back here.", artifact.handle),
            )
            .await?;
            send_artifact(client, bot_token, chat_id, &artifact).await?;
            return Ok(());
        }
    }

    if trimmed == "/start" || trimmed == "/help" {
        send_message(client, bot_token, chat_id, &render_help_text()).await?;
        return Ok(());
    }

    let payload = parse_input(
        text,
        Some(&session_name),
        attachments,
        Vec::new(),
        None,
    );

    let (reply_tx, mut reply_rx) = mpsc::channel::<HeraldEvent>(16);
    backend_tx
        .send(BackendRequest {
            channel: ChannelId("telegram".into()),
            payload,
            reply_tx,
        })
        .await
        .context("send telegram request to backend")?;

    let response = timeout(Duration::from_secs(300), async {
        while let Some(event) = reply_rx.recv().await {
            match event {
                HeraldEvent::AssistantMessage(reply) => return reply.content,
                HeraldEvent::Error(error) => return format!("Error: {error}"),
                HeraldEvent::Status(_) => {}
            }
        }
        String::from("No response from backend.")
    })
    .await
    .unwrap_or_else(|_| String::from("Timed out waiting for MAAT."));

    send_message(client, bot_token, chat_id, &response).await?;

    for artifact in artifacts_to_return(&*store, "user", &session_name, &response).await? {
        send_artifact(client, bot_token, chat_id, &artifact).await?;
    }
    Ok(())
}

async fn get_updates(
    client: &Client,
    bot_token: &str,
    offset: i64,
    poll_seconds: u64,
) -> anyhow::Result<Vec<TelegramUpdate>> {
    let url = format!("https://api.telegram.org/bot{bot_token}/getUpdates");
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "offset": offset,
            "timeout": poll_seconds,
            "allowed_updates": ["message"],
        }))
        .send()
        .await?;
    let body: TelegramResponse<Vec<TelegramUpdate>> = response.error_for_status()?.json().await?;
    Ok(body.result)
}

async fn get_file_path(client: &Client, bot_token: &str, file_id: &str) -> anyhow::Result<String> {
    let url = format!("https://api.telegram.org/bot{bot_token}/getFile");
    let response = client
        .post(url)
        .json(&serde_json::json!({ "file_id": file_id }))
        .send()
        .await?;
    let body: TelegramResponse<TelegramFile> = response.error_for_status()?.json().await?;
    Ok(body.result.file_path)
}

async fn download_attachment(
    client: &Client,
    bot_token: &str,
    download_dir: &Path,
    update_id: i64,
    file_id: &str,
    file_name: Option<&str>,
    mime_type: Option<&str>,
) -> anyhow::Result<HeraldAttachment> {
    let file_path = get_file_path(client, bot_token, file_id).await?;
    let url = format!("https://api.telegram.org/file/bot{bot_token}/{file_path}");
    let bytes = client.get(url).send().await?.error_for_status()?.bytes().await?;
    let name = file_name
        .map(sanitize_filename)
        .unwrap_or_else(|| default_download_name(&file_path));
    let local_path = download_dir.join(format!("{update_id}-{name}"));
    fs::write(&local_path, &bytes).await?;
    let absolute = std::fs::canonicalize(&local_path).unwrap_or(local_path);
    Ok(HeraldAttachment {
        mime_type: mime_type.unwrap_or_else(|| infer_mime_type(&absolute)).to_string(),
        size_bytes: bytes.len() as u64,
        pointer: absolute.display().to_string(),
    })
}

async fn send_message(client: &Client, bot_token: &str, chat_id: i64, text: &str) -> anyhow::Result<()> {
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
    client
        .post(url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": truncate_message(text),
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn send_artifact(
    client: &Client,
    bot_token: &str,
    chat_id: i64,
    artifact: &ArtifactRecord,
) -> anyhow::Result<()> {
    let bytes = fs::read(&artifact.storage_path).await
        .with_context(|| format!("read artifact {}", artifact.storage_path))?;
    let filename = Path::new(&artifact.storage_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&artifact.display_name)
        .to_string();
    let caption = format!("{} ({})", artifact.handle, artifact.display_name);
    let part = Part::bytes(bytes)
        .file_name(filename)
        .mime_str(&artifact.mime_type)?;
    let field = if artifact.mime_type.starts_with("image/") { "photo" } else { "document" };
    let endpoint = if field == "photo" { "sendPhoto" } else { "sendDocument" };
    let url = format!("https://api.telegram.org/bot{bot_token}/{endpoint}");
    let form = Form::new()
        .text("chat_id", chat_id.to_string())
        .text("caption", truncate_message(&caption))
        .part(field.to_string(), part);
    client.post(url).multipart(form).send().await?.error_for_status()?;
    Ok(())
}

fn render_help_text() -> String {
    let mut lines = vec![
        "MAAT Telegram".to_string(),
        "Commands:".to_string(),
    ];
    for spec in command_specs() {
        if spec.template.starts_with("/attach")
            || spec.template.starts_with("/detach")
            || spec.template == "/session use "
            || spec.template == "/session leave"
            || spec.template == "/verbose"
        {
            continue;
        }
        lines.push(format!("{} — {}", spec.template.trim_end(), spec.description));
    }
    lines.join("\n")
}

fn authorize_sender<'a>(
    cfg: &TelegramConfig,
    users: &'a UsersConfig,
    message: &TelegramMessage,
) -> Option<TelegramIngressIdentity<'a>> {
    let chat_id = message.chat.id;
    let sender_id = message.from.as_ref().map(|sender| sender.id);

    if users.has_telegram_identities() {
        let sender_id = sender_id?;
        let identity = users.resolve_telegram_identity(sender_id, chat_id)?;
        if !identity.can_instruct {
            return None;
        }
        return Some(TelegramIngressIdentity {
            principal_id: identity.principal.as_str(),
            display_name: users.principal_display_name(&identity.principal),
        });
    }

    if cfg.allowed_chat_ids.is_empty() || cfg.allowed_chat_ids.iter().any(|id| *id == chat_id) {
        return Some(TelegramIngressIdentity {
            principal_id: "owner",
            display_name: "Owner",
        });
    }

    None
}

async fn artifacts_to_return(
    store: &dyn MemoryStore,
    user_id: &str,
    session_name: &str,
    response: &str,
) -> anyhow::Result<Vec<ArtifactRecord>> {
    let handles = parse_artifact_handles(response);
    let mut artifacts = Vec::new();

    if !handles.is_empty() {
        for handle in handles {
            if let Some(record) = store.get_artifact_by_handle(user_id, &handle).await? {
                artifacts.push(record);
            }
        }
        return Ok(artifacts);
    }

    if should_return_latest_artifact(response) {
        if let Some(record) = store.latest_session_artifact(session_name).await? {
            artifacts.push(record);
        }
    }

    Ok(artifacts)
}

fn parse_artifact_handles(response: &str) -> Vec<String> {
    let lines = response.lines().collect::<Vec<_>>();
    let mut capture = false;
    lines
        .into_iter()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.eq_ignore_ascii_case("Generated artifacts:") {
                capture = true;
                return None;
            }
            if capture && !trimmed.starts_with('-') && !trimmed.is_empty() {
                capture = false;
            }
            if !capture {
                return None;
            }
            Some(line)
        })
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("- ") {
                return None;
            }
            let handle = trimmed.trim_start_matches("- ").split_whitespace().next()?;
            if handle.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                Some(handle.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn should_return_latest_artifact(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_channel_return = [
        "sending it back here",
        "returning it here",
        "sent it here",
        "delivered it here",
    ];
    mentions_channel_return.iter().any(|needle| lower.contains(needle))
}

fn should_hint_return_to_chat(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["send it to me", "send that to me", "send it back here", "send that back here", "return it to me"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn should_return_latest_artifact_request(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "send it to me",
        "send that to me",
        "send it back here",
        "send that back here",
        "return it to me",
        "send me the last artifact",
        "send me the image",
        "send me the last image",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect()
}

fn default_download_name(file_path: &str) -> String {
    Path::new(file_path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_filename)
        .unwrap_or_else(|| "telegram-file.bin".into())
}

fn infer_mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or_default() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn truncate_message(text: &str) -> String {
    const MAX_TELEGRAM_TEXT: usize = 3900;
    if text.chars().count() <= MAX_TELEGRAM_TEXT {
        return text.to_string();
    }
    let mut truncated = text.chars().take(MAX_TELEGRAM_TEXT).collect::<String>();
    truncated.push_str("\n\n[truncated]");
    truncated
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    result: T,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Clone, Debug, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    from: Option<TelegramUser>,
    text: Option<String>,
    caption: Option<String>,
    document: Option<TelegramDocument>,
    photo: Option<Vec<TelegramPhotoSize>>,
}

#[derive(Clone, Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct TelegramUser {
    id: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct TelegramDocument {
    file_id: String,
    file_name: Option<String>,
    mime_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    file_path: String,
}

#[derive(Clone, Copy, Debug)]
struct TelegramIngressIdentity<'a> {
    principal_id: &'a str,
    display_name: &'a str,
}

fn slugify_identity(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_dash = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            last_dash = false;
            ch.to_ascii_lowercase()
        } else if !last_dash {
            last_dash = true;
            '-'
        } else {
            continue;
        };
        out.push(mapped);
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use maat_config::{PrincipalConfig, TelegramIdentityConfig};

    #[test]
    fn truncates_long_messages_for_telegram() {
        let input = "x".repeat(5000);
        let output = truncate_message(&input);
        assert!(output.len() < input.len());
        assert!(output.contains("[truncated]"));
    }

    #[test]
    fn honors_allowed_chat_ids() {
        let mut cfg = TelegramConfig::default();
        let users = UsersConfig::default();
        let message = TelegramMessage {
            chat: TelegramChat { id: 42 },
            from: Some(TelegramUser { id: 5 }),
            text: None,
            caption: None,
            document: None,
            photo: None,
        };
        assert!(authorize_sender(&cfg, &users, &message).is_some());
        cfg.allowed_chat_ids = vec![7, 9];
        let allowed_message = TelegramMessage { chat: TelegramChat { id: 7 }, ..message.clone() };
        assert!(authorize_sender(&cfg, &users, &allowed_message).is_some());
        assert!(authorize_sender(&cfg, &users, &message).is_none());
    }

    #[test]
    fn parses_artifact_handles_from_reply_lines() {
        let text = "Generated artifacts:\n  - generated-image-8vh9  image/jpeg  generated-image-1.jpg";
        assert_eq!(parse_artifact_handles(text), vec!["generated-image-8vh9"]);
    }

    #[test]
    fn parse_artifact_handles_ignores_attached_artifact_lines() {
        let text = "Attached artifacts:\n  - source-image-a1b2  image/jpeg  source.jpg\n\nGenerated artifacts:\n  - edited-image-c3d4  image/jpeg  edited.jpg";
        assert_eq!(parse_artifact_handles(text), vec!["edited-image-c3d4"]);
    }

    #[test]
    fn registered_telegram_senders_are_required_when_identity_map_exists() {
        let cfg = TelegramConfig::default();
        let mut principals = BTreeMap::new();
        principals.insert(
            "troy".into(),
            PrincipalConfig {
                display_name: Some("Troy".into()),
                role: Some("owner".into()),
                permissions: vec![],
            },
        );
        let users = UsersConfig {
            principals,
            telegram: vec![TelegramIdentityConfig {
                principal: "troy".into(),
                user_id: 99,
                allowed_chat_ids: vec![7],
                can_instruct: true,
            }],
        };
        let allowed = TelegramMessage {
            chat: TelegramChat { id: 7 },
            from: Some(TelegramUser { id: 99 }),
            text: None,
            caption: None,
            document: None,
            photo: None,
        };
        let blocked_sender = TelegramMessage {
            from: Some(TelegramUser { id: 100 }),
            ..allowed.clone()
        };
        let blocked_chat = TelegramMessage {
            chat: TelegramChat { id: 8 },
            ..allowed.clone()
        };

        let identity = authorize_sender(&cfg, &users, &allowed).expect("authorized");
        assert_eq!(identity.principal_id, "troy");
        assert_eq!(identity.display_name, "Troy");
        assert!(authorize_sender(&cfg, &users, &blocked_sender).is_none());
        assert!(authorize_sender(&cfg, &users, &blocked_chat).is_none());
    }

    #[test]
    fn slugifies_identity_for_session_names() {
        assert_eq!(slugify_identity("Troy Travlos"), "troy-travlos");
        assert_eq!(slugify_identity("owner:troy"), "owner-troy");
    }
}
