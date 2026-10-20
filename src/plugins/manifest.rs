//! Plugin manifest (`plugin.toml`) parsing.
//!
//! Aligned with spec: supports `[permissions]` with `required`/`optional` lists,
//! `http_allowlist`, and `[resources]` for execution limits.

use serde::{Deserialize, Deserializer};
use std::path::Path;

use super::error::{PluginError, PluginResult};

pub const MAX_PLUGIN_MANIFEST_BYTES: u64 = 256 * 1024;

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
    #[serde(default, deserialize_with = "deserialize_event_patterns")]
    pub events: Vec<String>,
    /// Cron schedule expression (for cron capabilities), 5-field format.
    #[serde(default)]
    pub schedule: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EventPattern {
    Name(String),
    Detailed {
        pattern: String,
        #[serde(default)]
        _description: Option<String>,
    },
}

fn deserialize_event_patterns<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<EventPattern>::deserialize(deserializer).map(|patterns| {
        patterns
            .into_iter()
            .map(|pattern| match pattern {
                EventPattern::Name(name) => name,
                EventPattern::Detailed { pattern, .. } => pattern,
            })
            .collect()
    })
}

const fn default_priority() -> i32 {
    100
}

/// Permission declarations (spec-aligned: interface-based, not boolean).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Permissions {
    /// Required host interfaces. The operator's trust record must grant every
    /// one of them or the plugin is refused.
    #[serde(default)]
    pub required: Vec<String>,
    /// Optional host interfaces. They are granted only when the operator's
    /// trust record lists them; otherwise the plugin runs without them.
    #[serde(default)]
    pub optional: Vec<String>,
    /// Outbound URL patterns for `http-outbound` and `websocket-outbound`:
    /// `scheme://host[:port][/path-prefix][*]`, host optionally `*.parent`.
    /// Empty denies every URL.
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
    /// Optional cap on outbound HTTP/WebSocket requests in any rolling
    /// 60-second window, on top of the per-call cap. Absent means no window.
    #[serde(default)]
    pub max_http_requests_per_minute: Option<u32>,
    /// KV storage cap in MB.
    #[serde(default = "default_max_kv_storage_mb")]
    pub max_kv_storage_mb: u64,
    /// Instance pool size — number of warm instances to maintain.
    /// 0 means no pooling (default). Reserved for future use.
    #[serde(default)]
    pub pool_size: usize,
    /// Host-side cap on the payload handed to the guest in one call.
    /// Absent means "derive it from `max_memory_mb`".
    #[serde(default)]
    pub max_payload_bytes: Option<u64>,
}

/// Floor for the derived payload cap, so a small `max_memory_mb` cannot make a
/// plugin skip ordinary envelopes.
const MIN_PAYLOAD_CAP_BYTES: u64 = 64 * 1024;

impl Resources {
    /// Largest payload the host will hand to a guest in one call.
    ///
    /// A guest that parses its input into a DOM holds three live copies of the
    /// text at once — the canonical ABI copy of the argument, the parsed value
    /// and the serialized result — so the text has to stay well under the
    /// linear-memory cap. Above it the guest's `memory.grow` fails, which
    /// `trap_on_grow_failure` turns into a trap that poisons the whole store.
    /// An eighth of the cap leaves room for all three copies; an author whose
    /// guest streams instead of DOM-parsing can raise it with
    /// `max_payload_bytes`.
    pub fn payload_cap_bytes(&self) -> usize {
        let cap = self.max_payload_bytes.map_or_else(
            || (self.guest_memory_bytes() / 8).max(MIN_PAYLOAD_CAP_BYTES),
            |explicit| explicit.max(1),
        );
        usize::try_from(cap).unwrap_or(usize::MAX)
    }

    /// Linear memory the guest may grow to, in bytes.
    const fn guest_memory_bytes(&self) -> u64 {
        self.max_memory_mb.saturating_mul(1024).saturating_mul(1024)
    }

