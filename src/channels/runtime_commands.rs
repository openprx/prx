//! Runtime `/models` and `/providers` commands, provider cache and route selection.

use super::history::clear_sender_history;
use super::inbound_policy::ConversationKey;
use super::supervisor::send_recording_activity;
use super::{
    Arc, Channel, ChannelMessageRuntimeSnapshot, ChannelRouteSelection, ChannelRuntimeCommand, ChannelRuntimeContext,
    ChannelRuntimeDefaults, Config, MODEL_CACHE_FILE, MODEL_CACHE_PREVIEW_LIMIT, ModelCacheState, Path, Provider,
    SendMessage, Write, providers, traits,
};
#[cfg(test)]
use super::{HashMap, tests};

fn supports_runtime_model_switch(channel_name: &str) -> bool {
    matches!(channel_name, "telegram" | "discord")
}

fn parse_runtime_command(channel_name: &str, content: &str) -> Option<ChannelRuntimeCommand> {
    if !supports_runtime_model_switch(channel_name) {
        return None;
    }

    let trimmed = content.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let command_token = parts.next()?;
    let base_command = command_token
        .split('@')
        .next()
        .unwrap_or(command_token)
        .to_ascii_lowercase();

    match base_command.as_str() {
        "/models" => parts
            .next()
            .map_or(Some(ChannelRuntimeCommand::ShowProviders), |provider| {
                Some(ChannelRuntimeCommand::SetProvider(provider.trim().to_string()))
            }),
        "/model" => {
            let model = parts.collect::<Vec<_>>().join(" ").trim().to_string();
            if model.is_empty() {
                Some(ChannelRuntimeCommand::ShowModel)
            } else {
                Some(ChannelRuntimeCommand::SetModel(model))
            }
        }
        _ => None,
    }
}

/// The provider a `/models <name>` switch selects: a catalog provider only
/// (a chat message never names a URL), spelled by its catalog row unless the
/// spelling selects a different regional endpoint, which it then keeps — so
/// `/models qwen-us` switches to the US endpoint, not to `qwen`.
pub(super) fn resolve_provider_alias(name: &str) -> Option<String> {
    let candidate = name.trim().to_ascii_lowercase();
    let identity = providers::ProviderId::of(&candidate);
    if identity.namespace() != providers::ProviderNamespace::Catalog {
        return None;
    }
    let row = identity.catalog_name()?;
    if providers::ProviderId::of(row) == identity {
        Some(row.to_string())
    } else {
        Some(candidate)
    }
}

pub(super) fn resolved_default_provider(config: &Config) -> String {
    providers::selection::default_provider(config).to_string()
}

pub(super) fn resolved_default_model(config: &Config) -> String {
    config
        .default_model
        .clone()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string())
}

fn runtime_defaults_from_config(config: &Config) -> ChannelRuntimeDefaults {
    ChannelRuntimeDefaults {
        default_provider: resolved_default_provider(config),
        model: resolved_default_model(config),
        temperature: config.default_temperature,
    }
}

