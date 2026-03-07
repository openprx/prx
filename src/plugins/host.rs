//! Host state and basic host functions for WASM plugins.
//!
//! `HostState` is the per-instance state stored in each wasmtime `Store`.
//! It provides the plugin with access to logging, configuration, and KV storage.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Per-plugin-instance state stored in the wasmtime `Store<HostState>`.
///
/// Each loaded plugin gets its own `HostState` with isolated KV namespace
/// and a copy of its configuration.
pub struct HostState {
    /// Unique plugin identifier.
    pub plugin_name: String,

    /// Plugin-specific configuration (from config.toml `[plugins.<name>]` section).
    pub config: HashMap<String, String>,

    /// Namespaced in-memory KV store (plugin-isolated).
    ///
    /// In P1, this is a simple in-memory `HashMap` behind an `Arc<RwLock>`.
    /// P2 will migrate to a persistent SQLite-backed store.
    pub kv_store: Arc<RwLock<HashMap<String, Vec<u8>>>>,

    /// WASI context (reserved for P2 integration).
    /// Currently unused but declared for forward compatibility.
    _wasi_reserved: (),
}

impl HostState {
    /// Create a new `HostState` for a plugin.
    pub fn new(plugin_name: String, config: HashMap<String, String>) -> Self {
        Self {
            plugin_name,
            config,
            kv_store: Arc::new(RwLock::new(HashMap::new())),
            _wasi_reserved: (),
        }
    }
}

// ── Host function implementations (called by guest via WIT imports) ──

/// Log a message on behalf of a plugin.
///
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
///
/// Maps to `prx:host/config.get(key)`.
pub fn host_config_get(state: &HostState, key: &str) -> Option<String> {
    state.config.get(key).cloned()
}

/// Get a value from the plugin's KV store.
///
/// Maps to `prx:host/kv.get(key)`.
pub async fn host_kv_get(state: &HostState, key: &str) -> Option<Vec<u8>> {
    let store = state.kv_store.read().await;
    store.get(key).cloned()
}

/// Set a value in the plugin's KV store.
///
/// Maps to `prx:host/kv.set(key, value)`.
pub async fn host_kv_set(state: &HostState, key: String, value: Vec<u8>) {
    let mut store = state.kv_store.write().await;
    store.insert(key, value);
}

/// Delete a value from the plugin's KV store.
///
/// Maps to `prx:host/kv.delete(key)`. Returns `true` if key existed.
pub async fn host_kv_delete(state: &HostState, key: &str) -> bool {
    let mut store = state.kv_store.write().await;
    store.remove(key).is_some()
}

/// List keys matching a prefix in the plugin's KV store.
///
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

    #[test]
    fn host_state_creation() {
        let mut config = HashMap::new();
        config.insert("api_key".to_string(), "test-key".to_string());
        let state = HostState::new("test-plugin".to_string(), config);
        assert_eq!(state.plugin_name, "test-plugin");
        assert_eq!(
            host_config_get(&state, "api_key"),
            Some("test-key".to_string())
        );
        assert_eq!(host_config_get(&state, "missing"), None);
    }

    #[test]
    fn host_log_levels() {
        let state = HostState::new("test".to_string(), HashMap::new());
        // Should not panic for any level
        host_log(&state, "trace", "trace msg");
        host_log(&state, "debug", "debug msg");
        host_log(&state, "info", "info msg");
        host_log(&state, "warn", "warn msg");
        host_log(&state, "error", "error msg");
        host_log(&state, "unknown", "unknown level");
    }

    #[tokio::test]
    async fn kv_operations() {
        let state = HostState::new("test".to_string(), HashMap::new());
        assert_eq!(host_kv_get(&state, "key1").await, None);

        host_kv_set(&state, "key1".to_string(), b"value1".to_vec()).await;
        assert_eq!(host_kv_get(&state, "key1").await, Some(b"value1".to_vec()));

        let deleted = host_kv_delete(&state, "key1").await;
        assert!(deleted);
        assert_eq!(host_kv_get(&state, "key1").await, None);

        let deleted_again = host_kv_delete(&state, "key1").await;
        assert!(!deleted_again);
    }

    #[tokio::test]
    async fn kv_list_keys_with_prefix() {
        let state = HostState::new("test".to_string(), HashMap::new());
        host_kv_set(&state, "weather:tokyo".to_string(), b"sunny".to_vec()).await;
        host_kv_set(&state, "weather:london".to_string(), b"rainy".to_vec()).await;
        host_kv_set(&state, "config:api_key".to_string(), b"key".to_vec()).await;

        let mut keys = host_kv_list_keys(&state, "weather:").await;
        keys.sort();
        assert_eq!(keys, vec!["weather:london", "weather:tokyo"]);
    }
}
