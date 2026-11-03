use super::{Config, capture_logs};

#[test]
fn removed_reply_policy_loads_once_and_disappears_from_saved_config_and_schema() -> anyhow::Result<()> {
    for section in [
        "",
        "[router.automix]",
        "[router.automix]\nunknown_key = true",
        "[router.automix]\nconfidence_threshold = -9\npremium_model_id = 'prx-premium'",
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        std::fs::write(&path, format!("default_temperature = 0.7\n{section}\n"))?;
        if !section.is_empty() {
            std::fs::create_dir(dir.path().join("config.d"))?;
            std::fs::write(
                dir.path().join("config.d/90-legacy.toml"),
                "[router.automix]\nfragment_key = 1\n",
            )?;
        }
        let (loaded, logs) = capture_logs(|| Config::load_from_path(&path, dir.path().join("workspace")));
        let loaded = loaded?;
        let warning = "router.automix was removed in 0.8.171; the section is ignored";
        assert_eq!(
            logs.lines()
                .filter(|line| line.contains("WARN") && line.contains(warning))
                .count(),
            usize::from(!section.is_empty()),
            "{logs}"
        );
        assert!(!loaded.to_stored_toml_string()?.contains("automix"));
        assert!(
            !serde_json::to_string(&schemars::schema_for!(Config))?
                .to_lowercase()
                .contains("automix")
        );
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "default_temperature = 0.7\n[router]\nunknown_key = true\n[router.automix]\nanything = true\n",
    )?;
    assert!(Config::load_from_path(&path, dir.path().join("workspace")).is_err());
    Ok(())
}
