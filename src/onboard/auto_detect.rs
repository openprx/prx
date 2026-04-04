//! Zero-configuration credential detection for `prx go`.
//!
//! Resolves provider, API key, and model from file-based sources only.
//! Environment variables are NOT read for API keys.
//!
//! Priority:
//! 1. Explicit `--key` parameter + prefix-based provider inference
//! 2. `auth-profiles.json` (managed by `prx auth`)
//! 3. `~/.openprx/config.toml` `api_key` field
//! 4. `~/.claude/.credentials.json` (Claude Code OAuth)

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Default model for each provider when no explicit model is given.
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-20250514";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o";
const DEFAULT_OPENROUTER_MODEL: &str = "anthropic/claude-sonnet-4";

/// Detect provider, API key, and model from file-based sources.
///
/// Does NOT read environment variables for API keys.
///
/// Returns `(provider, api_key, model)` on success.
pub fn detect_credentials(
    explicit_key: Option<&str>,
    explicit_provider: Option<&str>,
    explicit_model: Option<&str>,
) -> Result<(String, String, String)> {
    // ── 1. Explicit key ────────────────────────────────────────────
    if let Some(key) = explicit_key.map(str::trim).filter(|k| !k.is_empty()) {
        let provider = explicit_provider
            .map(ToString::to_string)
            .unwrap_or_else(|| infer_provider_from_key(key));
        let model = explicit_model
            .map(ToString::to_string)
            .unwrap_or_else(|| default_model_for_provider(&provider));
        return Ok((provider, key.to_string(), model));
    }

    // ── 2. auth-profiles.json ──────────────────────────────────────
    if let Some((provider, key)) = try_auth_profiles() {
        let resolved_provider = explicit_provider.map(ToString::to_string).unwrap_or(provider);
        let model = explicit_model
            .map(ToString::to_string)
            .unwrap_or_else(|| default_model_for_provider(&resolved_provider));
        return Ok((resolved_provider, key, model));
    }

    // ── 3. config.toml api_key field ───────────────────────────────
    if let Some((provider, key)) = try_config_toml() {
        let resolved_provider = explicit_provider.map(ToString::to_string).unwrap_or(provider);
        let model = explicit_model
            .map(ToString::to_string)
            .unwrap_or_else(|| default_model_for_provider(&resolved_provider));
        return Ok((resolved_provider, key, model));
    }

    // ── 4. Claude Code OAuth (~/.claude/.credentials.json) ─────────
    if let Some(key) = try_claude_code_oauth() {
        let provider = explicit_provider
            .map(ToString::to_string)
            .unwrap_or_else(|| "anthropic".to_string());
        let model = explicit_model
            .map(ToString::to_string)
            .unwrap_or_else(|| default_model_for_provider(&provider));
        return Ok((provider, key, model));
    }

    anyhow::bail!(
        "No API key found. Provide one with: prx go -k <your-api-key>\n\
         Or configure credentials with: prx auth paste-token --provider anthropic"
    )
}

/// Infer provider from the API key prefix.
fn infer_provider_from_key(key: &str) -> String {
    if key.starts_with("sk-ant-") {
        "anthropic".to_string()
    } else if key.starts_with("sk-") {
        "openai".to_string()
    } else {
        "openrouter".to_string()
    }
}

/// Default model for a given provider.
fn default_model_for_provider(provider: &str) -> String {
    match provider {
        "anthropic" | "claude" | "claude-code" => DEFAULT_ANTHROPIC_MODEL.to_string(),
        "openai" => DEFAULT_OPENAI_MODEL.to_string(),
        _ => DEFAULT_OPENROUTER_MODEL.to_string(),
    }
}

/// Resolve the OpenPRX config directory (same logic as config::schema).
fn openprx_config_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().join(".openprx"))
}

