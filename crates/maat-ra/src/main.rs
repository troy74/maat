//! MAAT — entry point.
//!
//! Wiring order:
//!   .env → maat.toml + maat.workspace.toml → SecretResolver
//!   → LLM client → ToolRegistry → MemoryStore → Actors → TUI

use std::sync::Arc;

use kameo::request::MessageSend;
use maat_config::{
    default_skill_dirs, ensure_sample_automation, is_schedule_due, load_automations,
    load_installed_skills, prompts::PromptLibrary, secrets::build_resolver, AutomationDelivery,
    AutomationStatus,
    MaatConfig,
};
use maat_core::{
    BackendRequest, CancellationRegistry, HeraldEvent, HeraldPayload, ModelCostTier,
    ModelLatencyTier, ModelProfile, ModelReasoningTier, ModelRegistry, ModelRouteRule,
    ModelRouteScope, ModelSelectionPolicy, ModelProviderSpec, ParsedCommand, ProviderApiStyle,
    SessionId, StatusEvent, SupportCapabilityRule, ToolRegistry, UserId,
};
use maat_llm::OpenAiCompatClient;
use maat_memory::{sqlite::SqliteStore, ContextConfig, MemoryStore};
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
    let base_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let _ = ensure_sample_automation(&cfg.automations.dir);

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

    let model_registry = build_model_registry(&cfg);
    let route_rules = Arc::new(build_model_route_rules(&cfg));
    let support_rules = Arc::new(build_support_rules(&cfg));
    let spec = model_registry
        .resolve_default_spec()
        .ok_or_else(|| anyhow::anyhow!("No default model profile could be resolved"))?;
    let model_id = spec.model_id.clone();

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
                base_dir.clone(),
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
    maat_talents::FileTalent::new(base_dir).register_all(&mut registry);
    info!("File talent registered (file_read, file_write, file_list)");

    maat_talents::AutomationTalent::new(cfg.automations.dir.clone()).register_all(&mut registry);
    info!("Automation talent registered (automation_manage)");

    let skill_dirs = default_skill_dirs(&cfg.skills.dirs);
    let local_skill_root = skill_dirs
        .first()
        .cloned()
        .unwrap_or_else(|| std::path::PathBuf::from("skills"));
    maat_talents::SkillTalent::new(local_skill_root).register_all(&mut registry);
    info!("Skill talent registered (skill_manage)");

    let installed_skills = load_installed_skills(&skill_dirs);
    installed_skills.register_tools(&mut registry);
    let tool_registry = Arc::new(registry);
    let mut capability_registry = tool_registry.capability_registry();
    for card in installed_skills.capability_cards() {
        capability_registry.register(card);
    }
    let capability_registry = Arc::new(capability_registry);
    let installed_skill_summaries = installed_skills
        .all()
        .iter()
        .map(|skill| {
            format!(
                "{}:{:?}:{:?}",
                skill.name,
                skill.source,
                skill.trust,
            )
        })
        .collect::<Vec<_>>();
    info!(
        capabilities = capability_registry.ids().len(),
        installed_skills = installed_skill_summaries.len(),
        skill_summaries = ?installed_skill_summaries,
        "capability registry ready"
    );
    let model_registry = Arc::new(model_registry);
    let user_id = UserId("user".into());
    let prompts = PromptLibrary::load(&cfg.prompts.dir);
    let system_prompt = build_system_prompt(&tool_registry, &prompts, &user_id.0);

    // ── memory store ───────────────────────────────────────────────
    let store: Arc<dyn MemoryStore> =
        Arc::new(SqliteStore::open(std::path::Path::new(&cfg.memory.db_path))?);
    info!(db = %cfg.memory.db_path, "memory store ready");

    // ── status bus ─────────────────────────────────────────────────
    let (status_tx, _) = broadcast::channel::<StatusEvent>(256);
    let mut status_rx = status_tx.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = status_rx.recv().await {
            tracing::debug!(kind = ?event.kind, "status event");
        }
    });

    // ── actors ─────────────────────────────────────────────────────
    let session_id = load_primary_session_id(store.as_ref(), &user_id)
        .await
        .unwrap_or_else(SessionId::new);
    let ctx_config = ContextConfig::new(cfg.llm.token_budget, cfg.llm.compaction_threshold);
    let cancel_registry = CancellationRegistry::new();

    let primary_vizier = kameo::spawn(Vizier::new(
        user_id.clone(),
        session_id.clone(),
        llm.clone(),
        tool_registry.clone(),
        capability_registry.clone(),
        model_registry.clone(),
        route_rules.clone(),
        store.clone(),
        support_rules.clone(),
        prompts.intent_classifier.clone(),
        prompts.capability_nudge.clone(),
        status_tx.clone(),
        cancel_registry.clone(),
    ));

    let automation_dir = cfg.automations.dir.clone();
    let automation_poll_seconds = cfg.automations.poll_seconds;
    let telegram_cfg = cfg.telegram.clone();
    let telegram_users = cfg.users.clone();
    let scheduler_store = store.clone();
    let pharoh = kameo::spawn(Pharoh::new(
        user_id,
        session_id,
        system_prompt,
        primary_vizier,
        llm,
        tool_registry,
        store,
        ctx_config,
        spec,
        model_registry,
        route_rules,
        support_rules,
        capability_registry,
        prompts.clone(),
        Arc::new(cfg),
        resolver.clone(),
        status_tx.clone(),
        cancel_registry,
    ).await);
    spawn_automation_scheduler(
        pharoh.clone(),
        scheduler_store.clone(),
        automation_dir.clone(),
        automation_poll_seconds,
    );

    // ── channels ───────────────────────────────────────────────────
    let (backend_tx, mut backend_rx) = mpsc::channel::<BackendRequest>(64);
    let (tui_tx, tui_rx) = mpsc::channel::<HeraldEvent>(32);
    let mut tui_status_rx = status_tx.subscribe();
    let tui_status_tx = tui_tx.clone();
    tokio::spawn(async move {
        while let Ok(event) = tui_status_rx.recv().await {
            if tui_status_tx.send(HeraldEvent::Status(event)).await.is_err() {
                break;
            }
        }
    });

    // ── bridge ─────────────────────────────────────────────────────
    tokio::spawn(async move {
        while let Some(request) = backend_rx.recv().await {
            tracing::debug!(channel = %request.channel.0, "backend request");
            let event = match pharoh.ask(Inbound(request.payload)).send().await {
                Ok(reply) => HeraldEvent::AssistantMessage(reply),
                Err(e) => {
                    error!("pharoh error: {e}");
                    HeraldEvent::Error(e.to_string())
                }
            };
            let _ = request.reply_tx.send(event).await;
        }
    });

    if telegram_cfg.enabled {
        let mut delivery_rx = status_tx.subscribe();
        let delivery_store = scheduler_store.clone();
        let delivery_dir = automation_dir.clone();
        let delivery_cfg = telegram_cfg.clone();
        let delivery_token = resolver
            .get(delivery_cfg.token_key())
            .or_else(|| std::env::var(delivery_cfg.token_env()).ok());
        tokio::spawn(async move {
            let Some(bot_token) = delivery_token else {
                return;
            };
            while let Ok(event) = delivery_rx.recv().await {
                let maat_core::StatusKind::RunCompleted {
                    automation_id: Some(automation_id),
                    session_name,
                    status,
                    summary,
                    error,
                    started_at_ms,
                    ..
                } = event.kind else {
                    continue;
                };

                let spec = match maat_config::find_automation(&delivery_dir, &automation_id) {
                    Ok(Some(spec)) => spec,
                    _ => continue,
                };
                let Some(AutomationDelivery::Telegram { chat_id }) = spec.delivery.clone() else {
                    continue;
                };
                let chat_id = chat_id.or(delivery_cfg.default_chat_id);
                let Some(chat_id) = chat_id else {
                    error!(automation = %spec.id, "telegram delivery requested but no chat_id configured");
                    continue;
                };

                let mut artifacts = Vec::new();
                if let Ok(Some(meta)) = delivery_store
                    .load_session_meta_by_user_and_name("user", &session_name)
                    .await
                {
                    if let Ok(Some(artifact)) = delivery_store.latest_session_artifact(&meta.session_id).await {
                        if artifact.created_at_ms >= started_at_ms {
                            artifacts.push(artifact);
                        }
                    }
                }

                let text = match status {
                    maat_core::BackgroundRunStatus::Completed => {
                        format!("Automation '{}': {}", spec.name, summary)
                    }
                    maat_core::BackgroundRunStatus::Failed => {
                        format!(
                            "Automation '{}' failed: {}",
                            spec.name,
                            error.unwrap_or_else(|| summary.clone())
                        )
                    }
                    maat_core::BackgroundRunStatus::Cancelled => {
                        format!("Automation '{}' was cancelled.", spec.name)
                    }
                    maat_core::BackgroundRunStatus::Queued | maat_core::BackgroundRunStatus::Running => {
                        continue;
                    }
                };

                if let Err(err) = maat_heralds::telegram::send_telegram_delivery(
                    &bot_token,
                    chat_id,
                    &text,
                    &artifacts,
                )
                .await {
                    error!(automation = %spec.id, ?err, "telegram automation delivery failed");
                }
            }
        });
    }

    if telegram_cfg.enabled {
        let bot_token = resolver
            .get(telegram_cfg.token_key())
            .or_else(|| std::env::var(telegram_cfg.token_env()).ok());
        match bot_token {
            Some(bot_token) => {
                let telegram_tx = backend_tx.clone();
                let telegram_store = scheduler_store.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        maat_heralds::telegram::run_telegram(telegram_tx, telegram_cfg, telegram_users, bot_token, telegram_store).await
                    {
                        error!(?error, "telegram herald exited");
                    }
                });
                info!("Telegram herald started");
            }
            None => {
                error!(
                    "Telegram enabled but no bot token found. Set {} or store secret {}",
                    telegram_cfg.token_env(),
                    telegram_cfg.token_key()
                );
            }
        }
    }

    // ── TUI ────────────────────────────────────────────────────────
    maat_heralds::tui::run_tui(backend_tx, tui_tx, tui_rx, model_id).await?;
    Ok(())
}

