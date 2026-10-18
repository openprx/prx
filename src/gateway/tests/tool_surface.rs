#![cfg(test)]

use super::*;

/// A platform webhook's job row must not publish the chat it came from.
///
/// This label is the `prx tasks list` name of the detached turn, and
/// `GET /api/runtime/tasks` serves it to anyone the pairing check admits. On
/// this route `reply_target` is a real platform address, so leaving it in
/// the clear would have leaked through the operator plane exactly what the
/// outbound path fingerprints. Asserted against the shared function's own
/// output, because agreeing *by construction* is the property that broke
/// last time, not agreeing by coincidence.
///
/// MUTATION GUARD: interpolate `reply_target` into
/// `channel_webhook_job_label` and this fails.
#[test]
fn a_channel_webhook_job_label_fingerprints_the_chat() {
    let chat = "-1001234567890";
    let msg = ChannelMessage {
        reply_target: chat.to_string(),
        channel: "telegram".to_string(),
        ..Default::default()
    };
    let label = channel_webhook_job_label("telegram", Some(&msg));

    assert!(!label.contains(chat), "the chat must not appear in plaintext: {label}");
    assert!(
        label.starts_with("gateway:telegram:"),
        "the channel stays legible: {label}"
    );
    assert!(
        label.contains(&crate::security::op_id::ref_for_channel_recipient("telegram", chat)),
        "the chat must appear as the shared fingerprint: {label}"
    );
    // A delivery that carries no message has nothing to name, and must not
    // grow a placeholder that looks like an address.
    assert_eq!(channel_webhook_job_label("telegram", None), "gateway:telegram");
}

/// A gateway with no Signal account still publishes `sessions_spawn`.
///
/// The tool's channel argument is only the fallback announce destination, but
/// registration used to hang off `signal.as_ref().map(..)`: on a host without
/// Signal, a webhook turn simply had no way to start a sub-agent. The whole
/// registry is built here by the production builder, not by a copy of it.
///
/// Registration now lives in `register_gateway_turn_tools`, shared with the
/// `prx gateway` path, so this test covers both launch modes' registration
/// body (see `both_gateway_launch_paths_publish_the_same_tool_surface` for
/// the two-path equality).
///
/// MUTATION GUARD: put the registration in `register_gateway_turn_tools`
/// back behind `if let Some(signal)` and the first assertion goes red.
#[test]
fn a_gateway_without_signal_still_publishes_sessions_spawn() {
    let workspace = tempfile::tempdir().unwrap();
    let test_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: gateway construction runtime");
    let _test_runtime_guard = test_runtime.enter();
    let config = Config {
        workspace_dir: workspace.path().to_path_buf(),
        // The builder constructs a real provider chain, which refuses to come
        // up with no credential at all. Nothing here ever calls out.
        api_key: Some("test-key".to_string()),
        ..Config::default()
    };
    let manager = crate::config::new_shared(config);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).unwrap());
    let hooks = Arc::new(HookManager::new(workspace.path().to_path_buf()));

    let without_signal = build_gateway_turn_runtime(
        manager.pin(),
        manager.clone(),
        Arc::clone(&memory),
        None,
        None,
        None,
        None,
        Arc::clone(&hooks),
        #[cfg(feature = "wasm-plugins")]
        None,
    )
    .expect("test: the gateway turn runtime must build without any channel");
    let names: Vec<&str> = without_signal.tools_registry.iter().map(|tool| tool.name()).collect();
    assert!(
        names.contains(&"sessions_spawn"),
        "a gateway without Signal must still publish sessions_spawn: {names:?}"
    );

    // With Signal configured the tool is published exactly as before, so this
    // is a widening and not a swap.
    let signal = Arc::new(crate::channels::SignalChannel::new(
        "http://127.0.0.1:1".to_string(),
        "+10000000000".to_string(),
        None,
        Vec::new(),
        false,
        false,
        crate::config::MediaConfig::default(),
    ));
    let with_signal = build_gateway_turn_runtime(
        manager.pin(),
        manager.clone(),
        memory,
        Some(signal),
        None,
        None,
        None,
        hooks,
        #[cfg(feature = "wasm-plugins")]
        None,
    )
    .expect("test: the gateway turn runtime must build with Signal");
    let signal_names: Vec<&str> = with_signal.tools_registry.iter().map(|tool| tool.name()).collect();
    assert!(
        signal_names.contains(&"sessions_spawn"),
        "the Signal-configured gateway must keep sessions_spawn: {signal_names:?}"
    );
}