/// Try reading a token from auth-profiles.json.
///
/// Scans for any active profile that has a non-empty token or access_token.
/// Returns `(provider, api_key)` on success.
fn try_auth_profiles() -> Option<(String, String)> {
    let config_dir = openprx_config_dir()?;
    let profiles_path = config_dir.join("auth-profiles.json");
    if !profiles_path.exists() {
        return None;
    }

    let bytes = std::fs::read(&profiles_path).ok()?;
    let data: serde_json::Value = serde_json::from_slice(&bytes).ok()?;

    let active_profiles = data.get("active_profiles")?.as_object()?;
    let profiles = data.get("profiles")?.as_object()?;

    // Prefer active profiles in a deterministic order
    for preferred_provider in &["anthropic", "openai", "openrouter", "openai-codex"] {
        if let Some(profile_id) = active_profiles.get(*preferred_provider).and_then(|v| v.as_str()) {
            if let Some(token) = extract_token_from_profile(profiles, profile_id) {
                return Some((preferred_provider.to_string(), token));
            }
        }
    }

    // Fall back to any active profile
    for (provider, profile_id_val) in active_profiles {
        let Some(profile_id) = profile_id_val.as_str() else {
            continue;
        };
        if let Some(token) = extract_token_from_profile(profiles, profile_id) {
            return Some((provider.clone(), token));
        }
    }

    None
}

/// Extract a non-empty token from a profile JSON object.
fn extract_token_from_profile(
    profiles: &serde_json::Map<String, serde_json::Value>,
    profile_id: &str,
) -> Option<String> {
    let profile = profiles.get(profile_id)?;

    // Direct token field (Token kind)
    if let Some(token) = profile
        .get("token")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Some(token.to_string());
    }

    // OAuth token_set (OAuth kind)
    if let Some(access_token) = profile
        .get("token_set")
        .and_then(|ts| ts.get("access_token"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Some(access_token.to_string());
    }

    None
}

/// Try reading api_key + default_provider from config.toml.
fn try_config_toml() -> Option<(String, String)> {
    let config_dir = openprx_config_dir()?;
    let config_path = config_dir.join("config.toml");
    if !config_path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&config_path).ok()?;
    let parsed: toml::Value = toml::from_str(&content).ok()?;
    let table = parsed.as_table()?;

    let api_key = table
        .get("api_key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|k| !k.is_empty())?;

    // Skip encrypted values (enc: or enc2: prefix)
    if api_key.starts_with("enc:") || api_key.starts_with("enc2:") {
        return try_config_toml_with_decryption(&config_dir, &config_path);
    }

    let provider = table
        .get("default_provider")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or("openrouter");

    Some((provider.to_string(), api_key.to_string()))
}

/// Try reading config.toml with secret decryption via SecretStore.
fn try_config_toml_with_decryption(config_dir: &Path, config_path: &Path) -> Option<(String, String)> {
    let content = std::fs::read_to_string(config_path).ok()?;
    let parsed: toml::Value = toml::from_str(&content).ok()?;
    let table = parsed.as_table()?;

    let encrypted_key = table
        .get("api_key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|k| !k.is_empty())?;

    let encrypt_enabled = table
        .get("secrets")
        .and_then(|v| v.get("encrypt"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let store = crate::security::SecretStore::new(config_dir, encrypt_enabled);
    let decrypted = store.decrypt(encrypted_key).ok()?;
    let decrypted = decrypted.trim();
    if decrypted.is_empty() {
        return None;
    }

    let provider = table
        .get("default_provider")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or("openrouter");

    Some((provider.to_string(), decrypted.to_string()))
}

/// Try reading Claude Code OAuth credentials from ~/.claude/.credentials.json.
///
/// Only returns tokens that can be used as plain API keys (e.g. `sk-ant-api03-`).
/// OAuth setup tokens (`sk-ant-oat01-`) require a full OAuth flow with refresh_token
/// and Bearer auth, which the `detect_credentials` caller does not support.
/// Those tokens are handled separately by `resolve_claude_code_context` in the
/// provider layer when the provider is explicitly set to "claude-code".
fn try_claude_code_oauth() -> Option<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))?;

    let cred_path = home.join(".claude").join(".credentials.json");
    if !cred_path.exists() {
        return None;
    }

    let bytes = std::fs::read(&cred_path).ok()?;
    let file: serde_json::Value = serde_json::from_slice(&bytes).ok()?;

    let oauth = file.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())?;

    // OAuth setup tokens (sk-ant-oat01-) cannot be used as plain API keys.
    // They require Bearer auth + refresh_token flow, which is only supported
    // via the claude-code provider path (resolve_claude_code_context).
    // Passing them as a plain credential to the anthropic provider would send
    // them via x-api-key header, resulting in 400/401 errors.
    if is_claude_code_oauth_setup_token(access_token) {
        tracing::debug!(
            "Skipping Claude Code OAuth setup token from credentials.json \
             (not usable as plain API key)"
        );
        return None;
    }

    Some(access_token.to_string())
}

