#![cfg(test)]

use super::*;
use crate::decision_engine::config::*;

fn load(text: &str) -> Config {
    let mut root: toml::Value = toml::from_str(text).unwrap();
    legacy::migrate_legacy_config_keys(&mut root, None).unwrap();
    let config: Config = root.try_into().unwrap();
    config.validate().unwrap();
    config
}
const REMOTE: &str = "kind = 'compatible_http'\nremote_enabled = true\nendpoint = 'https://example.com/v1'\nmodel = 'model-1'\npinned_snapshot = ''\ncredential_ref = 'none'\ntimeout_ms = 200\nmax_request_bytes = 8192\nmax_response_bytes = 16384\ncircuit_breaker = { failure_threshold = 9, cooldown_secs = 120 }\n";
fn migrated(text: &str) -> (toml::Value, String) {
    let mut root: toml::Value = toml::from_str(text).unwrap();
    let (_, logs) = super::tests::capture_logs(|| legacy::migrate_legacy_config_keys(&mut root, None).unwrap());
    (root, logs)
}
#[test]
fn absent_tables_leave_engine_off() {
    let c = load("");
    assert!(!c.decision_engine.enabled);
    assert!(c.decision_engine.backend.is_none());
}
#[test]
fn saved_default_causal_tree_enabled_true_does_not_enable_engine() {
    assert!(
        !load("[causal_tree]\nschema_version=2\nenabled=true")
            .decision_engine
            .enabled
    );
}
#[test]
fn explicit_decision_engine_setting_wins_over_legacy() {
    let c = load("[causal_tree]\nschema_version=2\nenabled=false\n[decision_engine]\nenabled=true");
    assert!(c.decision_engine.enabled);
    assert!(!c.causal_tree.enabled);
}
#[test]
fn legacy_backend_copies_every_field_with_one_warn() {
    let text = format!("[decision_engine]\nenabled=true\n[causal_tree.backend]\n{REMOTE}");
    let (root, logs) = migrated(&text);
    assert_eq!(logs.matches("W1:").count(), 1, "{logs}");
    assert!(!logs.contains("W2:") && !logs.contains("W3:"));
    let c = load(&text);
    assert_eq!(c.decision_engine.backend.as_ref().unwrap(), &c.causal_tree.backend);
    assert_eq!(
        root.get("decision_engine")
            .expect("missing decision_engine table")
            .get("backend")
            .expect("missing decision_engine.backend key"),
        root.get("causal_tree")
            .expect("missing causal_tree table")
            .get("backend")
            .expect("missing causal_tree.backend key")
    );
    let (_, logs) = migrated(&format!("[causal_tree.backend]\n{REMOTE}"));
    assert_eq!(logs.matches("W3:").count(), 1);
}
#[test]
fn mixed_backend_tables_new_wins_and_w2_suppresses_w1_and_w3() {
    for enabled in [false, true] {
        let text = format!(
            "[decision_engine]\nenabled={enabled}\n[decision_engine.backend]\nkind='builtin'\n[causal_tree.backend]\n{REMOTE}"
        );
        let (_, logs) = migrated(&text);
        assert_eq!(logs.matches("W2:").count(), 1, "{logs}");
        assert!(!logs.contains("W1:") && !logs.contains("W3:"));
        assert_eq!(load(&text).decision_engine.backend.unwrap().endpoint, "");
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
    assert_eq!(c.causal_tree, saved.causal_tree);
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
async fn explicit_empty_new_backend_survives_save_reload_over_remote_legacy() {
    for explicit in ["[decision_engine.backend]", "[decision_engine.backend.circuit_breaker]"] {
        let c = round_trip(&format!("{explicit}\n[causal_tree.backend]\n{REMOTE}")).await;
        assert_eq!(c.decision_engine.backend, Some(DecisionBackendConfig::default()));
        let saved = toml::to_string(&c.decision_engine).unwrap();
        assert!(saved.contains("[backend]") && saved.contains("pinned_snapshot = \"\""));
    }
}

#[tokio::test]
async fn differing_legacy_and_new_tables_round_trip_through_save_reload() {
    let c = round_trip(&format!(
        "[decision_engine.backend]\nkind='builtin'\n[causal_tree.backend]\n{REMOTE}"
    ))
    .await;
    assert_ne!(c.decision_engine.backend.unwrap(), c.causal_tree.backend);
}
