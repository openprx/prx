#![cfg(test)]
//! PMM-C: the gateway resolves its default before it builds anything.

use super::super::*;
use crate::memory::SqliteMemory;

fn decision_only(workspace: &std::path::Path) -> Config {
    let mut config = Config {
        workspace_dir: workspace.to_path_buf(),
        api_key: Some("PMMC-GATEWAY-ROOT".to_string()),
        default_provider: Some("openrouter".to_string()),
        default_model: Some("jev-decision-test".to_string()),
        ..Config::default()
    };
    config.providers.models.push(
        toml::from_str(
            "provider = \"openrouter\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
        )
        .expect("test: declaration"),
    );
    config
}

fn build(manager: &crate::config::SharedConfig, workspace: &std::path::Path) -> anyhow::Result<TurnRuntimeGeneration> {
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: memory"));
    build_gateway_turn_runtime(
        manager.pin(),
        manager.clone(),
        memory,
        None,
        None,
        None,
        None,
        Arc::new(HookManager::new(workspace.to_path_buf())),
        #[cfg(feature = "wasm-plugins")]
        None,
    )
}

/// A generation whose default model cannot chat is refused by the
/// `gateway_turn_runtime` participant, and the previous generation stays
/// active.
#[test]
fn a_decision_only_default_is_rejected_and_the_previous_generation_stays() {
    let workspace = tempfile::tempdir().expect("test: temp dir");
    let test_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: gateway construction runtime");
    let _test_runtime_guard = test_runtime.enter();
    let mut good = decision_only(workspace.path());
    good.default_model = Some("vendor/chat-model".to_string());
    let manager = crate::config::new_shared(good);
    let runtime = build(&manager, workspace.path()).expect("test: the chat default builds");
    let owner: Arc<dyn crate::config::ConfigGenerationParticipant> = Arc::new(GatewayTurnRuntimeOwner::new(
        runtime,
        manager.clone(),
        Arc::new(SqliteMemory::new(workspace.path()).expect("test: memory")),
        None,
        None,
        None,
        None,
        Arc::new(HookManager::new(workspace.path().to_path_buf())),
        #[cfg(feature = "wasm-plugins")]
        None,
    ));
    manager.register_participant(&owner);

    let error = manager
        .apply_runtime_config(decision_only(workspace.path()), crate::config::ConfigReloadTrigger::Api)
        .expect_err("a decision-only default must not activate");
    let text = format!("{error:#}");
    assert!(text.contains("cannot be used for chat"), "{text}");
    assert!(!text.contains("PMMC-GATEWAY-ROOT"), "{text}");
    assert_eq!(
        manager.pin().effective.default_model.as_deref(),
        Some("vendor/chat-model"),
        "the previous generation stays active"
    );
}

/// R-PMMC F-1: a `hint:` default starts the gateway again.
#[test]
fn a_hint_default_builds_the_turn_runtime() {
    let workspace = tempfile::tempdir().expect("test: temp dir");
    let test_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: gateway construction runtime");
    let _test_runtime_guard = test_runtime.enter();
    let mut config = decision_only(workspace.path());
    config.default_model = Some("hint:fast".to_string());
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "openrouter".to_string(),
        model: "vendor/fast-chat".to_string(),
        api_key: None,
        credential_ref: None,
    });
    let manager = crate::config::new_shared(config);
    let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
    let sink = std::sync::Arc::clone(&captured);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || LogCapture(std::sync::Arc::clone(&sink)))
        .finish();
    let turn = tracing::subscriber::with_default(subscriber, || build(&manager, workspace.path()))
        .expect("a hint default builds (no restart loop)");
    assert_eq!(turn.model, "vendor/fast-chat");
    let logged = String::from_utf8_lossy(&captured.lock()).into_owned();
    assert!(
        !logged.contains("will be refused"),
        "routed hint was warned as unroutable: {logged}"
    );
}

#[test]
fn removing_a_live_default_hint_route_keeps_the_previous_generation() {
    let workspace = tempfile::tempdir().expect("test: temp dir");
    let test_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: gateway construction runtime");
    let _test_runtime_guard = test_runtime.enter();
    let mut good = decision_only(workspace.path());
    good.default_model = Some("hint:fast".to_string());
    good.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "openrouter".to_string(),
        model: "vendor/fast-chat".to_string(),
        api_key: None,
        credential_ref: None,
    });
    let manager = crate::config::new_shared(good.clone());
    let runtime = build(&manager, workspace.path()).expect("test: initial route builds");
    let owner: Arc<dyn crate::config::ConfigGenerationParticipant> = Arc::new(GatewayTurnRuntimeOwner::new(
        runtime,
        manager.clone(),
        Arc::new(SqliteMemory::new(workspace.path()).expect("test: memory")),
        None,
        None,
        None,
        None,
        Arc::new(HookManager::new(workspace.path().to_path_buf())),
        #[cfg(feature = "wasm-plugins")]
        None,
    ));
    manager.register_participant(&owner);
    let mut bad = good;
    bad.model_routes.clear();
    let error = manager
        .apply_runtime_config(bad, crate::config::ConfigReloadTrigger::Api)
        .expect_err("missing route cannot activate");
    assert!(format!("{error:#}").contains("fast"));
    assert_eq!(manager.pin().effective.model_routes.len(), 1);
    assert_eq!(manager.pin().effective.default_model.as_deref(), Some("hint:fast"));
}

struct LogCapture(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_decision_only_default_builds_no_turn_runtime() {
    let workspace = tempfile::tempdir().expect("test: temp dir");
    let manager = crate::config::new_shared(decision_only(workspace.path()));
    let Err(error) = build(&manager, workspace.path()) else {
        panic!("a decision-only default built the gateway turn runtime");
    };
    assert!(format!("{error:#}").contains("cannot be used for chat"), "{error:#}");
}
