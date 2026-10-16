//! Channel runtime start-up.

#[cfg(feature = "channel-matrix")]
use super::MatrixChannel;
#[cfg(test)]
use super::SecurityGen;
#[cfg(feature = "whatsapp-web")]
use super::WhatsAppWebChannel;
use super::dispatch::run_message_dispatch_loop_with_cleanup;
use super::history::load_persisted_histories;
use super::inbound_policy::{
    collect_bot_names, collect_bot_uuids, collect_group_reply_mode_by_channel, collect_mention_only_by_channel,
    normalize_allowlist, normalize_dm_policy, warn_open_policy_allowlist_alignment,
};
use super::runtime_commands::{build_channel_startup_provider, resolved_default_model, resolved_default_provider};
use super::supervisor::spawn_supervised_listener;
use super::{
    Arc, CHANNEL_INBOUND_QUEUE_CAPACITY, CancellationToken, Channel, ChannelRuntimeContext, Config,
    DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS, DEFAULT_CHANNEL_MAX_BACKOFF_SECS, DingTalkChannel, DiscordChannel,
    EmailChannel, HashMap, HookManager, IMessageChannel, InboundPolicyConfig, IrcChannel, LarkChannel, LinqChannel,
    MattermostChannel, Memory, Mutex, NextcloudTalkChannel, Observer, Provider, ProviderCacheKey, ProviderCacheMap,
    QQChannel, Result, SignalChannel, SignalNativeChannel, SlackChannel, TelegramChannel, WacliChannel,
    WhatsAppChannel, decision_runtime, irc, memory, observability, outbound_registry, providers, runtime, tools,
    traits, wacli,
};

