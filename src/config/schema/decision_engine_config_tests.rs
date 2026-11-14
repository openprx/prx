#![cfg(test)]

use super::*;
use crate::config::schema::decision_engine::*;

fn load(text: &str) -> Config {
    let mut root: toml::Value = toml::from_str(text).unwrap();
    legacy::migrate_legacy_config_keys(&mut root, None).unwrap();
    let config: Config = root.try_into().unwrap();
    config.validate().unwrap();
    config
}
const REMOTE: &str = "kind = 'compatible_http'\nremote_enabled = true\nendpoint = 'https://example.com/v1'\nmodel = 'model-1'\npinned_snapshot = ''\ncredential_ref = 'none'\ntimeout_ms = 200\nmax_request_bytes = 8192\nmax_response_bytes = 16384\ncircuit_breaker = { failure_threshold = 9, cooldown_secs = 120 }\n";
#[test]
fn absent_tables_leave_engine_off() {
    let c = load("");
    assert!(!c.decision_engine.enabled);
    assert!(c.decision_engine.backend.is_none());
}
/// R24: a `[causal_tree]` table is an unknown configuration path like any
/// other deleted key; nothing imports, warns about or strips it.
#[test]
fn a_causal_tree_table_is_an_unknown_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[causal_tree]\nenabled = true\n").unwrap();
    let error = Config::load_from_path(&path, dir.path().join("workspace")).unwrap_err();
    assert!(format!("{error:#}").contains("causal_tree"), "{error:#}");
}
/// R24: the turn limits live in `[decision_engine]` with today's defaults
/// and bounds.
#[test]
fn turn_limits_have_their_defaults_and_bounds() {
    let c = load("");
    assert_eq!(c.decision_engine.latency_budget_ms, 500);
    assert!((c.decision_engine.token_ratio_limit - 0.35).abs() < f32::EPSILON);
    assert_eq!(c.decision_engine.max_candidates, 12);
    let c = load("[decision_engine]\nlatency_budget_ms = 50\ntoken_ratio_limit = 1.0\nmax_candidates = 32\n");
    assert_eq!(c.decision_engine.latency_budget_ms, 50);
    assert_eq!(c.decision_engine.max_candidates, 32);
    for (key, value) in [
        ("latency_budget_ms", "49"),
        ("latency_budget_ms", "5001"),
        ("token_ratio_limit", "0.0"),
        ("token_ratio_limit", "-1.0"),
        ("max_candidates", "0"),
        ("max_candidates", "33"),
    ] {
        let parsed = toml::from_str::<DecisionEngineConfig>(&format!("{key} = {value}"));
        assert!(
            parsed.is_err() || parsed.unwrap().validate().is_err(),
            "{key} = {value}"
        );
    }
}
#[test]
fn remote_endpoint_without_remote_enabled_rejected_loopback_accepted() {
    for endpoint in ["https://example.com/v1", "http://127.0.0.1:9000/v1"] {
        let c: DecisionEngineConfig =
            toml::from_str(&format!("[backend]\nkind='compatible_http'\nendpoint='{endpoint}'")).unwrap();
        assert_eq!(c.validate().is_ok(), endpoint.contains("127.0.0.1"));
    }
}
#[test]
fn hosted_backend_without_remote_enabled_excluded_not_rejected() {
    let c: DecisionEngineConfig = toml::from_str("[backend]\nkind='typesafe'\nmodel='jev-1.13.0'").unwrap();
    c.validate().unwrap();
    assert!(!c.backend_available());
}
#[test]
fn numeric_bounds_reject_min_minus_one_and_max_plus_one() {
    for (key, min, max) in [
        ("prompt_budget_tokens", 50, 2_000),
        ("candidates", 1, 20),
        ("timeout_ms", 100, 30_000),
        ("telemetry_cap", 0, 100_000),
        ("observe_queue", 1, 1_024),
    ] {
        for value in [min - 1, max + 1] {
            let parsed = toml::from_str::<DecisionEngineConfig>(&format!("{key}={value}"));
            assert!(parsed.is_err() || parsed.unwrap().validate().is_err(), "{key}={value}");
        }
        for value in [min, max] {
            toml::from_str::<DecisionEngineConfig>(&format!("{key}={value}"))
                .unwrap()
                .validate()
                .unwrap();
        }
    }
}
async fn round_trip(text: &str) -> Config {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let c = Config::load_from_path(&path, dir.path().join("workspace")).unwrap();
    crate::config::files::write_split_config(&c, false).await.unwrap();
    let saved = Config::load_from_path(&path, dir.path().join("workspace")).unwrap();
    assert_eq!(c.decision_engine, saved.decision_engine);
    saved
}
#[tokio::test]
async fn save_reload_keeps_backend_and_explicit_disable() {
    let c = round_trip(&format!(
        "[decision_engine]\nenabled=false\n[decision_engine.backend]\n{REMOTE}"
    ))
    .await;
    assert!(!c.decision_engine.enabled);
    assert_eq!(c.decision_engine.backend.unwrap().timeout_ms, 200);
}
#[tokio::test]
async fn explicit_empty_backend_survives_save_reload() {
    for explicit in ["[decision_engine.backend]", "[decision_engine.backend.circuit_breaker]"] {
        let c = round_trip(explicit).await;
        assert_eq!(c.decision_engine.backend, Some(DecisionBackendConfig::default()));
        let saved = toml::to_string(&c.decision_engine).unwrap();
        assert!(saved.contains("[backend]") && saved.contains("pinned_snapshot = \"\""));
    }
}

