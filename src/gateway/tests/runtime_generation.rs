#![cfg(test)]

use super::*;

struct TestConfigParticipant;

struct TestPreparedConfig;

#[tokio::test]
async fn config_audit_sink_accepts_events_from_non_runtime_threads() {
    let tmp = tempfile::TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let workspace_id = tmp.path().to_string_lossy().to_string();
    let sink = Arc::new(ConfigGenerationMessageAuditSink {
        memory: Arc::clone(&memory),
        workspace_id: workspace_id.clone(),
        runtime_handle: tokio::runtime::Handle::current(),
    });
    let event = crate::config::ConfigGenerationAuditEvent {
        event_type: "config.reload.test".to_string(),
        generation_id: Some(crate::config::ConfigGenerationId(7)),
        source_revision: None,
        trigger: crate::config::ConfigReloadTrigger::Test,
        payload: serde_json::json!({ "source": "watcher-thread" }),
    };

    std::thread::spawn(move || {
        crate::config::ConfigGenerationAuditSink::record(sink.as_ref(), event);
    })
    .join()
    .unwrap();

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id,
        agent_id: Some("config-generation-manager".to_string()),
        persona_id: None,
        session_key: Some("config-generation".to_string()),
        channel: Some("daemon".to_string()),
        sender: Some("system".to_string()),
        owner_id: Some("system".to_string()),
        legacy_session_key: None,
    };
    let recorded = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let events = memory.list_message_events_since(&principal, 0, 10).await.unwrap();
            if let Some(event) = events
                .into_iter()
                .find(|event| event.event_type == "config.reload.test")
            {
                break event;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("config audit event should be persisted");

    assert_eq!(recorded.config_generation_id, Some(7));
    assert_eq!(recorded.content, r#"{"source":"watcher-thread"}"#);
}

impl crate::config::ConfigGenerationParticipant for TestConfigParticipant {
    fn name(&self) -> &'static str {
        "gateway_test_runtime"
    }

    fn supports_rebuild_field(&self, _field: &str) -> bool {
        true
    }

    fn prepares_for_field(&self, _field: &str) -> bool {
        true
    }

    fn prepare(
        &self,
        _generation: Arc<crate::config::ConfigGeneration>,
        _changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn crate::config::PreparedConfigGeneration>> {
        Ok(Box::new(TestPreparedConfig))
    }
}

impl crate::config::PreparedConfigGeneration for TestPreparedConfig {
    fn commit(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn rollback(&mut self) {}
}

#[test]
fn security_body_limit_is_64kb() {
    assert_eq!(MAX_BODY_SIZE, 65_536);
}

#[test]
fn security_api_body_limit_supports_media_batch_uploads() {
    assert!(MAX_API_BODY_SIZE >= (10 * 20 * 1024 * 1024));
}

#[tokio::test]
async fn gateway_health_heartbeat_refreshes_without_requests() {
    const COMPONENT: &str = "gateway-heartbeat-test";
    crate::health::register_component(
        COMPONENT,
        "gateway",
        true,
        Duration::from_secs(1),
        crate::health::ComponentState::Ready,
    );
    let before = crate::health::snapshot().components[COMPONENT].updated_at.clone();
    let shutdown = CancellationToken::new();
    let heartbeat = tokio::spawn(run_health_heartbeat(
        COMPONENT,
        Duration::from_millis(5),
        shutdown.clone(),
    ));

    tokio::time::sleep(Duration::from_millis(20)).await;
    shutdown.cancel();
    heartbeat.await.unwrap();

    let after = crate::health::snapshot().components[COMPONENT].clone();
    assert!(after.fresh);
    assert_eq!(after.state, crate::health::ComponentState::Ready);
    assert_ne!(after.updated_at, before);
}

#[test]
fn webhook_body_requires_message_field() {
    let valid = r#"{"message": "hello"}"#;
    let parsed: Result<WebhookBody, _> = serde_json::from_str(valid);
    assert!(parsed.is_ok());
    assert_eq!(parsed.unwrap().message, "hello");

    let missing = r#"{"other": "field"}"#;
    let parsed: Result<WebhookBody, _> = serde_json::from_str(missing);
    assert!(parsed.is_err());
}

#[test]
fn whatsapp_query_fields_are_optional() {
    let q = WhatsAppVerifyQuery {
        mode: None,
        verify_token: None,
        challenge: None,
    };
    assert!(q.mode.is_none());
}

/// A stdio MCP server whose one tool, `pid`, answers its process id.
const PID_MCP_SERVER: &str = r#"
import json
import os
import sys

for line in sys.stdin:

    message = json.loads(line)

    if "id" not in message:
        continue

    method = message.get("method")

    if method == "initialize":
        result = {
            "protocolVersion": message.get("params", {}).get("protocolVersion", "2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "pid-test", "version": "1.0.0"},
        }

    elif method == "tools/list":
        result = {"tools": [{"name": "pid", "description": "pid", "inputSchema": {"type": "object", "properties": {}}}]}

    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": str(os.getpid())}], "isError": False}

    else:
        result = {}

    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
"#;

/// Whether process `pid` still runs.
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Wait up to five seconds for process `pid` to end; whether it did.
async fn process_ends(pid: u32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while process_alive(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    !process_alive(pid)
}

/// The pid the `reg` server of `turn`'s MCP tool answers with.
async fn mcp_server_pid(turn: &TurnRuntimeGeneration) -> u32 {
    let tool = turn.mcp_tool.as_ref().expect("test: the gateway registers MCP");
    let output = tool
        .execute(serde_json::json!({"server": "reg", "tool": "pid"}))
        .await
        .expect("test: call");
    assert!(output.success, "test: MCP call failed: {:?}", output.error);
    output.output.trim().parse::<u32>().expect("test: a pid")
}

/// A configuration the production gateway builder accepts, with one
/// stdio MCP server `reg` whose arguments end in `tag`.
fn reg_server_config(dir: &std::path::Path, temperature: f64, tag: &str) -> Config {
    let mut config = Config {
        config_path: dir.join("config.toml"),
        workspace_dir: dir.join("workspace"),
        api_key: Some("test-key".into()),
        api_url: Some("http://127.0.0.1:1".into()),
        // A closed loopback endpoint: nothing here reaches a real host.
        default_provider: Some("custom:http://127.0.0.1:1/v1".into()),
        default_temperature: temperature,
        ..Config::default()
    };
    config.autonomy.level = crate::security::policy::AutonomyLevel::Full;
    config.mcp.servers.insert(
        "reg".into(),
        crate::config::McpServerConfig {
            command: Some("python3".into()),
            args: vec!["-u".into(), "-c".into(), PID_MCP_SERVER.into(), tag.into()],
            startup_timeout_ms: 10_000,
            request_timeout_ms: 10_000,
            ..crate::config::McpServerConfig::default()
        },
    );
    config
}

/// The Gateway turn runtime as `run_gateway` wires it: built by the
/// production builder and registered as a reload participant.
fn gateway_turn_runtime_owner(config: Config) -> (crate::config::SharedConfig, Arc<GatewayTurnRuntimeOwner>) {
    std::fs::create_dir_all(&config.workspace_dir).expect("test: workspace dir");
    let hooks = Arc::new(HookManager::new(config.workspace_dir.clone()));
    let manager = crate::config::new_shared(config);
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);
    let startup = build_gateway_turn_runtime(
        manager.pin(),
        Arc::clone(&manager),
        Arc::clone(&memory),
        None,
        None,
        None,
        None,
        Arc::clone(&hooks),
        #[cfg(feature = "wasm-plugins")]
        None,
    )
    .expect("test: the production gateway turn runtime must build");
    // The gateway's own startup function: nothing but the owner keeps
    // the startup generation.
    let owner = start_gateway_turn_runtime(
        startup,
        &manager,
        memory,
        None,
        None,
        None,
        None,
        hooks,
        #[cfg(feature = "wasm-plugins")]
        None,
    )
    .expect("test: the gateway turn runtime starts");
    (manager, owner)
}

/// RED before D-098 F-1: the gateway state kept the startup
/// generation's registry and MCP tool for its whole life, so the child
/// the first turn started never ended, and after a definition change it
/// kept running the old definition. Through the gateway's startup
/// function, the startup generation is now released by the first change
/// like any other, and a definition change stops the old process at
/// once: after every change exactly one child runs, the current one.
#[tokio::test]
async fn the_startup_generation_is_released_and_a_changed_definition_stops_its_process() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping MCP generation test: python3 is unavailable");
        return;
    }
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let (manager, owner) = gateway_turn_runtime_owner(reg_server_config(dir.path(), 0.1, "v1"));
    let mut ended = vec![mcp_server_pid(&owner.pin()).await];
    for change in 1..=3_u32 {
        manager
            .apply_runtime_config(
                reg_server_config(dir.path(), 0.1 + f64::from(change) / 10.0, "v1"),
                crate::config::ConfigReloadTrigger::Test,
            )
            .expect("test: apply an unrelated change");
        let current = mcp_server_pid(&owner.pin()).await;
        for pid in &ended {
            assert!(
                process_ends(*pid).await,
                "after change {change}, child {pid} of an earlier generation still runs (current {current})"
            );
        }
        ended.push(current);
    }
    let v1 = ended.pop().expect("test: the current child");
    assert!(process_alive(v1));

    // A turn still pinned to the v1 generation does not keep the
    // process running the old definition.
    let pinned = owner.pin();
    manager
        .apply_runtime_config(
            reg_server_config(dir.path(), 0.4, "v2"),
            crate::config::ConfigReloadTrigger::Test,
        )
        .expect("test: apply a changed definition");
    assert!(
        process_ends(v1).await,
        "the v1 process still runs after the definition changed"
    );
    let v2 = mcp_server_pid(&owner.pin()).await;
    assert_ne!(v2, v1);
    assert!(process_alive(v2));
    drop(pinned);
}

/// RED before D-097 M-3: every superseded Gateway runtime generation was
/// kept alive (its tool registry held itself through `sessions_spawn`),
/// so each configuration change left one more stdio MCP child per used
/// server until the daemon stopped. Now a generation is released when
/// the last turn pinned to it ends, and its children end with it.
#[tokio::test]
async fn superseded_gateway_generations_release_their_mcp_children() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping MCP generation test: python3 is unavailable");
        return;
    }
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let (manager, owner) = gateway_turn_runtime_owner(reg_server_config(dir.path(), 0.1, "v1"));
    let mut pids = Vec::new();
    for change in 1..=4_u32 {
        pids.push(mcp_server_pid(&owner.pin()).await);
        // An unrelated change: the server's definition stays the same.
        manager
            .apply_runtime_config(
                reg_server_config(dir.path(), 0.1 + f64::from(change) / 10.0, "v1"),
                crate::config::ConfigReloadTrigger::Test,
            )
            .expect("test: apply an unrelated change");
    }
    let current = mcp_server_pid(&owner.pin()).await;
    for pid in &pids {
        assert!(
            process_ends(*pid).await,
            "a superseded generation's MCP child {pid} still runs (children {pids:?}, current {current})"
        );
    }
    assert!(process_alive(current), "the current generation's MCP child ended");

    // A turn pinned to the old generation keeps its server while the
    // definition is unchanged, and the child ends when the turn does.
    let pinned = owner.pin();
    let pinned_pid = mcp_server_pid(&pinned).await;
    manager
        .apply_runtime_config(
            reg_server_config(dir.path(), 0.9, "v1"),
            crate::config::ConfigReloadTrigger::Test,
        )
        .expect("test: apply an unrelated change");
    assert_eq!(
        mcp_server_pid(&pinned).await,
        pinned_pid,
        "a pinned turn lost its server"
    );
    drop(pinned);
    assert!(
        process_ends(pinned_pid).await,
        "the pinned generation's MCP child outlived it"
    );
    assert!(process_alive(mcp_server_pid(&owner.pin()).await));
}

