//! Host state and host function implementations for WASM plugins.
//!
//! `HostState` is the per-instance state stored in each wasmtime `Store`.
//! Host functions provide plugins with controlled access to logging, config,
//! KV storage, memory system, and HTTP requests.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::memory::traits::Memory;
#[cfg(feature = "wasm-plugins")]
use crate::plugins::event_bus::EventBus;

/// Per-plugin-instance state stored in the wasmtime `Store<HostState>`.
///
/// Each loaded plugin gets its own `HostState` with isolated KV namespace,
/// permission enforcement, and a copy of its configuration.
pub struct HostState {
    /// Unique plugin identifier.
    pub plugin_name: String,

    /// Plugin-specific configuration (from plugin.toml `[config]` section).
    pub config: HashMap<String, String>,

    /// Granted permissions (checked on every host function call).
    pub granted_permissions: HashSet<String>,

    /// Optional permissions the plugin can request at runtime.
    pub optional_permissions: HashSet<String>,

    /// HTTP URL allowlist patterns (for http-outbound permission).
    pub http_allowlist: Vec<String>,

    /// Namespaced in-memory KV store (plugin-isolated).
    pub kv_store: Arc<RwLock<HashMap<String, Vec<u8>>>>,

    /// Resource limits.
    pub timeout_ms: u64,

    /// Memory backend reference for prx:host/memory host functions.
    pub memory: Option<Arc<dyn Memory>>,

    /// Event bus reference for prx:host/events host functions.
    #[cfg(feature = "wasm-plugins")]
    pub event_bus: Option<Arc<EventBus>>,
}

impl HostState {
    /// Create a new `HostState` for a plugin.
    /// Create a new `HostState` for a plugin.
    pub fn new(
        plugin_name: String,
        config: HashMap<String, String>,
        granted_permissions: HashSet<String>,
        optional_permissions: HashSet<String>,
        http_allowlist: Vec<String>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            plugin_name,
            config,
            granted_permissions,
            optional_permissions,
            http_allowlist,
            kv_store: Arc::new(RwLock::new(HashMap::new())),
            timeout_ms,
            memory: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        }
    }

    /// Create a new `HostState` with a memory backend reference.
    pub fn with_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Inject an event bus reference into this host state.
    #[cfg(feature = "wasm-plugins")]
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Check if a permission is granted. Returns an error if denied.
    pub fn check_permission(&self, interface: &str) -> Result<(), String> {
        // Basic interfaces are always allowed.
        if matches!(interface, "log" | "config" | "clock") {
            return Ok(());
        }
        if self.granted_permissions.contains(interface) {
            Ok(())
        } else if self.optional_permissions.contains(interface) {
            Err(format!(
                "permission '{interface}' not yet granted for plugin '{}' (optional, needs approval)",
                self.plugin_name
            ))
        } else {
            Err(format!(
                "permission '{interface}' denied for plugin '{}'",
                self.plugin_name
            ))
        }
    }

    /// Check if a URL is allowed by the http_allowlist.
    pub fn check_url_allowed(&self, url: &str) -> bool {
        if self.http_allowlist.is_empty() {
            // No allowlist = all URLs allowed (if http-outbound permission is granted)
            return true;
        }
        for pattern in &self.http_allowlist {
            if pattern.ends_with('*') {
                let prefix = &pattern[..pattern.len() - 1];
                if url.starts_with(prefix) {
                    return true;
                }
            } else if url == pattern {
                return true;
            }
        }
        false
    }
}

// ── Host function implementations ──

/// Log a message on behalf of a plugin.
/// Maps to `prx:host/log.log(level, message)`.
pub fn host_log(state: &HostState, level: &str, message: &str) {
    match level {
        "trace" => tracing::trace!(plugin = %state.plugin_name, "{message}"),
        "debug" => tracing::debug!(plugin = %state.plugin_name, "{message}"),
        "info" => tracing::info!(plugin = %state.plugin_name, "{message}"),
        "warn" => tracing::warn!(plugin = %state.plugin_name, "{message}"),
        "error" => tracing::error!(plugin = %state.plugin_name, "{message}"),
        _ => tracing::info!(plugin = %state.plugin_name, level = level, "{message}"),
    }
}

/// Retrieve a config value for the plugin.
/// Maps to `prx:host/config.get(key)`.
pub fn host_config_get(state: &HostState, key: &str) -> Option<String> {
    state.config.get(key).cloned()
}

