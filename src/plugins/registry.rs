//! Plugin registry — manages all loaded plugin instances.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::manifest::PluginManifest;

/// Status of a loaded plugin.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum PluginStatus {
    /// Plugin is loaded and ready.
    Active,
    /// Plugin failed to load or crashed.
    Error(String),
    /// Plugin is being unloaded.
    Unloading,
}

/// Summary information about a loaded plugin (returned by list operations).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub status: PluginStatus,
    pub permissions_required: Vec<String>,
    pub permissions_granted: Vec<String>,
}

/// A loaded plugin instance.
///
/// Holds the manifest, compiled component, and runtime status.
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub status: PluginStatus,
    /// Directory path where the plugin was loaded from.
    pub source_dir: std::path::PathBuf,
    /// Compiled wasmtime component (if WASM file was present).
    pub component: Option<wasmtime::component::Component>,
    /// Permissions that were granted to this plugin.
    pub granted_permissions: Vec<String>,
}

impl LoadedPlugin {
    /// Permissions that are safe to auto-grant without user approval.
    /// Sensitive permissions like network, filesystem, and shell access
    /// require explicit approval via the plugin trust configuration.
    const AUTO_GRANT_SAFE: &[&str] = &["log", "storage", "config", "kv"];

    /// Create a new loaded plugin entry.
    ///
    /// Only safe permissions (log, storage, config, kv) are auto-granted.
    /// Sensitive permissions (http-outbound, network, filesystem, shell) are
    /// denied by default and logged as warnings so administrators can review.
    pub fn new(
        manifest: PluginManifest,
        source_dir: std::path::PathBuf,
        component: Option<wasmtime::component::Component>,
    ) -> Self {
        let mut granted = Vec::new();
        let mut denied = Vec::new();

        for perm in &manifest.permissions.required {
            if Self::AUTO_GRANT_SAFE.contains(&perm.as_str()) {
                granted.push(perm.clone());
            } else {
                denied.push(perm.clone());
            }
        }

        if !denied.is_empty() {
            tracing::warn!(
                plugin = %manifest.plugin.name,
                denied_permissions = ?denied,
                granted_permissions = ?granted,
                "plugin requests sensitive permissions that are not auto-granted; \
                 add to trusted_plugins config to approve"
            );
        }

        Self {
            manifest,
            status: PluginStatus::Active,
            source_dir,
            component,
            granted_permissions: granted,
        }
    }

    /// Create a loaded plugin with explicit trust -- all requested permissions
    /// are granted. Use only for plugins listed in the trusted_plugins config.
    pub fn new_trusted(
        manifest: PluginManifest,
        source_dir: std::path::PathBuf,
        component: Option<wasmtime::component::Component>,
    ) -> Self {
        let granted = manifest.permissions.required.clone();
        if !granted.is_empty() {
            tracing::info!(
                plugin = %manifest.plugin.name,
                permissions = ?granted,
                "granting all permissions to trusted plugin"
            );
        }
        Self {
            manifest,
            status: PluginStatus::Active,
            source_dir,
            component,
            granted_permissions: granted,
        }
    }

    /// Convert to a summary `PluginInfo`.
    pub fn info(&self) -> PluginInfo {
        PluginInfo {
            name: self.manifest.plugin.name.clone(),
            version: self.manifest.plugin.version.clone(),
            description: self.manifest.plugin.description.clone(),
            capabilities: self
                .manifest
                .capabilities
                .iter()
                .map(|c| format!("{}:{}", c.capability_type, c.name))
                .collect(),
            status: self.status.clone(),
            permissions_required: self.manifest.permissions.required.clone(),
            permissions_granted: self.granted_permissions.clone(),
        }
    }
}

/// Thread-safe registry of all loaded plugins.
///
/// Uses `Arc<RwLock<...>>` so it can be shared across async tasks
/// and support concurrent reads with exclusive writes (for hot-reload).
#[derive(Clone)]
pub struct PluginRegistry {
    plugins: Arc<RwLock<HashMap<String, LoadedPlugin>>>,
}

