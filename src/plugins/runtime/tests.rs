use super::*;
use crate::plugins::trust::TRUST_FILE_NAME;
use tempfile::TempDir;

/// Runtime over a workspace whose current plugins the operator trusted.
async fn init_plugin_runtime(workspace: &Path, memory: Option<Arc<dyn Memory>>) -> Option<Arc<PluginRuntime>> {
    init_test_plugin_runtime(workspace, memory).await
}

/// `wasm_plugins_manage` dispatches on `action` like `cron` and `xin`, so it
/// owes the same contract: every parameter an action cannot be called
/// without has to name that action, and no parameter may ship undescribed.
#[test]
fn manage_schema_names_every_action_in_the_parameters_it_requires() {
    crate::tools::schema::action_contract::assert_action_tool_contract(
        "wasm_plugins_manage",
        PLUGIN_MANAGE_DESCRIPTION,
        plugin_manage_execution_schema(),
    );
}

fn write_manifest(dir: &Path, version: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            r#"[plugin]
name = "atomic-test"
version = "{version}"
description = "atomic reload test"
wasm = "missing.wasm"

[permissions]
required = []
optional = []
"#
        ),
    )
    .unwrap();
}

fn install_example_fixture(workspace: &Path, example: &str) {
    let plugin_dir = workspace.join("plugins").join(example);
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("pdk/rust/examples")
        .join(example);
    std::fs::copy(fixture.join("plugin.toml"), plugin_dir.join("plugin.toml")).unwrap();
    std::fs::copy(fixture.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).unwrap();
}

#[tokio::test]
async fn wasm_channel_polls_filters_recovers_and_follows_generation_reload() {
    use crate::channels::traits::SendMessage;
    let temp = TempDir::new().unwrap();
    install_example_fixture(temp.path(), "echo-channel");
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let proxy = runtime.channels().pop().unwrap();
    assert_eq!(proxy.name(), "wasm:echo-channel");
    let adapter = runtime.generation.load().channels.get(proxy.name()).unwrap().clone();
    let messages = adapter.poll().await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages.first().unwrap().sender, "prx_user");
    assert_eq!(messages.first().unwrap().channel, "wasm:echo-channel");
    assert!(adapter.poll().await.unwrap().is_empty());
    proxy.send(&SendMessage::new("normal", "prx_chat")).await.unwrap();
    assert!(
        proxy
            .send(&SendMessage::new("PRX_TRAP_PROBE", "prx_chat"))
            .await
            .is_err()
    );
    proxy.send(&SendMessage::new("recovered", "prx_chat")).await.unwrap();
    assert!(proxy.health_check().await);
    let path = temp.path().join("plugins/echo-channel/plugin.toml");
    let manifest = std::fs::read_to_string(&path)
        .unwrap()
        .replace("allowed_users = \"prx_user\"", "allowed_users = \"prx_other\"");
    std::fs::write(path, manifest).unwrap();
    trust_workspace_plugins(temp.path());
    runtime.reload_plugin("echo-channel").await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listener = tokio::spawn(async move { proxy.listen(tx).await });
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.sender, "prx_other");
    drop(rx);
    tokio::time::timeout(std::time::Duration::from_secs(5), listener)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(runtime.channel_diagnostics().len(), 1);
}

#[tokio::test]
async fn workspace_has_one_process_runtime_owner() {
    let temp = TempDir::new().unwrap();
    let first = init_plugin_runtime(temp.path(), None).await.unwrap();
    let second = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(&first.event_bus(), &second.event_bus()));
}

#[tokio::test]
async fn reload_swaps_complete_generation_and_failed_candidate_preserves_old() {
    let temp = TempDir::new().unwrap();
    let plugin_dir = temp.path().join("plugins/atomic-test");
    write_manifest(&plugin_dir, "1.0.0");
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert_eq!(runtime.generation_id(), 1);

    write_manifest(&plugin_dir, "2.0.0");
    assert_eq!(runtime.reload_plugin("atomic-test").await.unwrap(), 2);
    let plugins = runtime.list_plugins().await;
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins.first().unwrap().version, "2.0.0");

    std::fs::write(plugin_dir.join("plugin.toml"), "invalid = [").unwrap();
    assert!(runtime.reload_plugin("atomic-test").await.is_err());
    assert_eq!(runtime.generation_id(), 2);
    assert_eq!(runtime.list_plugins().await.first().unwrap().version, "2.0.0");
}

#[tokio::test]
async fn agent_control_tools_expose_generation_and_root_call_fallback() {
    let temp = TempDir::new().unwrap();
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    let router = runtime.tool_router();
    let status = runtime.status_tool();

    assert_eq!(router.name(), "wasm_plugin_call");
    assert_eq!(
        router.specs().first().map(|spec| spec.name.as_str()),
        Some("wasm_plugin_call")
    );
    assert!(router.supports_name("wasm_plugin_call"));
    assert!(
        router.parameters_schema().pointer("/properties/tool/enum").is_none(),
        "an empty plugin generation must not publish an invalid empty enum"
    );

    let result = status.execute(serde_json::json!({})).await.unwrap();
    assert!(result.success);
    assert!(result.output.contains("\"generation\": 1"));
}

#[tokio::test]
async fn manage_schema_declares_action_specific_required_fields() {
    let temp = TempDir::new().unwrap();
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    let config = crate::config::Config {
        workspace_dir: temp.path().to_path_buf(),
        ..crate::config::Config::default()
    };
    let manage = runtime.manage_tool(crate::runtime::bootstrap::build_security_policy(&config));
    crate::tools::schema::assert_flat_execution_contract(manage.as_ref(), plugin_manage_execution_schema()).await;
    let schema = plugin_manage_execution_schema();
    let conditions = schema
        .get("allOf")
        .and_then(serde_json::Value::as_array)
        .expect("manage schema should publish action-specific requirements");

    assert!(conditions.iter().any(|condition| {
        condition.pointer("/if/properties/action/const") == Some(&serde_json::json!("storage_forget"))
            && condition.pointer("/then/required") == Some(&serde_json::json!(["key"]))
    }));
    assert!(conditions.iter().any(|condition| {
        condition
            .pointer("/if/properties/action/enum")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|actions| actions.contains(&serde_json::json!("storage_forget")))
            && condition.pointer("/then/required") == Some(&serde_json::json!(["name"]))
    }));

    let message_items = schema
        .pointer("/properties/messages/items")
        .and_then(serde_json::Value::as_object)
        .expect("messages items should be an object schema");
    let message_properties = message_items
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("messages items should define its required properties");
    for required in ["role", "content"] {
        assert!(
            message_properties.contains_key(required),
            "required message property '{required}' must be defined"
        );
    }
    assert_eq!(
        message_properties.get("role").and_then(|value| value.get("type")),
        Some(&serde_json::json!("string"))
    );
    assert_eq!(
        message_properties.get("content").and_then(|value| value.get("type")),
        Some(&serde_json::json!("string"))
    );
}

#[tokio::test]
async fn plugin_instantiation_arms_epoch_deadline_after_engine_has_advanced() {
    let temp = TempDir::new().unwrap();
    install_example_fixture(temp.path(), "pipeline-middleware");

    // A Store created with epoch interruption starts with deadline zero.
    // Advancing the shared engine makes a missing pre-instantiation deadline
    // fail deterministically with `wasm trap: interrupt`.
    crate::plugins::shared_wasm_engine()
        .expect("test: shared wasm engine")
        .increment_epoch();

    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let output = runtime
        .middleware()
        .process(
            super::super::capabilities::middleware::MiddlewareStage::Outbound,
            r#"{"text":"epoch-ready"}"#,
        )
        .await;
    assert!(output.contains("[wasm-middleware] epoch-ready"), "{output}");
}

#[tokio::test]
async fn manage_tool_runs_install_call_disable_enable_remove_lifecycle() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("plugin-sources/voice-talk-realtime");
    std::fs::create_dir_all(&source).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/voice-talk-realtime");
    std::fs::copy(fixture.join("plugin.toml"), source.join("plugin.toml")).unwrap();
    std::fs::copy(fixture.join("plugin.wasm"), source.join("plugin.wasm")).unwrap();

    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    let manage = runtime.manage_tool(crate::runtime::bootstrap::build_security_policy(&config));

    let installed = manage
        .execute(serde_json::json!({
            "action": "install",
            "source": "plugin-sources/voice-talk-realtime",
            "grant_permissions": ["log", "config", "kv"]
        }))
        .await
        .unwrap();
    assert!(installed.success, "install failed: {:?}", installed.error);
    assert!(
        runtime
            .list_plugins()
            .await
            .iter()
            .any(|plugin| plugin.name == "voice-talk-realtime")
    );

    let called = runtime
        .tool_router()
        .execute_named("voice_session", serde_json::json!({"provider": "openai"}))
        .await
        .unwrap();
    assert!(called.success, "WASM call failed: {:?}", called.error);
    assert!(called.output.contains("wss://api.openai.com"));

    let disabled = manage
        .execute(serde_json::json!({"action": "disable", "name": "voice-talk-realtime"}))
        .await
        .unwrap();
    assert!(disabled.success, "disable failed: {:?}", disabled.error);
    assert!(runtime.list_plugins().await.is_empty());

    let enabled = manage
        .execute(serde_json::json!({"action": "enable", "name": "voice-talk-realtime"}))
        .await
        .unwrap();
    assert!(enabled.success, "enable failed: {:?}", enabled.error);
    assert_eq!(runtime.list_plugins().await.len(), 1);

    let removed = manage
        .execute(serde_json::json!({"action": "remove", "name": "voice-talk-realtime"}))
        .await
        .unwrap();
    assert!(removed.success, "remove failed: {:?}", removed.error);
    assert!(runtime.list_plugins().await.is_empty());
    assert!(temp.path().join(".plugins-trash").is_dir());
}

