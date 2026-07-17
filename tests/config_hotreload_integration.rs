//! Phase 4: Cross-module integration — config hot-reload via `SharedConfig`.
//!
//! Validates that `SharedConfig` provides immutable generation snapshots and
//! publishes reloads only through the generation manager.

use openprx::config::{Config, ConfigReloadTrigger, SharedConfig, new_shared};

fn shared_fixture() -> anyhow::Result<(tempfile::TempDir, SharedConfig)> {
    let temp = tempfile::TempDir::new()?;
    let config_path = temp.path().join("config.toml");
    let config = Config {
        config_path: config_path.clone(),
        workspace_dir: temp.path().join("workspace"),
        ..Config::default()
    };
    std::fs::write(&config_path, toml::to_string_pretty(&config)?)?;
    Ok((temp, new_shared(config)))
}

fn publish_temperature(shared: &SharedConfig, temperature: f64) -> anyhow::Result<()> {
    let mut candidate = shared.load_full().as_ref().clone();
    candidate.default_temperature = temperature;
    std::fs::write(&candidate.config_path, toml::to_string_pretty(&candidate)?)?;
    shared.reload_from_disk(ConfigReloadTrigger::Tool)?;
    Ok(())
}

#[test]
fn shared_config_initial_load() {
    let config = Config::default();
    let temp = config.default_temperature;
    let shared = new_shared(config);
    let loaded = shared.load_full();
    assert!((loaded.default_temperature - temp).abs() < f64::EPSILON);
}

#[test]
fn shared_config_swap_visible_to_readers() -> anyhow::Result<()> {
    let (_temp, shared) = shared_fixture()?;

    let old_temperature = shared.load_full().default_temperature;

    publish_temperature(&shared, 1.25)?;

    assert!((shared.load_full().default_temperature - old_temperature).abs() > f64::EPSILON);
    assert!((shared.load_full().default_temperature - 1.25).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn shared_config_old_snapshot_still_valid() -> anyhow::Result<()> {
    let (_temp, shared) = shared_fixture()?;

    let old_snapshot = shared.load_full();
    let old_temp = old_snapshot.default_temperature;

    publish_temperature(&shared, 1.5)?;

    // Old snapshot remains pinned to its generation.
    assert!((old_snapshot.default_temperature - old_temp).abs() < f64::EPSILON);
    // New reader sees new config
    assert!((shared.load_full().default_temperature - 1.5).abs() < f64::EPSILON);
    Ok(())
}

#[tokio::test]
async fn concurrent_readers_during_swap() -> anyhow::Result<()> {
    let (_temp, shared) = shared_fixture()?;

    let mut handles = Vec::new();
    for _ in 0..10 {
        let shared = shared.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..100 {
                let snapshot = shared.load_full();
                let _ = snapshot.default_temperature;
                let _ = snapshot.default_model.as_ref().map(std::string::String::len);
            }
        }));
    }

    // Swap while readers are running
    for i in 0..5 {
        publish_temperature(&shared, 0.5 + f64::from(i) / 10.0)?;
        tokio::task::yield_now().await;
    }

    for h in handles {
        h.await?;
    }

    assert!((shared.load_full().default_temperature - 0.9).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn shared_config_clone_shares_same_cell() -> anyhow::Result<()> {
    let (_temp, shared) = shared_fixture()?;
    let clone = shared.clone();

    publish_temperature(&clone, 1.1)?;

    assert!((shared.load_full().default_temperature - 1.1).abs() < f64::EPSILON);
    assert!((clone.load_full().default_temperature - 1.1).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn config_default_roundtrip_via_toml() -> anyhow::Result<()> {
    let config = Config::default();
    let toml_str = toml::to_string_pretty(&config)?;
    let restored: Config = toml::from_str(&toml_str)?;
    assert!((restored.default_temperature - config.default_temperature).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn config_modified_fields_survive_toml_roundtrip() -> anyhow::Result<()> {
    let config = Config {
        default_provider: Some("deepseek".to_string()),
        default_model: Some("ds-v3".to_string()),
        default_temperature: 0.42,
        ..Config::default()
    };

    let toml_str = toml::to_string_pretty(&config)?;
    let restored: Config = toml::from_str(&toml_str)?;
    assert_eq!(restored.default_provider.as_deref(), Some("deepseek"));
    assert_eq!(restored.default_model.as_deref(), Some("ds-v3"));
    assert!((restored.default_temperature - 0.42).abs() < f64::EPSILON);
    Ok(())
}
