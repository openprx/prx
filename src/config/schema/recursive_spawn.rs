//! Default-off recursive spawning policy and unchanged spawn defaults.
use super::SessionsSpawnConfig;

pub(super) const fn default_depth() -> u8 {
    1
}

pub(super) fn validate(config: &SessionsSpawnConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        !config.allow_recursive_delegation || config.max_recursive_depth > 0,
        "sessions_spawn.max_recursive_depth must be positive when recursive delegation is enabled"
    );
    Ok(())
}

impl Default for SessionsSpawnConfig {
    fn default() -> Self {
        Self {
            allow_recursive_delegation: false,
            max_recursive_depth: default_depth(),
            default_mode: super::default_sessions_spawn_mode(),
            process_memory_strategy: super::default_sessions_spawn_process_memory_strategy(),
            worker_workspace_root: None,
            cleanup_on_complete: super::default_sessions_spawn_cleanup_on_complete(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recursive_spawn_permission_is_default_off_and_requires_positive_depth() {
        let mut config: SessionsSpawnConfig = toml::from_str("").expect("legacy spawn configuration");
        assert!(!config.allow_recursive_delegation);
        assert_eq!(config.max_recursive_depth, 1);
        config.validate().unwrap();
        config.allow_recursive_delegation = true;
        config.validate().unwrap();
        config.max_recursive_depth = 0;
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("max_recursive_depth must be positive"));
    }
}