/// Both gateway launch paths publish the same tool surface.
///
/// `build_gateway_turn_runtime` (daemon, and again on every hot reload) and
/// `run_gateway` (`prx gateway`, no daemon) used to carry two symmetric
/// copies of the channel-aware registration block, and only the first was
/// covered: a change to one silently gave the two launch modes different
/// tool surfaces (independent audit 2026-09-16 §2.3). Both call
/// `register_gateway_turn_tools` now. This test drives the daemon path end
/// to end and the `prx gateway` path through the shared registrar over the
/// same configuration, then pins the two tool-name sets to each other.
///
/// MUTATION GUARD: drop the `sessions_spawn` push from
/// `register_gateway_turn_tools` and both `sessions_spawn` assertions fail;
/// make the registrar's registration conditional on Signal and they fail too.
#[test]
fn both_gateway_launch_paths_publish_the_same_tool_surface() {
    let workspace = tempfile::tempdir().unwrap();
    let test_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: gateway construction runtime");
    let _test_runtime_guard = test_runtime.enter();
    let config = Config {
        workspace_dir: workspace.path().to_path_buf(),
        // The provider chain refuses to come up with no credential at all.
        // Nothing here ever calls out.
        api_key: Some("test-key".to_string()),
        ..Config::default()
    };
    let manager = crate::config::new_shared(config.clone());
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).unwrap());
    let hooks = Arc::new(HookManager::new(workspace.path().to_path_buf()));

    // Path 1 — the daemon / hot-reload builder, end to end.
    let daemon = build_gateway_turn_runtime(
        manager.pin(),
        manager.clone(),
        Arc::clone(&memory),
        None,
        None,
        None,
        None,
        Arc::clone(&hooks),
        #[cfg(feature = "wasm-plugins")]
        None,
    )
    .expect("test: the daemon gateway turn runtime must build");
    let mut daemon_names: Vec<String> = daemon
        .tools_registry
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    daemon_names.sort_unstable();

    // Path 2 — `prx gateway`: the same base registry, then the shared
    // registrar with the arguments that entry point supplies.
    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);
    let provider: Arc<dyn Provider> = Arc::from(
        providers::create_provider_from_test_config(&config, &provider_runtime_options)
            .expect("test: the provider chain must build"),
    );
    let model = config
        .default_model
        .clone()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4".into());
    let security = crate::runtime::bootstrap::build_security_policy(&config);
    let runtime_adapter: Arc<dyn runtime::RuntimeAdapter> =
        Arc::from(runtime::create_runtime(&config.runtime).expect("test: the runtime adapter must build"));
    let extensions = hooks.control_tool_arcs(Arc::clone(&security));
    let mut direct_tools = tools::all_tools_with_runtime_ext_and_extensions(
        Arc::new(config.clone()),
        manager.clone(),
        &security,
        runtime_adapter,
        Arc::clone(&memory),
        None,
        None,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents,
        config.api_key.as_deref(),
        &config,
        extensions,
    )
    .tools;
    let (_channels_registry, spawn_tools_handle) = register_gateway_turn_tools(
        &mut direct_tools,
        &config,
        &manager,
        &security,
        &provider,
        &model,
        &memory,
        &provider_runtime_options,
        None,
        None,
        None,
        None,
    )
    .expect("gateway tool registration");
    let mut direct_names: Vec<String> = direct_tools.iter().map(|tool| tool.name().to_string()).collect();
    direct_names.sort_unstable();

    assert_eq!(
        daemon_names, direct_names,
        "the daemon gateway and `prx gateway` must publish the same tools"
    );
    // Named explicitly, so the equality above cannot be satisfied by two
    // equally broken surfaces.
    for names in [&daemon_names, &direct_names] {
        assert!(
            names.iter().any(|name| name == "sessions_spawn"),
            "every gateway launch path must publish sessions_spawn: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == "config_reload"),
            "every gateway launch path must publish config_reload: {names:?}"
        );
        // The webhook turn is budgeted with the default `switch` mode, and
        // a switch rollover refuses to run without its recovery tool.
        assert!(
            names
                .iter()
                .any(|name| name == crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME),
            "every gateway launch path must publish the context-switch recovery tool: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name == "message_send"),
            "message_send stays behind Signal on both paths: {names:?}"
        );
    }
    assert!(
        matches!(spawn_tools_handle.get(), Ok(None)),
        "the spawn tools handle is filled by the caller once the registry is sealed"
    );
}

