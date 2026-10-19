//! `[plugins]`: operator switch for the WASM plugin runtime.
//!
//! The section is operator-owned. With `enabled = false` (the default) the
//! runtime never scans `<workspace>/plugins` and no plugin tool, provider,
//! memory backend, hook or middleware exists. Which plugins may run, and with
//! which permissions, is decided by the operator's trust file, not by this
//! section (see `crate::plugins::trust`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// WASM plugin runtime configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Load WASM plugins from `<workspace>/plugins`. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_are_off_by_default() {
        assert!(!PluginsConfig::default().enabled);
        assert!(!crate::config::Config::default().plugins.enabled);
    }

    #[test]
    fn the_section_parses_and_rejects_unknown_keys() {
        let parsed: PluginsConfig = toml::from_str("enabled = true").expect("test: parse");
        assert!(parsed.enabled);
        let empty: PluginsConfig = toml::from_str("").expect("test: parse empty");
        assert!(!empty.enabled);
        assert!(toml::from_str::<PluginsConfig>("trusted = []").is_err());
    }
}