/// Resolve the channel runtime's default provider and model for chat before
/// anything is built, so a default that cannot chat stops start-up (an
/// existing id-only default keeps running, with the resolver's warning).
pub(super) fn check_channel_startup_default(config: &Config) -> Result<()> {
    providers::selection::check(
        config,
        &resolved_default_provider(config),
        &resolved_default_model(config),
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
    Ok(())
}

/// Start all configured channels and route messages to the agent
pub async fn start_channels(config: Config, shutdown: CancellationToken) -> Result<()> {
    let shared_config = crate::config::new_shared(config.clone());
    let generation = shared_config.pin();
    start_channels_with_config(config, shared_config, generation, shutdown).await
}

#[allow(clippy::too_many_lines)]
pub async fn start_channels_with_config(
    config: Config,
    shared_config: crate::config::SharedConfig,
    config_generation: Arc<crate::config::ConfigGeneration>,
    shutdown: CancellationToken,
) -> Result<()> {
    #[cfg(not(test))]
    let _ = &config_generation;
    let provider_name = resolved_default_provider(&config);
    #[cfg(feature = "wasm-plugins")]
    let wasm_early_runtime = if provider_name.starts_with("wasm:")
        || memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config))
            .starts_with("wasm:")
    {
        crate::plugins::init_plugin_runtime(&config.workspace_dir, None).await
    } else {
        None
    };
    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);
    check_channel_startup_default(&config)?;
    let default_is_hint = resolved_default_model(&config).trim().starts_with("hint:");
    let (provider, turn_provider_name, model) = build_channel_startup_provider(&config, &provider_runtime_options)?;

    // Warm up the provider connection pool (TLS handshake, DNS, HTTP/2 setup)
    // so the first real message doesn't hit a cold-start timeout.
    if let Err(e) = provider.warmup().await {
        tracing::warn!("Provider warmup failed (non-fatal): {e}");
    }

    let observer: Arc<dyn Observer> = Arc::from(observability::create_observer(&config.observability));
    let hooks = Arc::new(HookManager::new(config.workspace_dir.clone()).with_config(Arc::clone(&shared_config)));
    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    // FIX-P1-31: thread the real `security.audit` config so the side-effect gate
    // audit trail honours it (e.g. `enabled=false` ⇒ no per-message fsync).
    // BUG-D1-01 class: route through the single `build_security_policy` helper so
    // no mode can forget `with_audit_config`. Wiring is verbatim-identical to the
    // former local construction (gateway uses the same helper).
    //
    // Channel-aware tools built for this component generation retain this policy
    // until the daemon supervisor replaces the component. Per-message channel
    // gates independently derive their policy from the generation pinned at
    // admission, so one turn never mixes security generations.
    let security = crate::runtime::bootstrap::build_security_policy(&config);
    let temperature = config.default_temperature;
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage(
        &config.memory,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        memory::EmbeddingRootCredential::from_config(&config),
    )?);
    #[cfg(feature = "wasm-plugins")]
    let wasm_plugin_runtime = if let Some(runtime) = wasm_early_runtime {
        runtime
            .attach_memory(Arc::clone(&mem))
            .await
            .map_err(|error| anyhow::anyhow!("failed to attach WASM memory backend in channels: {error}"))?;
        Some(runtime)
    } else {
        crate::plugins::init_plugin_runtime(&config.workspace_dir, Some(Arc::clone(&mem))).await
    };
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        hooks.set_plugin_runtime(Arc::clone(runtime)).await;
    }
    let conversation_histories = Arc::new(Mutex::new(
        load_persisted_histories(&config.workspace_dir, mem.as_ref()).await,
    ));
    let (composio_key, composio_entity_id) = if config.composio.configured() {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };
    // Build system prompt from workspace identity files + skills
    let workspace = config.workspace_dir.clone();
    // Keep as mutable Vec so we can append channel-aware tools (e.g. sessions_spawn)
    // before wrapping in Arc below.
    #[allow(unused_mut)]
    let mut extensions = hooks.control_tool_arcs(Arc::clone(&security));
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        extensions.extend(runtime.control_tool_arcs(Arc::clone(&security)));
    }
    let tool_build = tools::all_tools_with_runtime_ext_and_extensions(
        Arc::new(config.clone()),
        Arc::clone(&shared_config),
        &security,
        runtime,
        Arc::clone(&mem),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &workspace,
        &config.agents_with_references(),
        config.api_key.as_deref(),
        &config,
        extensions,
    );
    let mcp_tool = tool_build.mcp_tool;
    let mut tools_list = tool_build.tools;

    // Skill metadata is published with the decision catalog; prompts are built
    // from each admitted selection. Startup performs no embedding selection.

    // Collect active channels
    let mut channels: Vec<Arc<dyn Channel>> = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        channels.push(Arc::new(
            TelegramChannel::new(tg.bot_token.clone(), tg.allowed_users.clone(), tg.mention_only)
                .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
                .with_group_reply_mode(crate::config::GroupReplyMode::resolve(
                    tg.group_reply_mode,
                    tg.mention_only,
                )),
        ));
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push(Arc::new(
            DiscordChannel::new(
                dc.bot_token.clone(),
                dc.guild_id.clone(),
                dc.allowed_users.clone(),
                dc.listen_to_bots,
                dc.mention_only,
            )
            .with_group_reply_mode(crate::config::GroupReplyMode::resolve(
                dc.group_reply_mode,
                dc.mention_only,
            )),
        ));
    }

    if let Some(ref sl) = config.channels_config.slack {
        channels.push(Arc::new(
            SlackChannel::new(sl.bot_token.clone(), sl.channel_id.clone(), sl.allowed_users.clone())
                .with_workspace_dir(config.workspace_dir.clone()),
        ));
    }

    if let Some(ref mm) = config.channels_config.mattermost {
        channels.push(Arc::new(MattermostChannel::new(
            mm.url.clone(),
            mm.bot_token.clone(),
            mm.channel_id.clone(),
            mm.allowed_users.clone(),
            mm.thread_replies.unwrap_or(true),
            mm.mention_only.unwrap_or(false),
        )));
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push(Arc::new(IMessageChannel::new(im.allowed_contacts.clone())));
    }

    #[cfg(feature = "channel-matrix")]
    if let Some(ref mx) = config.channels_config.matrix {
        channels.push(Arc::new(MatrixChannel::new_with_session_hint(
            mx.homeserver.clone(),
            mx.access_token.clone(),
            mx.room_id.clone(),
            mx.allowed_users.clone(),
            mx.user_id.clone(),
            mx.device_id.clone(),
        )));
    }

    #[cfg(not(feature = "channel-matrix"))]
    if config.channels_config.matrix.is_some() {
        tracing::warn!(
            "Matrix channel is configured but this build was compiled without `channel-matrix`; skipping Matrix runtime startup."
        );
    }

    if let Some(ref sig) = config.channels_config.signal {
        let media_artifacts = crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir);
        let signal_channel: Arc<dyn Channel + Send + Sync> = if sig.is_native_mode() {
            tracing::info!(
                "Signal: native mode — will spawn signal-cli daemon on port {}",
                sig.daemon_http_port.unwrap_or(16866)
            );
            Arc::new(
                SignalNativeChannel::new(
                    sig.cli_path.clone().unwrap_or_else(|| "signal-cli".to_string()),
                    sig.account.clone(),
                    sig.data_dir.clone(),
                    sig.daemon_http_port.unwrap_or(16866),
                    sig.startup_timeout_ms,
                    sig.group_id.clone(),
                    sig.allowed_from.clone(),
                    sig.ignore_attachments,
                    sig.ignore_stories,
                    config.media.clone(),
                    sig.storm_protection.clone(),
                )
                .with_artifact_owner(media_artifacts),
            )
        } else {
            Arc::new(
                SignalChannel::new_with_storm_protection(
                    sig.effective_http_url(),
                    sig.account.clone(),
                    sig.group_id.clone(),
                    sig.allowed_from.clone(),
                    sig.ignore_attachments,
                    sig.ignore_stories,
                    config.media.clone(),
                    sig.storm_protection.clone(),
                )
                .with_artifact_owner(media_artifacts),
            )
        };
        channels.push(signal_channel);
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        if wa.is_ambiguous_config() {
            tracing::warn!(
                "WhatsApp config has both phone_number_id and session_path set; preferring Cloud API mode. Remove one selector to avoid ambiguity."
            );
        }
        // Runtime negotiation: detect backend type from config
        match wa.backend_type() {
            "cloud" => {
                // Cloud API mode: requires phone_number_id, access_token, verify_token
                if wa.is_cloud_config() {
                    channels.push(Arc::new(WhatsAppChannel::new(
                        wa.access_token.clone().unwrap_or_default(),
                        wa.phone_number_id.clone().unwrap_or_default(),
                        wa.verify_token.clone().unwrap_or_default(),
                        wa.allowed_numbers.clone(),
                    )));
                } else {
                    tracing::warn!(
                        "WhatsApp Cloud API configured but missing required fields (phone_number_id, access_token, verify_token)"
                    );
                }
            }
            "web" => {
                // Web mode: requires session_path
                #[cfg(feature = "whatsapp-web")]
                if wa.is_web_config() {
                    channels.push(Arc::new(
                        WhatsAppWebChannel::new(
                            wa.session_path.clone().unwrap_or_default(),
                            wa.pair_phone.clone(),
                            wa.pair_code.clone(),
                            wa.allowed_numbers.clone(),
                        )
                        .with_ws_url(wa.ws_url.clone())
                        .with_read_pool_size(wa.read_pool_size),
                    ));
                } else {
                    tracing::warn!("WhatsApp Web configured but session_path not set");
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    tracing::warn!(
                        "WhatsApp Web backend requires 'whatsapp-web' feature. Enable with: cargo build --features whatsapp-web"
                    );
                }
            }
            _ => {
                tracing::warn!(
                    "WhatsApp config invalid: neither phone_number_id (Cloud API) nor session_path (Web) is set"
                );
            }
        }
    }

    if let Some(ref lq) = config.channels_config.linq {
        channels.push(Arc::new(LinqChannel::new(
            lq.api_token.clone(),
            lq.from_phone.clone(),
            lq.allowed_senders.clone(),
        )));
    }

    if let Some(ref wc) = config.channels_config.wacli {
        let (_, max_image_size_mb) = config.multimodal.effective_limits();
        channels.push(Arc::new(
            WacliChannel::new(wacli::WacliChannelConfig {
                webhook_listen: wc.webhook_listen.clone(),
                webhook_path: wc.webhook_path.clone(),
                webhook_secret: wc.webhook_secret.clone(),
                allow_unsigned_loopback: wc.allow_unsigned_loopback,
                allowed_from: wc.allowed_from.clone(),
                cli_path: wc.cli_path.clone(),
                store_dir: wc.store_dir.clone(),
                bot_jid: wc.bot_jid.clone(),
                bot_number: wc.bot_number.clone(),
                bot_lid: wc.bot_lid.clone(),
                self_chat_mode: wc.self_chat_mode,
                outbound_media_workspace_only: wc.outbound_media_workspace_only,
            })
            .with_media_artifacts(
                crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir),
                max_image_size_mb.saturating_mul(1024 * 1024),
            ),
        ));
    }

    if let Some(ref nc) = config.channels_config.nextcloud_talk {
        channels.push(Arc::new(NextcloudTalkChannel::new(
            nc.base_url.clone(),
            nc.app_token.clone(),
            nc.allowed_users.clone(),
        )));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push(Arc::new(EmailChannel::new(email_cfg.clone())));
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push(Arc::new(IrcChannel::new(irc::IrcChannelConfig {
            server: irc.server.clone(),
            port: irc.port,
            nickname: irc.nickname.clone(),
            username: irc.username.clone(),
            channels: irc.channels.clone(),
            allowed_users: irc.allowed_users.clone(),
            server_password: irc.server_password.clone(),
            nickserv_password: irc.nickserv_password.clone(),
            sasl_password: irc.sasl_password.clone(),
            verify_tls: irc.verify_tls.unwrap_or(true),
        })));
    }

    if let Some(ref lk) = config.channels_config.lark {
        channels.push(Arc::new(LarkChannel::from_config(lk)));
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push(Arc::new(DingTalkChannel::new(
            dt.client_id.clone(),
            dt.client_secret.clone(),
            dt.allowed_users.clone(),
        )));
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push(Arc::new(QQChannel::new(
            qq.app_id.clone(),
            qq.app_secret.clone(),
            qq.allowed_users.clone(),
        )));
    }

    if channels.is_empty() {
        println!("No channels configured. Run `prx onboard` to set up channels.");
        return Ok(());
    }

    // Per-turn routing registry, keyed by `Channel::name`: one map shared by
    // `message_send` (destination of an explicit `channel` argument) and
    // `sessions_spawn` (announce/kill routing). Sharing it is what guarantees the
    // two tools agree on exactly which channels exist.
    let channels_registry = Arc::new(
        channels
            .iter()
            .map(|ch| (ch.name().to_string(), Arc::clone(ch)))
            .collect::<HashMap<_, _>>(),
    );
    let mut provider_cache_seed: HashMap<ProviderCacheKey, Arc<dyn Provider>> = HashMap::new();
    if !default_is_hint {
        provider_cache_seed.insert(
            (config_generation.id.0, provider_name.clone(), model.clone()),
            Arc::clone(&provider),
        );
    }
    let provider_cache: ProviderCacheMap = Arc::new(Mutex::new(provider_cache_seed));
    let pinned_provider_resolver: Arc<dyn providers::generation::PinnedProviderResolver> =
        Arc::new(providers::generation::ConfigGenerationProviderResolver::with_cache(
            Arc::clone(&shared_config),
            provider_runtime_options.clone(),
            Arc::clone(&provider_cache),
        ));
    // Publish the same registry process-wide for as long as these channels are
    // running, so the gateway control plane (`POST /api/channels/{name}/send`)
    // can reach the very channel objects that own the live connections. The
    // guard is held for the lifetime of this component: a reconfigured or
    // stopped channels supervisor withdraws its entries instead of leaving dead
    // channel objects addressable.
    let _outbound_publication = outbound_registry::publish(Arc::clone(&channels_registry));

    // Register message_send tool backed by Signal (or first channel) for proactive messaging.
    // Always use SignalChannel (HTTP) pointing to the effective URL (local daemon in native mode,
    // external daemon in rest mode).  The daemon must be running by the time this tool is invoked.
    if let Some(ref sig) = config.channels_config.signal {
        let is_native = sig.is_native_mode();
        let effective_url = sig.effective_http_url();
        tracing::info!(
            "message_send tool: is_native={is_native} url={effective_url} mode={:?}",
            sig.mode
        );
        let sig_chan = Arc::new(
            SignalChannel::new_with_mode(
                effective_url,
                sig.account.clone(),
                sig.group_id.clone(),
                sig.allowed_from.clone(),
                sig.ignore_attachments,
                sig.ignore_stories,
                config.media.clone(),
                is_native,
                sig.data_dir.clone(),
                sig.storm_protection.clone(),
            )
            .with_artifact_owner(crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir)),
        );
        let msg_send_tool = tools::MessageSendTool::new_signal(sig_chan, security.clone())
            .with_channels(Arc::clone(&channels_registry));
        tools_list.push(Box::new(msg_send_tool));
    } else if let Some(first_channel) = channels.first().cloned() {
        let msg_send_tool =
            tools::MessageSendTool::new(first_channel, security.clone()).with_channels(Arc::clone(&channels_registry));
        tools_list.push(Box::new(msg_send_tool));
    }

    // Register sessions_spawn tool backed by the first available channel.
    // This enables fire-and-forget sub-agent spawning with auto-announce on completion.
    // We keep a handle to the OnceLock so we can inject the full tools_registry
    // after it's wrapped in Arc (resolves the chicken-and-egg registration problem).
    // Also extract the active_runs Arc so sessions_list, sessions_send,
    // subagents, and session_status can share the same run registry without duplication.
    let spawn_tools_handle = if let Some(first_channel) = channels.first().cloned() {
        // Per-turn announce/kill routing registry: every configured channel keyed
        // by name. sessions_spawn binds the *originating* channel name to each run
        // (from the launching message's scope) and resolves the channel object
        // from here at announce/kill time — so concurrent message processing can
        // never mis-route a sub-agent result to the wrong channel.
        let spawn_channels_by_name = Arc::clone(&channels_registry);
        let spawn_tool = tools::SessionsSpawnTool::new(
            first_channel,
            Arc::clone(&provider),
            &provider_name,
            &resolved_default_model(&config),
            temperature,
            security.clone(),
            config.workspace_dir.clone(),
            config.multimodal.clone(),
            config.agent.compaction.clone(),
            config.agents_with_references().into_owned(),
            config.api_key.clone(),
            provider_runtime_options.clone(),
            config.sessions_spawn.clone(),
        )
        .with_compaction_resolver(crate::router::CompactionResolver::new(
            config.agent.compaction.clone(),
            config.router.clone(),
            config.model_routes.clone(),
        ))
        .with_reliability(config.reliability.clone())
        .with_configured_providers(&providers::ConfiguredProviders::from_config(&config))
        .with_model_catalog(providers::selection::catalog_for(&config))
        .with_hint_config(&config)
        .with_pinned_provider_resolver(Arc::clone(&pinned_provider_resolver))
        .with_channels(spawn_channels_by_name)
        .with_shared_memory(Arc::clone(&mem));
        let spawn_tool = spawn_tool
            .with_event_recording(config.memory.event_recording_config())
            .with_process_recovery(config.causal_tree.enabled)?;
        let handle = spawn_tool.tools_handle();
        let active_runs = spawn_tool.active_runs_arc();

        // sessions_list: dedicated listing tool (OpenClaw alignment)
        let workspace_id = config.workspace_dir.to_string_lossy().to_string();
        tools_list.push(Box::new(
            tools::SessionsListTool::new(active_runs.clone())
                .with_shared_memory(Arc::clone(&mem), workspace_id.clone()),
        ));
        // sessions_send: cross-session message injection (OpenClaw alignment)
        tools_list.push(Box::new(
            tools::SessionsSendTool::with_security(active_runs.clone(), security.clone())
                .with_shared_memory(Arc::clone(&mem))
                .with_event_recording(config.memory.event_recording_config()),
        ));
        // subagents: OpenClaw-compatible subagent management interface
        tools_list.push(Box::new(
            tools::SubagentsTool::with_security(active_runs.clone(), security.clone())
                .with_shared_memory(Arc::clone(&mem))
                .with_event_recording(config.memory.event_recording_config()),
        ));
        // sessions_history: conversation log viewer (OpenClaw alignment)
        tools_list.push(Box::new(
            tools::SessionsHistoryTool::new(active_runs.clone())
                .with_shared_memory(Arc::clone(&mem), workspace_id.clone()),
        ));
        // session_status: runtime status card (OpenClaw alignment)
        let channel_names: Vec<String> = channels.iter().map(|c| c.name().to_string()).collect();
        tools_list.push(Box::new(
            tools::SessionStatusTool::new(active_runs, &turn_provider_name, &model, channel_names.clone())
                .with_shared_memory(Arc::clone(&mem), workspace_id),
        ));
        // gateway: daemon management (OpenClaw alignment)
        tools_list.push(Box::new(
            tools::GatewayTool::new(Arc::clone(&shared_config), &turn_provider_name, &model, channel_names)
                .with_security(security.clone()),
        ));

        // image: vision tool backed by the active provider (OpenClaw alignment)
        tools_list.push(Box::new(
            tools::ImageTool::new(
                Arc::clone(&provider),
                &model,
                temperature,
                security.clone(),
                config.multimodal.clone(),
            )
            .with_pinned_provider_resolver(Arc::clone(&pinned_provider_resolver), &turn_provider_name),
        ));

        tools_list.push(Box::new(spawn_tool));
        Some(handle)
    } else {
        None
    };

    // Register config_reload tool (allows AI to manually trigger hot-reload).
    {
        tools_list.push(Box::new(tools::ConfigReloadTool::with_security(
            Arc::clone(&shared_config),
            security.clone(),
        )));
    }
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        tracing::debug!(
            generation = runtime.generation_id(),
            "shared WASM plugin runtime ready in channels"
        );
    }

    // Wrap the tool list in Arc now that all channel-aware tools have been appended.
    let tools_registry = Arc::new(tools_list);

    // Inject the tools registry into sessions_spawn so sub-agents can use tools.
    if let Some(handle) = spawn_tools_handle {
        let _ = handle.set(&tools_registry);
    }

    println!("🦀 OpenPRX Channel Server");
    println!("  🤖 Model:    {model}");
    let effective_backend =
        memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    println!(
        "  🧠 Memory:   {} (semantic auto-promote: {})",
        effective_backend,
        if config.memory.auto_save && config.memory.semantic.auto_promote_user_messages {
            "on"
        } else {
            "off"
        }
    );
    println!(
        "  📡 Channels: {}",
        channels.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ")
    );
    println!();
    println!("  Listening for messages... (Ctrl+C to stop)");
    println!();

    crate::health::mark_component_ok("channels");

    let initial_backoff_secs = config
        .reliability
        .channel_initial_backoff_secs
        .max(DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS);
    let max_backoff_secs = config
        .reliability
        .channel_max_backoff_secs
        .max(DEFAULT_CHANNEL_MAX_BACKOFF_SECS);

    // Single message bus — all channels send messages here
    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(CHANNEL_INBOUND_QUEUE_CAPACITY);

    // Spawn a listener for each channel
    let mut handles = Vec::new();
    for ch in &channels {
        handles.push(spawn_supervised_listener(
            ch.clone(),
            tx.clone(),
            initial_backoff_secs,
            max_backoff_secs,
            shutdown.clone(),
        ));
    }
    drop(tx); // Drop our copy so rx closes when all channels stop

    let channels_by_name = Arc::clone(&channels_registry);
    // No cap: a wedged turn must not be able to stop the dispatch loop from
    // receiving. `prx tasks list` is where in-flight turns are counted now.
    println!("  🚦 In-flight messages: unbounded (inspect with `prx tasks list`)");

    let signal_inbound_policy = config.channels_config.signal.as_ref().map(|signal| {
        let allowed_from = normalize_allowlist(&signal.allowed_from);
        let group_allow_from = normalize_allowlist(&signal.group_allow_from);
        warn_open_policy_allowlist_alignment(
            "signal",
            signal.dm_policy,
            signal.group_policy,
            &allowed_from,
            &group_allow_from,
        );
        InboundPolicyConfig {
            dm_policy: normalize_dm_policy("signal", signal.dm_policy),
            group_policy: signal.group_policy,
            allowed_from,
            group_allow_from,
        }
    });
    let whatsapp_inbound_policy = config.channels_config.whatsapp.as_ref().map(|whatsapp| {
        let allowed_from = normalize_allowlist(&whatsapp.allowed_numbers);
        let group_allow_from = normalize_allowlist(&whatsapp.group_allow_from);
        warn_open_policy_allowlist_alignment(
            "whatsapp",
            whatsapp.dm_policy,
            whatsapp.group_policy,
            &allowed_from,
            &group_allow_from,
        );
        InboundPolicyConfig {
            dm_policy: normalize_dm_policy("whatsapp", whatsapp.dm_policy),
            group_policy: whatsapp.group_policy,
            allowed_from,
            group_allow_from,
        }
    });
    let interrupt_on_new_message = config
        .channels_config
        .telegram
        .as_ref()
        .is_some_and(|tg| tg.interrupt_on_new_message);
    let bot_names = collect_bot_names(&config);
    let bot_uuids = collect_bot_uuids(&config);
    let mention_only_by_channel = collect_mention_only_by_channel(&config);
    let group_reply_mode_by_channel = collect_group_reply_mode_by_channel(&config);

    let plugin_generation = {
        #[cfg(feature = "wasm-plugins")]
        {
            wasm_plugin_runtime
                .as_ref()
                .map_or(0, |runtime| runtime.generation_id())
        }
        #[cfg(not(feature = "wasm-plugins"))]
        {
            0
        }
    };
    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::new(mcp_tool, plugin_generation)),
        config: shared_config,
        #[cfg(test)]
        config_generation,
        channels_by_name,
        #[cfg(test)]
        provider: Arc::clone(&provider),
        #[cfg(test)]
        default_provider: Arc::new(provider_name),
        memory: Arc::clone(&mem),
        tools_registry: Arc::clone(&tools_registry),
        observer,
        hooks,
        #[cfg(test)]
        model: Arc::new(model.clone()),
        #[cfg(test)]
        temperature,
        auto_save_memory: config.memory.auto_save && config.memory.semantic.auto_promote_user_messages,
        memory_event_recording: config.memory.event_recording_config(),
        #[cfg(test)]
        read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
        #[cfg(test)]
        priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
        #[cfg(test)]
        low_priority_tools: config.agent.low_priority_tools.clone(),
        conversation_histories,
        provider_cache,
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(test)]
        api_key: config.api_key.clone(),
        #[cfg(test)]
        api_url: config.api_url.clone(),
        #[cfg(test)]
        reliability: Arc::new(config.reliability.clone()),
        provider_runtime_options,
        workspace_dir: Arc::new(config.workspace_dir.clone()),
        interrupt_on_new_message,
        #[cfg(test)]
        multimodal: config.multimodal.clone(),
        // Preserve the legacy fixture only in unit-test builds. Production
        // authorization derives security from the per-message generation.
        #[cfg(test)]
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::clone(&security),
        })),
        #[cfg(test)]
        tool_tiering: config.tool_tiering.clone(),
        signal_inbound_policy,
        whatsapp_inbound_policy,
        bot_names,
        bot_uuids,
        mention_only_by_channel,
        group_reply_mode_by_channel,
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        #[cfg(test)]
        test_inbound_authorizer: None,
    });
    #[cfg(not(test))]
    drop(provider);

    let dispatch_result = run_message_dispatch_loop_with_cleanup(rx, runtime_ctx, shutdown.clone()).await;
    if dispatch_result.is_err() {
        // A failed cleanup closes this component's admission and listeners;
        // otherwise waiting for live listeners here could hide the error forever.
        shutdown.cancel();
    }

    // Wait for all channel tasks. On shutdown the supervised listeners break out
    // of their loops (ListenerOutcome::Shutdown above), so these joins complete
    // instead of hanging on a still-running `listen()`.
    for h in handles {
        let _ = h.await;
    }

    dispatch_result
}
