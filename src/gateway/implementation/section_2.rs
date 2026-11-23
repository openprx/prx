/// Assemble a ready generation, or a standby with the same surface.
fn turn_state_of(
    chat: Result<GatewayTurnChat, TurnNotReadyReason>,
    publication_slot: Arc<turn_source::publication_slot::GatewayPublicationSlot>,
    config_generation: Arc<crate::config::ConfigGeneration>,
    tools_registry: Arc<Vec<Box<dyn Tool>>>,
    mcp_tool: Option<Arc<McpTool>>,
) -> GatewayTurnState {
    match chat {
        Ok((provider, model, provider_binding)) => GatewayTurnState::from(TurnRuntimeGeneration {
            provider_binding,
            temperature: config_generation.effective.default_temperature,
            publication_slot,
            config_generation,
            provider,
            model,
            tools_registry,
            mcp_tool,
        }),
        Err(reason) => GatewayTurnState::NotReady(TurnRuntimeStandby {
            reason,
            publication_slot,
            config_generation,
            tools_registry,
            mcp_tool,
        }),
    }
}

/// Build the pinned generation's default chat connection. The selected
/// provider, model and credential are resolved as one value before the exact
/// connection is constructed, including when the default names a route hint.
pub(crate) fn build_gateway_provider_selection(
    config: &Config,
    options: &providers::ProviderRuntimeOptions,
) -> anyhow::Result<(Arc<dyn Provider>, providers::catalog::ResolvedModel)> {
    let resolved = providers::selection::check_default(config, providers::catalog::UnknownPolicy::Invocation)?;
    let catalog = providers::selection::catalog_for(config);
    let provider: Arc<dyn Provider> = Arc::from(providers::create_routed_provider_for(
        &resolved, &catalog, config, options,
    )?);
    Ok((provider, resolved))
}

fn build_gateway_turn_provider(
    config: &Config,
    options: &providers::ProviderRuntimeOptions,
) -> anyhow::Result<GatewayTurnChat> {
    let (provider, resolved) = build_gateway_provider_selection(config, options)?;
    let model = resolved.model().to_owned();
    let binding = Arc::new(turn_source::GatewayProviderBinding::new(
        Arc::clone(&provider),
        resolved,
    ));
    Ok((provider, model, Some(binding)))
}

