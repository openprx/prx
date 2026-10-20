use super::{Deserialize, JsonSchema, PathBuf, Serialize, default_codex_auth_json_path, default_true};

// ── Secrets (encrypted credential store) ────────────────────────

/// Secrets encryption configuration (`[secrets]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecretsConfig {
    /// Enable encryption for API keys and tokens in config.toml
    #[serde(default = "default_true")]
    pub encrypt: bool,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self { encrypt: true }
    }
}

/// Authentication and external credential import configuration (`[auth]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AuthConfig {
    /// Automatically import `openai-codex` OAuth credentials from Codex CLI auth.json.
    #[serde(default = "default_true")]
    pub codex_auth_json_auto_import: bool,
    /// Source path for Codex CLI auth.json import.
    #[serde(default = "default_codex_auth_json_path")]
    pub codex_auth_json_path: PathBuf,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            codex_auth_json_auto_import: true,
            codex_auth_json_path: default_codex_auth_json_path(),
        }
    }
}
