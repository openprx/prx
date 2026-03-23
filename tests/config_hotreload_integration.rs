//! Phase 4: Cross-module integration — config hot-reload via SharedConfig.
//!
//! Validates that SharedConfig (ArcSwap) provides lock-free reads and
//! atomic swaps, simulating the hot-reload path without needing
//! the actual file-watcher.

use openprx::config::{Config, new_shared};

#[test]
fn shared_config_initial_load() {
    let config = Config::default();
    let temp = config.default_temperature;
    let shared = new_shared(config);
    let loaded = shared.load_full();
    assert!((loaded.default_temperature - temp).abs() < f64::EPSILON);
}

#[test]
fn shared_config_swap_visible_to_readers() {
    let config = Config {
        default_provider: Some("anthropic".to_string()),
        ..Config::default()
    };
    let shared = new_shared(config);

    assert_eq!(shared.load_full().default_provider.as_deref(), Some("anthropic"));

    // Simulate hot-reload: swap in new config
    let new_config = Config {
        default_provider: Some("openai".to_string()),
        ..Config::default()
    };
    shared.store(std::sync::Arc::new(new_config));

    assert_eq!(shared.load_full().default_provider.as_deref(), Some("openai"));
}

#[test]
fn shared_config_old_snapshot_still_valid() {
    let shared = new_shared(Config::default());

    let old_snapshot = shared.load_full();
    let old_temp = old_snapshot.default_temperature;

    let new_config = Config {
        default_temperature: 1.5,
        ..Config::default()
    };
    shared.store(std::sync::Arc::new(new_config));

    // Old snapshot still has old value (ArcSwap guarantee)
    assert!((old_snapshot.default_temperature - old_temp).abs() < f64::EPSILON);
    // New reader sees new config
    assert!((shared.load_full().default_temperature - 1.5).abs() < f64::EPSILON);
}

#[tokio::test]
async fn concurrent_readers_during_swap() {
    let shared = new_shared(Config::default());

    let mut handles = Vec::new();
    for _ in 0..10 {
        let shared = shared.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..100 {
                let snapshot = shared.load_full();
                let _ = snapshot.default_temperature;
                let _ = snapshot.default_model.as_ref().map(|m| m.len());
            }
        }));
    }

    // Swap while readers are running
    for i in 0..5 {
        let cfg = Config {
            default_provider: Some(format!("provider-{i}")),
            ..Config::default()
        };
        shared.store(std::sync::Arc::new(cfg));
        tokio::task::yield_now().await;
    }

    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(shared.load_full().default_provider.as_deref(), Some("provider-4"));
}

#[test]
fn shared_config_clone_shares_same_cell() {
    let shared = new_shared(Config::default());
    let clone = shared.clone();

    let cfg = Config {
        default_provider: Some("from-clone".to_string()),
        ..Config::default()
    };
    clone.store(std::sync::Arc::new(cfg));

    assert_eq!(shared.load_full().default_provider.as_deref(), Some("from-clone"));
    assert_eq!(clone.load_full().default_provider.as_deref(), Some("from-clone"));
}

#[test]
fn config_default_roundtrip_via_toml() {
    let config = Config::default();
    let toml_str = toml::to_string_pretty(&config).expect("test: serialize");
    let restored: Config = toml::from_str(&toml_str).expect("test: deserialize");
    assert!((restored.default_temperature - config.default_temperature).abs() < f64::EPSILON);
}

#[test]
fn config_modified_fields_survive_toml_roundtrip() {
    let config = Config {
        default_provider: Some("deepseek".to_string()),
        default_model: Some("ds-v3".to_string()),
        default_temperature: 0.42,
        ..Config::default()
    };

    let toml_str = toml::to_string_pretty(&config).expect("test: serialize");
    let restored: Config = toml::from_str(&toml_str).expect("test: deserialize");
    assert_eq!(restored.default_provider.as_deref(), Some("deepseek"));
    assert_eq!(restored.default_model.as_deref(), Some("ds-v3"));
    assert!((restored.default_temperature - 0.42).abs() < f64::EPSILON);
}