/// Run the HTTP gateway using axum with proper HTTP/1.1 compliance.
///
/// `shared_config` is the single ConfigGeneration owner the gateway uses for all
/// authorization decisions. When the gateway runs under the daemon, the
/// daemon owns this handle and also wires it to the file-watch hot-reload manager,
/// so config.toml edits become visible at every gateway authz point. When invoked
/// directly via `prx gateway` (no daemon), `None` is passed and the gateway builds
/// its own fallback handle — in that mode only the in-gateway ConfigReloadTool / the
/// `/api/config/reload` route can update it (no file watcher).
#[allow(clippy::too_many_lines)]
pub async fn run_gateway(
    host: &str,
    port: u16,
    config: Config,
    shared_config: Option<crate::config::SharedConfig>,
    shutdown: CancellationToken,
) -> Result<()> {
    run_gateway_inner(host, port, config, shared_config, shutdown, None).await
}
async fn run_gateway_inner(
    host: &str,
    port: u16,
    config: Config,
    shared_config: Option<crate::config::SharedConfig>,
    shutdown: CancellationToken,
    mut producer: Option<crate::runtime::h2_entry::TerminalInputProducer>,
) -> Result<()> {
    let _a2a_listener = crate::messaging::a2a::start(&config, shutdown.child_token()).await?;
    let controlled = producer.as_ref().map(|producer| producer.profile());
    if let Some(profile) = &controlled {
        profile.verify(&config)?;
        anyhow::ensure!(shared_config.is_none(), "controlled Gateway requires direct OS launch");
    }
    let start_time = Instant::now();
    crate::health::register_component(
        "gateway",
        "gateway",
        true,
        Duration::from_mins(1),
        crate::health::ComponentState::Starting,
    );
    // ── Security: refuse public bind without tunnel or explicit opt-in ──
    if is_public_bind(host) && config.tunnel.provider == "none" && !config.gateway.allow_public_bind {
        anyhow::bail!(
            "🛑 Refusing to bind to {host} — gateway would be exposed to the internet.\n\
             Fix: use --host 127.0.0.1 (default), configure a tunnel, or set\n\
             [gateway] allow_public_bind = true in config.toml (NOT recommended)."
        );
    }
    // ── Install owner (HOST-1a): every paired bearer acts for it. Loaded
    // before the listener binds, so an unusable id file leaves no route
    // answering; it is never regenerated.
    let install_owner = crate::security::install_owner::load_or_create(&config.workspace_dir)
        .context("gateway start-up refused: the install owner id is unusable")?;
    tracing::info!(owner_id = %install_owner, "install owner id loaded");
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual_port = listener.local_addr()?.port();
    let _runtime_file =
        crate::runtime::host_bootstrap::RuntimeFile::write(&config, actual_port, install_owner.as_str())?;
    let display_addr = format!("{host}:{actual_port}");

    // The chat-session mailbox is durable, and this is the one startup path
    // both launch modes share (`prx gateway`, and the daemon's supervised
    // gateway), so it is the only place the store has to be attached. Attaching
    // replays what the previous process left queued, and is idempotent: the
    // daemon's supervisor restarts the gateway inside one process, and a replay
    // never duplicates a session that is still live in memory.
    if let Some(path) = runtime::chat_sessions::attach_workspace_store(&config.workspace_dir) {
        tracing::debug!(path = %path.display(), "chat session mailbox is durable");
    }

    #[cfg(feature = "wasm-plugins")]
    let wasm_early_runtime = if config
        .default_provider
        .as_deref()
        .is_some_and(|provider| provider.starts_with("wasm:"))
        || memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config))
            .starts_with("wasm:")
    {
        crate::plugins::init_plugin_runtime(&config, None).await
    } else {
        None
    };
    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);
    // A default provider without a usable credential leaves the turn runtime
    // standing by instead of failing the start: every management route
    // serves, and a reload after the credential is stored makes it ready.
    let chat = match build_gateway_turn_provider(&config, &provider_runtime_options) {
        Ok(chat) => Ok(chat),
        Err(error) => {
            let reason = TurnNotReadyReason::of_provider_error(&error).ok_or(error)?;
            tracing::warn!(
                reason = reason.code(),
                "Gateway turn runtime is not ready; management routes serve and turns are refused"
            );
            Err(reason)
        }
    };
    let model = chat.as_ref().map_or_else(
        |_| config.default_model.clone().unwrap_or_default(),
        |(_, model, _)| model.clone(),
    );
    // Test builds keep the fixture field filled; no turn reads it, since
    // this state always holds its turn runtime.
    #[cfg(test)]
    let fixture_provider: Arc<dyn Provider> = match &chat {
        Ok((provider, _, _)) => Arc::clone(provider),
        Err(_) => Arc::new(tests::MockProvider::default()),
    };
    let temperature = config.default_temperature;
    let shared_config_for_reload = shared_config.unwrap_or_else(|| crate::config::new_shared(config.clone()));
    if controlled.is_some() {
        shared_config_for_reload.freeze_runtime_writes();
    }
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage(
        &config.memory,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        memory::EmbeddingRootCredential::from_config(&config),
    )?);
    let scrub_memory = Arc::clone(&mem);
    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    // FIX-P1-31: honour the configured `security.audit` block on the gate audit path.
    // Built via the shared `build_security_policy` helper so this site cannot drift
    // from (or forget) the audit-config wiring — byte-for-byte identical to the
    // former local from_config + audit-config construction.
    //
    // This policy bootstraps static gateway gates and the initial tool registry.
    // Gateway turn tools are rebuilt from one candidate ConfigGeneration and swapped
    // atomically by GatewayTurnRuntimeOwner. Resource-mutation routes independently
    // pin the manager's active generation at each authorization decision.
    let security = crate::runtime::bootstrap::build_security_policy(&config);
    // Workspace command hooks follow the live `security.workspace_hooks`
    // opt-in (HOOKS-ENV).
    let hooks =
        Arc::new(HookManager::new(config.workspace_dir.clone()).with_config(Arc::clone(&shared_config_for_reload)));
    // Kept for the shutdown drain of queued hook events.
    let hooks_for_shutdown = Arc::clone(&hooks);
    #[cfg(feature = "wasm-plugins")]
    let wasm_plugin_runtime = if let Some(runtime) = wasm_early_runtime {
        runtime
            .attach_memory(Arc::clone(&mem))
            .await
            .map_err(|error| anyhow::anyhow!("failed to attach WASM memory backend in gateway: {error}"))?;
        Some(runtime)
    } else {
        crate::plugins::init_plugin_runtime(&config, Some(Arc::clone(&mem))).await
    };

    let (composio_key, composio_entity_id) = if config.composio.configured() {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };

    // Build the base tool list (mutable so we can append channel-aware tools below)
    let extensions = hooks.control_tool_arcs(Arc::clone(&security));
    #[cfg(feature = "wasm-plugins")]
    let extensions = {
        let mut extensions = extensions;
        if let Some(runtime) = &wasm_plugin_runtime {
            extensions.extend(runtime.control_tool_arcs(Arc::clone(&security)));
        }
        extensions
    };
    let tools_result = if let Some(profile) = &controlled {
        tools::ToolsRegistryResult {
            tools: profile.registry(&config, &security)?,
            mcp_tool: None,
        }
    } else {
        tools::all_tools_with_runtime_ext_and_extensions(
            Arc::new(config.clone()),
            Arc::clone(&shared_config_for_reload),
            &security,
            runtime,
            Arc::clone(&mem),
            composio_key,
            composio_entity_id,
            &config.browser,
            &config.http_request,
            &config.workspace_dir,
            &config.agents_with_references(),
            config.api_key.as_deref(),
            &config,
            extensions,
            Some(Arc::clone(&hooks)),
        )
    };
    let mut tools_list = tools_result.tools;
    let mcp_tool = tools_result.mcp_tool;
    // Generic /webhook auth can require a standalone token and/or HMAC signature.
    let webhook_token_hash: Option<Arc<str>> = config.webhook.token.as_ref().and_then(|raw_token| {
        let trimmed_token = raw_token.trim();
        (!trimmed_token.is_empty()).then(|| Arc::<str>::from(hash_webhook_secret(trimmed_token)))
    });
    let webhook_signing_secret: Option<Arc<str>> = config.channels_config.webhook.as_ref().and_then(|webhook| {
        webhook.secret.as_ref().and_then(|raw_secret| {
            let trimmed_secret = raw_secret.trim();
            (!trimmed_secret.is_empty()).then(|| Arc::<str>::from(trimmed_secret.to_string()))
        })
    });

    // WhatsApp channel (if configured)
    let whatsapp_channel: Option<Arc<WhatsAppChannel>> = config
        .channels_config
        .whatsapp
        .as_ref()
        .filter(|wa| wa.is_cloud_config())
        .map(|wa| {
            Arc::new(WhatsAppChannel::new(
                wa.access_token.clone().unwrap_or_default(),
                wa.phone_number_id.clone().unwrap_or_default(),
                wa.verify_token.clone().unwrap_or_default(),
                wa.allowed_numbers.clone(),
            ))
        });
    let signal_media_config = config.media.clone();
    let signal_channel: Option<Arc<SignalChannel>> = config.channels_config.signal.as_ref().map(|sg| {
        Arc::new(
            SignalChannel::new_with_mode(
                sg.effective_http_url(),
                sg.account.clone(),
                sg.group_id.clone(),
                sg.allowed_from.clone(),
                sg.ignore_attachments,
                sg.ignore_stories,
                signal_media_config.clone(),
                sg.is_native_mode(),
                sg.data_dir.clone(),
                sg.storm_protection.clone(),
            )
            .with_artifact_owner(crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir)),
        )
    });

    // Linq channel (if configured). Built here (ahead of message_send and the spawn
    // tool) so it can join the shared per-turn channel routing registry below.
    let linq_channel: Option<Arc<LinqChannel>> = config.channels_config.linq.as_ref().map(|lq| {
        Arc::new(LinqChannel::new(
            lq.api_token.clone(),
            lq.from_phone.clone(),
            lq.allowed_senders.clone(),
        ))
    });

    // Nextcloud Talk channel (if configured). Built here (ahead of message_send and
    // the spawn tool) so it can join the shared per-turn channel routing registry.
    let nextcloud_talk_channel: Option<Arc<NextcloudTalkChannel>> =
        config.channels_config.nextcloud_talk.as_ref().map(|nc| {
            Arc::new(NextcloudTalkChannel::new(
                nc.base_url.clone(),
                nc.app_token.clone(),
                nc.allowed_users.clone(),
            ))
        });

    let (channels_registry, spawn_tools_handle) = register_gateway_turn_tools(
        &mut tools_list,
        &config,
        &shared_config_for_reload,
        &security,
        &hooks,
        &model,
        &mem,
        &provider_runtime_options,
        signal_channel.as_ref(),
        whatsapp_channel.as_ref(),
        linq_channel.as_ref(),
        nextcloud_talk_channel.as_ref(),
    )?;
    // The process cost outbox projector (P2-R5): started eagerly before the
    // first turn and cancelled with the gateway's shutdown token.
    let cost_projector = shared_config_for_reload.ensure_runtime_writable().is_ok().then(|| {
        crate::runtime::cost_recovery::warn_legacy_files_once(&config.workspace_dir);
        crate::runtime::cost_recovery::CostOutboxProjector::start(
            Arc::clone(&mem),
            config.workspace_dir.clone(),
            config.cost.clone(),
            &shutdown,
        )
    });
    // Publish the routing registry process-wide for as long as this gateway
    // runs, so `POST /api/channels/{name}/send` can address the channels the
    // gateway itself constructed. The channels supervisor publishes its own
    // (larger) registry alongside this one; the two are merged at lookup time.
    let _outbound_publication = crate::channels::outbound_registry::publish(Arc::clone(&channels_registry));
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        tracing::debug!(generation = runtime.generation_id(), "WASM plugin runtime ready");
    }

    let tools_registry = Arc::new(tools_list);

    // Inject the tools registry into sessions_spawn so sub-agents can use tools.
    let _ = spawn_tools_handle.set(&tools_registry);

    // WhatsApp app secret for webhook signature verification (from config)
    let whatsapp_app_secret: Option<Arc<str>> = config
        .channels_config
        .whatsapp
        .as_ref()
        .and_then(|wa| {
            wa.app_secret
                .as_deref()
                .map(str::trim)
                .filter(|secret| !secret.is_empty())
                .map(ToOwned::to_owned)
        })
        .map(Arc::from);

    // Linq signing secret for webhook signature verification (from config).
    // (The `linq_channel` object itself is built earlier, ahead of the spawn tool.)
    let linq_signing_secret: Option<Arc<str>> = config
        .channels_config
        .linq
        .as_ref()
        .and_then(|lq| {
            lq.signing_secret
                .as_deref()
                .map(str::trim)
                .filter(|secret| !secret.is_empty())
                .map(ToOwned::to_owned)
        })
        .map(Arc::from);

    // Nextcloud Talk webhook secret for signature verification (from config).
    // (The `nextcloud_talk_channel` object is built earlier, ahead of the spawn tool.)
    let nextcloud_talk_webhook_secret: Option<Arc<str>> = config
        .channels_config
        .nextcloud_talk
        .as_ref()
        .and_then(|nc| {
            nc.webhook_secret
                .as_deref()
                .map(str::trim)
                .filter(|secret| !secret.is_empty())
                .map(ToOwned::to_owned)
        })
        .map(Arc::from);

    // ── Pairing guard ──────────────────────────────────────
    let pairing = Arc::new(
        PairingGuard::new(config.gateway.require_pairing, &config.gateway.paired_tokens)
            .with_install_owner(install_owner),
    );
    let rate_limit_max_keys = normalize_max_keys(config.gateway.rate_limit_max_keys, RATE_LIMIT_MAX_KEYS_DEFAULT);
    let rate_limiter = Arc::new(GatewayRateLimiter::new(
        config.gateway.pair_rate_limit_per_minute,
        config.gateway.webhook_rate_limit_per_minute,
        config.gateway.api_rate_limit_per_minute,
        rate_limit_max_keys,
    ));
    let idempotency_max_keys = normalize_max_keys(config.gateway.idempotency_max_keys, IDEMPOTENCY_MAX_KEYS_DEFAULT);
    let idempotency_store = Arc::new(IdempotencyStore::new(
        Duration::from_secs(config.gateway.idempotency_ttl_secs.max(1)),
        idempotency_max_keys,
    ));

    // ── Tunnel ────────────────────────────────────────────────
    let tunnel = crate::tunnel::create_tunnel(&config.tunnel)?;
    let mut tunnel_url: Option<String> = None;

    if let Some(ref tun) = tunnel {
        println!("🔗 Starting {} tunnel...", tun.name());
        match tun.start(host, actual_port).await {
            Ok(url) => {
                println!("🌐 Tunnel active: {url}");
                tunnel_url = Some(url);
            }
            Err(e) => {
                println!("⚠️  Tunnel failed to start: {e}");
                println!("   Falling back to local-only mode.");
            }
        }
    }

    println!("🦀 OpenPRX Gateway listening on http://{display_addr}");
    if let Some(ref url) = tunnel_url {
        println!("  🌐 Public URL: {url}");
    }
    println!("  POST /pair      — pair a new client (X-Pairing-Code header)");
    println!("  POST /webhook   — {{\"message\": \"your prompt\"}}");
    if whatsapp_channel.is_some() {
        println!("  GET  /whatsapp  — Meta webhook verification");
        println!("  POST /whatsapp  — WhatsApp message webhook");
    }
    if linq_channel.is_some() {
        println!("  POST /linq      — Linq message webhook (iMessage/RCS/SMS)");
    }
    if nextcloud_talk_channel.is_some() {
        println!("  POST /nextcloud-talk — Nextcloud Talk bot webhook");
    }
    println!("  GET  /health    — health check");
    println!("  GET  /metrics   — Prometheus metrics");
    if let Some(code) = pairing.pairing_code() {
        println!();
        println!("  🔐 PAIRING REQUIRED — use this one-time code:");
        println!("     ┌──────────────┐");
        println!("     │  {code}  │");
        println!("     └──────────────┘");
        println!("     Send: POST /pair with header X-Pairing-Code: {code}");
    } else if pairing.require_pairing() {
        println!("  🔒 Pairing: ACTIVE (bearer token required)");
    } else {
        println!("  ⚠️  Pairing: DISABLED (all requests accepted)");
    }
    println!("  Press Ctrl+C to stop.\n");

    // Build shared state
    let observer: Arc<dyn crate::observability::Observer> =
        Arc::from(crate::observability::create_observer(&config.observability));
    let (logs_broadcast_tx, _) = broadcast::channel(1024);
    // The startup registry and MCP tool move into the turn runtime: nothing
    // else keeps them, so the first configuration change releases them and
    // their stdio MCP children (D-098 F-1).
    let turn_runtime_owner = start_gateway_turn_runtime(
        turn_state_of(
            chat,
            Arc::new(
                turn_source::publication_slot::GatewayPublicationSlot::new(Arc::clone(&security), {
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
                })
                .with_controlled(controlled.clone()),
            ),
            shared_config_for_reload.pin(),
            tools_registry,
            mcp_tool,
        ),
        &shared_config_for_reload,
        Arc::clone(&mem),
        signal_channel.clone(),
        whatsapp_channel.clone(),
        linq_channel.clone(),
        nextcloud_talk_channel.clone(),
        Arc::clone(&hooks),
        #[cfg(feature = "wasm-plugins")]
        wasm_plugin_runtime.clone(),
    )?;
    shared_config_for_reload.register_audit_sink(Arc::new(ConfigGenerationMessageAuditSink {
        memory: Arc::clone(&mem),
        workspace_id: config.workspace_dir.to_string_lossy().to_string(),
        runtime_handle: tokio::runtime::Handle::current(),
    }));

    let state = AppState {
        rf_streams: Default::default(),
        rf_approvals: Default::default(),
        config: Arc::clone(&shared_config_for_reload),
        #[cfg(test)]
        provider: fixture_provider,
        model,
        temperature,
        mem,
        auto_save: config.memory.auto_save && config.memory.semantic.auto_promote_user_messages,
        // Test fixtures only; this state serves every turn from `turn_runtime`.
        #[cfg(test)]
        tools_registry: Arc::new(Vec::new()),
        #[cfg(test)]
        mcp_tool: None,
        turn_runtime: Some(Arc::clone(&turn_runtime_owner)),
        hooks,
        webhook_token_hash,
        webhook_signing_secret,
        pairing,
        trust_forwarded_headers: config.gateway.trust_forwarded_headers,
        rate_limiter,
        idempotency_store,
        whatsapp: whatsapp_channel,
        signal: signal_channel,
        whatsapp_app_secret,
        linq: linq_channel,
        linq_signing_secret,
        nextcloud_talk: nextcloud_talk_channel,
        nextcloud_talk_webhook_secret,
        observer,
        start_time,
        gateway_port: actual_port,
        logs_broadcast_tx,
        #[cfg(feature = "wasm-plugins")]
        plugin_runtime: wasm_plugin_runtime,
    };

    // HookManager resolves hooks and event delivery from this same generation owner.
    #[cfg(feature = "wasm-plugins")]
    if let Some(ref runtime) = state.plugin_runtime {
        state.hooks.set_plugin_runtime(Arc::clone(runtime)).await;
    }

    if controlled.is_some() {
        let producer = producer
            .take()
            .ok_or_else(|| anyhow::anyhow!("local control producer missing"))?;
        h2_control::start_local_control(producer, shutdown.clone())?;
    }
    let limited_public_routes = Router::new()
        .route("/pair", post(handle_pair))
        .route("/webhook", post(handle_webhook))
        .route("/whatsapp", get(handle_whatsapp_verify))
        .route("/whatsapp", post(handle_whatsapp_message))
        .route("/linq", post(handle_linq_webhook))
        .route("/nextcloud-talk", post(handle_nextcloud_talk_webhook))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_SIZE));

    api::sync_startup::startup(&state, &shutdown).await?;
    let api_routes = api::router(state.clone()).layer(RequestBodyLimitLayer::new(MAX_API_BODY_SIZE));

    // Build router with middleware
    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .route(
            "/mcp/v1/initialize",
            get(compat::mcp_initialize).post(compat::mcp_initialize),
        )
        .route(
            "/mcp/v1/list_servers",
            get(compat::mcp_list_servers).post(compat::mcp_list_servers),
        )
        .route(
            "/mcp/v1/tools/list",
            get(compat::mcp_tools_list).post(compat::mcp_tools_list),
        )
        .route("/mcp/v1/tools/call", post(compat::mcp_tools_call))
        .route("/a2a/v1/identity", get(compat::a2a_identity).post(compat::a2a_identity))
        .route("/a2a/v1/discover", get(compat::a2a_discover).post(compat::a2a_discover))
        .route("/.well-known/agent.json", get(compat::well_known_agent_json))
        .route("/a2a/v1/.well-known/jwks.json", get(compat::a2a_jwks))
        .merge(limited_public_routes)
        .nest("/api", api_routes)
        .merge(ui::router())
        .with_state(state)
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let controlled = controlled.clone();
                async move {
                    if let Some(profile) = controlled {
                        if profile.ensure_live().is_err()
                            || !crate::runtime::h2_entry::ControlledH2Profile::permits_route(
                                request.method().as_str(),
                                request.uri().path(),
                            )
                        {
                            return (
                                StatusCode::FORBIDDEN,
                                "route is outside the fixed controlled Gateway profile",
                            )
                                .into_response();
                        }
                    }
                    next.run(request).await
                }
            },
        ));

    // The listener is bound and every route/state dependency is now constructed.
    // This explicit acknowledgement, rather than supervisor task survival, is the
    // gateway readiness boundary.
    crate::health::mark_component_ok("gateway");

    // A quiet listener is still a live listener. Refresh gateway health from an
    // owner-controlled heartbeat rather than relying on `/health` requests to
    // keep the required component fresh.
    let health_shutdown = CancellationToken::new();
    let health_heartbeat = tokio::spawn(run_health_heartbeat(
        "gateway",
        GATEWAY_HEALTH_HEARTBEAT_INTERVAL,
        health_shutdown.clone(),
    ));

    // Run the server with graceful shutdown (D5/D9 step 3, DEV-02). On root token
    // cancellation axum stops accepting and drains in-flight requests. The drain
    // bound must only start counting *after* shutdown is requested — never while
    // the server is idle — otherwise the gateway would self-exit after the timeout
    // on a quiet listener. We therefore arm the bounded timer inside a select! arm
    // gated on `shutdown.cancelled()`, so the timeout window opens only post-shutdown.
    let drain_deadline = shutdown.clone();
    let serve_fut = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .into_future();
    tokio::pin!(serve_fut);
    let serve_result = tokio::select! {
        res = &mut serve_fut => res,
        _ = async {
            drain_deadline.cancelled().await;
            tokio::time::sleep(GATEWAY_GRACEFUL_TIMEOUT).await;
        } => {
            tracing::warn!(
                timeout_secs = GATEWAY_GRACEFUL_TIMEOUT.as_secs(),
                "gateway graceful shutdown timed out; forcing exit"
            );
            Ok(())
        }
    };

    let decision_cleanup_result = turn_runtime_owner
        .cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(15))
        .await;
    hooks_for_shutdown.drain_for_shutdown().await;
    if let Some(projector) = cost_projector {
        projector.stop().await;
    }
    // AC-1b2b: a clean stop leaves no deleted rf content in the WAL.
    match scrub_memory.scrub_deleted_wal().await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("deleted rf content stays in brain.db-wal at stop; it is scrubbed at the next open")
        }
        Err(error) => tracing::warn!(
            error = %crate::agent::diagnostics::error_class(&error),
            "deleted rf content WAL scrub failed at stop"
        ),
    }
    health_shutdown.cancel();
    let _ = health_heartbeat.await;
    serve_result?;
    decision_cleanup_result?;

    crate::health::mark_component_stopping("gateway");
    crate::health::mark_component_stopped("gateway");
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// AXUM HANDLERS
// ══════════════════════════════════════════════════════════════════════════════

