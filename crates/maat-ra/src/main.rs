//! MAAT — entry point.
//!
//! Wiring order:
//!   .env → maat.toml + maat.workspace.toml → SecretResolver
//!   → LLM client → ToolRegistry → MemoryStore → Actors → TUI

use std::sync::Arc;

use kameo::request::MessageSend;
use maat_config::{
    secrets::build_resolver,
    MaatConfig,
};
use maat_core::{HeraldPayload, ModelSpec, SessionId, StatusEvent, ToolRegistry, TuiEvent, UserId};
use maat_llm::OpenAiCompatClient;
use maat_memory::sqlite::SqliteStore;
use maat_pharoh::{Inbound, Pharoh};
use maat_vizier::Vizier;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── env file (lowest priority — overridden by everything else) ──
    let _ = dotenvy::dotenv();

    // ── logging → file ─────────────────────────────────────────────
    let file_appender = tracing_appender::rolling::never(".", "maat.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "maat=debug".parse().unwrap()),
        )
        .with_writer(non_blocking)
        .with_target(false)
        .with_ansi(false)
        .init();

    // ── config ─────────────────────────────────────────────────────
    let cfg = MaatConfig::load().unwrap_or_else(|e| {
        info!("config load error ({e}), using defaults");
        MaatConfig::default()
    });
    info!(model = %cfg.llm.model, db = %cfg.memory.db_path, "config loaded");

    // ── secret resolver ────────────────────────────────────────────
    let resolver = Arc::new(build_resolver(
        cfg.secrets.onepassword_vault.as_deref(),
        cfg.secrets.encrypted_file_path.as_deref(),
    ));

    // ── LLM client ─────────────────────────────────────────────────
    let api_key = resolver
        .get("maat/openrouter/api_key")
        .ok_or_else(|| anyhow::anyhow!(
            "OpenRouter API key not found. Set OPENROUTER_API_KEY or store via `/secret set maat/openrouter/api_key`"
        ))?;

    let model_id = std::env::var("MAAT_MODEL").unwrap_or_else(|_| cfg.llm.model.clone());
    let spec = ModelSpec {
        model_id: model_id.clone(),
        base_url: cfg.llm.base_url.clone(),
        api_key_env: "OPENROUTER_API_KEY".into(), // kept for compat; key already resolved above
        temperature: 0.7,
        max_tokens: 4096,
    };

    // Temporarily set the env var so OpenAiCompatClient finds it.
    // Phase 12 will inject the key directly.
    std::env::set_var("OPENROUTER_API_KEY", &api_key);

    let llm: Arc<dyn maat_llm::LlmClient> = Arc::new(OpenAiCompatClient::from_spec(&spec)?);
    info!(model = %model_id, "LLM client ready");

    // ── tool registry ──────────────────────────────────────────────
    let mut registry = ToolRegistry::new();

    // IMAP — resolve credentials via secret chain
    let imap_host = cfg.imap.as_ref().and_then(|c| c.host.clone())
        .or_else(|| std::env::var("IMAP_HOST").ok());
    let imap_user = cfg.imap.as_ref().and_then(|c| c.username.clone())
        .or_else(|| std::env::var("IMAP_USERNAME").ok());
    let imap_pass_key = cfg.imap.as_ref()
        .map(|c| c.password_key().to_string())
        .unwrap_or_else(|| "maat/imap/password".into());
    let imap_pass = resolver.get(&imap_pass_key);
    let imap_port = cfg.imap.as_ref().and_then(|c| c.port).unwrap_or(993);

    match (imap_host, imap_user, imap_pass) {
        (Some(host), Some(username), Some(password)) => {
            let imap_cfg = maat_talents::imap::ImapConfig { host, port: imap_port, username, password };
            maat_talents::ImapTalent::new(imap_cfg).register_all(&mut registry);
            info!("IMAP talent registered (email_list, email_read, email_search)");
        }
        _ => {
            info!("IMAP talent not loaded — configure [imap] in maat.toml and set maat/imap/password secret");
        }
    }

    // Google — register if client_id + client_secret are available.
    // gmail_send requires auth (/auth google) at runtime; registering eagerly
    // means the tool appears in the system prompt once credentials are configured.
    let google_client_id = cfg.google.as_ref().and_then(|g| g.client_id.clone());
    let google_secret_key = cfg.google.as_ref()
        .map(|g| g.client_secret_key().to_string())
        .unwrap_or_else(|| "maat/google/client_secret".into());
    let google_client_secret = resolver.get(&google_secret_key);

    match (google_client_id, google_client_secret) {
        (Some(client_id), Some(client_secret)) => {
            maat_talents::GoogleTalent::new(
                client_id,
                client_secret,
                resolver.clone(),
                Arc::new(cfg.clone()),
            )
            .register_all(&mut registry);
            info!("Google talent registered (gmail_send, calendar_list, calendar_create)");
        }
        _ => {
            info!("Google talent not loaded — add [google] client_id to maat.toml and /secret set maat/google/client_secret");
        }
    }

    // Tavily web search — resolve key from secret chain or env.
    let tavily_key = resolver
        .get("maat/tavily/api_key")
        .or_else(|| std::env::var("TAVILY_API_KEY").ok());
    match tavily_key {
        Some(key) => {
            maat_talents::SearchTalent::new(key).register_all(&mut registry);
            info!("Search talent registered (web_search)");
        }
        None => {
            info!("Search talent not loaded — add TAVILY_API_KEY to .env or /secret set maat/tavily/api_key");
        }
    }

    // File tools — always available, scoped to the current working directory.
    let base_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    maat_talents::FileTalent::new(base_dir).register_all(&mut registry);
    info!("File talent registered (file_read, file_write, file_list)");

    let tool_registry = Arc::new(registry);
    let system_prompt = build_system_prompt(&tool_registry);

    // ── memory store ───────────────────────────────────────────────
    let store: Arc<dyn maat_memory::MemoryStore> =
        Arc::new(SqliteStore::open(std::path::Path::new(&cfg.memory.db_path))?);
    info!(db = %cfg.memory.db_path, "memory store ready");

    // ── status bus ─────────────────────────────────────────────────
    let (status_tx, mut status_rx) = broadcast::channel::<StatusEvent>(256);
    tokio::spawn(async move {
        while let Ok(event) = status_rx.recv().await {
            tracing::debug!(kind = ?event.kind, "status event");
        }
    });

    // ── actors ─────────────────────────────────────────────────────
    let user_id = UserId("user".into());
    let session_id = SessionId::new();

    let primary_vizier = kameo::spawn(Vizier::new(
        user_id.clone(),
        session_id.clone(),
        llm.clone(),
        tool_registry.clone(),
        status_tx.clone(),
    ));

    let pharoh = kameo::spawn(Pharoh::new(
        user_id,
        session_id,
        system_prompt,
        primary_vizier,
        llm,
        tool_registry,
        store,
        Arc::new(cfg),
        resolver,
        status_tx,
    ));

    // ── channels ───────────────────────────────────────────────────
    let (user_tx, mut user_rx) = mpsc::channel::<HeraldPayload>(32);
    let (tui_tx, tui_rx) = mpsc::channel::<TuiEvent>(32);

    // ── bridge ─────────────────────────────────────────────────────
    tokio::spawn(async move {
        while let Some(payload) = user_rx.recv().await {
            let event = match pharoh.ask(Inbound(payload)).send().await {
                Ok(reply) => TuiEvent::AssistantMessage(reply),
                Err(e) => {
                    error!("pharoh error: {e}");
                    TuiEvent::Error(e.to_string())
                }
            };
            if tui_tx.send(event).await.is_err() {
                break;
            }
        }
    });

    // ── TUI ────────────────────────────────────────────────────────
    maat_heralds::tui::run_tui(user_tx, tui_rx, model_id).await?;
    Ok(())
}

fn build_system_prompt(registry: &ToolRegistry) -> String {
    let defs = registry.all_definitions();
    if defs.is_empty() {
        return "You are MAAT, a thoughtful and concise AI assistant.".into();
    }
    let tool_lines: Vec<String> = defs
        .iter()
        .map(|d| format!("  - {} — {}", d.name, d.description))
        .collect();
    format!(
        "You are MAAT, a thoughtful and concise AI assistant.\n\n\
         You have access to the following tools and MUST use them when relevant \
         instead of saying you cannot perform a task:\n{}\n\n\
         Rules:\n\
         - Call tools immediately when relevant rather than apologising or suggesting alternatives.\n\
         - After every tool call, always report the outcome to the user in plain language \
           (e.g. \"Done — wrote 42 bytes to notes.txt\" or \"Sent email to alice@example.com\").\n\
         - If a tool returns an error, explain what went wrong and suggest how to fix it.\n\
         - Be concise: one or two sentences of confirmation is enough unless the user asked for detail.",
        tool_lines.join("\n")
    )
}
