//! Plugin manifest (`plugin.toml`) parsing.

use serde::Deserialize;
use std::path::Path;

use super::error::{PluginError, PluginResult};

/// Top-level plugin manifest parsed from `plugin.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub permissions: Permissions,
}

/// Core plugin metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    #[serde(default)]
    pub description: String,
    /// Path to the WASM component file, relative to the plugin directory.
    #[serde(default = "default_wasm_path")]
    pub wasm: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
}

/// A declared capability (tool, channel, hook, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct Capability {
    #[serde(rename = "type")]
    pub capability_type: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Permission declarations.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub filesystem: Vec<String>,
    #[serde(default)]
    pub memory: bool,
    #[serde(default)]
    pub browser: bool,
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: u64,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_api_version() -> String {
    "1".to_string()
}

fn default_wasm_path() -> String {
    "plugin.wasm".to_string()
}

fn default_max_memory_mb() -> u64 {
    64
}

fn default_timeout_ms() -> u64 {
    5000
}

impl PluginManifest {
    /// Parse a `plugin.toml` from a file path.
    pub fn from_file(path: &Path) -> PluginResult<Self> {
        let content = std::fs::read_to_string(path).map_err(PluginError::Io)?;
        let manifest: Self =
            toml::from_str(&content).map_err(|e| PluginError::ManifestParse {
                path: path.display().to_string(),
                source: e,
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Basic validation of required fields.
    fn validate(&self) -> PluginResult<()> {
        if self.plugin.name.is_empty() {
            return Err(PluginError::Manifest(
                "plugin name cannot be empty".to_string(),
            ));
        }
        if self.plugin.version.is_empty() {
            return Err(PluginError::Manifest(
                "plugin version cannot be empty".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_manifest() {
        let toml_str = r#"
[plugin]
name = "example"
version = "0.1.0"

[permissions]
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.plugin.name, "example");
        assert_eq!(manifest.plugin.version, "0.1.0");
        assert_eq!(manifest.plugin.api_version, "1");
        assert_eq!(manifest.plugin.wasm, "plugin.wasm");
    }

    #[test]
    fn parse_full_manifest() {
        let toml_str = r#"
[plugin]
name = "example"
version = "0.1.0"
api_version = "1"
description = "Example plugin"
wasm = "plugin.wasm"

[[capabilities]]
type = "tool"
name = "example_tool"
description = "An example tool"

[permissions]
network = false
filesystem = []
memory = false
browser = false
max_memory_mb = 64
timeout_ms = 5000
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.plugin.name, "example");
        assert_eq!(manifest.capabilities.len(), 1);
        assert_eq!(manifest.capabilities[0].capability_type, "tool");
        assert_eq!(manifest.capabilities[0].name, "example_tool");
        assert!(!manifest.permissions.network);
        assert_eq!(manifest.permissions.max_memory_mb, 64);
    }
}