/// GET /health — always public (no secrets leaked)
async fn handle_health(State(state): State<AppState>) -> impl IntoResponse {
    // A successfully dispatched health request is also an explicit freshness
    // acknowledgement from the gateway owner.
    crate::health::mark_component_ok("gateway");
    let runtime = crate::health::snapshot();
    health_response(state.pairing.is_paired(), runtime)
}

fn health_response(paired: bool, runtime: crate::health::HealthSnapshot) -> (StatusCode, Json<serde_json::Value>) {
    let readiness = crate::health::readiness_from_snapshot(&runtime);
    let status = if readiness.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = serde_json::json!({
        "status": readiness.status,
        "paired": paired,
        "readiness": readiness,
        "runtime": runtime,
    });
    (status, Json(body))
}

/// Prometheus content type for text exposition format.
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// GET /metrics — Prometheus text exposition format
async fn handle_metrics(State(state): State<AppState>) -> impl IntoResponse {
    // Merge the PrometheusObserver's main registry with the separate
    // chat_metrics registry (S2.5 P1-A). The four chat counters live in a
    // registry that is physically isolated from the observer, so they have to
    // be merged explicitly or a scrape never sees them.
    let chat_reg = crate::observability::chat_metrics::chat_registry();
    let body = state
        .observer
        .as_ref()
        .as_any()
        .downcast_ref::<crate::observability::PrometheusObserver>()
        .map_or_else(
            || {
                // The chat metrics still have to be exposed when the observer is not Prometheus.
                format!(
                    "# Prometheus backend not enabled. Set [observability] backend = \"prometheus\" in config.\n{}",
                    crate::observability::prometheus::encode_registries(&[chat_reg])
                )
            },
            |prom| prom.encode_with_extras(&[chat_reg]),
        );

    (StatusCode::OK, [(header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)], body)
}