/// Assert that every name in `exposed` resolves in both production gateway
/// registries, using the predicate the MCP handler itself applies.
///
/// `context` names the configuration the caller set up, so a failure says
/// which one broke parity.
fn assert_exposed_mcp_tools_resolve(config: &Config, exposed: &[String], context: &str) {
    assert!(
        !exposed.is_empty(),
        "sanity: the advertised MCP surface must not be empty, or the loop below passes vacuously"
    );
    let (daemon, direct, _direct_mcp) = production_gateway_registries(config);
    for (path, registry) in [("the daemon gateway", daemon.as_ref()), ("`prx gateway`", &direct)] {
        for name in exposed {
            // `supports_name`, not `name()`: this is the exact predicate
            // `mcp_tools_call` admits a request with, so an alias-carrying
            // adapter counts here exactly as it does in production.
            assert!(
                registry.iter().any(|tool| tool.supports_name(name)),
                "{path} advertises the default-exposed MCP tool `{name}` through \
                     `GET /mcp/v1/tools/list` but cannot resolve it, so \
                     `POST /mcp/v1/tools/call` answers 404 `exposed tool is not available` \
                     ({context}); registry: {:?}",
                registry.iter().map(|tool| tool.name()).collect::<Vec<_>>()
            );
        }
    }
}

/// V02-R7: every default-exposed MCP tool resolves in the production
/// gateway registry.
///
/// Discovery reads configuration — `GET /mcp/v1/tools/list` projects
/// `mcp_server.exposed_tools` verbatim — while execution reads the registry:
/// `POST /mcp/v1/tools/call` admits a name only when
/// `state.tools_registry` resolves it. Nothing bound the two, so
/// `sessions_list` and `sessions_history` were advertised as default
/// capabilities and answered 404 `exposed tool is not available`.
///
/// The advertised set is read from the configuration default rather than
/// from a hardcoded list, so a new entry in
/// `default_mcp_server_exposed_tools` extends this test automatically.
///
/// MUTATION GUARD: remove either session-read push from
/// `register_gateway_turn_tools` and this goes red on both launch paths.
#[test]
fn every_default_exposed_mcp_tool_resolves_in_the_production_gateway_registry() {
    let workspace = tempfile::tempdir().unwrap();
    let test_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: gateway construction runtime");
    let _test_runtime_guard = test_runtime.enter();
    let config = Config {
        workspace_dir: workspace.path().to_path_buf(),
        // The builder constructs a real provider chain, which refuses to
        // come up with no credential at all. Nothing here ever calls out.
        api_key: Some("test-key".to_string()),
        ..Config::default()
    };
    let exposed = config.mcp_server.exposed_tools.clone();
    // Anchors, so the loop cannot pass because the two names were quietly
    // withdrawn from the advertised set instead of registered.
    for anchor in ["sessions_list", "sessions_history"] {
        assert!(
            exposed.iter().any(|name| name == anchor),
            "sanity: `{anchor}` must stay default-exposed for this test to mean anything: {exposed:?}"
        );
    }

    assert_exposed_mcp_tools_resolve(&config, &exposed, "default configuration");
}

