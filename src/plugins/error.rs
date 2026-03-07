//! Plugin system error types.

use thiserror::Error;

/// Errors that can occur in the plugin system.
#[derive(Error, Debug)]
pub enum PluginError {
    #[error("manifest error: {0}")]
    Manifest(String),

    #[error("manifest parse error in {path}: {source}")]
    ManifestParse {
        path: String,
        source: toml::de::Error,
    },

    #[error("plugin '{name}' not found")]
    NotFound { name: String },

    #[error("plugin '{name}' already loaded")]
    AlreadyLoaded { name: String },

    #[error("WASM compilation error: {0}")]
    Compilation(String),

    #[error("WASM instantiation error: {0}")]
    Instantiation(String),

    #[error("plugin runtime error: {0}")]
    Runtime(String),

    #[error("permission denied: {permission}")]
    PermissionDenied { permission: String },

    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("plugin system not available (feature 'wasm-plugins' not enabled)")]
    NotAvailable,
}

pub type PluginResult<T> = std::result::Result<T, PluginError>;