/// POST /pair — exchange one-time code for bearer token
#[axum::debug_handler]
async fn handle_pair(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let rate_key = client_key_from_request(Some(peer_addr), &headers, state.trust_forwarded_headers);
    if !state.rate_limiter.allow_pair(&rate_key) {
        tracing::warn!("/pair rate limit exceeded");
        let err = serde_json::json!({
            "error": "Too many pairing requests. Please retry later.",
            "retry_after": RATE_LIMIT_WINDOW_SECS,
        });
        return (StatusCode::TOO_MANY_REQUESTS, Json(err));
    }

    let code = headers
        .get("X-Pairing-Code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Err(error) = authorize_gateway_resource_mutation(&state, "gateway:pair", ResourceRiskLevel::Low) {
        return error;
    }

    match state.pairing.try_pair(code, &rate_key).await {
        Ok(Some(token)) => {
            tracing::info!("🔐 New client paired successfully");
            if let Err(err) = persist_pairing_tokens(&state.config, &state.pairing).await {
                tracing::error!(error = %crate::security::redaction::redact_provider_text(&format!("{err:#}")), "🔐 Pairing succeeded but token persistence failed");
                let body = serde_json::json!({
                    "paired": true,
                    "persisted": false,
                    "token": token,
                    "message": "Paired for this process, but failed to persist token to config.toml. Check config path and write permissions.",
                });
                return (StatusCode::OK, Json(body));
            }

            let body = serde_json::json!({
                "paired": true,
                "persisted": true,
                "token": token,
                "message": "Save this token — use it as Authorization: Bearer <token>"
            });
            (StatusCode::OK, Json(body))
        }
        Ok(None) => {
            tracing::warn!("🔐 Pairing attempt with invalid code");
            let err = serde_json::json!({"error": "Invalid pairing code"});
            (StatusCode::FORBIDDEN, Json(err))
        }
        Err(lockout_secs) => {
            tracing::warn!("🔐 Pairing locked out — too many failed attempts ({lockout_secs}s remaining)");
            let err = serde_json::json!({
                "error": format!("Too many failed attempts. Try again in {lockout_secs}s."),
                "retry_after": lockout_secs
            });
            (StatusCode::TOO_MANY_REQUESTS, Json(err))
        }
    }
}

