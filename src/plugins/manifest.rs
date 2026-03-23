//! Plugin manifest (`plugin.toml`) parsing.
//!
//! Aligned with spec: supports `[permissions]` with `required`/`optional` lists,
//! `http_allowlist`, and `[resources]` for execution limits.

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
    #[serde(default)]
    pub resources: Resources,
    /// Plugin-specific config key-value pairs, injected via `prx:host/config`.
    #[serde(default)]
    pub config: std::collections::HashMap<String, String>,
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

/// A declared capability (tool, middleware, hook, cron, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct Capability {
    #[serde(rename = "type")]
    pub capability_type: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Priority for middleware ordering (lower = higher priority). Default: 100.
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// List of events this hook listens to (for hook capabilities).
    #[serde(default)]
    pub events: Vec<String>,
    /// Cron schedule expression (for cron capabilities), 5-field format.
    #[serde(default)]
    pub schedule: Option<String>,
}

fn default_priority() -> i32 {
    100
}

/// Permission declarations (spec-aligned: interface-based, not boolean).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Permissions {
    /// Required host interfaces (must be granted for plugin to load).
    #[serde(default)]
    pub required: Vec<String>,
    /// Optional host interfaces (can be requested at runtime).
    #[serde(default)]
    pub optional: Vec<String>,
    /// HTTP outbound URL whitelist patterns.
    #[serde(default)]
    pub http_allowlist: Vec<String>,
    /// Filesystem path whitelist patterns.
    #[serde(default)]
    pub filesystem_allowlist: Vec<String>,
}

/// Resource limits for the plugin sandbox.
#[derive(Debug, Clone, Deserialize)]
pub struct Resources {
    /// wasmtime fuel upper bound.
    #[serde(default = "default_max_fuel")]
    pub max_fuel: u64,
    /// Linear memory cap in MB.
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: u64,
    /// Per-call timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub max_execution_time_ms: u64,
    /// Max HTTP requests per single call.
    #[serde(default = "default_max_http_requests")]
    pub max_http_requests_per_call: u32,
    /// KV storage cap in MB.
    #[serde(default = "default_max_kv_storage_mb")]
    pub max_kv_storage_mb: u64,
    /// Instance pool size — number of warm instances to maintain.
    /// 0 means no pooling (default). Reserved for future use.
    #[serde(default)]
    pub pool_size: usize,
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            max_fuel: default_max_fuel(),
            max_memory_mb: default_max_memory_mb(),
            max_execution_time_ms: default_timeout_ms(),
            max_http_requests_per_call: default_max_http_requests(),
            max_kv_storage_mb: default_max_kv_storage_mb(),
            pool_size: 0,
        }
    }
}

fn default_api_version() -> String {
    "1".to_string()
}

fn default_wasm_path() -> String {
    "plugin.wasm".to_string()
}

fn default_max_fuel() -> u64 {
    1_000_000_000
}