fn build_system_prompt(registry: &ToolRegistry, prompts: &PromptLibrary, user_id: &str) -> String {
    let defs = registry.all_definitions();
    if defs.is_empty() {
        return prompts.render_primary_system(user_id, "");
    }
    let tool_lines: Vec<String> = defs
        .iter()
        .map(|d| format!("  - {} — {}", d.name, d.description))
        .collect();
    prompts.render_primary_system(user_id, &tool_lines.join("\n"))
}

async fn load_primary_session_id(
    store: &dyn MemoryStore,
    user_id: &UserId,
) -> Option<SessionId> {
    let meta = store
        .load_session_meta_by_user_and_name(&user_id.0, "primary")
        .await
        .ok()
        .flatten()?;
    let parsed = meta.session_id.parse::<ulid::Ulid>().ok()?;
    Some(SessionId(parsed))
}

fn build_model_registry(cfg: &MaatConfig) -> ModelRegistry {
    let mut registry = ModelRegistry::new();

    registry.register_provider(ModelProviderSpec {
        id: "openrouter".into(),
        api_style: ProviderApiStyle::OpenAiCompat,
        base_url: cfg.llm.base_url.clone(),
        api_key_env: "OPENROUTER_API_KEY".into(),
    });

    registry.register_profile(ModelProfile {
        id: "default".into(),
        provider_id: "openrouter".into(),
        model_id: std::env::var("MAAT_MODEL").unwrap_or_else(|_| cfg.llm.model.clone()),
        temperature: 0.7,
        max_tokens: 4096,
        cost_tier: ModelCostTier::Standard,
        latency_tier: ModelLatencyTier::Balanced,
        reasoning_tier: ModelReasoningTier::Medium,
        context_window: cfg.llm.token_budget,
        supports_tool_calling: true,
        tags: vec!["default".into()],
        traits: vec![maat_core::ModelTrait::ToolCalling],
    });

    registry.register_profile(ModelProfile {
        id: "image_preview".into(),
        provider_id: "openrouter".into(),
        model_id: "google/gemini-3.1-flash-image-preview".into(),
        temperature: 0.4,
        max_tokens: 4096,
        cost_tier: ModelCostTier::Premium,
        latency_tier: ModelLatencyTier::Balanced,
        reasoning_tier: ModelReasoningTier::Medium,
        context_window: 32_768,
        supports_tool_calling: false,
        tags: vec!["image".into(), "generate".into(), "edit".into()],
        traits: vec![maat_core::ModelTrait::Vision],
    });

    registry.register_profile(ModelProfile {
        id: "gemini_flash".into(),
        provider_id: "openrouter".into(),
        model_id: "google/gemini-2.5-flash".into(),
        temperature: 0.4,
        max_tokens: 4096,
        cost_tier: ModelCostTier::Cheap,
        latency_tier: ModelLatencyTier::Fast,
        reasoning_tier: ModelReasoningTier::Medium,
        context_window: cfg.llm.token_budget,
        supports_tool_calling: true,
        tags: vec!["fast".into(), "cheap".into(), "routing".into()],
        traits: vec![maat_core::ModelTrait::ToolCalling, maat_core::ModelTrait::Vision],
    });

    registry.register_profile(ModelProfile {
        id: "codex".into(),
        provider_id: "openrouter".into(),
        model_id: "openai/gpt-5.3-codex".into(),
        temperature: 0.2,
        max_tokens: 8192,
        cost_tier: ModelCostTier::Premium,
        latency_tier: ModelLatencyTier::Balanced,
        reasoning_tier: ModelReasoningTier::Heavy,
        context_window: cfg.llm.token_budget,
        supports_tool_calling: true,
        tags: vec!["coding".into(), "agentic".into()],
        traits: vec![maat_core::ModelTrait::ToolCalling, maat_core::ModelTrait::StructuredOutput],
    });

    registry.register_profile(ModelProfile {
        id: "claude_sonnet".into(),
        provider_id: "openrouter".into(),
        model_id: "anthropic/claude-sonnet-4.5".into(),
        temperature: 0.4,
        max_tokens: 8192,
        cost_tier: ModelCostTier::Premium,
        latency_tier: ModelLatencyTier::Balanced,
        reasoning_tier: ModelReasoningTier::Heavy,
        context_window: cfg.llm.token_budget,
        supports_tool_calling: true,
        tags: vec!["reasoning".into(), "writing".into()],
        traits: vec![maat_core::ModelTrait::ToolCalling, maat_core::ModelTrait::StructuredOutput],
    });

    registry.register_profile(ModelProfile {
        id: "deepseek_v3".into(),
        provider_id: "openrouter".into(),
        model_id: "deepseek/deepseek-v3.2".into(),
        temperature: 0.3,
        max_tokens: 8192,
        cost_tier: ModelCostTier::Cheap,
        latency_tier: ModelLatencyTier::Balanced,
        reasoning_tier: ModelReasoningTier::Heavy,
        context_window: cfg.llm.token_budget,
        supports_tool_calling: true,
        tags: vec!["reasoning".into(), "budget".into()],
        traits: vec![maat_core::ModelTrait::ToolCalling, maat_core::ModelTrait::StructuredOutput],
    });

    for (provider_id, provider) in &cfg.llm.providers {
        registry.register_provider(ModelProviderSpec {
            id: provider_id.clone(),
            api_style: ProviderApiStyle::OpenAiCompat,
            base_url: provider.base_url.clone(),
            api_key_env: provider.api_key_env.clone(),
        });
    }

    for (profile_id, profile) in &cfg.llm.profiles {
        let inherited = registry.profile(profile_id).cloned();
        registry.register_profile(ModelProfile {
            id: profile_id.clone(),
            provider_id: profile.provider.clone(),
            model_id: profile.model_id.clone(),
            temperature: profile.temperature,
            max_tokens: profile.max_tokens,
            cost_tier: inherited
                .as_ref()
                .map(|profile| profile.cost_tier)
                .unwrap_or(ModelCostTier::Standard),
            latency_tier: inherited
                .as_ref()
                .map(|profile| profile.latency_tier)
                .unwrap_or(ModelLatencyTier::Balanced),
            reasoning_tier: inherited
                .as_ref()
                .map(|profile| profile.reasoning_tier)
                .unwrap_or(ModelReasoningTier::Medium),
            context_window: inherited
                .as_ref()
                .map(|profile| profile.context_window)
                .unwrap_or(cfg.llm.token_budget),
            supports_tool_calling: inherited
                .as_ref()
                .map(|profile| profile.supports_tool_calling)
                .unwrap_or(true),
            tags: profile.tags.clone(),
            traits: inherited
                .map(|profile| profile.traits)
                .unwrap_or_else(|| infer_profile_traits(profile_id, &profile.model_id, &profile.tags)),
        });
    }

    registry.set_default_profile(
        cfg.llm.routing.default_profile.clone().unwrap_or_else(|| "default".into())
    );
    registry
}