/// Get a value from the plugin's KV store.
/// Maps to `prx:host/kv.get(key)`.
pub async fn host_kv_get(state: &HostState, key: &str) -> Option<Vec<u8>> {
    let store = state.kv_store.read().await;
    store.get(key).cloned()
}

/// Set a value in the plugin's KV store.
/// Maps to `prx:host/kv.set(key, value)`.
pub async fn host_kv_set(state: &HostState, key: String, value: Vec<u8>) {
    let mut store = state.kv_store.write().await;
    store.insert(key, value);
}

/// Delete a value from the plugin's KV store.
/// Maps to `prx:host/kv.delete(key)`. Returns `true` if key existed.
pub async fn host_kv_delete(state: &HostState, key: &str) -> bool {
    let mut store = state.kv_store.write().await;
    store.remove(key).is_some()
}

/// List keys matching a prefix in the plugin's KV store.
/// Maps to `prx:host/kv.list-keys(prefix)`.
pub async fn host_kv_list_keys(state: &HostState, prefix: &str) -> Vec<String> {
    let store = state.kv_store.read().await;
    store
        .keys()
        .filter(|k| k.starts_with(prefix))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> HostState {
        let mut config = HashMap::new();
        config.insert("api_key".to_string(), "test-key".to_string());
        HostState::new(
            "test-plugin".to_string(),
            config,
            HashSet::from([
                "log".to_string(),
                "kv".to_string(),
                "http-outbound".to_string(),
            ]),
            HashSet::from(["llm".to_string()]),
            vec!["https://api.example.com/*".to_string()],
            30_000,
        )
    }

    #[test]
    fn host_state_creation() {
        let state = test_state();
        assert_eq!(state.plugin_name, "test-plugin");
        assert_eq!(
            host_config_get(&state, "api_key"),
            Some("test-key".to_string())
        );
        assert_eq!(host_config_get(&state, "missing"), None);
    }

    #[test]
    fn permission_check() {
        let state = test_state();
        // Always-allowed
        assert!(state.check_permission("log").is_ok());
        assert!(state.check_permission("config").is_ok());
        // Granted
        assert!(state.check_permission("kv").is_ok());
        assert!(state.check_permission("http-outbound").is_ok());
        // Optional but not granted
        assert!(state.check_permission("llm").is_err());
        // Not declared at all
        assert!(state.check_permission("browser").is_err());
    }

    #[test]
    fn url_allowlist_check() {
        let state = test_state();
        assert!(state.check_url_allowed("https://api.example.com/v1/data"));
        assert!(!state.check_url_allowed("https://evil.com/hack"));
    }

    #[test]
    fn url_allowlist_empty_allows_all() {
        let state = HostState::new(
            "test".to_string(),
            HashMap::new(),
            HashSet::new(),
            HashSet::new(),
            vec![],
            5000,
        );
        assert!(state.check_url_allowed("https://anything.com"));
    }

    #[test]
    fn host_log_levels() {
        let state = test_state();
        host_log(&state, "trace", "trace msg");
        host_log(&state, "debug", "debug msg");
        host_log(&state, "info", "info msg");
        host_log(&state, "warn", "warn msg");
        host_log(&state, "error", "error msg");
        host_log(&state, "unknown", "unknown level");
    }

    #[tokio::test]
    async fn kv_operations() {
        let state = test_state();
        assert_eq!(host_kv_get(&state, "key1").await, None);

        host_kv_set(&state, "key1".to_string(), b"value1".to_vec()).await;
        assert_eq!(host_kv_get(&state, "key1").await, Some(b"value1".to_vec()));

        let deleted = host_kv_delete(&state, "key1").await;
        assert!(deleted);
        assert_eq!(host_kv_get(&state, "key1").await, None);
    }

    #[tokio::test]
    async fn kv_list_keys_with_prefix() {
        let state = test_state();
        host_kv_set(&state, "weather:tokyo".to_string(), b"sunny".to_vec()).await;
        host_kv_set(&state, "weather:london".to_string(), b"rainy".to_vec()).await;
        host_kv_set(&state, "config:api_key".to_string(), b"key".to_vec()).await;

        let mut keys = host_kv_list_keys(&state, "weather:").await;
        keys.sort();
        assert_eq!(keys, vec!["weather:london", "weather:tokyo"]);
    }
}