impl PluginRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a loaded plugin. Returns an error if a plugin with the
    /// same name is already loaded.
    pub async fn register(&self, plugin: LoadedPlugin) -> Result<(), String> {
        let name = plugin.manifest.plugin.name.clone();
        let mut plugins = self.plugins.write().await;
        if plugins.contains_key(&name) {
            return Err(format!("plugin '{name}' already loaded"));
        }
        plugins.insert(name, plugin);
        Ok(())
    }

    /// Remove a plugin from the registry. Returns `true` if it existed.
    pub async fn unregister(&self, name: &str) -> bool {
        let mut plugins = self.plugins.write().await;
        plugins.remove(name).is_some()
    }

    /// Replace an existing plugin (for hot-reload).
    pub async fn replace(&self, plugin: LoadedPlugin) {
        let name = plugin.manifest.plugin.name.clone();
        let mut plugins = self.plugins.write().await;
        plugins.insert(name, plugin);
    }

    /// List all loaded plugins.
    pub async fn list(&self) -> Vec<PluginInfo> {
        let plugins = self.plugins.read().await;
        plugins.values().map(|p| p.info()).collect()
    }

    /// Check if a plugin with the given name is loaded.
    pub async fn contains(&self, name: &str) -> bool {
        let plugins = self.plugins.read().await;
        plugins.contains_key(name)
    }

    /// Get plugin info by name.
    pub async fn get_info(&self, name: &str) -> Option<PluginInfo> {
        let plugins = self.plugins.read().await;
        plugins.get(name).map(|p| p.info())
    }

    /// Get a reference to a plugin's manifest (read lock held).
    pub async fn get_manifest(&self, name: &str) -> Option<PluginManifest> {
        let plugins = self.plugins.read().await;
        plugins.get(name).map(|p| p.manifest.clone())
    }

    /// Get a plugin's source directory.
    pub async fn get_source_dir(&self, name: &str) -> Option<std::path::PathBuf> {
        let plugins = self.plugins.read().await;
        plugins.get(name).map(|p| p.source_dir.clone())
    }

    /// Get the compiled component for a plugin, if available.
    ///
    /// `Component` is `Clone` (ref-counted internally), so this is cheap.
    pub async fn get_component(&self, name: &str) -> Option<wasmtime::component::Component> {
        let plugins = self.plugins.read().await;
        plugins.get(name).and_then(|p| p.component.clone())
    }

    /// Get the granted (policy-filtered) permissions for a plugin.
    ///
    /// These are the permissions that passed through `LoadedPlugin::new()`
    /// security filtering, NOT the raw manifest permissions.
    pub async fn get_granted_permissions(&self, name: &str) -> Option<Vec<String>> {
        let plugins = self.plugins.read().await;
        plugins.get(name).map(|p| p.granted_permissions.clone())
    }

    /// Get the number of loaded plugins.
    pub async fn len(&self) -> usize {
        let plugins = self.plugins.read().await;
        plugins.len()
    }

    /// Returns `true` when no plugins are loaded.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::manifest::{Permissions, PluginManifest, PluginMeta, Resources};

    fn test_manifest(name: &str) -> PluginManifest {
        PluginManifest {
            plugin: PluginMeta {
                name: name.to_string(),
                version: "0.1.0".to_string(),
                api_version: "1".to_string(),
                description: "Test plugin".to_string(),
                wasm: "plugin.wasm".to_string(),
                author: None,
                license: None,
            },
            capabilities: vec![],
            permissions: Permissions::default(),
            resources: Resources::default(),
            config: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn register_and_list() {
        let registry = PluginRegistry::new();
        let plugin = LoadedPlugin::new(test_manifest("test"), "/tmp".into(), None);
        registry.register(plugin).await.unwrap();

        let list = registry.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test");
    }

    #[tokio::test]
    async fn duplicate_register_fails() {
        let registry = PluginRegistry::new();
        let p1 = LoadedPlugin::new(test_manifest("test"), "/tmp".into(), None);
        let p2 = LoadedPlugin::new(test_manifest("test"), "/tmp".into(), None);
        registry.register(p1).await.unwrap();
        assert!(registry.register(p2).await.is_err());
    }

    #[tokio::test]
    async fn unregister() {
        let registry = PluginRegistry::new();
        let plugin = LoadedPlugin::new(test_manifest("test"), "/tmp".into(), None);
        registry.register(plugin).await.unwrap();
        assert!(registry.unregister("test").await);
        assert!(!registry.unregister("test").await);
        assert_eq!(registry.len().await, 0);
    }

    #[tokio::test]
    async fn get_component_returns_none_for_unregistered() {
        let registry = PluginRegistry::new();
        let component = registry.get_component("not-registered").await;
        assert!(component.is_none(), "unregistered plugin should have no component");
    }

    #[tokio::test]
    async fn get_component_returns_none_when_no_component() {
        // LoadedPlugin created with component=None
        let registry = PluginRegistry::new();
        let plugin = LoadedPlugin::new(test_manifest("no-wasm"), "/tmp".into(), None);
        registry.register(plugin).await.unwrap();
        let component = registry.get_component("no-wasm").await;
        assert!(
            component.is_none(),
            "plugin loaded without component should return None"
        );
    }

    #[tokio::test]
    async fn get_info_returns_none_for_unregistered() {
        let registry = PluginRegistry::new();
        let info = registry.get_info("ghost").await;
        assert!(info.is_none());
    }

    #[tokio::test]
    async fn get_info_returns_correct_fields() {
        let registry = PluginRegistry::new();
        let mut manifest = test_manifest("info-test");
        manifest.plugin.description = "A test plugin".to_string();
        manifest.permissions.required = vec!["log".to_string(), "config".to_string()];

        let plugin = LoadedPlugin::new(manifest, "/opt/plugins/info-test".into(), None);
        registry.register(plugin).await.unwrap();

        let info = registry.get_info("info-test").await.expect("should find plugin");
        assert_eq!(info.name, "info-test");
        assert_eq!(info.version, "0.1.0");
        assert_eq!(info.description, "A test plugin");
        assert_eq!(info.status, PluginStatus::Active);
        assert!(info.permissions_required.contains(&"log".to_string()));
        assert!(info.permissions_granted.contains(&"log".to_string()));
    }

    #[tokio::test]
    async fn contains_returns_false_for_missing() {
        let registry = PluginRegistry::new();
        assert!(!registry.contains("absent").await);
    }

    #[tokio::test]
    async fn contains_returns_true_after_register() {
        let registry = PluginRegistry::new();
        let plugin = LoadedPlugin::new(test_manifest("present"), "/tmp".into(), None);
        registry.register(plugin).await.unwrap();
        assert!(registry.contains("present").await);
    }

    #[tokio::test]
    async fn replace_overwrites_existing_plugin() {
        let registry = PluginRegistry::new();
        let p1 = LoadedPlugin::new(test_manifest("hot-reload"), "/tmp/v1".into(), None);
        registry.register(p1).await.unwrap();

        // Replace with a new version (different source_dir to distinguish).
        let p2 = LoadedPlugin::new(test_manifest("hot-reload"), "/tmp/v2".into(), None);
        registry.replace(p2).await;

        // Should still have exactly 1 plugin.
        assert_eq!(registry.len().await, 1);

        // Source dir should reflect the new plugin.
        let source_dir = registry.get_source_dir("hot-reload").await.unwrap();
        assert_eq!(source_dir, std::path::PathBuf::from("/tmp/v2"));
    }

    #[tokio::test]
    async fn list_returns_all_registered_plugins() {
        let registry = PluginRegistry::new();
        for name in ["alpha", "beta", "gamma"] {
            let plugin = LoadedPlugin::new(test_manifest(name), "/tmp".into(), None);
            registry.register(plugin).await.unwrap();
        }
        let list = registry.list().await;
        assert_eq!(list.len(), 3);
        let names: std::collections::HashSet<_> = list.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains("alpha"));
        assert!(names.contains("beta"));
        assert!(names.contains("gamma"));
    }

    #[tokio::test]
    async fn loaded_plugin_info_reflects_capabilities() {
        let registry = PluginRegistry::new();
        let mut manifest = test_manifest("cap-plugin");
        manifest.capabilities = vec![crate::plugins::manifest::Capability {
            capability_type: "tool".to_string(),
            name: "my_tool".to_string(),
            description: "A tool".to_string(),
            priority: 100,
            events: vec![],
            schedule: None,
        }];
        let plugin = LoadedPlugin::new(manifest, "/tmp".into(), None);
        registry.register(plugin).await.unwrap();

        let info = registry.get_info("cap-plugin").await.unwrap();
        assert_eq!(info.capabilities, vec!["tool:my_tool"]);
    }
}