fn infer_profile_traits(profile_id: &str, model_id: &str, tags: &[String]) -> Vec<maat_core::ModelTrait> {
    let mut traits = Vec::new();
    let tags_lower = tags
        .iter()
        .map(|tag| tag.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let profile_lower = profile_id.to_ascii_lowercase();
    let model_lower = model_id.to_ascii_lowercase();

    let has = |needle: &str| {
        profile_lower.contains(needle)
            || model_lower.contains(needle)
            || tags_lower.iter().any(|tag| tag.contains(needle))
    };

    if has("vision") || has("image") {
        traits.push(maat_core::ModelTrait::Vision);
    }
    if has("tool") || has("agent") || has("codex") {
        traits.push(maat_core::ModelTrait::ToolCalling);
    }
    if has("reason") {
        traits.push(maat_core::ModelTrait::Reasoning);
    }
    if has("fast") || has("flash") {
        traits.push(maat_core::ModelTrait::FastResponse);
    }
    if has("cheap") || has("budget") {
        traits.push(maat_core::ModelTrait::LowCost);
    }

    let mut deduped = Vec::new();
    for trait_value in traits {
        if !deduped.contains(&trait_value) {
            deduped.push(trait_value);
        }
    }
    deduped
}

fn build_model_route_rules(cfg: &MaatConfig) -> Vec<ModelRouteRule> {
    let mut rules = Vec::new();

    let global_policy = ModelSelectionPolicy {
        preferred_profiles: Vec::new(),
        allow_profiles: cfg.llm.routing.allow_profiles.clone(),
        deny_profiles: cfg.llm.routing.deny_profiles.clone(),
        required_traits: Vec::new(),
        max_cost_tier: None,
        max_latency_tier: None,
        min_reasoning_tier: None,
        require_tool_calling: None,
    };
    if !global_policy.allow_profiles.is_empty() || !global_policy.deny_profiles.is_empty() {
        rules.push(ModelRouteRule {
            scope: ModelRouteScope::Global,
            policy: global_policy,
            fallback_profile: cfg.llm.routing.default_profile.clone(),
        });
    }

    if let Some(profile) = &cfg.llm.routing.pharoh_profile {
        rules.push(preferred_profile_rule(
            ModelRouteScope::PharohPrimary,
            profile.clone(),
        ));
    }
    if let Some(profile) = &cfg.llm.routing.intent_classifier_profile {
        rules.push(preferred_profile_rule(
            ModelRouteScope::IntentClassifier,
            profile.clone(),
        ));
    }
    if let Some(profile) = &cfg.llm.routing.session_default_profile {
        rules.push(preferred_profile_rule(
            ModelRouteScope::SessionDefault,
            profile.clone(),
        ));
    }
    if let Some(profile) = &cfg.llm.routing.planner_profile {
        rules.push(preferred_profile_rule(
            ModelRouteScope::Planner,
            profile.clone(),
        ));
    }
    if let Some(profile) = &cfg.llm.routing.capability_nudge_profile {
        rules.push(preferred_profile_rule(
            ModelRouteScope::CapabilityNudge,
            profile.clone(),
        ));
    }

    rules.push(preferred_profile_rule(
        ModelRouteScope::Intent("image_generate".into()),
        "image_preview".into(),
    ));

    rules.push(preferred_profile_rule(
        ModelRouteScope::Intent("image_edit".into()),
        "image_preview".into(),
    ));

    for (route_key, route) in &cfg.llm.routing.routes {
        if let Some(scope) = parse_route_scope(route_key) {
            rules.push(ModelRouteRule {
                scope,
                policy: ModelSelectionPolicy {
                    preferred_profiles: route.prefer_profiles.clone(),
                    allow_profiles: route.allow_profiles.clone(),
                    deny_profiles: route.deny_profiles.clone(),
                    required_traits: Vec::new(),
                    max_cost_tier: None,
                    max_latency_tier: None,
                    min_reasoning_tier: None,
                    require_tool_calling: None,
                },
                fallback_profile: route.fallback_profile.clone(),
            });
        }
    }

    rules
}

fn spawn_automation_scheduler(
    pharoh: kameo::actor::ActorRef<Pharoh>,
    store: Arc<dyn MemoryStore>,
    dir: String,
    poll_seconds: u64,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_seconds.max(10)));
        loop {
            interval.tick().await;
            let specs = match load_automations(&dir) {
                Ok(specs) => specs,
                Err(error) => {
                    error!(%error, "automation load failed");
                    continue;
                }
            };

            for spec in specs.into_iter().filter(|spec| spec.status == AutomationStatus::Active) {
                let due = match store.latest_automation_run(&spec.id).await {
                    Ok(Some(run)) => {
                        is_schedule_due(&spec.schedule, Some(run.finished_at_ms), maat_core::now_ms())
                    }
                    Ok(None) => is_schedule_due(&spec.schedule, None, maat_core::now_ms()),
                    Err(error) => {
                        error!(%error, automation = %spec.id, "automation latest run lookup failed");
                        false
                    }
                };
                if !due {
                    continue;
                }

                let _ = pharoh
                    .ask(Inbound(HeraldPayload::Command(ParsedCommand::AutomationRun {
                        name: spec.id.clone(),
                    })))
                    .send()
                    .await;
            }
        }
    });
}