fn default_max_memory_mb() -> u64 {
    64
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn default_max_http_requests() -> u32 {
    10
}

fn default_max_kv_storage_mb() -> u64 {
    10
}

impl PluginManifest {
    /// Parse a `plugin.toml` from a file path.
    pub fn from_file(path: &Path) -> PluginResult<Self> {
        let content = std::fs::read_to_string(path).map_err(PluginError::Io)?;
        let manifest: Self = toml::from_str(&content).map_err(|e| PluginError::ManifestParse {
            path: path.display().to_string(),
            source: e,
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Basic validation of required fields.
    fn validate(&self) -> PluginResult<()> {
        if self.plugin.name.is_empty() {
            return Err(PluginError::Manifest("plugin name cannot be empty".to_string()));
        }
        if self.plugin.version.is_empty() {
            return Err(PluginError::Manifest("plugin version cannot be empty".to_string()));
        }
        Ok(())
    }

    /// Check if this manifest declares a specific capability type.
    pub fn has_capability(&self, cap_type: &str) -> bool {
        self.capabilities.iter().any(|c| c.capability_type == cap_type)
    }

    /// Get all capabilities of a specific type.
    pub fn capabilities_of_type(&self, cap_type: &str) -> Vec<&Capability> {
        self.capabilities
            .iter()
            .filter(|c| c.capability_type == cap_type)
            .collect()
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
name = "weather-tool"
version = "1.0.0"
description = "Get weather forecasts"
author = "community"

[[capabilities]]
type = "tool"
name = "weather_lookup"
description = "Look up weather by city"

[permissions]
required = ["log", "config", "kv", "http-outbound", "clock"]
optional = ["messaging", "llm"]
http_allowlist = ["https://api.openweathermap.org/*", "https://wttr.in/*"]

[resources]
max_fuel = 1000000000
max_memory_mb = 64
max_execution_time_ms = 30000
max_http_requests_per_call = 10
max_kv_storage_mb = 10

[config]
api_key = "test-key"
default_units = "metric"
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.plugin.name, "weather-tool");
        assert_eq!(manifest.capabilities.len(), 1);
        assert_eq!(manifest.capabilities[0].capability_type, "tool");
        assert_eq!(manifest.permissions.required.len(), 5);
        assert!(manifest.permissions.required.contains(&"log".to_string()));
        assert_eq!(manifest.permissions.optional.len(), 2);
        assert_eq!(manifest.permissions.http_allowlist.len(), 2);
        assert_eq!(manifest.resources.max_fuel, 1_000_000_000);
        assert_eq!(manifest.config.get("api_key"), Some(&"test-key".to_string()));
    }

    #[test]
    fn has_capability_works() {
        let toml_str = r#"
[plugin]
name = "multi"
version = "0.1.0"

[[capabilities]]
type = "tool"
name = "my_tool"

[[capabilities]]
type = "hook"
name = "my_hook"
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        assert!(manifest.has_capability("tool"));
        assert!(manifest.has_capability("hook"));
        assert!(!manifest.has_capability("channel"));
    }

    #[test]
    fn parse_provider_capability_manifest() {
        let toml_str = r#"
[plugin]
name = "my-llm-provider"
version = "0.2.0"
description = "Custom LLM provider"
author = "team"

[[capabilities]]
type = "provider"
name = "my-llm"
description = "My custom LLM backend"

[permissions]
required = ["log", "http-outbound"]
http_allowlist = ["https://api.example.com/*"]

[resources]
max_execution_time_ms = 60000

[config]
api_endpoint = "https://api.example.com/v1"
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.plugin.name, "my-llm-provider");
        assert!(manifest.has_capability("provider"));
        let providers = manifest.capabilities_of_type("provider");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "my-llm");
        assert_eq!(manifest.resources.max_execution_time_ms, 60000);
        assert_eq!(
            manifest.config.get("api_endpoint"),
            Some(&"https://api.example.com/v1".to_string())
        );
    }

    #[test]
    fn parse_storage_capability_manifest() {
        let toml_str = r#"
[plugin]
name = "redis-storage"
version = "1.0.0"
description = "Redis memory backend"

[[capabilities]]
type = "storage"
name = "redis"
description = "Redis-backed persistent memory"

[permissions]
required = ["log", "config", "http-outbound"]

[resources]
max_execution_time_ms = 5000
max_kv_storage_mb = 100

[config]
redis_url = "redis://localhost:6379"
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.plugin.name, "redis-storage");
        assert!(manifest.has_capability("storage"));
        let storage_caps = manifest.capabilities_of_type("storage");
        assert_eq!(storage_caps.len(), 1);
        assert_eq!(storage_caps[0].name, "redis");
        assert_eq!(manifest.resources.max_execution_time_ms, 5000);
        assert_eq!(manifest.resources.max_kv_storage_mb, 100);
        assert_eq!(
            manifest.config.get("redis_url"),
            Some(&"redis://localhost:6379".to_string())
        );
    }

    #[test]
    fn invalid_manifest_missing_name_fails_validation() {
        let toml_str = r#"
[plugin]
name = ""
version = "0.1.0"
"#;
        // Parse succeeds, but validate() should reject empty name.
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        let result = manifest.validate();
        assert!(result.is_err(), "empty plugin name should fail validation");
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("name"), "error should mention name: {err}");
    }

    #[test]
    fn invalid_manifest_missing_version_fails_validation() {
        let toml_str = r#"
[plugin]
name = "my-plugin"
version = ""
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        let result = manifest.validate();
        assert!(result.is_err(), "empty plugin version should fail validation");
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("version"), "error should mention version: {err}");
    }

    #[test]
    fn pool_size_field_parsing() {
        let toml_str = r#"
[plugin]
name = "pooled-plugin"
version = "0.1.0"

[resources]
pool_size = 4
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.resources.pool_size, 4);
    }

    #[test]
    fn pool_size_defaults_to_zero() {
        let toml_str = r#"
[plugin]
name = "no-pool"
version = "0.1.0"
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.resources.pool_size, 0, "pool_size should default to 0");
    }

    #[test]
    fn capabilities_of_type_filters_correctly() {
        let toml_str = r#"
[plugin]
name = "multi-cap"
version = "0.1.0"

[[capabilities]]
type = "tool"
name = "tool-a"

[[capabilities]]
type = "tool"
name = "tool-b"

[[capabilities]]
type = "middleware"
name = "auth-middleware"
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        let tools = manifest.capabilities_of_type("tool");
        assert_eq!(tools.len(), 2);
        let middlewares = manifest.capabilities_of_type("middleware");
        assert_eq!(middlewares.len(), 1);
        assert_eq!(middlewares[0].name, "auth-middleware");
        let hooks = manifest.capabilities_of_type("hook");
        assert!(hooks.is_empty());
    }

    #[test]
    fn cron_capability_with_schedule() {
        let toml_str = r#"
[plugin]
name = "scheduler"
version = "0.1.0"

[[capabilities]]
type = "cron"
name = "daily-cleanup"
description = "Run cleanup daily"
schedule = "0 0 * * *"
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        let cron_caps = manifest.capabilities_of_type("cron");
        assert_eq!(cron_caps.len(), 1);
        assert_eq!(cron_caps[0].schedule.as_deref(), Some("0 0 * * *"));
    }

    #[test]
    fn hook_capability_with_events() {
        let toml_str = r#"
[plugin]
name = "hook-plugin"
version = "0.1.0"

[[capabilities]]
type = "hook"
name = "message-hook"
events = ["message.received", "message.sent"]
"#;
        let manifest: PluginManifest = toml::from_str(toml_str).unwrap();
        let hooks = manifest.capabilities_of_type("hook");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].events.len(), 2);
        assert!(hooks[0].events.contains(&"message.received".to_string()));
    }
}