/// RED before D-097 M-3: a server kept by name but with a changed launch
/// definition (command, arguments, environment, working directory) kept
/// running the old definition next to the new one. Committing the new
/// generation now stops the old process at once, even while a turn is
/// still pinned to the old generation.
#[tokio::test]
async fn a_changed_mcp_server_definition_stops_the_old_process() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping MCP generation test: python3 is unavailable");
        return;
    }
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let (manager, owner) = gateway_turn_runtime_owner(reg_server_config(dir.path(), 0.1, "v1"));
    let pinned = owner.pin();
    let old_pid = mcp_server_pid(&pinned).await;
    manager
        .apply_runtime_config(
            reg_server_config(dir.path(), 0.1, "v2"),
            crate::config::ConfigReloadTrigger::Test,
        )
        .expect("test: apply a changed definition");
    assert!(
        process_ends(old_pid).await,
        "the process running the old definition still runs"
    );
    let new_pid = mcp_server_pid(&owner.pin()).await;
    assert_ne!(new_pid, old_pid);
    assert!(process_alive(new_pid));
    drop(pinned);
}

/// RED before PMME-h (D-095 M-2): a configuration change that drops an
/// MCP server (a narrowed `mcp.workspace_servers`, a removed server)
/// replaced the gateway's MCP tool, but the old tool's running child
/// kept running until the daemon stopped. Committing the new generation
/// now stops it.
#[tokio::test]
async fn a_generation_that_drops_an_mcp_server_stops_its_child() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping MCP generation test: python3 is unavailable");
        return;
    }
    let server = crate::config::McpServerConfig {
        command: Some("python3".into()),
        args: vec!["-u".into(), "-c".into(), PID_MCP_SERVER.into()],
        startup_timeout_ms: 10_000,
        request_timeout_ms: 10_000,
        ..crate::config::McpServerConfig::default()
    };
    let alive = process_alive;
    let launch = tempfile::TempDir::new().expect("test: temp dir");
    let workspace = tempfile::TempDir::new().expect("test: temp dir");
    let tool_for = |servers: &[&str]| {
        let mut config = crate::config::McpConfig::default();
        for name in servers {
            config.servers.insert((*name).to_string(), server.clone());
        }
        Arc::new(
            McpTool::new(
                Arc::new(crate::security::SecurityPolicy::default()),
                config,
                workspace.path().to_path_buf(),
            )
            .with_launch_root(launch.path().to_path_buf()),
        )
    };
    let old_tool = tool_for(&["workspace-server"]);
    let output = old_tool
        .execute(serde_json::json!({"server": "workspace-server", "tool": "pid"}))
        .await
        .expect("test: call");
    let pid = output.output.trim().parse::<u32>().expect("test: a pid");
    assert!(alive(pid));

    let shared = crate::config::new_shared(Config::default());
    let generation = |tool: Arc<McpTool>| TurnRuntimeGeneration {
        provider_binding: None,
        decision_slot: Arc::new(decision_runtime::GatewayDecisionSlot::default()),
        config_generation: shared.pin(),
        provider: Arc::new(MockProvider::default()),
        model: "test-model".into(),
        temperature: 0.0,
        tools_registry: Arc::new(vec![]),
        mcp_tool: Some(tool),
    };
    let previous = Arc::new(generation(Arc::clone(&old_tool)));
    let mut prepared = PreparedGatewayTurnRuntime {
        current: Arc::new(ArcSwap::from(Arc::clone(&previous))),
        previous,
        generation: generation(tool_for(&[])),
    };
    crate::config::PreparedConfigGeneration::commit(&mut prepared).expect("test: commit");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while alive(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !alive(pid),
        "the dropped server's child still runs after the new generation"
    );
}