/// Add what pairing learned to the configured list, **without rewriting the
/// entries an operator authored**.
///
/// Persistence used to assign `pairing.tokens()` over the whole list, which
/// treated the in-memory set as authoritative over the file. It is not:
/// `gateway.paired_tokens` is a restart-only field, so between startup and the
/// next restart the file can legitimately hold entries the running guard never
/// loaded, and it can hold them in a *spelling* the guard cannot reproduce.
/// Both were destroyed silently.
///
/// The spelling matters because [`PairingGuard`] stores only hashes: a
/// plaintext entry is hashed on load, so `tokens()` returns a hash for it and
/// the assignment wrote that hash back in its place. The token stays valid as a
/// bearer either way — but chat's `/sessions --daemon` takes its credential out
/// of this list and deliberately skips hashes (a stored hash, re-hashed by the
/// gateway on receipt, authenticates as nothing), so the entry it was reading
/// would have become useless to it with no edit by anyone and nothing in the
/// file to show what changed.
///
/// That particular sequence turns out not to be reachable through `/pair`:
/// a pairing code is minted only when the token set is *empty*, so a config
/// that already carries a plaintext entry can never complete a pairing. What is
/// reachable is the same destruction one step earlier — an operator adds an
/// entry by hand while the gateway is up and a still-live code is later
/// redeemed, and the entry is dropped from disk outright rather than merely
/// rehashed.
///
/// So nothing is rewritten or removed here. Every non-empty configured entry is
/// kept, in order and as written, and a live hash is appended only when no
/// configured entry already names that token. Revocation is not a thing this
/// gateway has, so there is nothing this would wrongly resurrect.
///
/// This never puts a *generated* token into the file in plaintext: `/pair`
/// hands that to the client once and persists only its hash, unchanged. The
/// only plaintext preserved is plaintext the operator wrote themselves.
///
/// MUTATION GUARD: assign `pairing.tokens()` wholesale again and
/// `a_configured_token_survives_a_later_pairing` fails.
fn merge_paired_tokens(configured: &[String], live: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::with_capacity(configured.len().saturating_add(live.len()));
    let mut covered: HashSet<String> = HashSet::new();
    for entry in configured {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Compared by hash, kept by spelling: a plaintext entry and the hash
        // pairing holds for it are the same credential, and the operator's
        // spelling is the one that stays.
        let hash = if crate::security::pairing::is_token_hash(trimmed) {
            trimmed.to_string()
        } else {
            crate::security::pairing::hash_token(trimmed)
        };
        if covered.insert(hash) {
            merged.push(trimmed.to_string());
        }
    }
    for token in live {
        if covered.insert(token.clone()) {
            merged.push(token.clone());
        }
    }
    merged
}

async fn persist_pairing_tokens(config: &crate::config::SharedConfig, pairing: &PairingGuard) -> Result<()> {
    let paired_tokens = pairing.tokens();
    let mut updated_cfg = (*config.desired().config).clone();
    updated_cfg.gateway.paired_tokens = merge_paired_tokens(&updated_cfg.gateway.paired_tokens, &paired_tokens);
    updated_cfg
        .save()
        .await
        .context("Failed to persist paired tokens to config.toml")?;
    let manager = Arc::clone(config);
    crate::runtime::blocking::spawn_blocking(move || {
        manager.reload_from_disk(crate::config::ConfigReloadTrigger::PairingPersistence)
    })
    .await
    .context("Pairing config reload worker failed")??;
    Ok(())
}

/// Build the turn-root spawn context for one gateway/webhook turn.
///
/// Two things travel down from here into every sub-agent this turn starts:
///
/// * `run_id` — the turn's own id becomes the child's `parent_run_id`, so a
///   webhook-launched run is attached to the turn that asked for it instead of
///   floating as a root.
/// * `channel_excluded_tools` — the names the turn's own surface hides. The
///   gateway is not an IM channel and has no `channel_exclude` list: it publishes
///   `tool_tiering` as configured, not `for_channel_surface()`. So the boundary a
///   child inherits here is exactly `always_exclude`: the child's surface is the
///   gateway's own surface minus the names the operator hid from the gateway, and
///   `spawn` is not a way to get one of them back. `always_include` is untouched,
///   exactly as on the channel path.
fn gateway_turn_spawn_context(
    run_id: String,
    fabric_ctx: &GatewayFabricContext,
    tiering: &crate::config::ToolTieringConfig,
) -> crate::tools::sessions_spawn::SpawnExecutionContext {
    crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
        run_id,
        format!(
            "{}:{}:{}",
            fabric_ctx.channel, fabric_ctx.session_key, fabric_ctx.sender
        ),
    )
    .with_channel_excluded_tools(tiering.always_exclude.clone())
}

/// Run `inner` as the webhook turn itself: seeded with this turn's spawn
/// boundary, so anything it spawns inherits the gateway's own surface.
///
/// This is the production seam. `gateway_turn_spawn_context` only builds the
/// context; nothing enforced that the value actually reached the tool loop, so
/// emptying the list at the call site (or shadowing the binding) left every
/// test green while `sessions_spawn` handed a child the tools the operator hid
/// from the gateway. Binding construction and scoping together here gives that
/// wiring one place to be tested, and
/// `the_gateway_webhook_turn_seeds_its_spawn_boundary` in `tests/architecture_boundaries/`
/// pins that `run_gateway_chat_with_multimodal` still goes through it.
async fn within_gateway_turn_spawn_context<F>(
    run_id: String,
    fabric_ctx: &GatewayFabricContext,
    tiering: &crate::config::ToolTieringConfig,
    inner: F,
) -> F::Output
where
    F: std::future::Future,
{
    let turn_spawn_ctx = gateway_turn_spawn_context(run_id, fabric_ctx, tiering);
    crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
        .scope(turn_spawn_ctx, inner)
        .await
}

struct GatewayTurnResult {
    response: String,
    replay_unavailable: Option<crate::agent::terminal::oversized_reply::ReplayUnavailable>,
}