#[tokio::test]
async fn documented_audit_hook_instantiates_and_records_real_delivery() {
    let temp = TempDir::new().unwrap();
    let plugin_dir = temp.path().join("plugins/audit-hook");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/audit-hook");
    std::fs::copy(fixture.join("plugin.toml"), plugin_dir.join("plugin.toml")).unwrap();
    std::fs::copy(fixture.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).unwrap();

    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let before = runtime.hook_diagnostics();
    assert_eq!(before.len(), 1);
    assert_eq!(before.first().unwrap().invocation_count, 0);

    runtime
        .emit_hook("prx.lifecycle.turn_complete", r#"{"source":"regression-test"}"#)
        .await;

    let after = runtime.hook_diagnostics();
    let hook = after.first().unwrap();
    assert_eq!(hook.invocation_count, 1);
    assert_eq!(hook.last_event.as_deref(), Some("prx.lifecycle.turn_complete"));
    assert!(hook.last_error.is_none(), "{:?}", hook.last_error);
}

#[tokio::test]
async fn every_wasm_capability_executes_through_the_live_generation() {
    let temp = TempDir::new().unwrap();
    for example in ["pipeline-middleware", "counter-cron", "echo-provider", "map-storage"] {
        install_example_fixture(temp.path(), example);
    }

    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    assert_eq!(runtime.middleware_diagnostics().len(), 1);
    assert_eq!(runtime.cron_diagnostics().len(), 1);
    assert_eq!(runtime.provider_names(), vec!["wasm-echo"]);
    assert_eq!(runtime.storage_names(), vec!["wasm-map"]);

    let middleware_output = runtime
        .middleware()
        .process(
            super::super::capabilities::middleware::MiddlewareStage::Outbound,
            r#"{"text":"hello"}"#,
        )
        .await;
    let middleware_output: serde_json::Value = serde_json::from_str(&middleware_output).unwrap();
    assert_eq!(
        middleware_output.get("text").and_then(serde_json::Value::as_str),
        Some("[wasm-middleware] hello")
    );
    assert_eq!(
        middleware_output
            .pointer("/middleware_probe/stage")
            .and_then(serde_json::Value::as_str),
        Some("outbound")
    );

    // The committed fixture is scheduled every 30 s and is due at start-up,
    // so the runtime's own scheduler may run it between these manual runs
    // (every run is its own task and yields); the counter still has to
    // advance strictly from one manual run to the next.
    let run_number = |output: String| -> u64 {
        output
            .strip_prefix("counter-cron-run-")
            .and_then(|number| number.parse().ok())
            .unwrap_or_else(|| panic!("unexpected cron output: {output}"))
    };
    let first = run_number(runtime.cron().run_job("counter-cron").await.unwrap());
    let second = run_number(runtime.cron().run_job("counter-cron").await.unwrap());
    assert!(second > first, "run {second} must follow run {first}");

    let provider = runtime.provider("wasm-echo").unwrap();
    let response = provider
        .chat(
            crate::providers::traits::ChatRequest {
                messages: &[crate::providers::traits::ChatMessage::user("provider probe")],
                tools: None,
            },
            "fixture-model",
            0.0,
        )
        .await
        .unwrap();
    assert_eq!(
        response.text.as_deref(),
        Some("wasm-echo/fixture-model: provider probe")
    );
    assert!(crate::providers::create_provider("wasm:wasm-echo", None).is_ok());

    let storage = runtime.storage("wasm-map").unwrap();
    storage
        .store(
            "fixture-key",
            "fixture-content",
            crate::memory::traits::MemoryCategory::Core,
            Some("fixture-session"),
        )
        .await
        .unwrap();
    assert!(storage.health_check().await);
    assert_eq!(storage.count().await.unwrap(), 1);
    let recalled = storage.recall("fixture", 10, Some("fixture-session")).await.unwrap();
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled.first().map(|entry| entry.key.as_str()), Some("fixture-key"));
    assert!(storage.forget("fixture-key").await.unwrap());
    assert_eq!(storage.count().await.unwrap(), 0);

    runtime.refresh_all().await.unwrap();
    let refreshed = provider
        .simple_chat("after refresh", "fixture-model", 0.0)
        .await
        .unwrap();
    assert_eq!(refreshed, "wasm-echo/fixture-model: after refresh");
    assert!(
        storage.health_check().await,
        "storage proxy must resolve the refreshed generation"
    );
}

#[tokio::test]
async fn mixed_capability_stress_executes_one_thousand_component_calls() {
    let temp = TempDir::new().unwrap();
    for example in ["pipeline-middleware", "counter-cron", "echo-provider", "map-storage"] {
        install_example_fixture(temp.path(), example);
    }
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    let provider = runtime.provider("wasm-echo").unwrap();
    let storage = runtime.storage("wasm-map").unwrap();

    let mut calls = 0usize;
    for index in 0..200usize {
        let middleware_output = runtime
            .middleware()
            .process(
                super::super::capabilities::middleware::MiddlewareStage::LlmRequest,
                &serde_json::json!({"index": index}).to_string(),
            )
            .await;
        assert!(middleware_output.contains("middleware_probe"));
        calls += 1;

        let cron_output = runtime.cron().run_job("counter-cron").await.unwrap();
        assert!(cron_output.starts_with("counter-cron-run-"));
        calls += 1;

        let response = provider
            .simple_chat(&format!("probe-{index}"), "stress-model", 0.0)
            .await
            .unwrap();
        assert!(response.ends_with(&format!("probe-{index}")));
        calls += 1;

        let key = format!("stress-{index}");
        storage
            .store(
                &key,
                "stress-content",
                crate::memory::traits::MemoryCategory::Custom("stress".to_string()),
                None,
            )
            .await
            .unwrap();
        calls += 1;

        let recalled = storage.recall(&key, 1, None).await.unwrap();
        assert_eq!(recalled.first().map(|entry| entry.key.as_str()), Some(key.as_str()));
        calls += 1;
    }

    assert_eq!(calls, 1_000);
    assert_eq!(storage.count().await.unwrap(), 200);
}

/// The committed `pipeline-middleware` component is reused verbatim; only
/// its `[resources]` table changes, so no WASM guest has to be rebuilt to
/// drive a store-poisoning trap.
const MIDDLEWARE_RECOVERY_STAGE: super::super::capabilities::middleware::MiddlewareStage =
    super::super::capabilities::middleware::MiddlewareStage::Outbound;

/// Keeps the fixture healthy for ordinary envelopes while leaving one
/// deterministic trap reachable: the guest DOM-parses the whole envelope, so
/// a multi-megabyte envelope cannot fit an 8 MB linear memory and
/// `trap_on_grow_failure` turns the failed `memory.grow` into a trap.
/// `max_payload_bytes` is the largest value `validate()` accepts for an 8 MB
/// guest, which keeps this trap reachable past the host-side oversize guard
/// so the trap, not the guard, stays the subject of these tests.
const TRAPPING_MIDDLEWARE_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

/// Same guest memory and timeout, host-side oversize guard left at its
/// derived default (`max_memory_mb` / 8 = 1 MiB here).
const GUARDED_MIDDLEWARE_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

/// Fuel the `middleware_fuel_is_metered_per_invocation` test grants: the
/// measured cost of instantiating the fixture and serving one call, plus
/// half again. Fuel cost differs per platform and wasmtime version
/// (instantiation is tens of thousands of units on one target and nearly
/// free on another), so no figure is hard-coded; see
/// `measure_one_call_fuel`.
fn funded_middleware_fuel(measured_one_call_fuel: u64) -> u64 {
    measured_one_call_fuel.saturating_add(measured_one_call_fuel / 2)
}

/// Outcome of loading the middleware fixture under `max_fuel` and sending
/// it one envelope.
#[derive(Debug, PartialEq, Eq)]
enum FuelProbe {
    /// The adapter loaded and the call returned the transformed text.
    Funded,
    /// The adapter never loaded because the budget ran out.
    StarvedAtLoad,
    /// The adapter loaded but the call did not return the transformed text.
    StarvedAtCall,
}