#[test]
fn app_state_is_clone() {
    fn assert_clone<T: Clone>() {}
    assert_clone::<AppState>();
}

/// Build a minimal `AppState` whose cached C and hot D both start from `config`,
/// for exercising the gateway authorization helpers in isolation.
fn authz_test_state(config: Config) -> AppState {
    AppState {
        config: crate::config::new_shared(config),
        provider: Arc::new(MockProvider::default()),
        model: "test-model".into(),
        temperature: 0.0,
        mem: Arc::new(MockMemory),
        auto_save: false,
        tools_registry: Arc::new(vec![]),
        mcp_tool: None,
        turn_runtime: None,
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
        webhook_token_hash: None,
        webhook_signing_secret: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
        whatsapp: None,
        signal: None,
        whatsapp_app_secret: None,
        linq: None,
        linq_signing_secret: None,
        nextcloud_talk: None,
        nextcloud_talk_webhook_secret: None,
        observer: Arc::new(crate::observability::NoopObserver),
        start_time: Instant::now(),
        gateway_port: 0,
        logs_broadcast_tx: broadcast::channel(16).0,
        #[cfg(feature = "wasm-plugins")]
        plugin_runtime: None,
    }
}

/// D2 / T1 (core mutation point): `authorize_gateway_resource_mutation` reads the
/// hot SharedConfig (D). Publishing a ReadOnly config to D ONLY (C left stale)
/// flips a previously-allowed core gateway mutation to denied — no restart, and
/// proving the decision no longer depends on C.
#[test]
fn gateway_core_authz_reads_hot_shared_config_after_reload() {
    use crate::security::policy::ResourceRiskLevel;

    let state = authz_test_state(Config::default());
    let participant: Arc<dyn crate::config::ConfigGenerationParticipant> = Arc::new(TestConfigParticipant);
    state.config.register_participant(&participant);
    assert!(
        authorize_gateway_resource_mutation(&state, "gateway:pair", ResourceRiskLevel::Low).is_ok(),
        "default autonomous policy should allow a low-risk core gateway mutation"
    );

    let read_only = Config {
        autonomy: crate::config::AutonomyConfig {
            level: crate::security::policy::AutonomyLevel::ReadOnly,
            ..crate::config::AutonomyConfig::default()
        },
        ..Config::default()
    };
    state
        .config
        .apply_runtime_config(read_only, crate::config::ConfigReloadTrigger::Test)
        .expect("apply read-only config");

    let denied = authorize_gateway_resource_mutation(&state, "gateway:pair", ResourceRiskLevel::Low)
        .expect_err("ReadOnly published to D must deny the core gateway mutation");
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    assert_eq!(
        state.config.load_full().autonomy.level,
        crate::security::policy::AutonomyLevel::ReadOnly
    );
}