/// V02-R7b: the same parity question with `memory.acl_enabled = true`.
/// The production registry keeps `memory_recall` callable and constructs
/// it in ACL mode, matching the name advertised by MCP discovery.
#[test]
fn every_default_exposed_mcp_tool_resolves_when_memory_acl_is_enabled() {
    let workspace = tempfile::tempdir().unwrap();
    let test_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: gateway construction runtime");
    let _test_runtime_guard = test_runtime.enter();
    let mut config = Config {
        workspace_dir: workspace.path().to_path_buf(),
        // The builder constructs a real provider chain, which refuses to
        // come up with no credential at all. Nothing here ever calls out.
        api_key: Some("test-key".to_string()),
        ..Config::default()
    };
    config.memory.acl_enabled = true;
    let exposed = config.mcp_server.exposed_tools.clone();
    assert!(
        exposed.iter().any(|name| name == "memory_recall"),
        "sanity: `memory_recall` must stay default-exposed for this guard to mean anything: {exposed:?}"
    );

    assert_exposed_mcp_tools_resolve(&config, &exposed, "memory.acl_enabled = true");
}

/// The gateway's `sessions_list` reads the registry its own
/// `sessions_spawn` writes to.
///
/// Registering the two session read tools over a registry of their own
/// would satisfy
/// `every_default_exposed_mcp_tool_resolves_in_the_production_gateway_registry`
/// while answering every caller with an empty list — a 200 that lies,
/// which is worse than the 404 it replaced. This drives the production
/// registrar's own `sessions_spawn`, then asks the production registrar's
/// own `sessions_list` to name the run back.
///
/// Nothing here reaches the network: `api_url` points at a closed loopback
/// port, and the run is registered before the sub-agent ever contacts a
/// provider (the pattern proven in `gateway::api::runtime`'s spawn tests).
///
/// The assertion is on the run's `source=runtime` marker, not merely on the
/// task string. `sessions_list` falls through to the memory projection when
/// the registry does not answer, and `sessions_spawn` has already written a
/// `workspace`-visibility `task.spawned` event carrying the same task
/// string — so a disconnected registry still renders the probe, as
/// `source=memory, manageable=false`. Only `source=runtime` distinguishes
/// the two wirings.
///
/// MUTATION GUARD: construct the two tools over a fresh
/// `Arc::new(RwLock::new(Vec::new()))` instead of
/// `spawn_tool.active_runs_arc()` and this goes red while the parity test
/// stays green.
#[tokio::test]
async fn the_gateway_sessions_list_sees_a_run_started_by_the_gateway_sessions_spawn() {
    let workspace = tempfile::tempdir().expect("test: gateway registry workspace");
    let state = two_token_webhook_state(workspace.path());
    let spawned = webhook_as(
        &state,
        "token-alpha-0001",
        "Use sessions_spawn for SHARED-REGISTRY-SPAWN",
        "registry-probe",
    )
    .await;
    assert!(spawned.contains("run_id"), "test: the gateway spawn ran: {spawned}");

    let runtime = state.pin_turn_runtime();
    let list = runtime
        .tools_registry
        .iter()
        .find(|tool| tool.supports_name("sessions_list"))
        .expect("test: the gateway must publish sessions_list");
    let mut output = String::new();
    for _ in 0..200 {
        output = list
            .execute(serde_json::json!({}))
            .await
            .expect("test: sessions_list must answer")
            .output;
        if output.contains(SHARED_RUN_REGISTRY_PROBE_TASK) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // `sessions_list` joins one run's lines with a blank line, so a run's
    // `source=` marker and its `task:` line live in the same block.
    fn probe_block(output: &str) -> Option<&str> {
        output
            .split("\n\n")
            .find(|block| block.contains(SHARED_RUN_REGISTRY_PROBE_TASK))
    }

    let block = probe_block(&output).unwrap_or_default();
    assert!(
        !block.is_empty(),
        "the gateway's sessions_list must name the run its sessions_spawn started: {output}"
    );
    assert!(
        block.contains("source=runtime") && block.contains("manageable=true"),
        "the gateway's sessions_list must read the same run registry its sessions_spawn writes \
             to; a registry of its own falls through to the memory projection and renders the same \
             run as `source=memory, manageable=false`: {output}"
    );
}

/// Signal keeps priority as the announce destination; without it the gateway
/// announces on its own sink rather than withholding the tool.
///
/// MUTATION GUARD: make `gateway_spawn_announce_channel` ignore its argument
/// and the Signal assertion goes red; make it return `signal` unchanged and
/// the fallback assertion goes red.
#[tokio::test]
async fn the_spawn_announce_channel_prefers_signal_and_falls_back_to_the_gateway() {
    let signal = Arc::new(crate::channels::SignalChannel::new(
        "http://127.0.0.1:1".to_string(),
        "+10000000000".to_string(),
        None,
        Vec::new(),
        false,
        false,
        crate::config::MediaConfig::default(),
    )) as Arc<dyn Channel>;
    let chosen = gateway_spawn_announce_channel(Some(Arc::clone(&signal)));
    assert!(
        Arc::ptr_eq(&chosen, &signal),
        "a configured Signal channel must stay the announce destination"
    );

    let fallback = gateway_spawn_announce_channel(None);
    assert_eq!(
        fallback.name(),
        "gateway",
        "without Signal the announcement falls back to the gateway's own sink"
    );
    // The sink accepts and drops; it must never fail a run, and it must never
    // become an inbound listener.
    fallback
        .send(&SendMessage::new("sub-agent result", "webhook-client"))
        .await
        .expect("the gateway sink must accept an announcement");
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    assert!(
        fallback.listen(tx).await.is_err(),
        "the gateway sink must refuse to become an inbound listener"
    );
}

/// A sub-agent spawned from a webhook turn inherits the gateway's own
/// boundary. The gateway is not an IM channel, so it has no `channel_exclude`
/// list; the names it must hand down are exactly `always_exclude`, i.e. the
/// child surface is the gateway's surface minus what the operator hid.
///
/// The filter that consumes this list is pinned separately by
/// `agent::turn_engine::tests::engine_surface::a_spawned_sub_agent_inherits_the_parent_channels_tool_exclusions`;
/// what is pinned here is that the gateway turn actually seeds it.
///
/// MUTATION GUARD: drop `.with_channel_excluded_tools(..)` from
/// `gateway_turn_spawn_context` and the first assertion goes red.
#[tokio::test]
async fn a_webhook_turn_hands_the_gateways_own_boundary_to_what_it_spawns() {
    let fabric_ctx = GatewayFabricContext::generic_webhook(Some("webhook-client"), "public", None);
    let tiering = crate::config::ToolTieringConfig {
        always_exclude: vec!["gateway".to_string(), "config_reload".to_string()],
        ..crate::config::ToolTieringConfig::default()
    };
    let ctx = gateway_turn_spawn_context("turn-run-1".to_string(), &fabric_ctx, &tiering);
    let inherited = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
        .scope(ctx, async {
            crate::tools::sessions_spawn::ambient_channel_excluded_tools()
        })
        .await;
    assert_eq!(
        inherited,
        vec!["gateway".to_string(), "config_reload".to_string()],
        "a webhook-spawned sub-agent must run behind the gateway's own boundary"
    );

    // Fixture proof: with nothing hidden from the gateway the child inherits
    // nothing, so the assertion above is about the seed and not about the
    // task-local always being non-empty.
    let open_ctx = gateway_turn_spawn_context(
        "turn-run-2".to_string(),
        &fabric_ctx,
        &crate::config::ToolTieringConfig::default(),
    );
    assert!(
        crate::config::ToolTieringConfig::default().always_exclude.is_empty(),
        "fixture is useless unless the default gateway surface hides nothing"
    );
    let open = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
        .scope(open_ctx, async {
            crate::tools::sessions_spawn::ambient_channel_excluded_tools()
        })
        .await;
    assert!(
        open.is_empty(),
        "an unrestricted gateway hands down no boundary: {open:?}"
    );
}

/// The webhook turn's tool loop really runs inside the seeded context.
///
/// `a_webhook_turn_hands_the_gateways_own_boundary_to_what_it_spawns` pins
/// the builder; this pins the seam the builder is useless without. The
/// future handed to `within_gateway_turn_spawn_context` stands in for the
/// tool loop `run_gateway_chat_with_multimodal` passes, and it reads exactly
/// what `sessions_spawn` reads when a webhook turn calls it.
///
/// MUTATION GUARD: emptying the list at the seeding site
/// (`.with_channel_excluded_tools(Vec::new())` on the value the wrapper
/// scopes) or shadowing `turn_spawn_ctx` with a bare
/// `SpawnExecutionContext::seed_turn_context(..)` turns both assertion
/// groups below red. That `run_gateway_chat_with_multimodal` still routes
/// through this wrapper is pinned by
/// `tests/architecture_boundaries.rs::the_gateway_webhook_turn_seeds_its_spawn_boundary`.
#[tokio::test]
async fn the_webhook_turns_tool_loop_runs_inside_the_seeded_spawn_context() {
    let fabric_ctx = GatewayFabricContext::generic_webhook(Some("webhook-client"), "public", None);
    let tiering = crate::config::ToolTieringConfig {
        always_exclude: vec!["gateway".to_string(), "config_reload".to_string()],
        ..crate::config::ToolTieringConfig::default()
    };

    let (excluded, parent_run_id, is_turn_root) =
        within_gateway_turn_spawn_context("turn-run-42".to_string(), &fabric_ctx, &tiering, async {
            (
                crate::tools::sessions_spawn::ambient_channel_excluded_tools(),
                crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                    .try_with(|ctx| ctx.run_id.clone())
                    .ok(),
                crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                    .try_with(|ctx| ctx.is_turn_root)
                    .ok(),
            )
        })
        .await;

    assert_eq!(
        excluded,
        vec!["gateway".to_string(), "config_reload".to_string()],
        "the tool loop the webhook turn runs must already stand behind the gateway's boundary,              or one sessions_spawn call hands a child the tools the operator hid"
    );
    assert_eq!(
        parent_run_id.as_deref(),
        Some("turn-run-42"),
        "a task spawned from the webhook turn must attach to that turn's run_id"
    );
    assert_eq!(
        is_turn_root,
        Some(true),
        "the webhook turn is a turn root: its first child reports spawn depth 0"
    );

    // Fixture proof: the assertions above are about the seeding, not about
    // the task-local being populated by something else. Nothing hidden from
    // the gateway means nothing inherited, under the same call.
    let open = within_gateway_turn_spawn_context(
        "turn-run-43".to_string(),
        &fabric_ctx,
        &crate::config::ToolTieringConfig::default(),
        async { crate::tools::sessions_spawn::ambient_channel_excluded_tools() },
    )
    .await;
    assert!(
        crate::config::ToolTieringConfig::default().always_exclude.is_empty(),
        "fixture is useless unless the default gateway surface hides nothing"
    );
    assert!(
        open.is_empty(),
        "an unrestricted gateway hands down no boundary: {open:?}"
    );
}