pub(super) fn runtime_defaults_for_generation(
    ctx: &ChannelRuntimeContext,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> ChannelRuntimeDefaults {
    #[cfg(test)]
    if !Arc::ptr_eq(&ctx.config.pin(), &ctx.config_generation) {
        // Legacy unit fixtures historically constructed `config` and
        // `config_generation` from two independent managers, then injected
        // explicit provider/model/runtime fields on the context. Preserve those
        // fixture semantics while dedicated generation tests use a same-owner
        // pair and therefore exercise the production path below.
        return ChannelRuntimeDefaults {
            default_provider: ctx.default_provider.as_str().to_string(),
            model: ctx.model.as_str().to_string(),
            temperature: ctx.temperature,
        };
    }
    #[cfg(not(test))]
    let _ = ctx;
    runtime_defaults_from_config(&generation.effective)
}

pub(super) fn message_runtime_snapshot(
    ctx: &ChannelRuntimeContext,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> ChannelMessageRuntimeSnapshot {
    #[cfg(test)]
    if !Arc::ptr_eq(&ctx.config.pin(), &ctx.config_generation) {
        return ChannelMessageRuntimeSnapshot {
            multimodal: ctx.multimodal.clone(),
            read_only_tool_concurrency_window: ctx.read_only_tool_concurrency_window,
            priority_scheduling_enabled: ctx.priority_scheduling_enabled,
            low_priority_tools: ctx.low_priority_tools.clone(),
            tool_tiering: ctx.tool_tiering.for_channel_surface(),
        };
    }
    #[cfg(not(test))]
    let _ = ctx;

    let config = generation.effective.as_ref();
    ChannelMessageRuntimeSnapshot {
        multimodal: config.multimodal.clone(),
        read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
        priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
        low_priority_tools: config.agent.low_priority_tools.clone(),
        // IM channel turns run under the channel tiering policy: the
        // operations/development surface is folded into `always_exclude` here,
        // once, so capability routing and the prompt compilers below cannot
        // disagree about what this turn may reach.
        tool_tiering: config.tool_tiering.for_channel_surface(),
    }
}

fn default_route_selection_for_generation(
    ctx: &ChannelRuntimeContext,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> ChannelRouteSelection {
    let defaults = runtime_defaults_for_generation(ctx, generation);
    ChannelRouteSelection {
        provider: defaults.default_provider,
        model: defaults.model,
    }
}

pub(super) fn get_route_selection_for_generation(
    ctx: &ChannelRuntimeContext,
    key: &ConversationKey,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> ChannelRouteSelection {
    let routes = ctx.route_overrides.lock();
    // Read-merge for the single-value route map: prefer the canonical override,
    // fall back to the legacy (sender-scoped) override as read-only pre-history
    // so a route preference set before the canonical cutover still applies to
    // every recipient of that sender. The legacy entry is never moved or removed.
    routes
        .get(&key.canonical)
        .or_else(|| {
            if key.legacy == key.canonical {
                None
            } else {
                routes.get(&key.legacy)
            }
        })
        .cloned()
        .unwrap_or_else(|| default_route_selection_for_generation(ctx, generation))
}

pub(super) fn set_route_selection(
    ctx: &ChannelRuntimeContext,
    generation: &Arc<crate::config::ConfigGeneration>,
    key: &ConversationKey,
    next: ChannelRouteSelection,
) {
    let default_route = default_route_selection_for_generation(ctx, generation);
    let mut routes = ctx.route_overrides.lock();
    if next == default_route {
        routes.remove(&key.canonical);
    } else {
        routes.insert(key.canonical.clone(), next);
    }
}

fn load_cached_model_preview(workspace_dir: &Path, provider_name: &str) -> Vec<String> {
    let cache_path = workspace_dir.join("state").join(MODEL_CACHE_FILE);
    let Ok(raw) = std::fs::read_to_string(cache_path) else {
        return Vec::new();
    };
    let Ok(state) = serde_json::from_str::<ModelCacheState>(&raw) else {
        return Vec::new();
    };

    state
        .entries
        .into_iter()
        .find(|entry| entry.provider == provider_name)
        .map(|entry| {
            entry
                .models
                .into_iter()
                .take(MODEL_CACHE_PREVIEW_LIMIT)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub(super) async fn get_or_create_provider_for_generation(
    ctx: &ChannelRuntimeContext,
    provider_name: &str,
    model_name: &str,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> anyhow::Result<Arc<dyn Provider>> {
    providers::generation::get_or_create_provider_for_generation(
        &ctx.config,
        &ctx.provider_cache,
        &ctx.provider_runtime_options,
        provider_name,
        model_name,
        generation,
    )
    .await
}

/// A hint is one selected route, not a provider-wide chain. Constructing the
/// selected destination separately keeps two hints for the same provider from
/// sharing whichever credential was cached first.
fn build_channel_hint_provider(
    config: &Config,
    selected_provider: &str,
    selected_model: &str,
    options: &providers::ProviderRuntimeOptions,
) -> anyhow::Result<Option<(ChannelRouteSelection, Arc<dyn Provider>)>> {
    if !selected_model.trim().starts_with("hint:") {
        return Ok(None);
    }
    let catalog = providers::selection::catalog_for(config);
    let resolved = providers::selection::check_with(
        config,
        &catalog,
        selected_provider,
        selected_model,
        providers::selection::credential_for(config, selected_provider),
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
    let provider = providers::create_provider_for(&resolved, &catalog, &config.reliability, &config.cost, options)?;
    let route = ChannelRouteSelection {
        provider: resolved.provider_label().to_string(),
        model: resolved.model().to_string(),
    };
    Ok(Some((route, Arc::from(provider))))
}

/// Build the default channel provider from the exact resolver release used by
/// channel turns. The returned names are the wire destination after a route
/// hint has been resolved.
pub(crate) fn build_channel_startup_provider(
    config: &Config,
    options: &providers::ProviderRuntimeOptions,
) -> anyhow::Result<(Arc<dyn Provider>, String, String)> {
    let catalog = providers::selection::catalog_for(config);
    let resolved = providers::selection::check_default(config, providers::catalog::UnknownPolicy::Invocation)?;
    let provider: Arc<dyn Provider> = Arc::from(providers::create_routed_provider_for(
        &resolved, &catalog, config, options,
    )?);
    Ok((
        provider,
        resolved.provider_label().to_string(),
        resolved.model().to_string(),
    ))
}

fn build_models_help_response(current: &ChannelRouteSelection, workspace_dir: &Path) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.display_provider(),
        current.model
    );
    response.push_str("\nSwitch model with `/model <model-id>`.\n");

    let cached_models = load_cached_model_preview(workspace_dir, &current.provider);
    if cached_models.is_empty() {
        let _ = writeln!(
            response,
            "\nNo cached model list found for `{}`. Ask the operator to run `prx models refresh --provider {}`.",
            current.display_provider(),
            current.display_provider()
        );
    } else {
        let _ = writeln!(response, "\nCached model IDs (top {}):", cached_models.len());
        for model in cached_models {
            let _ = writeln!(response, "- `{model}`");
        }
    }

    response
}

pub(super) fn build_providers_help_response(current: &ChannelRouteSelection) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.display_provider(),
        current.model
    );
    response.push_str("\nSwitch provider with `/models <provider>`.\n");
    response.push_str("Switch model with `/model <model-id>`.\n\n");
    response.push_str("Available providers:\n");
    for provider in providers::list_providers() {
        if provider.aliases.is_empty() {
            let _ = writeln!(response, "- {}", provider.name);
        } else {
            let _ = writeln!(
                response,
                "- {} (aliases: {})",
                provider.name,
                provider.aliases.join(", ")
            );
        }
    }
    response
}

pub(super) async fn handle_runtime_command_if_needed(
    ctx: &ChannelRuntimeContext,
    config_generation: &Arc<crate::config::ConfigGeneration>,
    msg: &traits::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> bool {
    let Some(command) = parse_runtime_command(&msg.channel, &msg.content) else {
        return false;
    };

    let Some(channel) = target_channel else {
        return true;
    };

    let sender_key = ConversationKey::from_message(msg);
    let mut current = get_route_selection_for_generation(ctx, &sender_key, config_generation);

    let response = match command {
        ChannelRuntimeCommand::ShowProviders => build_providers_help_response(&current),
        ChannelRuntimeCommand::SetProvider(raw_provider) => match resolve_provider_alias(&raw_provider) {
            Some(provider_name) => {
                // The new provider is resolved with the sender's current model
                // before anything is built or switched.
                let catalog = providers::selection::catalog_for(&config_generation.effective);
                let route = providers::selection::route_target(&catalog, &provider_name, &current.model)
                    .ok()
                    .flatten();
                let refusal = providers::selection::check_with(
                    &config_generation.effective,
                    &catalog,
                    &provider_name,
                    &current.model,
                    providers::selection::credential_for(&config_generation.effective, &provider_name),
                    &providers::catalog::Requirement::chat(),
                    providers::catalog::UnknownPolicy::Invocation,
                )
                .map_err(|refusal| providers::selection::route_refusal(route, refusal))
                .err();
                if let Some(refusal) = refusal {
                    format!(
                        "{refusal}. Route unchanged; switch the model first with `/model <model-id>` or pick \
                         another provider."
                    )
                } else {
                    let ready = if current.model.trim().starts_with("hint:") {
                        build_channel_hint_provider(
                            &config_generation.effective,
                            &provider_name,
                            &current.model,
                            &providers::provider_runtime_options_from_config(&config_generation.effective),
                        )
                        .map(|_| ())
                    } else {
                        get_or_create_provider_for_generation(ctx, &provider_name, &current.model, config_generation)
                            .await
                            .map(|_| ())
                    };
                    match ready {
                        Ok(_) => {
                            if provider_name != current.provider {
                                current.provider = provider_name.clone();
                                set_route_selection(ctx, config_generation, &sender_key, current.clone());
                                clear_sender_history(ctx, &sender_key);
                            }

                            format!(
                                "Provider switched to `{provider_name}` for this sender session. Current model is `{}`.\nUse `/model <model-id>` to set a provider-compatible model.",
                                current.model
                            )
                        }
                        Err(err) => {
                            let safe_err = providers::sanitize_api_error(&err.to_string());
                            format!(
                                "Failed to initialize provider `{provider_name}`. Route unchanged.\nDetails: {safe_err}"
                            )
                        }
                    }
                }
            }
            None => format!("Unknown provider `{raw_provider}`. Use `/models` to list valid providers."),
        },
        ChannelRuntimeCommand::ShowModel => build_models_help_response(&current, ctx.workspace_dir.as_path()),
        ChannelRuntimeCommand::SetModel(raw_model) => {
            let model = raw_model.trim().trim_matches('`').trim().to_string();
            let refusal = (!model.is_empty())
                .then(|| {
                    providers::selection::check(
                        &config_generation.effective,
                        &current.provider,
                        &model,
                        &providers::catalog::Requirement::chat(),
                        providers::catalog::UnknownPolicy::Invocation,
                    )
                    .err()
                })
                .flatten();
            if model.is_empty() {
                "Model ID cannot be empty. Use `/model <model-id>`.".to_string()
            } else if let Some(refusal) = refusal {
                format!("{refusal}. Route unchanged.")
            } else {
                current.model = model.clone();
                set_route_selection(ctx, config_generation, &sender_key, current.clone());
                clear_sender_history(ctx, &sender_key);

                format!(
                    "Model switched to `{model}` for provider `{}` in this sender session.",
                    current.display_provider()
                )
            }
        }
    };

    if let Err(err) = send_recording_activity(
        channel.as_ref(),
        &SendMessage::new(response, &msg.reply_target).in_thread(msg.thread_ts.clone()),
    )
    .await
    {
        tracing::warn!("Failed to send runtime command response on {}: {err}", channel.name());
    }

    true
}

/// Exercise the production `/models` handler with a retained route refusal.
/// Kept test-only because the runtime owns its context and outbound channel.
#[cfg(test)]
pub(crate) async fn broken_route_models_command_response(config: &Config) -> String {
    #[derive(Default)]
    struct RefusalChannel {
        sent: tokio::sync::Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl Channel for RefusalChannel {
        fn name(&self) -> &str {
            "telegram"
        }
        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().await.push(message.content.clone());
            Ok(())
        }
        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let shared = crate::config::new_shared(config.clone());
    let generation = shared.pin();
    let channel_impl = Arc::new(RefusalChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut ctx = tests::ctx_with_histories(HashMap::new());
    ctx.config = shared;
    ctx.config_generation = Arc::clone(&generation);

    let msg = traits::ChannelMessage {
        id: "prx-broken-route-command".to_string(),
        sender: "prx_user".to_string(),
        reply_target: "prx_channel".to_string(),
        content: "/models openai".to_string(),
        channel: "telegram".to_string(),
        ..traits::ChannelMessage::default()
    };
    let key = ConversationKey::from_message(&msg);
    set_route_selection(
        &ctx,
        &generation,
        &key,
        ChannelRouteSelection {
            provider: providers::selection::default_provider(config).to_string(),
            model: "hint:broken".to_string(),
        },
    );
    assert!(
        handle_runtime_command_if_needed(&ctx, &generation, &msg, Some(&channel)).await,
        "the production /models handler did not claim the command"
    );
    channel_impl
        .sent
        .lock()
        .await
        .last()
        .cloned()
        .expect("the production /models handler did not send a refusal")
}