async fn run_gateway_chat_with_multimodal(
    state: &AppState,
    provider_label: &str,
    message: &str,
    fabric_ctx: &GatewayFabricContext,
    autosave_key: Option<&str>,
    stage: &mut TurnFailureClass,
) -> anyhow::Result<GatewayTurnResult> {
    // `stage` names the step that runs next, so a returned error is reported
    // by the step that failed and never by its text.
    *stage = TurnFailureClass::TurnNotReady;
    // The webhook routes admitted this turn; a typed refusal here only
    // covers a job that outlived its admission check.
    let turn_runtime = state.admit_turn_runtime()?;
    *stage = TurnFailureClass::ProviderBinding;
    turn_runtime
        .provider_binding
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("gateway provider binding is missing"))?
        .verify(&turn_runtime)?;
    let provider_label = turn_runtime
        .provider_binding
        .as_ref()
        .map_or(provider_label, |binding| binding.provider_name());
    let config_generation = Arc::clone(&turn_runtime.config_generation);
    let config_snapshot = Arc::clone(&config_generation.effective);
    let workspace_id = config_generation.effective.workspace_dir.to_string_lossy().to_string();
    let event_recording = config_generation.effective.memory.event_recording_config();
    let fabric = MemoryFabric::new(state.mem.clone(), workspace_id.clone()).with_event_recording(event_recording);
    let run_id = Uuid::new_v4().to_string();
    // D4 C4: migrate the gateway fabric durable session_key to the recipient-aware
    // canonical (`gateway:{channel}:{sender}:{recipient}`) while carrying the
    // pre-cutover legacy key (`gateway:webhook:{target}` / `gateway:{ch}:{sender}`)
    // for read-merge, so existing legacy history stays visible under the new key.
    let webhook_owner_id = fabric_ctx.webhook_owner_id(&workspace_id);
    // D-025: a `/webhook` turn's durable key is its credential-qualified
    // `gateway:webhook:{credential}:{reply_target}` itself, with no legacy key:
    // history under the old `gateway:webhook:{reply_target}` key cannot be
    // attributed to a credential, so it is neither migrated nor read-merged, and
    // continuity restarts once. Platform channel messages keep the canonical
    // key and their legacy read-merge.
    let (canonical_session_key, legacy_session_key) = if fabric_ctx.credential.is_some() {
        (fabric_ctx.session_key.clone(), None)
    } else {
        let legacy_session_key = fabric_ctx.session_key.clone();
        let canonical_session_key = RuntimeEnvelope::gateway(
            workspace_id.clone(),
            legacy_session_key.clone(),
            fabric_ctx.channel.clone(),
            fabric_ctx.sender.clone(),
            fabric_ctx.recipient.clone(),
            MemoryVisibility::Session,
        )
        .canonical_session_key();
        (canonical_session_key, Some(legacy_session_key))
    };
    let runtime_envelope = RuntimeEnvelope::gateway(
        workspace_id,
        canonical_session_key,
        fabric_ctx.channel.clone(),
        fabric_ctx.sender.clone(),
        fabric_ctx.recipient.clone(),
        // FIX-P1-04: webhook-driven gateway turns are session-scoped by default.
        // Persisting them as workspace-visible would leak per-session conversation
        // content to the whole workspace; Session keeps it bound to this session.
        MemoryVisibility::Session,
    )
    .with_run_id(run_id)
    .with_config_generation(&config_generation);
    let runtime_envelope = match legacy_session_key {
        Some(legacy_session_key) => runtime_envelope.with_legacy_session_key(legacy_session_key),
        None => runtime_envelope,
    };
    let runtime_envelope = match webhook_owner_id.as_deref() {
        Some(owner_id) => runtime_envelope.with_owner_id(owner_id),
        None => runtime_envelope,
    };
    *stage = TurnFailureClass::ReplayLookup;
    if fabric.records_recovery_terminal() {
        if let Some(key) = fabric_ctx.idempotency_key.as_deref() {
            if let Some(unavailable) = crate::agent::terminal::oversized_reply::for_input(
                fabric.memory(),
                fabric.workspace_id(),
                &runtime_envelope.message_scope(),
                key,
                message,
            )
            .await?
            {
                return Err(unavailable.into());
            }
        }
    }
    *stage = TurnFailureClass::ResourceDenied;
    authorize_gateway_config_resource_mutation(
        &config_snapshot,
        "gateway:webhook:message_event:user",
        ResourceRiskLevel::Low,
    )
    .map_err(|(_, body)| {
        let error = body
            .0
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("gateway resource mutation denied")
            .to_string();
        anyhow::anyhow!(error)
    })?;
    let cancellation = jobs::current_cancellation().map_or_else(CancellationToken::new, |token| token.child_token());
    let _turn_cancel_guard = cancellation.clone().drop_guard();
    *stage = TurnFailureClass::InputRecord;
    let pending_input = turn_source::GatewayPendingInput {
        generation: Arc::clone(&config_generation),
        fabric: fabric.clone(),
        envelope: runtime_envelope,
        // Arguments evaluate in order: a refused registration returns before
        // any acknowledgement exists, and the owner holds both from here on.
        owner: turn_source::PreInputOwner::new(
            turn_runtime.publication_slot.cleanup.register(cancellation.clone())?,
            jobs::register_terminal_cleanup(),
        ),
    };
    let (source_receipt, gateway_user_event, mut admission_guard) = pending_input
        .record(message.to_owned(), fabric_ctx.idempotency_key.clone())
        .await?;
    let runtime_envelope = source_receipt.envelope().clone();
    let base_scope = runtime_envelope.message_scope();
    *stage = TurnFailureClass::SourceBinding;
    let gateway_proof = source_receipt.bind(&turn_runtime, fabric_ctx)?;
    if let Some(key) = autosave_key {
        let write_scope = runtime_envelope.memory_write_context(if fabric_ctx.channel == "webhook" {
            "webhook"
        } else {
            "private"
        });
        let _ = state
            .mem
            .store_with_context_and_metadata(
                key,
                message,
                MemoryCategory::Conversation,
                None,
                Some(&write_scope),
                crate::memory::MemoryStoreMetadata {
                    workspace_id: Some(runtime_envelope.workspace_id.clone()),
                    owner_id: Some(runtime_envelope.resolved_owner_id()),
                    source_event_id: runtime_envelope.source_message_event_id.clone(),
                    source: Some("semantic_promotion".into()),
                    channel: Some(fabric_ctx.channel.clone()),
                    ..Default::default()
                },
            )
            .await;
    }
    // A spawned webhook job cannot inherit the caller's cleanup task-local.
    // Prediction and later work must retain the same concrete cleanup owner.
    *stage = TurnFailureClass::DecisionAdmission;
    let child_cleanup = match crate::runtime::turn_cleanup::CleanupScope::new(
        Some(&turn_runtime.publication_slot.cleanup),
        cancellation.clone(),
    ) {
        Ok(scope) => scope.with_acknowledgement(jobs::register_terminal_cleanup()),
        Err(error) => {
            if let Err(record_error) = admission_guard
                .finish_failure(
                    cancellation.is_cancelled(),
                    &format!("decision cleanup admission failed: {error}"),
                )
                .await
            {
                return Err(error.context(format!(
                    "Gateway admission failure persistence is unconfirmed: {record_error}"
                )));
            }
            return Err(error);
        }
    };
    let admission = child_cleanup
        .scope_admission(Box::pin(async {
            let service = turn_runtime.publication_slot.publish(&turn_runtime, state).await?;
            let context = service
                .open_issued_turn_context(
                    &gateway_proof,
                    crate::runtime::turn_context::AgentTurnKind::SingleShot,
                    &cancellation,
                )
                .await?;
            Ok::<_, anyhow::Error>((service, context))
        }))
        .await;
    let (service, context) = match admission {
        Ok(admitted) => admitted,
        Err(error) => {
            // Admission is no longer polled. Settle its owned accounting work
            // before acknowledging the persisted input's failure.
            let admission_error = format!("{error:#}");
            let error = match child_cleanup.run(Box::pin(async { Err::<(), _>(error) })).await {
                Err(error) if error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>() => {
                    let context = format!(
                        "decision admission failed: {admission_error}; Gateway admission cleanup is unconfirmed: {error:#}"
                    );
                    error.context(context)
                }
                Err(error) => error,
                Ok(()) => anyhow::anyhow!(admission_error),
            };
            if let Err(record_error) = admission_guard
                .finish_failure(
                    cancellation.is_cancelled(),
                    &format!("decision admission failed: {error}"),
                )
                .await
            {
                return Err(error.context(format!(
                    "Gateway admission failure persistence is unconfirmed: {record_error}"
                )));
            }
            return Err(error);
        }
    };
    // Bind before the handoff: a refusal here still has the admission guard
    // to record the failed input and answer the job monitor.
    *stage = TurnFailureClass::TerminalBinding;
    let terminal_binding = service.terminal_binding(&context)?;
    let (cleanup_ticket, admission_acknowledgement) = admission_guard.into_terminal_parts()?;
    let terminal_guard = turn_source::publication_slot::GatewayTerminalGuard::new(
        cleanup_ticket,
        Arc::clone(service),
        Arc::clone(&context),
        fabric.clone(),
        Arc::clone(&config_snapshot),
        runtime_envelope.clone(),
        admission_acknowledgement,
    )?;
    *stage = TurnFailureClass::TurnPreparation;
    let turn_result: anyhow::Result<GatewayTurnResult> = child_cleanup
        .run(async {
            let mem_context = crate::agent::context_recall::scoped_recall(
                state.mem.as_ref(),
                &crate::agent::context_recall::RecallScope::from_envelope(
                    &runtime_envelope,
                    &config_snapshot.memory,
                    if fabric_ctx.channel == "webhook" {
                        "webhook"
                    } else {
                        "private"
                    },
                )
                .with_current_input_key(autosave_key.map(str::to_owned)),
                message,
                crate::agent::context_recall::RECALL_CONTEXT_BUDGET,
            )
            .await;
            let enriched_message = if mem_context.preamble.is_empty() {
                message.to_string()
            } else {
                format!("{}{}", mem_context.preamble, message)
            };

            let user_messages = vec![crate::agent::turn_contracts::inbound_user_message(&enriched_message)];
            let image_marker_count = crate::multimodal::count_image_markers(&user_messages);
            let adapter_capabilities = turn_runtime.provider.capabilities_for(
                &turn_runtime.model,
                crate::providers::traits::ProviderRequestMode::NonStreaming,
            );
            let mode_capabilities = providers::selection::catalog_for(&config_snapshot)
                .effective_provider_capabilities(provider_label, &turn_runtime.model, adapter_capabilities);
            if image_marker_count > 0 && !mode_capabilities.vision {
                return Err(ProviderCapabilityError {
                    provider: providers::redact_endpoint_userinfo(provider_label).into_owned(),
                    capability: "vision".to_string(),
                    message: format!(
                        "received {image_marker_count} image marker(s), but this provider does not support vision input"
                    ),
                }
                .into());
            }

            // Build system prompt with native_tools flag so the prompt instructs the
            // LLM to use tools rather than emit XML tags.
            let multimodal_config = config_snapshot.multimodal.clone();
            let native_tools = mode_capabilities.native_tool_calling;
            let selected_skills = service.agent_prompt_skills(&gateway_proof, &context)?;
            // The runtime prompt wrapper owns the whole prompt policy — the
            // `skill_read` gate on the skill list and the `[agent] compact_context`
            // bootstrap cap — so the webhook builds its prompt exactly as CLI, chat and
            // Console sessions do. It used to call the canonical builder directly with
            // a hardcoded `bootstrap_max_chars = None` (E3, webhook half).
            let system_prompt = crate::agent::runtime_prompt::build_runtime_system_prompt(
                &config_snapshot,
                &turn_runtime.model,
                &selected_skills,
                native_tools,
            );

            let mut history = Vec::with_capacity(2 + user_messages.len());
            history.push(ChatMessage::system(system_prompt));
            history.extend(user_messages);

            let noop_observer = NoopObserver;

            // P1-a: route the webhook agent loop through the unified `SecurityPolicy::decide`
            // gate instead of leaving `scope_ctx = None` (which short-circuits to `Allow`
            // and bypasses both the scope ACL and the autonomy level entirely).
            //
            // Webhook turns are Bearer-token authenticated (`handle_webhook` rejects
            // unauthenticated requests), so the request is a trusted principal and we can
            // construct a `ScopeContext` from the gateway identity. This makes every tool
            // call honour the configured autonomy level: under `supervised`, side-effecting
            // tools resolve to `Ask`, and with no `ApprovalManager` wired on this path the
            // tool loop fail-closes (deny) — only genuinely read-only tools run. Under
            // `full` autonomy the behaviour is unchanged (everything allowed).
            let gateway_policy =
                crate::security::SecurityPolicy::from_config(&config_snapshot.autonomy, &config_snapshot.workspace_dir);
            let gateway_scope_ctx = crate::agent::turn_contracts::ScopeContext {
                policy: &gateway_policy,
                sender: fabric_ctx.sender.as_str(),
                channel: fabric_ctx.channel.as_str(),
                // Gateway/webhook turns are 1:1 (no group semantics); use a stable
                // chat_type so scope rules can target the gateway surface.
                chat_type: "gateway",
                chat_id: fabric_ctx.session_key.as_str(),
                session_key: Some(runtime_envelope.session_key.as_str()),
                // R-MCPB F-2: the credential-bound owner, so run views and owner-scoped
                // rows follow the authenticated caller rather than its `reply_target`.
                owner_id: webhook_owner_id.as_deref(),
                owner_isolated: runtime_envelope.owner_isolated,
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                config_generation_id: runtime_envelope.config_generation_id,
                config_source_revision: runtime_envelope.config_source_revision.as_deref(),
            };

            // OQ-23: the webhook turn runs the same bounded-request contract as every
            // other execution surface — pre-turn flush/paging, the preflight before
            // each provider call (with the published tool schemas reserved) and the
            // reserved mid-turn trim. The window is the *effective* one, which applies
            // the operator/discovery/catalog limits of the model this turn actually serves, as the
            // CLI and TUI do.
            let mut effective_compaction = service.effective_compaction(provider_label, &turn_runtime.model);
            effective_compaction.config = context.main_compaction().clone();
            crate::providers::context_window::trace_effective_compaction_resolution(&effective_compaction);

            *stage = TurnFailureClass::TurnLoop;
            let provider_started_at = chrono::Utc::now();
            let route_decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
                provider_label,
                turn_runtime.model.clone(),
                runtime_envelope.resolved_owner_id(),
                runtime_envelope.session_key.clone(),
                runtime_envelope.source_message_event_id.clone(),
                None,
                "gateway_webhook",
                u32::try_from(message.chars().count() / 4).unwrap_or(u32::MAX),
                !turn_runtime.tools_registry.is_empty(),
                false,
            );
            // R3: name this turn's real origin before any tool runs. The gateway owns no
            // channel of its own — `message_send` falls back to whichever handle the
            // daemon registered — so without this every webhook turn's outbound scope
            // reads as that handle's channel and a rule written for the surface the
            // message actually came from can never match. Reply routing is untouched;
            // only rule matching changes.
            //
            // G2: the same turn also seeds the spawn execution context. A sub-agent the
            // webhook turn starts inherits (a) this turn's run_id as its parent, and (b)
            // the gateway surface's own hidden tool names. The gateway is not an IM
            // channel, so it has no `channel_exclude` list of its own: it publishes
            // `config_snapshot.tool_tiering` rather than `for_channel_surface()`. The
            // boundary a child must therefore inherit is exactly `always_exclude` — the
            // gateway's own tool set minus what the operator hid from it — so a spawn can
            // never be a way to get back a tool the gateway itself may not call.
            let loop_result = within_gateway_turn_spawn_context(
                runtime_envelope
                    .run_id
                    .clone()
                    .unwrap_or_else(|| Uuid::new_v4().to_string()),
                fabric_ctx,
                &config_snapshot.tool_tiering,
                crate::tools::message_send::with_outbound_origin_channel(fabric_ctx.channel.as_str(), async {
                    run_turn(
                        &mut history,
                        crate::agent::turn_request::TurnRequest {
                            advice: service.advisory().analyze_turn(context.runtime(), &cancellation).await,
                            // no approval manager
                            channel_name: "webhook",
                            // no streaming delta sender
                            scope: Some(&gateway_scope_ctx),
                            mode: crate::agent::turn_contracts::ChatMode::default(),
                            silent: true,
                            expose_stay_silent: false,
                            multimodal: &multimodal_config,
                            schedule: crate::agent::turn_request::TurnSchedule {
                                read_only_tool_concurrency_window: config_snapshot
                                    .agent
                                    .read_only_tool_concurrency_window,
                                priority_scheduling_enabled: config_snapshot.agent.priority_scheduling_enabled,
                                low_priority_tool_names: config_snapshot.agent.low_priority_tools.clone(),
                            },
                            compaction: Some(&effective_compaction.config),
                            // no tool call notifications
                            decision: crate::agent::turn_request::TurnDecision {
                                service,
                                context: &context,
                                runtime: context.runtime(),
                                proof: &gateway_proof,
                            },
                        },
                        crate::agent::turn_request::TurnServices {
                            provider: turn_runtime.provider.as_ref(),
                            provider_name: provider_label,
                            model: &turn_runtime.model,
                            temperature: turn_runtime.temperature,
                            tools_registry: Arc::clone(&turn_runtime.tools_registry),
                            observer: &noop_observer,
                            hooks: state.hooks.as_ref(),
                            // silent
                            approval: None,
                            approval_resolver: None,
                            runtime_adapter: Some({
                                crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
                                    live_frames: None,
                                    guarded_delivery: false,
                                    provider_trace: None,
                                    events: None,
                                    stream_provider: false,
                                    tool_execution_service: None,
                                    tool_execution_context: crate::tools::ToolExecutionContext::new(
                                        runtime_envelope.clone(),
                                        if fabric_ctx.channel == "webhook" {
                                            "webhook"
                                        } else {
                                            "private"
                                        },
                                    )
                                    .with_dispatch_origin(crate::tools::DispatchOrigin::ModelTurn)
                                    .with_approval_ledger(service.execution_approval_ledger())
                                    .with_chat_id(&runtime_envelope.session_key),
                                    allowed_tool_names: None,
                                }
                            }),
                            memory: crate::agent::turn_request::ToolLoopMemory::new(
                                &state.mem,
                                &config_snapshot.workspace_dir,
                                Some(DocumentIngestRuntime::from_envelope(
                                    state.mem.clone(),
                                    &runtime_envelope,
                                )),
                            )
                            .with_event_fabric(
                                MemoryFabric::new(state.mem.clone(), runtime_envelope.workspace_id.clone())
                                    .with_event_recording(config_snapshot.memory.event_recording_config()),
                            )
                            .with_request_event_scope({
                                let mut scope = base_scope.clone();
                                scope.causation_event_id =
                                    gateway_user_event.as_ref().map(|event| event.event_id.clone());
                                scope
                            }),
                        },
                        crate::agent::turn_request::TurnDelivery {
                            cancellation: Some(cancellation.clone()),
                            // no cancellation token
                            on_delta: None,
                            // P1-a: gateway turns now route through decide()
                            on_tool_call: None,
                        },
                    )
                    .await
                    .map(|(outcome, trace)| (outcome.into_text(), trace))
                }),
            )
            .await;
            let (response, trace) = match loop_result {
                Ok(result) => result,
                Err(error) => {
                    let cancelled =
                        cancellation.is_cancelled() || crate::agent::turn_contracts::is_tool_loop_cancelled(&error);
                    *stage = TurnFailureClass::Internal;
                    let terminal_id = runtime_envelope
                        .run_id
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("gateway source run id is missing"))?;
                    if let Err(finalize_error) = turn_source::publication_slot::commit_terminal(
                        Some(&terminal_guard),
                        &fabric,
                        crate::agent::terminal::TurnTerminalDraft {
                            binding: Some(terminal_binding.clone()),
                            terminal_id,
                            scope: runtime_envelope.message_scope(),
                            status: if cancelled {
                                crate::agent::terminal::TurnTerminalStatus::Cancelled
                            } else {
                                crate::agent::terminal::TurnTerminalStatus::Failed
                            },
                            history: None,
                            history_scope: None,
                            provider_outcome: None,
                            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                                summary: error.to_string(),
                                started_at: provider_started_at,
                                finished_at: chrono::Utc::now(),
                            },
                            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                        },
                        &config_snapshot,
                    )
                    .await
                    {
                        *stage = terminal_guard.failure_reason().into();
                        return Err(finalize_error);
                    }
                    *stage = if cancelled {
                        TurnFailureClass::Cancelled
                    } else {
                        TurnFailureClass::TurnLoop
                    };
                    return Err(error);
                }
            };
            *stage = TurnFailureClass::Internal;
            anyhow::ensure!(
                trace.execution_trace.as_ref() == Some(&context.terminal_execution_trace()),
                "gateway returned execution trace differs from its owning decision"
            );

            *stage = TurnFailureClass::ResourceDenied;
            authorize_gateway_config_resource_mutation(
                &config_snapshot,
                "gateway:webhook:message_event:assistant",
                ResourceRiskLevel::Low,
            )
            .map_err(|(_, body)| {
                let error = body
                    .0
                    .get("error")
                    .and_then(|value| value.as_str())
                    .unwrap_or("gateway resource mutation denied")
                    .to_string();
                anyhow::anyhow!(error)
            })?;
            let provider_outcome =
                crate::agent::terminal::provider_outcome_from_trace(&route_decision, provider_started_at, trace);
            let terminal_id = runtime_envelope
                .run_id
                .clone()
                .unwrap_or_else(|| provider_outcome.decision_id.clone());
            let committed = turn_source::publication_slot::commit_terminal(
                Some(&terminal_guard),
                &fabric,
                crate::agent::terminal::TurnTerminalDraft {
                    binding: Some(terminal_binding.clone()),
                    terminal_id,
                    scope: runtime_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Completed,
                    history: Some(crate::agent::terminal::TurnHistoryProjection {
                        assistant_content: response.clone(),
                        history_commit_len: history.len(),
                    }),
                    history_scope: Some(
                        base_scope
                            .clone()
                            .with_sender(format!("{provider_label}/{}", turn_runtime.model))
                            .with_recipient(fabric_ctx.sender.clone()),
                    ),
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: "gateway webhook completed".to_string(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                },
                &config_snapshot,
            )
            .await;
            let receipt = match committed {
                Ok(receipt) => receipt,
                Err(error) => {
                    *stage = terminal_guard.failure_reason().into();
                    return Err(error);
                }
            };
            *stage = TurnFailureClass::ReplayLookup;
            let replay_unavailable = crate::agent::terminal::oversized_reply::from_receipt(&receipt)?;
            // The turn's own work is done; what fails from here is its
            // owned cleanup settling.
            *stage = TurnFailureClass::TurnCleanup;
            Ok(GatewayTurnResult {
                response,
                replay_unavailable,
            })
        })
        .await;
    if let Err(error) = &turn_result {
        if let Err(finish_error) = terminal_guard.finish_failure(error, cancellation.is_cancelled()).await {
            *stage = terminal_guard.failure_reason().into();
            return Err(finish_error);
        }
    }
    turn_result
}

/// Webhook request body
#[derive(serde::Deserialize)]
pub struct WebhookBody {
    pub message: String,
    #[serde(default)]
    pub reply_target: Option<String>,
}