async fn probe_middleware_fuel(max_fuel: u64) -> FuelProbe {
    let temp = TempDir::new().unwrap();
    install_middleware_fixture_with_resources(
        temp.path(),
        &format!("max_fuel = {max_fuel}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
    );
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    if !runtime.adapter_errors().is_empty() {
        assert!(
            runtime
                .adapter_errors()
                .iter()
                .any(|error| error.contains("all fuel consumed")),
            "the adapter must be missing because the budget ran out, not for another reason \
                 (max_fuel {max_fuel}): {:?}",
            runtime.adapter_errors()
        );
        return FuelProbe::StarvedAtLoad;
    }
    let output = runtime
        .middleware()
        .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
        .await;
    if transformed_middleware_text(&output).as_deref() == Some("[wasm-middleware] hello") {
        FuelProbe::Funded
    } else {
        FuelProbe::StarvedAtCall
    }
}

/// Measures, on the current platform and engine, the smallest `max_fuel`
/// that instantiates the fixture and serves one `process` call, as the
/// boundary between a starved and a funded probe. Returns
/// `(largest_starved, smallest_funded)`, at most `PRECISION` apart. Fuel
/// cost is deterministic for a given engine and guest, so the probe is
/// monotonic and the search needs no retries.
async fn measure_one_call_fuel() -> (u64, u64) {
    const PRECISION: u64 = 64;
    const CEILING: u64 = 1_000_000_000;
    let mut starved = 0u64;
    let mut funded = 1_024u64;
    while probe_middleware_fuel(funded).await != FuelProbe::Funded {
        assert!(
            funded < CEILING,
            "one middleware call is not funded even by {CEILING} fuel"
        );
        starved = funded;
        funded = funded.saturating_mul(2);
    }
    while funded - starved > PRECISION {
        let middle = starved + (funded - starved) / 2;
        if probe_middleware_fuel(middle).await == FuelProbe::Funded {
            funded = middle;
        } else {
            starved = middle;
        }
    }
    (starved, funded)
}

/// The per-invocation fuel limit is real and discriminating, on every
/// platform. Fuel costs differ per target and wasmtime version, so the
/// oracle is measured here rather than hard-coded:
///
/// 1. `measure_one_call_fuel` finds the boundary between a budget that
///    cannot instantiate the fixture and serve one call and a budget that
///    can.
/// 2. A budget just below the boundary must fail, either because the
///    adapter runs out of fuel while loading (`all fuel consumed`) or, when
///    instantiation is nearly free, because the call itself runs out. In
///    both cases the call must not return the transformed text.
/// 3. A budget half again above the boundary must serve several calls in
///    a row; each call that succeeds after the first proves fuel is reset
///    per invocation, since the sum of the calls exceeds that budget.
///
/// The old oracle asserted that instantiation alone costs over 48 000
/// fuel, which holds on aarch64 but not on x86_64.
#[tokio::test]
async fn the_per_invocation_fuel_bound_stays_discriminating() {
    let (starved_fuel, funded_fuel) = measure_one_call_fuel().await;
    assert!(starved_fuel > 0, "a zero-fuel budget must not fund a call");

    let temp = TempDir::new().unwrap();
    install_middleware_fixture_with_resources(
        temp.path(),
        &format!("max_fuel = {starved_fuel}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
    );
    let starved = init_plugin_runtime(temp.path(), None).await.unwrap();
    if starved.adapter_errors().is_empty() {
        let output = starved
            .middleware()
            .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
            .await;
        assert_ne!(
            transformed_middleware_text(&output).as_deref(),
            Some("[wasm-middleware] hello"),
            "a budget below the measured boundary ({starved_fuel}..{funded_fuel}) must not fund a call: {output}"
        );
    } else {
        assert!(
            starved.middleware_diagnostics().is_empty(),
            "a starved load must publish no middleware: {:?}",
            starved.middleware_diagnostics()
        );
        assert!(
            starved
                .adapter_errors()
                .iter()
                .any(|error| error.contains("all fuel consumed")),
            "the adapter must be missing because the budget ran out, not for another reason: {:?}",
            starved.adapter_errors()
        );
    }

    let budget = funded_middleware_fuel(funded_fuel);
    let temp = TempDir::new().unwrap();
    install_middleware_fixture_with_resources(
        temp.path(),
        &format!("max_fuel = {budget}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
    );
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    for call in 1..=8 {
        let output = runtime
            .middleware()
            .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
            .await;
        assert_eq!(
            transformed_middleware_text(&output).as_deref(),
            Some("[wasm-middleware] hello"),
            "call {call} under {budget} fuel (boundary {funded_fuel}) must succeed: {output}"
        );
    }
}

fn install_middleware_fixture_with_resources(workspace: &Path, resources: &str) {
    let plugin_dir = workspace.join("plugins").join("pipeline-middleware");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware");
    std::fs::copy(fixture.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        format!(
            r#"[plugin]
name = "pipeline-middleware"
version = "0.1.0"
description = "middleware recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "middleware"
name = "pipeline-marker"
priority = 10

[permissions]
required = []
optional = []

[resources]
{resources}
"#
        ),
    )
    .unwrap();
}

/// Four megabytes of envelope: three live copies inside the guest (argument,
/// parsed value, serialized result) cannot fit an 8 MB linear memory.
fn oversized_middleware_envelope() -> String {
    serde_json::json!({"text": "hello", "filler": "x".repeat(4 * 1024 * 1024)}).to_string()
}

fn middleware_adapter_status(runtime: &PluginRuntime) -> serde_json::Value {
    let mut adapters = runtime.middleware_diagnostics();
    assert_eq!(adapters.len(), 1, "{adapters:?}");
    adapters.remove(0)
}

fn adapter_counter(adapter: &serde_json::Value, field: &str) -> u64 {
    adapter
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("adapter status must publish '{field}': {adapter}"))
}

fn adapter_text(adapter: &serde_json::Value, field: &str) -> String {
    adapter
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("adapter status must publish '{field}': {adapter}"))
        .to_string()
}

fn transformed_middleware_text(output: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(output)
        .ok()?
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

#[tokio::test]
async fn middleware_recovers_on_a_fresh_store_after_a_guest_trap() {
    let temp = TempDir::new().unwrap();
    install_middleware_fixture_with_resources(temp.path(), TRAPPING_MIDDLEWARE_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

    let trapping = oversized_middleware_envelope();
    let trapped = runtime.middleware().process(MIDDLEWARE_RECOVERY_STAGE, &trapping).await;
    assert!(
        !trapped.contains("middleware_probe"),
        "the oversized envelope must trap the guest"
    );

    let recovered = runtime
        .middleware()
        .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
        .await;
    assert_eq!(
        transformed_middleware_text(&recovered).as_deref(),
        Some("[wasm-middleware] hello"),
        "{recovered}"
    );

    let adapter = middleware_adapter_status(&runtime);
    assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
    assert_eq!(
        adapter_counter(&adapter, "failure_count"),
        1,
        "the healthy call must not have entered the poisoned store: {adapter}"
    );
    let last_error = adapter_text(&adapter, "last_error");
    assert!(
        !last_error.contains("cannot enter component instance"),
        "the recorded reason must be the first trap, not a re-entry refusal: {last_error}"
    );
    assert!(
        adapter
            .get("last_error_at")
            .and_then(serde_json::Value::as_str)
            .is_some(),
        "{adapter}"
    );
}

#[tokio::test]
async fn middleware_quarantines_after_the_restart_budget_is_spent() {
    let temp = TempDir::new().unwrap();
    install_middleware_fixture_with_resources(temp.path(), TRAPPING_MIDDLEWARE_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

    let trapping = oversized_middleware_envelope();
    for _ in 0..5 {
        let output = runtime.middleware().process(MIDDLEWARE_RECOVERY_STAGE, &trapping).await;
        assert_eq!(output.len(), trapping.len(), "every trapping call must fail open");
    }

    let adapter = middleware_adapter_status(&runtime);
    assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
    assert!(!adapter_text(&adapter, "last_error").is_empty(), "{adapter}");
    assert_eq!(
        adapter_counter(&adapter, "restart_count"),
        3,
        "quarantine must follow a spent budget, not a shortcut around it: {adapter}"
    );
    let failures = adapter_counter(&adapter, "failure_count");
    let skipped = adapter_counter(&adapter, "skipped_quarantined");

    let refused = runtime
        .middleware()
        .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
        .await;
    assert_eq!(refused, r#"{"text":"hello"}"#, "a quarantined adapter still fails open");
    let after = middleware_adapter_status(&runtime);
    assert_eq!(adapter_text(&after, "state"), "quarantined", "{after}");
    assert_eq!(
        adapter_counter(&after, "failure_count"),
        failures,
        "a refused call must not be counted as a new guest failure: {after}"
    );
    assert_eq!(
        adapter_counter(&after, "skipped_quarantined"),
        skipped + 1,
        "a refused call must be visible as a quarantined skip: {after}"
    );

    let status = runtime.status_tool();
    assert_ne!(
        status.availability().level,
        crate::capability::CapabilityAvailabilityLevel::Healthy,
        "availability must degrade while an adapter is quarantined"
    );
    let reported = status.execute(serde_json::json!({})).await.unwrap();
    assert!(reported.output.contains("\"quarantined\""), "{}", reported.output);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_middleware_waiter_never_enters_a_poisoned_store() {
    let temp = TempDir::new().unwrap();
    install_middleware_fixture_with_resources(temp.path(), TRAPPING_MIDDLEWARE_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

    let chain = runtime.middleware();
    let trapping = oversized_middleware_envelope();
    // Announce only after the trapping call owns the real store mutex and
    // has checked the live state. Copying the envelope and scheduling the
    // detached guest task must not let the healthy caller enter first.
    let (entered, has_entered) = tokio::sync::oneshot::channel();
    chain.notify_next_store_entry(entered);
    let trapping_call = tokio::spawn({
        let chain = Arc::clone(&chain);
        async move { chain.process(MIDDLEWARE_RECOVERY_STAGE, &trapping).await }
    });
    has_entered.await.unwrap();
    // A late waiter may observe the already-recorded trap; either way, it
    // must rebuild exactly once instead of entering the poisoned store.
    let healthy_call = tokio::spawn({
        let chain = Arc::clone(&chain);
        async move { chain.process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#).await }
    });

    let trapped = trapping_call.await.unwrap();
    let healthy = healthy_call.await.unwrap();
    assert!(
        !trapped.contains("middleware_probe"),
        "the oversized envelope must trap"
    );
    assert_eq!(
        transformed_middleware_text(&healthy).as_deref(),
        Some("[wasm-middleware] hello"),
        "the queued waiter must run against a live store: {healthy}"
    );

    let adapter = middleware_adapter_status(&runtime);
    assert_eq!(
        adapter_counter(&adapter, "failure_count"),
        1,
        "only the trapping caller may record a failure: {adapter}"
    );
    // Exactly one: the waiter found the store dead and rebuilt it. Zero
    // would mean the healthy caller won the lock and nothing ever queued,
    // which is the ordering this test exists to rule out.
    assert_eq!(
        adapter_counter(&adapter, "restart_count"),
        1,
        "the waiter must have queued behind the trapping caller and rebuilt once: {adapter}"
    );
    assert!(
        !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
        "{adapter}"
    );
}

#[tokio::test]
async fn middleware_skips_a_payload_that_cannot_fit_the_guest() {
    let temp = TempDir::new().unwrap();
    install_middleware_fixture_with_resources(temp.path(), GUARDED_MIDDLEWARE_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

    let oversize = oversized_middleware_envelope();
    let passed_through = runtime.middleware().process(MIDDLEWARE_RECOVERY_STAGE, &oversize).await;
    assert_eq!(passed_through, oversize, "an over-cap payload passes through unchanged");

    let adapter = middleware_adapter_status(&runtime);
    assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
    assert_eq!(
        adapter_counter(&adapter, "skipped_quarantined"),
        0,
        "an oversize skip is not a quarantine: {adapter}"
    );
    assert_eq!(
        adapter_counter(&adapter, "failure_count"),
        0,
        "an over-cap payload must never reach the guest: {adapter}"
    );
    assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
    assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
    assert_eq!(adapter.get("last_error"), Some(&serde_json::Value::Null), "{adapter}");

    let next = runtime
        .middleware()
        .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
        .await;
    assert_eq!(
        transformed_middleware_text(&next).as_deref(),
        Some("[wasm-middleware] hello"),
        "{next}"
    );
}

#[tokio::test]
async fn middleware_fuel_is_metered_per_invocation() {
    let (_, funded_fuel) = measure_one_call_fuel().await;
    let one_call_fuel = funded_middleware_fuel(funded_fuel);
    let temp = TempDir::new().unwrap();
    install_middleware_fixture_with_resources(
        temp.path(),
        &format!("max_fuel = {one_call_fuel}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
    );
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

    for call in 1..=3 {
        let output = runtime
            .middleware()
            .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
            .await;
        assert_eq!(
            transformed_middleware_text(&output).as_deref(),
            Some("[wasm-middleware] hello"),
            "call {call} exhausted a store-lifetime fuel budget: {output}"
        );
    }

    let adapter = middleware_adapter_status(&runtime);
    assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
    assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
}

/// Install `fixture/plugin.wasm` under `plugins/<name>` with a test-written
/// manifest, so a committed component can be driven into a trap by its
/// `[resources]` table alone.
fn install_fixture_with_manifest(workspace: &Path, name: &str, fixture: &str, manifest: &str) {
    let plugin_dir = workspace.join("plugins").join(name);
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture);
    std::fs::copy(source.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).unwrap();
    std::fs::write(plugin_dir.join("plugin.toml"), manifest).unwrap();
}

/// `adapter_key` of `wasm_plugins_status`, which is where operators and the
/// model read adapter health.
async fn status_adapters(runtime: &Arc<PluginRuntime>, adapter_key: &str) -> Vec<serde_json::Value> {
    let status = runtime.status_tool().execute(serde_json::json!({})).await.unwrap();
    let status: serde_json::Value = serde_json::from_str(&status.output).unwrap();
    status
        .get(adapter_key)
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_else(|| panic!("wasm_plugins_status must publish '{adapter_key}': {status}"))
}

async fn single_status_adapter(runtime: &Arc<PluginRuntime>, adapter_key: &str) -> serde_json::Value {
    let mut adapters = status_adapters(runtime, adapter_key).await;
    assert_eq!(adapters.len(), 1, "{adapters:?}");
    adapters.remove(0)
}

fn voice_tool_manifest(resources: &str) -> String {
    format!(
        r#"[plugin]
name = "voice-talk-realtime"
version = "0.1.0"
description = "tool recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "tool"
name = "voice_session"

[permissions]
required = ["log", "config", "kv"]
optional = []

[resources]
{resources}
"#
    )
}

fn install_voice_tool(workspace: &Path, resources: &str) {
    install_fixture_with_manifest(
        workspace,
        "voice-talk-realtime",
        "plugins/voice-talk-realtime",
        &voice_tool_manifest(resources),
    );
}

/// The guest parses its arguments into a DOM and echoes `instructions`
/// into its result, so four megabytes of instructions cannot fit an 8 MB
/// linear memory. `max_payload_bytes` keeps the call past the host-side
/// guard so the trap stays the subject of the test.
const TRAPPING_TOOL_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

/// Same guest memory, host-side guard at its derived default (1 MiB here).
const GUARDED_TOOL_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

fn trapping_tool_args() -> serde_json::Value {
    serde_json::json!({"provider": "openai", "instructions": "x".repeat(4 * 1024 * 1024)})
}

async fn call_voice_tool(runtime: &Arc<PluginRuntime>, args: serde_json::Value) -> ToolResult {
    runtime
        .tool_router()
        .execute_named("voice_session", args)
        .await
        .unwrap()
}

async fn assert_voice_tool_healthy(runtime: &Arc<PluginRuntime>, context: &str) {
    let result = call_voice_tool(runtime, serde_json::json!({"provider": "openai"})).await;
    assert!(result.success, "{context}: {:?}", result.error);
    assert!(
        result.output.contains("wss://api.openai.com"),
        "{context}: {}",
        result.output
    );
}

#[tokio::test]
async fn tool_recovers_on_a_fresh_store_after_a_guest_trap() {
    let temp = TempDir::new().unwrap();
    install_voice_tool(temp.path(), TRAPPING_TOOL_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    assert_voice_tool_healthy(&runtime, "before the trap").await;

    let trapped = call_voice_tool(&runtime, trapping_tool_args()).await;
    assert!(!trapped.success, "four megabytes of instructions must trap the guest");

    assert_voice_tool_healthy(&runtime, "the call after the trap").await;

    let adapter = single_status_adapter(&runtime, "tool_adapters").await;
    assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
    assert_eq!(adapter_text(&adapter, "tool"), "voice_session", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "failure_count"), 1, "{adapter}");
    assert!(
        !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
        "the recorded reason must be the trap, not a re-entry refusal: {adapter}"
    );
}

#[tokio::test]
async fn tool_quarantines_after_the_restart_budget_is_spent() {
    let temp = TempDir::new().unwrap();
    install_voice_tool(temp.path(), TRAPPING_TOOL_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let (_, mut quarantined_events) = runtime
        .event_bus()
        .subscribe("tool-recovery-test", "prx.plugin.tool_quarantined")
        .await
        .unwrap();

    for call in 1..=5 {
        let result = call_voice_tool(&runtime, trapping_tool_args()).await;
        assert!(!result.success, "trapping call {call} must fail, never report success");
    }

    let adapter = single_status_adapter(&runtime, "tool_adapters").await;
    assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 3, "{adapter}");
    let skipped = adapter_counter(&adapter, "skipped_quarantined");

    let refused = call_voice_tool(&runtime, serde_json::json!({"provider": "openai"})).await;
    assert!(!refused.success, "a quarantined tool must return a tool error");
    assert!(
        refused.error.as_deref().unwrap_or_default().contains("quarantined"),
        "{refused:?}"
    );
    let after = single_status_adapter(&runtime, "tool_adapters").await;
    assert_eq!(adapter_counter(&after, "skipped_quarantined"), skipped + 1, "{after}");

    let event = quarantined_events
        .try_recv()
        .expect("test: one quarantine event must have been published");
    let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
    assert_eq!(
        payload.get("capability").and_then(serde_json::Value::as_str),
        Some("tool")
    );
    assert_eq!(
        payload.get("plugin").and_then(serde_json::Value::as_str),
        Some("voice-talk-realtime")
    );

    assert!(
        runtime.tool_router().supports_name("voice_session"),
        "a quarantined tool stays listed so the model is told why it fails"
    );
    assert_ne!(
        runtime.tool_router().availability().level,
        crate::capability::CapabilityAvailabilityLevel::Healthy
    );
    assert_ne!(
        runtime.status_tool().availability().level,
        crate::capability::CapabilityAvailabilityLevel::Healthy
    );
}

#[tokio::test]
async fn tool_refuses_arguments_that_cannot_fit_the_guest() {
    let temp = TempDir::new().unwrap();
    install_voice_tool(temp.path(), GUARDED_TOOL_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

    let refused = call_voice_tool(&runtime, trapping_tool_args()).await;
    assert!(!refused.success, "an over-cap call is a tool error, not a success");
    assert!(
        refused
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("max_payload_bytes"),
        "{refused:?}"
    );

    let adapter = single_status_adapter(&runtime, "tool_adapters").await;
    assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
    assert_voice_tool_healthy(&runtime, "the call after an oversize refusal").await;
}

/// Measured against the committed `voice-talk-realtime` component with a
/// store-lifetime budget: instantiation plus `get-spec` fits in 100 000
/// fuel units with no call left, 150 000 and 200 000 fund exactly one
/// `execute`, and each further call costs roughly 50 000. 175 000 sits in
/// the middle of the one-call band.
const ONE_TOOL_CALL_FUEL: u64 = 175_000;

#[tokio::test]
async fn tool_fuel_is_metered_per_invocation() {
    let temp = TempDir::new().unwrap();
    install_voice_tool(
        temp.path(),
        &format!("max_fuel = {ONE_TOOL_CALL_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
    );
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    for call in 1..=3 {
        assert_voice_tool_healthy(&runtime, &format!("call {call} under a per-call fuel budget")).await;
    }
}

fn audit_hook_manifest(resources: &str) -> String {
    format!(
        r#"[plugin]
name = "audit-hook"
version = "0.1.0"
description = "hook recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "hook"
name = "audit"

[[capabilities.events]]
pattern = "prx.lifecycle.*"

[permissions]
required = ["log", "kv", "events"]
optional = []

[resources]
{resources}
"#
    )
}

/// `audit-hook` needs `events`, which is not auto-granted, so the fixture
/// is installed as a managed plugin like the documented-hook test does.
fn install_audit_hook(workspace: &Path, resources: &str) {
    install_fixture_with_manifest(
        workspace,
        "audit-hook",
        "pdk/rust/examples/audit-hook",
        &audit_hook_manifest(resources),
    );
}

/// The guest JSON-parses the payload of `tool_call` events into a DOM, so
/// four megabytes of payload cannot fit an 8 MB linear memory.
/// `max_payload_bytes` keeps the delivery past the host-side guard.
const TRAPPING_HOOK_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

const GUARDED_HOOK_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

const TRAPPING_HOOK_EVENT: &str = "prx.lifecycle.tool_call";
const HEALTHY_HOOK_EVENT: &str = "prx.lifecycle.turn_complete";

fn trapping_hook_payload() -> String {
    serde_json::json!({"tool": "shell", "filler": "x".repeat(4 * 1024 * 1024)}).to_string()
}

#[tokio::test]
async fn hook_recovers_on_a_fresh_store_after_a_guest_trap() {
    let temp = TempDir::new().unwrap();
    install_audit_hook(temp.path(), TRAPPING_HOOK_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

    runtime.emit_hook(TRAPPING_HOOK_EVENT, &trapping_hook_payload()).await;
    runtime
        .emit_hook(HEALTHY_HOOK_EVENT, r#"{"source":"recovery-test"}"#)
        .await;

    let adapter = single_status_adapter(&runtime, "hook_adapters").await;
    assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "invocation_count"), 2, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
    assert_eq!(
        adapter_counter(&adapter, "failure_count"),
        1,
        "only the trapping delivery may fail: {adapter}"
    );
    assert!(
        !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
        "the recorded reason must be the trap, not a re-entry refusal: {adapter}"
    );
}

#[tokio::test]
async fn hook_quarantines_after_the_restart_budget_is_spent() {
    let temp = TempDir::new().unwrap();
    install_audit_hook(temp.path(), TRAPPING_HOOK_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let (_, mut quarantined_events) = runtime
        .event_bus()
        .subscribe("hook-recovery-test", "prx.plugin.hook_quarantined")
        .await
        .unwrap();

    let payload = trapping_hook_payload();
    for _ in 0..5 {
        runtime.emit_hook(TRAPPING_HOOK_EVENT, &payload).await;
    }

    let adapter = single_status_adapter(&runtime, "hook_adapters").await;
    assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 3, "{adapter}");
    let skipped = adapter_counter(&adapter, "skipped_quarantined");

    runtime.emit_hook(HEALTHY_HOOK_EVENT, "{}").await;
    let after = single_status_adapter(&runtime, "hook_adapters").await;
    assert_eq!(adapter_counter(&after, "skipped_quarantined"), skipped + 1, "{after}");

    let event = quarantined_events
        .try_recv()
        .expect("test: one quarantine event must have been published");
    let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
    assert_eq!(
        payload.get("capability").and_then(serde_json::Value::as_str),
        Some("hook")
    );
    assert_ne!(
        runtime.status_tool().availability().level,
        crate::capability::CapabilityAvailabilityLevel::Healthy
    );
}

#[tokio::test]
async fn hook_skips_a_payload_that_cannot_fit_the_guest() {
    let temp = TempDir::new().unwrap();
    install_audit_hook(temp.path(), GUARDED_HOOK_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

    runtime.emit_hook(TRAPPING_HOOK_EVENT, &trapping_hook_payload()).await;
    let adapter = single_status_adapter(&runtime, "hook_adapters").await;
    assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");

    runtime.emit_hook(HEALTHY_HOOK_EVENT, "{}").await;
    let after = single_status_adapter(&runtime, "hook_adapters").await;
    assert_eq!(adapter_text(&after, "state"), "live", "{after}");
    assert_eq!(adapter_counter(&after, "failure_count"), 0, "{after}");
    assert_eq!(
        after.get("last_event").and_then(serde_json::Value::as_str),
        Some(HEALTHY_HOOK_EVENT),
        "{after}"
    );
}

/// Measured against the committed `audit-hook` component with a
/// store-lifetime budget: instantiation costs roughly 40 000 fuel units and
/// one delivery roughly 5 000, so 50 000 funds exactly two deliveries.
const TWO_HOOK_DELIVERIES_FUEL: u64 = 50_000;

#[tokio::test]
async fn hook_fuel_is_metered_per_invocation() {
    let temp = TempDir::new().unwrap();
    install_audit_hook(
        temp.path(),
        &format!("max_fuel = {TWO_HOOK_DELIVERIES_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
    );
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    for delivery in 1..=4 {
        runtime.emit_hook(HEALTHY_HOOK_EVENT, "{}").await;
        let hook = runtime.hook_diagnostics().remove(0);
        assert!(
            hook.last_error.is_none(),
            "delivery {delivery} exhausted a store-lifetime fuel budget: {:?}",
            hook.last_error
        );
    }
}

/// Once a year, at midnight on 1 January: never due while a test runs, so
/// the runtime's own scheduler cannot take a run from under the test.
const QUIET_CRON_SCHEDULE: &str = "0 0 0 1 1 *";

fn counter_cron_manifest(resources: &str) -> String {
    let schedule = QUIET_CRON_SCHEDULE;
    format!(
        r#"[plugin]
name = "counter-cron"
version = "0.1.0"
description = "cron recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "cron"
name = "counter"
schedule = "{schedule}"

[permissions]
required = []
optional = []

[resources]
{resources}
"#
    )
}

fn install_counter_cron(workspace: &Path, resources: &str) {
    install_fixture_with_manifest(
        workspace,
        "counter-cron",
        "pdk/rust/examples/counter-cron",
        &counter_cron_manifest(resources),
    );
}

#[tokio::test]
async fn cron_status_publishes_recovery_health() {
    let temp = TempDir::new().unwrap();
    install_counter_cron(
        temp.path(),
        "max_fuel = 1000000000\nmax_memory_mb = 8\nmax_execution_time_ms = 30000",
    );
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    assert_eq!(
        runtime.cron().run_job("counter-cron").await.unwrap(),
        "counter-cron-run-1"
    );

    let adapter = single_status_adapter(&runtime, "cron_adapters").await;
    assert_eq!(adapter_text(&adapter, "schedule"), QUIET_CRON_SCHEDULE, "{adapter}");
    assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
}

/// Measured against the committed `counter-cron` component with a
/// store-lifetime budget: 16 000 fuel units cannot instantiate it, 17 000
/// instantiates it with no run left, and each run costs roughly 1 000, so
/// 18 500 funds at most two runs.
const FEW_CRON_RUNS_FUEL: u64 = 18_500;

#[tokio::test]
async fn cron_fuel_is_metered_per_invocation() {
    let temp = TempDir::new().unwrap();
    install_counter_cron(
        temp.path(),
        &format!("max_fuel = {FEW_CRON_RUNS_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
    );
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    for run in 1..=4 {
        let output = runtime.cron().run_job("counter-cron").await;
        assert_eq!(
            output.as_deref().ok(),
            Some(format!("counter-cron-run-{run}").as_str()),
            "run {run} exhausted a store-lifetime fuel budget: {output:?}"
        );
    }
}

fn echo_provider_manifest(resources: &str) -> String {
    format!(
        r#"[plugin]
name = "echo-provider"
version = "0.1.0"
description = "provider recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "provider"
name = "wasm-echo"

[permissions]
required = []
optional = []

[resources]
{resources}
"#
    )
}

fn install_echo_provider(workspace: &Path, resources: &str) {
    install_fixture_with_manifest(
        workspace,
        "echo-provider",
        "pdk/rust/examples/echo-provider",
        &echo_provider_manifest(resources),
    );
}

/// The guest copies the last user message into its reply, so four
/// megabytes of message (argument copy plus formatted reply) cannot fit
/// an 8 MB linear memory. `max_payload_bytes` keeps the request past the
/// host-side guard so the trap stays the subject of the test.
const TRAPPING_PROVIDER_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

const GUARDED_PROVIDER_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

fn trapping_provider_message() -> String {
    "x".repeat(4 * 1024 * 1024)
}

async fn echo_chat(
    provider: &dyn crate::providers::traits::Provider,
    message: &str,
) -> anyhow::Result<crate::providers::traits::ChatResponse> {
    provider
        .chat(
            crate::providers::traits::ChatRequest {
                messages: &[crate::providers::traits::ChatMessage::user(message)],
                tools: None,
            },
            "fixture-model",
            0.0,
        )
        .await
}

async fn assert_echo_healthy(provider: &dyn crate::providers::traits::Provider, context: &str) {
    let response = echo_chat(provider, "probe")
        .await
        .unwrap_or_else(|error| panic!("{context}: {error}"));
    assert_eq!(
        response.text.as_deref(),
        Some("wasm-echo/fixture-model: probe"),
        "{context}"
    );
}

#[tokio::test]
async fn provider_recovers_on_a_fresh_store_after_a_guest_trap() {
    let temp = TempDir::new().unwrap();
    install_echo_provider(temp.path(), TRAPPING_PROVIDER_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let provider = runtime.provider("wasm-echo").unwrap();

    let trapped = echo_chat(provider.as_ref(), &trapping_provider_message()).await;
    assert!(trapped.is_err(), "the oversized request must trap and surface an error");
    assert_echo_healthy(provider.as_ref(), "the request after the trap").await;

    let adapter = single_status_adapter(&runtime, "provider_adapter_status").await;
    assert_eq!(adapter_text(&adapter, "provider"), "wasm-echo", "{adapter}");
    assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "failure_count"), 1, "{adapter}");
    assert!(
        !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
        "{adapter}"
    );
    assert_eq!(
        runtime.provider_names(),
        vec!["wasm-echo"],
        "the name list is unchanged"
    );
}

#[tokio::test]
async fn provider_quarantines_after_the_restart_budget_is_spent() {
    let temp = TempDir::new().unwrap();
    install_echo_provider(temp.path(), TRAPPING_PROVIDER_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let (_, mut quarantined_events) = runtime
        .event_bus()
        .subscribe("provider-recovery-test", "prx.plugin.provider_quarantined")
        .await
        .unwrap();
    let provider = runtime.provider("wasm-echo").unwrap();

    let message = trapping_provider_message();
    for request in 1..=5 {
        assert!(
            echo_chat(provider.as_ref(), &message).await.is_err(),
            "trapping request {request} must surface an error, never a response"
        );
    }

    let adapter = single_status_adapter(&runtime, "provider_adapter_status").await;
    assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 3, "{adapter}");

    let refused = echo_chat(provider.as_ref(), "probe").await;
    let error = refused.expect_err("a quarantined provider must return an error");
    assert!(error.to_string().contains("quarantined"), "{error}");

    let event = quarantined_events
        .try_recv()
        .expect("test: one quarantine event must have been published");
    let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
    assert_eq!(
        payload.get("capability").and_then(serde_json::Value::as_str),
        Some("provider")
    );
    assert_ne!(
        runtime.status_tool().availability().level,
        crate::capability::CapabilityAvailabilityLevel::Healthy
    );
}

#[tokio::test]
async fn provider_refuses_a_request_that_cannot_fit_the_guest() {
    let temp = TempDir::new().unwrap();
    install_echo_provider(temp.path(), GUARDED_PROVIDER_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let provider = runtime.provider("wasm-echo").unwrap();

    let error = echo_chat(provider.as_ref(), &trapping_provider_message())
        .await
        .expect_err("an over-cap request is an error, never a truncated response");
    assert!(error.to_string().contains("max_payload_bytes"), "{error}");

    let adapter = single_status_adapter(&runtime, "provider_adapter_status").await;
    assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
    assert_echo_healthy(provider.as_ref(), "the request after an oversize refusal").await;
}

/// Measured against the committed `echo-provider` component with a
/// store-lifetime budget: 20 000 fuel units instantiate it and answer
/// `name()` with no chat left, 30 000 fund four chats and 40 000 six, so
/// 25 000 cannot fund six chats unless fuel is re-armed per call.
const FEW_PROVIDER_CHATS_FUEL: u64 = 25_000;

#[tokio::test]
async fn provider_fuel_is_metered_per_invocation() {
    let temp = TempDir::new().unwrap();
    install_echo_provider(
        temp.path(),
        &format!("max_fuel = {FEW_PROVIDER_CHATS_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
    );
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let provider = runtime.provider("wasm-echo").unwrap();
    for chat in 1..=6 {
        assert_echo_healthy(provider.as_ref(), &format!("chat {chat} under a per-call fuel budget")).await;
    }
}

fn map_storage_manifest(resources: &str) -> String {
    format!(
        r#"[plugin]
name = "map-storage"
version = "0.1.0"
description = "storage recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "storage"
name = "wasm-map"

[permissions]
required = []
optional = []

[resources]
{resources}
"#
    )
}

fn install_map_storage(workspace: &Path, resources: &str) {
    install_fixture_with_manifest(
        workspace,
        "map-storage",
        "pdk/rust/examples/map-storage",
        &map_storage_manifest(resources),
    );
}

/// The guest keeps every entry in its own linear memory, so a few
/// four-megabyte entries cannot fit an 8 MB guest. `max_payload_bytes`
/// keeps the writes past the host-side guard.
const TRAPPING_STORAGE_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

const GUARDED_STORAGE_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

fn large_memory_content() -> String {
    "x".repeat(4 * 1024 * 1024)
}

async fn store_entry(storage: &Arc<dyn Memory>, key: &str, content: &str) -> anyhow::Result<()> {
    storage
        .store(key, content, crate::memory::traits::MemoryCategory::Core, None)
        .await
}

/// Write large entries until the guest traps; returns the key whose write
/// failed.
async fn store_until_trap(storage: &Arc<dyn Memory>, prefix: &str) -> String {
    let content = large_memory_content();
    for attempt in 1..=4 {
        let key = format!("{prefix}-{attempt}");
        if store_entry(storage, &key, &content).await.is_err() {
            return key;
        }
    }
    panic!("four-megabyte entries must exhaust an 8 MB guest within four writes");
}

#[tokio::test]
async fn storage_recovers_on_a_fresh_store_and_announces_the_reset() {
    let temp = TempDir::new().unwrap();
    install_map_storage(temp.path(), TRAPPING_STORAGE_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let (_, mut restarted_events) = runtime
        .event_bus()
        .subscribe("storage-recovery-test", "prx.plugin.storage_restarted")
        .await
        .unwrap();
    let storage = runtime.storage("wasm-map").unwrap();

    store_entry(&storage, "before", "kept in guest memory").await.unwrap();
    let failed_key = store_until_trap(&storage, "large").await;

    store_entry(&storage, "after", "written after the rebuild")
        .await
        .expect("test: the write after the trap runs on a fresh store");
    assert!(
        storage.get(&failed_key).await.unwrap().is_none(),
        "a write that failed must not be reported present"
    );
    assert_eq!(
        storage.count().await.unwrap(),
        1,
        "the rebuilt guest starts empty: only the post-rebuild entry exists"
    );
    assert!(storage.health_check().await);

    let adapter = single_status_adapter(&runtime, "storage_adapter_status").await;
    assert_eq!(adapter_text(&adapter, "storage"), "wasm-map", "{adapter}");
    assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
    assert!(
        !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
        "{adapter}"
    );
    assert_eq!(runtime.storage_names(), vec!["wasm-map"], "the name list is unchanged");

    let event = restarted_events
        .try_recv()
        .expect("test: a storage rebuild must be announced");
    let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
    assert_eq!(
        payload.get("capability").and_then(serde_json::Value::as_str),
        Some("storage")
    );
    assert_eq!(
        payload.get("restart_count").and_then(serde_json::Value::as_u64),
        Some(1)
    );
}

#[tokio::test]
async fn storage_quarantines_after_the_restart_budget_is_spent() {
    let temp = TempDir::new().unwrap();
    install_map_storage(temp.path(), TRAPPING_STORAGE_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let (_, mut quarantined_events) = runtime
        .event_bus()
        .subscribe("storage-recovery-test", "prx.plugin.storage_quarantined")
        .await
        .unwrap();
    let storage = runtime.storage("wasm-map").unwrap();

    for round in 1..=4 {
        store_until_trap(&storage, &format!("round-{round}")).await;
    }

    // The first call after the fourth trap finds the budget spent and
    // quarantines the backend; every operation is refused from then on.
    for (operation, failed) in [
        ("store", store_entry(&storage, "refused", "x").await.is_err()),
        ("recall", storage.recall("", 10, None).await.is_err()),
        ("count", storage.count().await.is_err()),
        ("forget", storage.forget("refused").await.is_err()),
    ] {
        assert!(failed, "a quarantined backend must fail '{operation}', never answer it");
    }
    assert!(!storage.health_check().await, "a quarantined backend is not healthy");

    let adapter = single_status_adapter(&runtime, "storage_adapter_status").await;
    assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 3, "{adapter}");

    let event = quarantined_events
        .try_recv()
        .expect("test: one quarantine event must have been published");
    let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
    assert_eq!(
        payload.get("capability").and_then(serde_json::Value::as_str),
        Some("storage")
    );
    assert_ne!(
        runtime.status_tool().availability().level,
        crate::capability::CapabilityAvailabilityLevel::Healthy
    );
}

#[tokio::test]
async fn storage_refuses_a_write_that_cannot_fit_the_guest() {
    let temp = TempDir::new().unwrap();
    install_map_storage(temp.path(), GUARDED_STORAGE_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let storage = runtime.storage("wasm-map").unwrap();

    store_entry(&storage, "small", "fits").await.unwrap();
    let error = store_entry(&storage, "large", &large_memory_content())
        .await
        .expect_err("an over-cap write is an error, never a silent drop");
    assert!(error.to_string().contains("max_payload_bytes"), "{error}");

    let adapter = single_status_adapter(&runtime, "storage_adapter_status").await;
    assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
    assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
    assert_eq!(storage.count().await.unwrap(), 1, "the earlier entry is intact");
}

/// Measured against the committed `map-storage` component with a
/// store-lifetime budget: 10 000 fuel units cannot instantiate it and
/// answer `name()`, 20 000 fund three writes and 30 000 six, so 20 000
/// cannot fund six writes unless fuel is re-armed per call.
const FEW_STORAGE_WRITES_FUEL: u64 = 20_000;

#[tokio::test]
async fn storage_fuel_is_metered_per_invocation() {
    let temp = TempDir::new().unwrap();
    install_map_storage(
        temp.path(),
        &format!("max_fuel = {FEW_STORAGE_WRITES_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
    );
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let storage = runtime.storage("wasm-map").unwrap();
    for write in 1..=6 {
        store_entry(&storage, &format!("entry-{write}"), "value")
            .await
            .unwrap_or_else(|error| panic!("write {write} exhausted a store-lifetime fuel budget: {error}"));
    }
    assert_eq!(storage.count().await.unwrap(), 6);
}

/// R-C3X F4: the operation that rebuilds a storage backend runs against a
/// fresh, empty guest. A read answering from it would look like "no
/// memories" to the memory layer, so the first read after a rebuild is an
/// error naming the reset; the next read answers from the rebuilt store.
#[tokio::test]
async fn storage_reports_its_reset_on_the_first_read_after_a_rebuild() {
    let temp = TempDir::new().unwrap();
    install_map_storage(temp.path(), TRAPPING_STORAGE_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
    let storage = runtime.storage("wasm-map").unwrap();

    store_entry(&storage, "before", "kept in guest memory").await.unwrap();
    store_until_trap(&storage, "large").await;

    let first = storage.count().await;
    let error = first.expect_err("the read that rebuilt the backend must not answer from the empty store");
    assert!(error.to_string().contains("rebuilt"), "{error}");
    assert_eq!(
        storage.count().await.unwrap(),
        0,
        "the next read answers from the rebuilt, empty store"
    );
}

/// R-C3X F8: provider and storage failures reach callers as `anyhow`
/// errors. They must keep their `PluginError` so the reliability layer can
/// tell a quarantine, a timeout or a refused payload apart by type, not by
/// matching message text.
#[tokio::test]
async fn provider_and_storage_errors_keep_their_plugin_error_type() {
    let temp = TempDir::new().unwrap();
    install_echo_provider(temp.path(), GUARDED_PROVIDER_RESOURCES);
    install_map_storage(temp.path(), GUARDED_STORAGE_RESOURCES);
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

    let provider = runtime.provider("wasm-echo").unwrap();
    let error = echo_chat(provider.as_ref(), &trapping_provider_message())
        .await
        .expect_err("test: an over-cap request is refused");
    assert!(
        matches!(error.downcast_ref::<PluginError>(), Some(PluginError::ResourceLimit(_))),
        "{error:?}"
    );

    let storage = runtime.storage("wasm-map").unwrap();
    let error = store_entry(&storage, "large", &large_memory_content())
        .await
        .expect_err("test: an over-cap write is refused");
    assert!(
        matches!(error.downcast_ref::<PluginError>(), Some(PluginError::ResourceLimit(_))),
        "{error:?}"
    );
}

// ── Finding 95: trust, switch, install binding ──────────────────────

/// Copy a committed example into `plugins/<example>` without trusting it.
fn plant_untrusted(workspace: &Path, example: &str) -> PathBuf {
    install_example_fixture(workspace, example);
    workspace.join("plugins").join(example)
}

async fn plugin_status(runtime: &PluginRuntime, name: &str) -> Option<PluginInfo> {
    runtime
        .list_plugins()
        .await
        .into_iter()
        .find(|plugin| plugin.name == name)
}

#[tokio::test]
async fn the_managed_marker_no_longer_grants_trust() {
    let temp = TempDir::new().unwrap();
    let plugin_dir = plant_untrusted(temp.path(), "audit-hook");
    // What the old code treated as full trust, and what an agent could
    // create with one medium-risk file_write.
    std::fs::write(plugin_dir.join(".prx-managed"), "installed by wasm_plugins_manage\n").unwrap();

    let runtime = init_plugin_runtime_with_policy(temp.path(), test_policy(temp.path()), None)
        .await
        .unwrap();
    let info = plugin_status(&runtime, "audit-hook")
        .await
        .expect("refused plugins stay listed");
    assert!(matches!(info.status, PluginStatus::Error(_)), "{:?}", info.status);
    assert!(info.permissions_granted.is_empty(), "{:?}", info.permissions_granted);
    assert!(runtime.hook_diagnostics().is_empty(), "a refused hook must not run");
    assert_eq!(runtime.adapter_errors().len(), 0);
}

#[tokio::test]
async fn an_untrusted_plugin_that_asks_for_nothing_is_still_refused() {
    let temp = TempDir::new().unwrap();
    plant_untrusted(temp.path(), "pipeline-middleware");
    let runtime = init_plugin_runtime_with_policy(temp.path(), test_policy(temp.path()), None)
        .await
        .unwrap();
    let info = plugin_status(&runtime, "pipeline-middleware").await.unwrap();
    assert!(matches!(&info.status, PluginStatus::Error(reason) if reason.contains("not trusted")));
    assert_eq!(runtime.middleware_diagnostics().len(), 0);
    let output = runtime
        .middleware()
        .process(
            super::super::capabilities::middleware::MiddlewareStage::Outbound,
            r#"{"text":"hello"}"#,
        )
        .await;
    assert!(!output.contains("[wasm-middleware]"), "{output}");
}

#[tokio::test]
async fn a_trusted_plugin_whose_component_changed_is_refused() {
    let temp = TempDir::new().unwrap();
    let plugin_dir = plant_untrusted(temp.path(), "pipeline-middleware");
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert!(matches!(
        plugin_status(&runtime, "pipeline-middleware").await.unwrap().status,
        PluginStatus::Active
    ));
    assert_eq!(runtime.middleware_diagnostics().len(), 1);

    // The agent swaps the component after the operator trusted it.
    let mut bytes = std::fs::read(plugin_dir.join("plugin.wasm")).unwrap();
    bytes.extend_from_slice(b"\0tampered");
    std::fs::write(plugin_dir.join("plugin.wasm"), bytes).unwrap();
    runtime.refresh_all().await.unwrap();

    let info = plugin_status(&runtime, "pipeline-middleware").await.unwrap();
    assert!(
        matches!(&info.status, PluginStatus::Error(reason) if reason.contains("hash")),
        "{:?}",
        info.status
    );
    assert!(runtime.middleware_diagnostics().is_empty(), "the swapped component ran");
}

#[tokio::test]
async fn a_trusted_plugin_whose_manifest_widened_is_refused() {
    let temp = TempDir::new().unwrap();
    let plugin_dir = plant_untrusted(temp.path(), "pipeline-middleware");
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    assert_eq!(runtime.middleware_diagnostics().len(), 1);

    let mut manifest = std::fs::read_to_string(plugin_dir.join("plugin.toml")).unwrap();
    manifest = manifest.replace(
        "required = []",
        "required = []\nhttp_allowlist = [\"https://evil.example/*\"]",
    );
    std::fs::write(plugin_dir.join("plugin.toml"), manifest).unwrap();
    runtime.refresh_all().await.ok();
    let info = plugin_status(&runtime, "pipeline-middleware").await;
    assert!(
        info.is_none_or(|info| matches!(info.status, PluginStatus::Error(_))),
        "an edited manifest kept its trust"
    );
    assert_eq!(runtime.middleware_diagnostics().len(), 0);
}

#[tokio::test]
async fn a_missing_or_unreadable_trust_file_trusts_nothing() {
    let temp = TempDir::new().unwrap();
    plant_untrusted(temp.path(), "pipeline-middleware");
    let policy = test_policy(temp.path());
    std::fs::create_dir_all(policy.trust_file().parent().unwrap()).unwrap();
    std::fs::write(policy.trust_file(), "{ garbage").unwrap();
    let runtime = init_plugin_runtime_with_policy(temp.path(), policy, None)
        .await
        .unwrap();
    let info = plugin_status(&runtime, "pipeline-middleware").await.unwrap();
    assert!(matches!(info.status, PluginStatus::Error(_)));
    assert_eq!(runtime.middleware_diagnostics().len(), 0);
}

#[tokio::test]
async fn with_the_switch_off_nothing_is_scanned_or_loaded() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    install_example_fixture(&workspace, "pipeline-middleware");
    // Even a plugin the operator trusted must not load while the switch is off.
    trust_workspace_plugins(&workspace);

    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.clone();
    config.config_path = temp.path().join("config.toml");
    assert!(!config.plugins.enabled, "the switch defaults to off");
    assert!(crate::plugins::init_plugin_runtime(&config, None).await.is_none());
    assert!(
        !workspace.join(".prx-state/plugin-cache").exists(),
        "a disabled runtime must not even prepare its cache"
    );
    // The policy form refuses too.
    let off = PluginPolicy::new(false, workspace.join(".prx-state").join(TRUST_FILE_NAME));
    assert!(
        init_plugin_runtime_with_policy(&workspace, off.clone(), None)
            .await
            .is_none()
    );
    assert!(matches!(
        PluginManager::new(workspace.join("plugins"), off).err(),
        Some(PluginError::Disabled)
    ));
}

#[tokio::test]
async fn the_switch_on_with_an_unsafe_state_directory_stays_off() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.clone();
    config.config_path = workspace.join("config.toml");
    config.plugins.enabled = true;
    assert!(crate::plugins::init_plugin_runtime(&config, None).await.is_none());
}

fn manage_for(
    runtime: &Arc<PluginRuntime>,
    workspace: &Path,
    autonomy: crate::security::AutonomyLevel,
) -> Box<dyn Tool> {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config.autonomy.level = autonomy;
    runtime.manage_tool(crate::runtime::bootstrap::build_security_policy(&config))
}

fn source_for_install(workspace: &Path, example: &str) -> String {
    let source = workspace.join("plugin-sources").join(example);
    std::fs::create_dir_all(&source).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("pdk/rust/examples")
        .join(example);
    std::fs::copy(fixture.join("plugin.toml"), source.join("plugin.toml")).unwrap();
    std::fs::copy(fixture.join("plugin.wasm"), source.join("plugin.wasm")).unwrap();
    format!("plugin-sources/{example}")
}

#[tokio::test]
async fn install_records_trust_bound_to_name_hash_and_the_granted_permissions() {
    let temp = TempDir::new().unwrap();
    let source = source_for_install(temp.path(), "audit-hook");
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    let manage = manage_for(&runtime, temp.path(), crate::security::AutonomyLevel::Full);

    // A grant that leaves out a required permission is refused up front.
    let short = manage
        .execute(serde_json::json!({"action": "install", "source": source, "grant_permissions": ["log"]}))
        .await
        .unwrap();
    assert!(!short.success);
    assert!(
        short.error.unwrap_or_default().contains("requires"),
        "required permission check"
    );
    // A grant of an undeclared permission is refused.
    let wide = manage
        .execute(serde_json::json!({
            "action": "install",
            "source": source,
            "grant_permissions": ["log", "kv", "events", "shell"]
        }))
        .await
        .unwrap();
    assert!(!wide.success);
    // Nothing was trusted by the failed attempts.
    assert!(
        test_policy(temp.path())
            .records()
            .unwrap()
            .iter()
            .all(|r| r.name != "audit-hook")
    );

    let installed = manage
        .execute(serde_json::json!({
            "action": "install",
            "source": source,
            "grant_permissions": ["events", "kv", "log"]
        }))
        .await
        .unwrap();
    assert!(installed.success, "{:?}", installed.error);
    let records = test_policy(temp.path()).records().unwrap();
    let record = records.iter().find(|r| r.name == "audit-hook").expect("trust recorded");
    let wasm = std::fs::read(temp.path().join("plugins/audit-hook/plugin.wasm")).unwrap();
    assert_eq!(record.sha256, crate::plugins::trust::sha256_hex(&wasm));
    assert_eq!(record.permissions, vec!["events", "kv", "log"]);
    assert!(!temp.path().join("plugins/audit-hook/.prx-managed").exists());
    assert!(
        !test_policy(temp.path())
            .trust_file()
            .starts_with(temp.path().join("plugins")),
        "the trust file must live outside the agent-writable plugins directory"
    );
    assert_eq!(runtime.hook_diagnostics().len(), 1);

    let removed = manage
        .execute(serde_json::json!({"action": "remove", "name": "audit-hook"}))
        .await
        .unwrap();
    assert!(removed.success, "{:?}", removed.error);
    assert!(
        test_policy(temp.path())
            .records()
            .unwrap()
            .iter()
            .all(|r| r.name != "audit-hook")
    );
}

#[tokio::test]
async fn install_approval_is_bound_to_the_exact_build_and_grant() {
    let temp = TempDir::new().unwrap();
    let source = source_for_install(temp.path(), "audit-hook");
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    let manage = manage_for(&runtime, temp.path(), crate::security::AutonomyLevel::Supervised);

    let denied = manage
        .execute(serde_json::json!({
            "action": "install",
            "source": source,
            "grant_permissions": ["log", "kv", "events"]
        }))
        .await
        .unwrap();
    assert!(!denied.success);
    let error = denied.error.unwrap_or_default();
    assert!(error.contains("runtime approval grant"), "{error}");
    let wasm = std::fs::read(temp.path().join("plugin-sources/audit-hook/plugin.wasm")).unwrap();
    assert!(
        error.contains(&crate::plugins::trust::sha256_hex(&wasm)),
        "the approval operation must name the component hash: {error}"
    );
    assert!(error.contains("audit-hook"), "{error}");
    assert!(
        error.contains("grants_events_kv_log"),
        "the approval text must show the sorted grant: {error}"
    );
    let grant_digest = crate::plugins::trust::sha256_hex(br#"["events","kv","log"]"#);
    assert!(
        error.contains(&grant_digest),
        "the operation must carry the digest of the sorted grant: {error}"
    );
    assert!(!temp.path().join("plugins/audit-hook").exists());
    assert_eq!(test_policy(temp.path()).records().unwrap().len(), 0);
}

#[tokio::test]
async fn install_refuses_expected_hashes_that_differ_from_the_bytes_it_reads() {
    let temp = TempDir::new().unwrap();
    let source = source_for_install(temp.path(), "audit-hook");
    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    let manage = manage_for(&runtime, temp.path(), crate::security::AutonomyLevel::Full);
    let mismatched = manage
        .execute(serde_json::json!({
            "action": "install",
            "source": source,
            "grant_permissions": ["events", "kv", "log"],
            "expected_sha256": "0".repeat(64),
            "expected_manifest_sha256": "1".repeat(64),
        }))
        .await
        .unwrap();
    assert!(!mismatched.success);
    let error = mismatched.error.unwrap_or_default();
    assert!(
        error.contains("plugin hashes differ from the reviewed artifacts"),
        "{error}"
    );
    assert!(
        test_policy(temp.path())
            .records()
            .unwrap()
            .iter()
            .all(|record| record.name != "audit-hook")
    );
    assert!(!temp.path().join("plugins/audit-hook").exists());
}

#[tokio::test]
async fn optional_permissions_are_granted_only_through_the_trust_record() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("plugins/optional-tool");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "[plugin]\nname = \"optional-tool\"\nversion = \"0.1.0\"\nwasm = \"missing.wasm\"\n\n\
             [permissions]\nrequired = [\"log\"]\noptional = [\"memory\"]\n",
    )
    .unwrap();
    let policy = test_policy(temp.path());
    let material = PluginMaterial::read(&dir).unwrap();
    policy
        .record(material.record_for(&["log".to_string()]).unwrap())
        .unwrap();
    let runtime = init_plugin_runtime_with_policy(temp.path(), policy.clone(), None)
        .await
        .unwrap();
    let info = plugin_status(&runtime, "optional-tool").await.unwrap();
    assert_eq!(info.permissions_granted, vec!["log"]);

    policy
        .record(material.record_for(&["log".to_string(), "memory".to_string()]).unwrap())
        .unwrap();
    runtime.refresh_all().await.unwrap();
    let info = plugin_status(&runtime, "optional-tool").await.unwrap();
    assert_eq!(info.permissions_granted, vec!["log", "memory"]);
}

/// Trust is keyed by manifest name. Removing folder `foo` (manifest `bar`)
/// must revoke `bar` and leave the unrelated trusted plugin named `foo`.
#[tokio::test]
async fn remove_revokes_the_manifest_name_not_the_folder_name() {
    let temp = TempDir::new().unwrap();
    install_example_fixture(temp.path(), "pipeline-middleware");
    install_example_fixture(temp.path(), "echo-provider");
    let plugins = temp.path().join("plugins");
    std::fs::rename(plugins.join("pipeline-middleware"), plugins.join("foo")).unwrap();
    std::fs::rename(plugins.join("echo-provider"), plugins.join("other")).unwrap();
    let rewrite = |dir: &str, from: &str, to: &str| {
        let path = plugins.join(dir).join("plugin.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&format!("name = \"{from}\"")), "fixture name line");
        std::fs::write(
            &path,
            text.replacen(&format!("name = \"{from}\""), &format!("name = \"{to}\""), 1),
        )
        .unwrap();
    };
    rewrite("foo", "pipeline-middleware", "bar");
    rewrite("other", "echo-provider", "foo");

    let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
    let names = |records: Vec<crate::plugins::trust::TrustRecord>| {
        let mut names: Vec<String> = records.into_iter().map(|record| record.name).collect();
        names.sort();
        names
    };
    assert_eq!(names(test_policy(temp.path()).records().unwrap()), vec!["bar", "foo"]);

    let manage = manage_for(&runtime, temp.path(), crate::security::AutonomyLevel::Full);
    let removed = manage
        .execute(serde_json::json!({"action": "remove", "name": "foo"}))
        .await
        .unwrap();
    assert!(removed.success, "{:?}", removed.error);
    assert_eq!(
        names(test_policy(temp.path()).records().unwrap()),
        vec!["foo"],
        "the folder-name collision revoked an unrelated plugin's trust"
    );
    assert!(plugins.join("other").is_dir());
}