    /// Normalise an explicit `max_payload_bytes` so a manifest cannot configure
    /// a cap that is useless in one direction or dangerous in the other.
    ///
    /// `0` is rejected: it caps the payload at one byte, which skips every call
    /// for the adapter's whole life, and no author writes it meaning that. An
    /// author who wants every call skipped removes the capability instead.
    ///
    /// A cap above the guest's own linear memory is clamped rather than
    /// rejected. Such a cap re-arms the trap-rebuild-quarantine loop the
    /// host-side guard exists to close, but it was accepted before, so
    /// rejecting it would turn a tuning mistake into a plugin that no longer
    /// loads at all. Clamping keeps the plugin running with the footgun
    /// removed, and the warning names the manifest that has to be corrected.
    fn normalize_payload_cap(&mut self, plugin: &str) -> PluginResult<()> {
        let Some(explicit) = self.max_payload_bytes else {
            return Ok(());
        };
        if explicit == 0 {
            return Err(PluginError::Manifest(format!(
                "plugin '{plugin}' sets resources.max_payload_bytes = 0, which refuses every call \
                 into the plugin; remove the key to derive the cap from max_memory_mb"
            )));
        }
        let guest_bytes = self.guest_memory_bytes();
        // `max_memory_mb = 0` leaves nothing to clamp against and cannot
        // instantiate a guest at all, so it is left to fail where it is armed.
        if guest_bytes > 0 && explicit > guest_bytes {
            tracing::warn!(
                plugin,
                max_payload_bytes = explicit,
                max_memory_mb = self.max_memory_mb,
                clamped_to = guest_bytes,
                "resources.max_payload_bytes exceeds the guest linear-memory cap and was clamped to it"
            );
            self.max_payload_bytes = Some(guest_bytes);
        }
        Ok(())
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            max_fuel: default_max_fuel(),
            max_memory_mb: default_max_memory_mb(),
            max_execution_time_ms: default_timeout_ms(),
            max_http_requests_per_call: default_max_http_requests(),
            max_http_requests_per_minute: None,
            max_kv_storage_mb: default_max_kv_storage_mb(),
            pool_size: 0,
            max_payload_bytes: None,
        }
    }
}

fn default_api_version() -> String {
    "1".to_string()
}

fn default_wasm_path() -> String {
    "plugin.wasm".to_string()
}

const fn default_max_fuel() -> u64 {
    1_000_000_000
}

const fn default_max_memory_mb() -> u64 {
    64
}

const fn default_timeout_ms() -> u64 {
    30_000
}

const fn default_max_http_requests() -> u32 {
    10
}

const fn default_max_kv_storage_mb() -> u64 {
    10
}

impl PluginManifest {
    /// Parse a `plugin.toml` from a file path.
    pub fn from_file(path: &Path) -> PluginResult<Self> {
        Self::from_file_with_digest(path).map(|(manifest, _)| manifest)
    }