#[test]
fn shared_backend_validation_preserves_typed_source() {
    use std::error::Error;
    let config: DecisionEngineConfig =
        toml::from_str("[backend]\nkind='compatible_http'\nendpoint='https://example.com/v1'").unwrap();
    let error = config.validate().unwrap_err();
    assert!(matches!(error, DecisionEngineConfigError::Backend(_)));
    assert!(
        error
            .source()
            .unwrap()
            .is::<crate::config::schema::decision_engine::BackendConfigError>()
    );
}

#[test]
fn disabled_direct_backend_obeys_the_configured_latency_budget() {
    let mut config = Config::default();
    config.decision_engine.latency_budget_ms = 300;
    config.decision_engine.backend = Some(DecisionBackendConfig {
        timeout_ms: 400,
        ..Default::default()
    });
    let error = config.validate().unwrap_err();
    assert!(matches!(
        error.downcast_ref::<DecisionEngineConfigError>(),
        Some(DecisionEngineConfigError::OutOfRange {
            key: "backend.timeout_ms",
            max: 300,
            ..
        })
    ));
    config.decision_engine.latency_budget_ms = 500;
    config.validate().unwrap();
}

fn remote() -> crate::config::schema::decision_engine::DecisionBackendConfig {
    toml::from_str(REMOTE).unwrap()
}

fn factory_for(config: &Config) -> crate::decision_engine::prediction::factory::DecisionBackendFactory {
    struct NoTransports;
    impl crate::decision_engine::prediction::factory::DecisionTransports for NoTransports {
        fn build(
            &self,
            _: &crate::decision_engine::prediction::factory::BackendSpec<'_>,
        ) -> Result<
            Box<dyn crate::decision_engine::prediction::backend::DecisionBackend>,
            crate::cost::decision_attempt::DecisionError,
        > {
            Err(crate::cost::decision_attempt::DecisionError::Unavailable)
        }
    }
    let catalog = crate::providers::catalog::CatalogView::from_config(config);
    crate::decision_engine::prediction::factory::DecisionBackendFactory::from_config(
        config,
        crate::decision_engine::prediction::factory::FactoryDeps {
            catalog: &catalog,
            key: None,
            transports: &NoTransports,
            config_generation: 1,
            events: std::sync::Arc::new(crate::observability::causal_event::testing::RecordingSink::default()),
        },
    )
    .unwrap()
}

#[test]
fn effective_backend_reads_the_engine_table() {
    let mut config = Config::default();
    config.decision_engine.enabled = true;
    let mut explicit = crate::config::schema::decision_engine::DecisionBackendConfig::default();
    explicit.max_request_bytes = 4096;
    explicit.model = "explicit-model".into();
    config.decision_engine.backend = Some(explicit.clone());
    let effective = effective_engine_backend(&config);
    assert_eq!(effective, &explicit);
    assert!(crate::config::schema::decision_engine::configured_selection(effective).is_none());
    let factory = factory_for(&config);
    assert!(factory.status().order.is_empty(), "no remote slot");
    assert_eq!(
        crate::decision_engine::status::snapshot(&config).backend_kind,
        explicit.kind.as_str()
    );
    config.decision_engine.backend = Some(remote());
    let selection =
        crate::config::schema::decision_engine::configured_selection(effective_engine_backend(&config)).unwrap();
    assert_eq!(
        selection.kind,
        crate::config::schema::decision_engine::BackendKind::CompatibleHttp
    );
}

/// R24 ruling (2): an absent `[decision_engine.backend]` is the backend's own
/// serde default, the one a fresh `prx init` writes.
#[test]
fn absent_engine_backend_is_the_serde_default() {
    let mut config = load("[decision_engine]\nenabled = true\n");
    assert!(config.decision_engine.backend.is_none());
    let effective = effective_engine_backend(&config);
    assert_eq!(
        effective,
        &crate::config::schema::decision_engine::DecisionBackendConfig::default()
    );
    let parsed: crate::config::schema::decision_engine::DecisionBackendConfig = toml::from_str("").unwrap();
    assert_eq!(effective, &parsed);
    config.decision_engine.backend = Some(parsed);
    let factory = factory_for(&config);
    assert_eq!(factory.status().order, [], "no remote construction");
    assert_eq!(
        factory.status().excluded,
        [(
            crate::config::schema::decision_engine::BackendKind::CompatibleHttp,
            crate::config::schema::decision_engine::BackendLocality::Loopback,
            crate::decision_engine::prediction::factory::Exclusion::NotConfigured,
        )]
    );
}