fn build_support_rules(cfg: &MaatConfig) -> Vec<SupportCapabilityRule> {
    cfg.llm
        .routing
        .support_rules
        .iter()
        .map(|rule| SupportCapabilityRule {
            match_any_terms: rule.match_any_terms.clone(),
            capability_ids: rule
                .capability_ids
                .iter()
                .cloned()
                .map(maat_core::CapabilityId)
                .collect(),
        })
        .collect()
}

fn preferred_profile_rule(scope: ModelRouteScope, profile: String) -> ModelRouteRule {
    ModelRouteRule {
        scope,
        policy: ModelSelectionPolicy {
            preferred_profiles: vec![profile.clone()],
            allow_profiles: Vec::new(),
            deny_profiles: Vec::new(),
            required_traits: Vec::new(),
            max_cost_tier: None,
            max_latency_tier: None,
            min_reasoning_tier: None,
            require_tool_calling: None,
        },
        fallback_profile: Some(profile),
    }
}

fn parse_route_scope(route_key: &str) -> Option<ModelRouteScope> {
    match route_key {
        "global" => Some(ModelRouteScope::Global),
        "pharoh" => Some(ModelRouteScope::PharohPrimary),
        "session_default" => Some(ModelRouteScope::SessionDefault),
        "planner" => Some(ModelRouteScope::Planner),
        "intent_classifier" => Some(ModelRouteScope::IntentClassifier),
        "capability_nudge" => Some(ModelRouteScope::CapabilityNudge),
        "summarizer" => Some(ModelRouteScope::Summarizer),
        _ => {
            let (prefix, value) = route_key.split_once(':')?;
            match prefix {
                "capability" => Some(ModelRouteScope::Capability(maat_core::CapabilityId(value.to_string()))),
                "intent" => Some(ModelRouteScope::Intent(value.to_string())),
                "capability_tag" => Some(ModelRouteScope::CapabilityTag(value.to_string())),
                "talent" => Some(ModelRouteScope::Talent(value.to_string())),
                "skill" => Some(ModelRouteScope::Skill(value.to_string())),
                "session" => Some(ModelRouteScope::SessionNamed(value.to_string())),
                _ => None,
            }
        }
    }
}