    /// Parse a `plugin.toml` and return the SHA-256 (lowercase hex) of the
    /// exact bytes that were parsed.
    pub fn from_file_with_digest(path: &Path) -> PluginResult<(Self, String)> {
        let metadata = std::fs::symlink_metadata(path).map_err(PluginError::Io)?;
        if metadata.file_type().is_symlink() {
            return Err(PluginError::Manifest(format!(
                "plugin manifest must not be a symlink: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_PLUGIN_MANIFEST_BYTES {
            return Err(PluginError::ResourceLimit(format!(
                "plugin manifest {} exceeds {MAX_PLUGIN_MANIFEST_BYTES} bytes",
                path.display()
            )));
        }
        let bytes = std::fs::read(path).map_err(PluginError::Io)?;
        if bytes.len() as u64 > MAX_PLUGIN_MANIFEST_BYTES {
            return Err(PluginError::ResourceLimit(format!(
                "plugin manifest {} exceeds {MAX_PLUGIN_MANIFEST_BYTES} bytes",
                path.display()
            )));
        }
        let content = std::str::from_utf8(&bytes)
            .map_err(|error| PluginError::Manifest(format!("plugin manifest is not UTF-8: {error}")))?;
        let mut manifest: Self = toml::from_str(&content).map_err(|e| PluginError::ManifestParse {
            path: path.display().to_string(),
            source: e,
        })?;
        manifest.validate()?;
        Ok((manifest, super::trust::sha256_hex(&bytes)))
    }

    /// Basic validation of required fields, plus the `[resources]`
    /// normalisation that keeps an explicit payload cap inside what the guest
    /// can actually hold.
    fn validate(&mut self) -> PluginResult<()> {
        if self.plugin.name.is_empty() {
            return Err(PluginError::Manifest("plugin name cannot be empty".to_string()));
        }
        if self.plugin.name.len() > 128
            || !self
                .plugin
                .name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        {
            return Err(PluginError::Manifest(
                "plugin name must be 1-128 ASCII letters, digits, '-' or '_'".to_string(),
            ));
        }
        if self.plugin.version.is_empty() {
            return Err(PluginError::Manifest("plugin version cannot be empty".to_string()));
        }
        for capability in &self.capabilities {
            if capability.name.trim().is_empty() {
                return Err(PluginError::Manifest(
                    "plugin capability name cannot be empty".to_string(),
                ));
            }
            if !matches!(
                capability.capability_type.as_str(),
                "tool" | "hook" | "middleware" | "cron" | "provider" | "storage"
            ) {
                return Err(PluginError::Manifest(format!(
                    "unsupported plugin capability type '{}'",
                    capability.capability_type
                )));
            }
            if capability.capability_type == "cron"
                && capability
                    .schedule
                    .as_deref()
                    .is_none_or(|schedule| schedule.trim().is_empty())
            {
                return Err(PluginError::Manifest(format!(
                    "cron capability '{}' requires a schedule",
                    capability.name
                )));
            }
            if capability.capability_type == "hook" && capability.events.iter().any(|event| event.trim().is_empty()) {
                return Err(PluginError::Manifest(format!(
                    "hook capability '{}' contains an empty event pattern",
                    capability.name
                )));
            }
        }
        if let Err(error) = super::outbound::OutboundAllowlist::parse_all(&self.permissions.http_allowlist) {
            return Err(PluginError::Manifest(format!(
                "plugin '{}' has an invalid permissions.http_allowlist entry: {error}",
                self.plugin.name
            )));
        }
        self.resources.normalize_payload_cap(&self.plugin.name)?;
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

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_cap_is_derived_from_guest_memory_and_manifest_overridable() {
        let derived = Resources {
            max_memory_mb: 8,
            ..Resources::default()
        };
        assert_eq!(derived.payload_cap_bytes(), 1024 * 1024);

        let tiny_guest = Resources {
            max_memory_mb: 1,
            ..Resources::default()
        };
        assert_eq!(tiny_guest.payload_cap_bytes(), 128 * 1024);

        let unset_guest_memory = Resources {
            max_memory_mb: 0,
            ..Resources::default()
        };
        assert_eq!(
            unset_guest_memory.payload_cap_bytes(),
            usize::try_from(MIN_PAYLOAD_CAP_BYTES).unwrap()
        );

        // An override the guest can hold is taken verbatim; `validate()` is what
        // clamps one it cannot, so the raw accessor never has to second-guess it.
        let overridden = Resources {
            max_memory_mb: 64,
            max_payload_bytes: Some(16 * 1024 * 1024),
            ..Resources::default()
        };
        assert_eq!(overridden.payload_cap_bytes(), 16 * 1024 * 1024);

        let parsed: PluginManifest = toml::from_str(
            r#"
[plugin]
name = "streaming"
version = "0.1.0"

[resources]
max_memory_mb = 8
max_payload_bytes = 32
"#,
        )
        .unwrap();
        assert_eq!(parsed.resources.payload_cap_bytes(), 32);
    }

    fn manifest_with_payload_cap(max_memory_mb: u64, max_payload_bytes: u64) -> PluginManifest {
        toml::from_str(&format!(
            r#"
[plugin]
name = "capped"
version = "0.1.0"

[resources]
max_memory_mb = {max_memory_mb}
max_payload_bytes = {max_payload_bytes}
"#
        ))
        .unwrap()
    }

    #[test]
    fn a_payload_cap_above_guest_memory_is_clamped_to_it() {
        let mut clamped = manifest_with_payload_cap(8, 16 * 1024 * 1024);
        clamped
            .validate()
            .expect("test: an over-large cap is clamped, not rejected");
        assert_eq!(clamped.resources.payload_cap_bytes(), 8 * 1024 * 1024);

        let mut kept = manifest_with_payload_cap(64, 16 * 1024 * 1024);
        kept.validate().expect("test: a cap the guest can hold is kept");
        assert_eq!(kept.resources.payload_cap_bytes(), 16 * 1024 * 1024);
    }

    #[test]
    fn a_zero_payload_cap_is_rejected() {
        let mut zero = manifest_with_payload_cap(8, 0);
        let error = zero
            .validate()
            .expect_err("test: a zero cap skips every call and must be rejected")
            .to_string();
        assert!(error.contains("max_payload_bytes"), "{error}");
    }

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
    fn hook_events_accept_strings_and_detailed_tables() {
        let string_manifest: PluginManifest = toml::from_str(
            r#"
[plugin]
name = "string-hook"
version = "0.1.0"

[[capabilities]]
type = "hook"
name = "audit"
events = ["turn_complete", "prx.lifecycle.tool_call"]
"#,
        )
        .unwrap();
        assert_eq!(
            string_manifest.capabilities[0].events,
            ["turn_complete", "prx.lifecycle.tool_call"]
        );

        let detailed_manifest = PluginManifest::from_file(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/audit-hook/plugin.toml"),
        )
        .expect("the documented audit hook manifest must be loadable");
        assert_eq!(detailed_manifest.capabilities[0].events, ["prx.lifecycle.*"]);
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
        let mut manifest: PluginManifest = toml::from_str(toml_str).unwrap();
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
        let mut manifest: PluginManifest = toml::from_str(toml_str).unwrap();
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

    #[test]
    fn from_file_rejects_oversized_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("plugin.toml");
        std::fs::write(&path, vec![b' '; MAX_PLUGIN_MANIFEST_BYTES as usize + 1]).unwrap();
        let error = PluginManifest::from_file(&path).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn from_file_rejects_symlink_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.toml");
        let link = temp.path().join("plugin.toml");
        std::fs::write(&target, "[plugin]\nname = \"x\"\nversion = \"1\"\n").unwrap();
        std::os::unix::fs::symlink(target, &link).unwrap();
        let error = PluginManifest::from_file(&link).unwrap_err();
        assert!(error.to_string().contains("symlink"));
    }
}
