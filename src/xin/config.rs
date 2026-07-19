//! Configuration for the xin (心) autonomous task engine.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const fn default_interval_minutes() -> u32 {
    5
}
const fn default_max_concurrent() -> usize {
    4
}
const fn default_max_tasks() -> usize {
    128
}
const fn default_stale_timeout_minutes() -> u32 {
    60
}
/// Configuration for the xin (心) autonomous task engine (`[xin]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct XinConfig {
    /// Tick interval in minutes (minimum 1). Default: 5.
    #[serde(default = "default_interval_minutes")]
    pub interval_minutes: u32,

    /// Maximum concurrent task executions per tick. Default: 4.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,

    /// Maximum total tasks in the store. Default: 128.
    #[serde(default = "default_max_tasks")]
    pub max_tasks: usize,

    /// Minutes after which a running task is marked stale. Default: 60.
    #[serde(default = "default_stale_timeout_minutes")]
    pub stale_timeout_minutes: u32,

    /// Adopt orphaned, stale, non-recurring legacy tasks into lease-managed
    /// goal/step records on startup (FIX-P2-16). Default: false (zero-breakage).
    #[serde(default)]
    pub adopt_legacy_tasks: bool,
}

impl Default for XinConfig {
    fn default() -> Self {
        Self {
            interval_minutes: default_interval_minutes(),
            max_concurrent: default_max_concurrent(),
            max_tasks: default_max_tasks(),
            stale_timeout_minutes: default_stale_timeout_minutes(),
            adopt_legacy_tasks: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_safe_limits() {
        let cfg = XinConfig::default();
        assert_eq!(cfg.interval_minutes, 5);
        assert_eq!(cfg.max_concurrent, 4);
        assert_eq!(cfg.max_tasks, 128);
        assert_eq!(cfg.stale_timeout_minutes, 60);
        assert!(!cfg.adopt_legacy_tasks);
    }

    #[test]
    fn deserialize_minimal_toml() {
        let toml_str = "";
        let cfg: XinConfig = toml::from_str(toml_str).expect("parse minimal xin config");
        assert_eq!(cfg.interval_minutes, 5);
    }

    #[test]
    fn deserialize_full_toml() {
        let toml_str = r#"
            interval_minutes = 10
            max_concurrent = 8
            max_tasks = 256
            stale_timeout_minutes = 120
        "#;
        let cfg: XinConfig = toml::from_str(toml_str).expect("parse full xin config");
        assert_eq!(cfg.interval_minutes, 10);
        assert_eq!(cfg.max_concurrent, 8);
        assert_eq!(cfg.max_tasks, 256);
        assert_eq!(cfg.stale_timeout_minutes, 120);
    }
}