/// Returns `true` if the token is a Claude Code OAuth setup token that cannot
/// be used as a plain Anthropic API key.
pub(crate) fn is_claude_code_oauth_setup_token(token: &str) -> bool {
    token.starts_with("sk-ant-oat01-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_anthropic_from_sk_ant_prefix() {
        assert_eq!(infer_provider_from_key("sk-ant-api03-xyz"), "anthropic");
    }

    #[test]
    fn infer_openai_from_sk_prefix() {
        assert_eq!(infer_provider_from_key("sk-proj-abc123"), "openai");
    }

    #[test]
    fn infer_openrouter_from_unknown_prefix() {
        assert_eq!(infer_provider_from_key("or-v1-something"), "openrouter");
    }

    #[test]
    fn default_models_match_expected() {
        assert_eq!(default_model_for_provider("anthropic"), DEFAULT_ANTHROPIC_MODEL);
        assert_eq!(default_model_for_provider("openai"), DEFAULT_OPENAI_MODEL);
        assert_eq!(default_model_for_provider("openrouter"), DEFAULT_OPENROUTER_MODEL);
    }

    #[test]
    fn explicit_key_with_provider_override() {
        let result = detect_credentials(Some("sk-ant-test"), Some("openrouter"), Some("custom-model"));
        assert!(result.is_ok());
        let (provider, key, model) = result.expect("test: detect_credentials should succeed");
        assert_eq!(provider, "openrouter");
        assert_eq!(key, "sk-ant-test");
        assert_eq!(model, "custom-model");
    }

    #[test]
    fn explicit_key_infers_provider() {
        let result = detect_credentials(Some("sk-ant-foobar"), None, None);
        assert!(result.is_ok());
        let (provider, key, model) = result.expect("test: detect_credentials should succeed");
        assert_eq!(provider, "anthropic");
        assert_eq!(key, "sk-ant-foobar");
        assert_eq!(model, DEFAULT_ANTHROPIC_MODEL);
    }

    #[test]
    fn empty_key_falls_through() {
        // With empty explicit key and no files, should fail
        let result = detect_credentials(Some("  "), None, None);
        // This may succeed or fail depending on file system state,
        // but at minimum empty key should not be treated as explicit
        if let Ok((_, key, _)) = &result {
            assert!(!key.trim().is_empty());
        }
    }

    #[test]
    fn oauth_setup_token_detected() {
        assert!(is_claude_code_oauth_setup_token("sk-ant-oat01-abcdef123"));
        assert!(is_claude_code_oauth_setup_token("sk-ant-oat01-"));
    }

    #[test]
    fn regular_api_key_not_flagged_as_setup_token() {
        assert!(!is_claude_code_oauth_setup_token("sk-ant-api03-xyz"));
        assert!(!is_claude_code_oauth_setup_token("sk-proj-abc"));
        assert!(!is_claude_code_oauth_setup_token("or-v1-something"));
        assert!(!is_claude_code_oauth_setup_token(""));
    }
}