#[test]
fn turn_runtime_swap_keeps_in_flight_generation_pinned_and_is_reversible() {
    let manager = crate::config::new_shared(Config::default());
    let generation_zero = manager.pin();
    let current = Arc::new(ArcSwap::from_pointee(TurnRuntimeGeneration {
        provider_binding: None,
        decision_slot: Arc::new(decision_runtime::GatewayDecisionSlot::default()),
        config_generation: Arc::clone(&generation_zero),
        provider: Arc::new(MockProvider::default()),
        model: "model-zero".to_string(),
        temperature: 0.7,
        tools_registry: Arc::new(Vec::new()),
        mcp_tool: None,
    }));
    let in_flight = current.load_full();
    let generation_one = Arc::new(crate::config::ConfigGeneration {
        id: crate::config::ConfigGenerationId(1),
        source_revision: generation_zero.source_revision.clone(),
        effective: Arc::clone(&generation_zero.effective),
        applied_at: chrono::Utc::now(),
        trigger: crate::config::ConfigReloadTrigger::Test,
        deferred_changes: Arc::from([]),
    });
    let mut prepared = PreparedGatewayTurnRuntime {
        current: Arc::clone(&current),
        previous: current.load_full(),
        generation: TurnRuntimeGeneration {
            provider_binding: None,
            decision_slot: Arc::new(decision_runtime::GatewayDecisionSlot::default()),
            config_generation: generation_one,
            provider: Arc::new(MockProvider::default()),
            model: "model-one".to_string(),
            temperature: 0.2,
            tools_registry: Arc::new(Vec::new()),
            mcp_tool: None,
        },
    };

    crate::config::PreparedConfigGeneration::commit(&mut prepared).unwrap();
    assert_eq!(current.load().config_generation.id.0, 1);
    assert_eq!(current.load().model, "model-one");
    assert_eq!(in_flight.config_generation.id.0, 0);
    assert_eq!(in_flight.model, "model-zero");

    crate::config::PreparedConfigGeneration::rollback(&mut prepared);
    assert_eq!(current.load().config_generation.id.0, 0);
    assert_eq!(current.load().model, "model-zero");
}

#[tokio::test]
async fn metrics_endpoint_returns_hint_when_prometheus_is_disabled() {
    let state = AppState {
        config: crate::config::new_shared(Config::default()),
        provider: Arc::new(MockProvider::default()),
        model: "test-model".into(),
        temperature: 0.0,
        mem: Arc::new(MockMemory),
        auto_save: false,
        tools_registry: Arc::new(vec![]),
        mcp_tool: None,
        turn_runtime: None,
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
        webhook_token_hash: None,
        webhook_signing_secret: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
        whatsapp: None,
        signal: None,
        whatsapp_app_secret: None,
        linq: None,
        linq_signing_secret: None,
        nextcloud_talk: None,
        nextcloud_talk_webhook_secret: None,
        observer: Arc::new(crate::observability::NoopObserver),
        start_time: Instant::now(),
        gateway_port: 0,
        logs_broadcast_tx: broadcast::channel(16).0,
        #[cfg(feature = "wasm-plugins")]
        plugin_runtime: None,
    };

    let response = handle_metrics(State(state)).await.into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(PROMETHEUS_CONTENT_TYPE)
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Prometheus backend not enabled"));
}
