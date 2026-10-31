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

    // ── 2-4. File-based credential stores ──────────────────────────
    let Some((provider, key)) = detect_stored_credential(explicit_provider) else {
        anyhow::bail!(
            "No API key found. Provide one with: prx go -k <your-api-key>\n\
             Or configure credentials with: prx auth paste-token --provider anthropic"
        );
    };

    let resolved_provider = bind_detected_provider(explicit_provider, &provider)?;
    let model = explicit_model
        .map(ToString::to_string)
        .unwrap_or_else(|| default_model_for_provider(&resolved_provider));
    Ok((resolved_provider, key, model))
}

/// Walk the file-based credential stores in their documented priority order.
///
/// Without `--provider` that order is the whole rule, unchanged.
///
/// With `--provider` the stores are asked for *that* provider's own credential
/// first, in the same order, because the requested provider is the one whose
/// credential is wanted: another provider's credential sitting earlier in the
/// order must not hide one the requested provider actually has. Only when the
/// requested provider has no credential of its own does the plain walk run,
/// and what it finds is then refused by `bind_detected_provider` rather than
/// rebound.
fn detect_stored_credential(explicit_provider: Option<&str>) -> Option<(String, String)> {
    if let Some(requested) = explicit_provider {
        if let Some(owned) = credential_owned_by(requested) {
            return Some(owned);
        }
    }

    try_auth_profiles()
        .or_else(try_config_toml)
        .or_else(|| try_claude_code_oauth().map(|key| ("anthropic".to_string(), key)))
}

/// The credential the requested provider owns, from whichever store records
/// it. Identity is decided by the same canonicalization the credential
/// resolver uses, so an alias of the requested provider is the requested
/// provider and a differently spelled endpoint URL is the same endpoint.
fn credential_owned_by(requested: &str) -> Option<(String, String)> {
    let requested_key = crate::providers::canonical_provider_key(requested);
    let owns = |candidate: &str| crate::providers::canonical_provider_key(candidate) == requested_key;

    if let Some(owned) = try_auth_profile_for(&requested_key) {
        return Some(owned);
    }

    // `config.toml` records one credential and the provider it is the default
    // for. A config with no `default_provider` resolves to `openrouter`, the
    // same literal the rest of provider name resolution uses, so a key of
    // unrecorded ownership is still not rebound onto an arbitrary endpoint.
    if let Some(owned) = try_config_toml().filter(|(provider, _)| owns(provider)) {
        return Some(owned);
    }

    if !owns("anthropic") {
        return None;
    }
    try_claude_code_oauth().map(|key| ("anthropic".to_string(), key))
}

/// Decide which provider a detected credential may be bound to.
///
/// A detected credential belongs to the provider it was stored for. When
/// `--provider` names that same provider — including through an alias or a
/// differently spelled endpoint URL — the requested spelling is kept. When it
/// names a different one, the credential is NOT rebound: it would be sent to
/// an endpoint that never issued it. There is no fallback, so the caller is
/// told what is missing instead of being silently pointed at the wrong key.
fn bind_detected_provider(explicit_provider: Option<&str>, detected_provider: &str) -> Result<String> {
    let Some(requested) = explicit_provider else {
        return Ok(detected_provider.to_string());
    };

    if crate::providers::canonical_provider_key(requested)
        == crate::providers::canonical_provider_key(detected_provider)
    {
        return Ok(requested.to_string());
    }

    let requested = crate::providers::redact_endpoint_userinfo(requested);
    anyhow::bail!(
        "no credential configured for `{requested}`. Provide one with: prx go -p {requested} -k <your-api-key>\n\
         Or configure credentials with: prx auth paste-token --provider {requested}"
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
    match crate::providers::catalog::ReleaseCatalog::embedded() {
        Ok(catalog) => catalog
            .default_model(provider)
            .or_else(|| {
                catalog
                    .models
                    .iter()
                    .find(|model| model.id == catalog.defaults.model)
                    .map(|model| model.api_model.as_str())
            })
            .map(str::to_owned)
            .unwrap_or_else(|| crate::providers::selection::default_model_for_unset().to_string()),
        Err(error) => {
            tracing::error!(error = %error, "embedded model catalog is invalid");
            crate::providers::selection::default_model_for_unset().to_string()
        }
    }
}

/// Resolve the OpenPRX config directory (same logic as config::schema).
///
/// A run scoped with `--config-dir` must detect credentials from its own
/// directory; `~/.openprx` stays the answer for an unscoped run, which is what
/// `prx go` relies on.
fn openprx_config_dir() -> Option<PathBuf> {
    if let Some(config_dir) = crate::config::process_config_dir() {
        return Some(config_dir);
    }
    crate::config::default_openprx_dir()
}

/// Read and parse `auth-profiles.json` from the scoped config directory.
fn load_auth_profiles() -> Option<serde_json::Value> {
    let config_dir = openprx_config_dir()?;
    let profiles_path = config_dir.join("auth-profiles.json");
    if !profiles_path.exists() {
        return None;
    }

    let bytes = std::fs::read(&profiles_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Try reading the active profile stored for one specific provider identity.
///
/// The store keys its active profiles by the provider each credential was
/// stored for, so the requested provider is found by identity — aliases and
/// endpoint spellings included — instead of by position in a preference list.
/// Returns `(provider, api_key)` on success.
fn try_auth_profile_for(requested_key: &str) -> Option<(String, String)> {
    let data = load_auth_profiles()?;
    let active_profiles = data.get("active_profiles")?.as_object()?;
    let profiles = data.get("profiles")?.as_object()?;

    for (provider, profile_id_val) in active_profiles {
        if crate::providers::canonical_provider_key(provider) != requested_key {
            continue;
        }
        let Some(profile_id) = profile_id_val.as_str() else {
            continue;
        };
        if let Some(token) = extract_token_from_profile(profiles, profile_id) {
            return Some((provider.clone(), token));
        }
    }

    None
}

/// Try reading a token from auth-profiles.json.
///
/// Scans for any active profile that has a non-empty token or access_token.
/// Returns `(provider, api_key)` on success.
fn try_auth_profiles() -> Option<(String, String)> {
    let data = load_auth_profiles()?;

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
        .map(str::to_owned)
        .unwrap_or_else(|| crate::providers::default_provider_for_unset().to_string());

    Some((provider, api_key.to_string()))
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
        .map(str::to_owned)
        .unwrap_or_else(|| crate::providers::default_provider_for_unset().to_string());

    Some((provider, decrypted.to_string()))
}

/// Try reading Claude Code OAuth credentials from ~/.claude/.credentials.json.
///
/// Only returns tokens that can be used as plain API keys (e.g. `sk-ant-api03-`).
/// OAuth setup tokens (`sk-ant-oat01-`) require a full OAuth flow with refresh_token
/// and Bearer auth, which the `detect_credentials` caller does not support.
/// Those tokens are handled separately by `resolve_claude_code_context` in the
/// provider layer when the provider is explicitly set to "claude-code".
fn try_claude_code_oauth() -> Option<String> {
    let cred_path = claude_code_credentials_path()?;
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

/// Where Claude Code keeps its OAuth credentials:
/// `~/.claude/.credentials.json`, or the test seam's file under `cfg(test)`.
fn claude_code_credentials_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = claude_credentials_for_tests::current() {
        return Some(path);
    }
    crate::config::user_home_dir().map(|home| home.join(".claude").join(".credentials.json"))
}

/// Test seam for the Claude Code credentials file (R-C1f G-3). `HOME` is
/// process-global and tests run in parallel, so a test points detection at
/// its own file here, on its own thread, instead of at the operator's.
#[cfg(test)]
mod claude_credentials_for_tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        static PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub(super) fn current() -> Option<PathBuf> {
        PATH.with(|path| path.borrow().clone())
    }

    pub(super) fn swap(path: Option<PathBuf>) -> Option<PathBuf> {
        PATH.with(|slot| slot.replace(path))
    }
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
        assert_eq!(default_model_for_provider("anthropic"), "claude-sonnet-5");
        assert_eq!(default_model_for_provider("openai"), "gpt-5.6-sol");
        assert_eq!(default_model_for_provider("openrouter"), "anthropic/claude-sonnet-5");
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
        assert_eq!(model, default_model_for_provider("anthropic"));
    }

    /// A credential-shaped sentinel for the Claude Code store. Not a real key.
    const CLAUDE_SENTINEL: &str = "sk-ant-api03-claude-code-sentinel";

    /// R-C1f G-3: an empty `--key` is not an explicit key; detection falls
    /// through to the stored credentials. Every store it reads is the test's
    /// own: the config directory and the Claude Code file both live in the
    /// test's temp dir, so the sentinel is what it finds, never the
    /// operator's credentials.
    #[test]
    fn empty_key_falls_through() {
        let outcome = detect_in(
            |dir| {
                let claude = dir.join(".claude");
                std::fs::create_dir_all(&claude).expect("test: claude dir");
                std::fs::write(
                    claude.join(".credentials.json"),
                    serde_json::to_vec(&serde_json::json!({ "claudeAiOauth": { "accessToken": CLAUDE_SENTINEL } }))
                        .expect("test: serialize credentials"),
                )
                .expect("test: write credentials");
            },
            || detect_credentials(Some("  "), None, None),
        );
        let (provider, key, _) = outcome.expect("test: the sentinel Claude Code credential is found");
        assert_eq!(provider, "anthropic");
        assert_eq!(key, CLAUDE_SENTINEL, "only the test's own Claude Code file may be read");
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

    // ── Detected credentials belong to the provider they were stored for ──

    /// A credential-shaped sentinel stored for one provider. Not a real secret.
    const DETECTED_SENTINEL: &str = "root-secret";
    /// A credential-shaped sentinel stored for the provider that is requested
    /// on the command line. Not a real secret.
    const REQUESTED_SENTINEL: &str = "requested-secret";
    /// A bring-your-own endpoint with a fixed port. Nothing connects to it.
    const REQUESTED_ENDPOINT: &str = "custom:http://127.0.0.1:18086/v1";

    /// Write an auth-profile store with one active profile per entry.
    ///
    /// The fixtures never make `anthropic` the *requested* provider without
    /// also storing its credential here, so no test can fall through to the
    /// Claude Code source and read the operator's own `~/.claude` file.
    fn write_auth_profiles(dir: &Path, entries: &[(&str, &str)]) {
        let mut active = serde_json::Map::new();
        let mut profiles = serde_json::Map::new();
        for (index, (provider, token)) in entries.iter().enumerate() {
            let profile_id = format!("profile-under-test-{index}");
            active.insert((*provider).to_string(), serde_json::Value::String(profile_id.clone()));
            profiles.insert(profile_id, serde_json::json!({ "token": token }));
        }
        let store = serde_json::json!({ "active_profiles": active, "profiles": profiles });
        std::fs::write(
            dir.join("auth-profiles.json"),
            serde_json::to_vec(&store).expect("test: serialize auth profiles"),
        )
        .expect("test: write auth profiles");
    }

    /// Write an auth-profile store holding one active profile for `provider`.
    fn write_auth_profile(dir: &Path, provider: &str) {
        write_auth_profiles(dir, &[(provider, DETECTED_SENTINEL)]);
    }

    /// Write a `config.toml` holding `key` for `provider`.
    fn write_config_toml_for(dir: &Path, provider: &str, key: &str) {
        std::fs::write(
            dir.join("config.toml"),
            format!("default_provider = \"{provider}\"\napi_key = \"{key}\"\n"),
        )
        .expect("test: write config.toml");
    }

    /// Write a `config.toml` holding a credential for `provider`.
    fn write_config_toml(dir: &Path, provider: &str) {
        write_config_toml_for(dir, provider, DETECTED_SENTINEL);
    }

    /// Run `detect_credentials` against an isolated config directory and an
    /// isolated Claude Code credentials file, so the test reads only the
    /// stores it wrote and never the operator's own.
    fn detect_in<R>(prepare: impl FnOnce(&Path), run: impl FnOnce() -> R) -> R {
        let _lock = crate::config::schema::process_config_dir_test_lock();
        let temp = tempfile::tempdir().expect("test: temp config dir");
        prepare(temp.path());
        let previous = crate::config::schema::swap_process_config_dir_for_tests(Some(temp.path().to_path_buf()));
        let previous_claude =
            claude_credentials_for_tests::swap(Some(temp.path().join(".claude").join(".credentials.json")));
        let outcome = run();
        claude_credentials_for_tests::swap(previous_claude);
        crate::config::schema::swap_process_config_dir_for_tests(previous);
        outcome
    }

    /// PMM NF-1. A credential detected from an auth profile belongs to the
    /// provider it was stored for. `prx go -p <other>` must not rename it onto
    /// the requested provider: the key would be sent to an endpoint that never
    /// issued it. Fail fast instead (decision D-012).
    ///
    /// RED before the fix: the sentinel is bound to the requested endpoint.
    #[test]
    fn an_explicit_provider_does_not_adopt_a_detected_key_from_another_provider() {
        let outcome = detect_in(
            |dir| write_auth_profile(dir, "anthropic"),
            || detect_credentials(None, Some(REQUESTED_ENDPOINT), None),
        );

        let error = outcome
            .err()
            .map(|error| format!("{error:#}"))
            .expect("test: a foreign credential must not be bound to the requested provider");
        assert!(
            error.contains("no credential configured for"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains(REQUESTED_ENDPOINT),
            "the error must name what was requested: {error}"
        );
        assert!(
            !error.contains(DETECTED_SENTINEL),
            "the error must not carry a credential"
        );
    }

    /// The same rule on the `config.toml` source.
    #[test]
    fn an_explicit_provider_does_not_adopt_a_detected_key_from_config_toml() {
        let outcome = detect_in(
            |dir| write_config_toml(dir, "openai"),
            || detect_credentials(None, Some(REQUESTED_ENDPOINT), None),
        );

        let error = outcome
            .err()
            .map(|error| format!("{error:#}"))
            .expect("test: a foreign credential must not be bound to the requested provider");
        assert!(
            error.contains("no credential configured for"),
            "unexpected error: {error}"
        );
        assert!(
            !error.contains(DETECTED_SENTINEL),
            "the error must not carry a credential"
        );
    }

    /// An alias of the detected provider is the same provider, so its own
    /// credential is still bound and the requested spelling is kept.
    #[test]
    fn an_explicit_alias_of_the_detected_provider_still_binds_its_key() {
        let outcome = detect_in(
            |dir| write_auth_profile(dir, "anthropic"),
            || detect_credentials(None, Some("claude-code"), None),
        );

        let (provider, key, model) = outcome.expect("test: an alias of the detected provider is the same provider");
        assert_eq!(provider, "claude-code");
        assert_eq!(key, DETECTED_SENTINEL);
        assert_eq!(model, default_model_for_provider("anthropic"));
    }

    /// R-C1 F-2. `--provider` names the provider whose credential is wanted,
    /// so the auth-profile store is searched for *that* provider's own active
    /// profile rather than walked in a fixed preference order. Another
    /// provider's profile sitting earlier in that order must not hide it.
    ///
    /// RED at `c7d3fead`: the fixed order returns the `anthropic` profile and
    /// the run is refused with ``no credential configured for `openai` ``
    /// while the `openai` profile sits unread in the same file.
    #[test]
    fn an_explicit_provider_binds_its_own_active_auth_profile() {
        let requested = detect_in(
            |dir| {
                write_auth_profiles(dir, &[("anthropic", DETECTED_SENTINEL), ("openai", REQUESTED_SENTINEL)]);
            },
            || detect_credentials(None, Some("openai"), None),
        );

        let (provider, key, model) = requested.expect("test: the requested provider has an active profile of its own");
        assert_eq!(provider, "openai");
        assert_eq!(
            key, REQUESTED_SENTINEL,
            "the requested provider's own key must be bound"
        );
        assert_eq!(model, default_model_for_provider("openai"));

        // The other provider in the same store still gets its own key.
        let other = detect_in(
            |dir| {
                write_auth_profiles(dir, &[("anthropic", DETECTED_SENTINEL), ("openai", REQUESTED_SENTINEL)]);
            },
            || detect_credentials(None, Some("anthropic"), None),
        );
        let (provider, key, _model) = other.expect("test: the other provider has an active profile of its own");
        assert_eq!(provider, "anthropic");
        assert_eq!(key, DETECTED_SENTINEL);
    }

    /// R-C1 F-2, second source. A foreign credential in an earlier source must
    /// not stop the walk before the source that does hold the requested
    /// provider's key.
    ///
    /// RED at `c7d3fead`: the `anthropic` profile is read first and the run is
    /// refused without ever reading `config.toml`.
    #[test]
    fn an_explicit_provider_reaches_its_config_toml_key_past_a_foreign_profile() {
        let outcome = detect_in(
            |dir| {
                write_auth_profiles(dir, &[("anthropic", DETECTED_SENTINEL)]);
                write_config_toml_for(dir, "openai", REQUESTED_SENTINEL);
            },
            || detect_credentials(None, Some("openai"), None),
        );

        let (provider, key, _model) = outcome.expect("test: `config.toml` records the requested provider's key");
        assert_eq!(provider, "openai");
        assert_eq!(key, REQUESTED_SENTINEL);
    }

    /// The fail-fast refusal survives the change: a `config.toml` key recorded
    /// for one provider is still not rebound onto a different one (D-012).
    #[test]
    fn a_config_toml_key_is_not_rebound_to_a_different_requested_provider() {
        let outcome = detect_in(
            |dir| write_config_toml_for(dir, "openai", DETECTED_SENTINEL),
            || detect_credentials(None, Some("openrouter"), None),
        );

        let error = outcome
            .err()
            .map(|error| format!("{error:#}"))
            .expect("test: a foreign credential must not be bound to the requested provider");
        assert!(
            error.contains("no credential configured for `openrouter`"),
            "unexpected error: {error}"
        );
        assert!(
            !error.contains(DETECTED_SENTINEL),
            "the error must not carry a credential"
        );
    }

    /// The requested provider is echoed back in the refusal, and a
    /// bring-your-own endpoint URL may carry userinfo, which is a credential.
    /// It must not reach terminal output or an error chain (R-C1 F-8).
    ///
    /// RED at `c7d3fead`: the URL is interpolated verbatim, userinfo included.
    #[test]
    fn a_refusal_does_not_echo_endpoint_userinfo() {
        let endpoint_with_userinfo = format!("custom:http://{REQUESTED_SENTINEL}@127.0.0.1:18086/v1");
        let outcome = detect_in(
            |dir| write_auth_profile(dir, "anthropic"),
            || detect_credentials(None, Some(&endpoint_with_userinfo), None),
        );

        let error = outcome
            .err()
            .map(|error| format!("{error:#}"))
            .expect("test: a foreign credential must not be bound to the requested provider");
        assert!(
            error.contains("no credential configured for"),
            "unexpected error: {error}"
        );
        assert!(
            !error.contains(REQUESTED_SENTINEL),
            "endpoint userinfo must not be echoed: {error}"
        );
        assert!(
            error.contains("127.0.0.1:18086"),
            "the operator must still recognise the endpoint: {error}"
        );
    }

    /// Detection reads files only. No environment variable is consulted for a
    /// credential on this path, so an isolated config directory fully
    /// determines the outcome: whatever the process environment carries, the
    /// bound key is the one the isolated store holds.
    #[test]
    fn no_environment_variable_can_supply_the_detected_credential() {
        const CANDIDATES: &[&str] = &[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_OAUTH_TOKEN",
            "OPENAI_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENPRX_API_KEY",
            "PRX_API_KEY",
        ];
        let ambient: Vec<String> = CANDIDATES
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty())
            .collect();

        let outcome = detect_in(
            |dir| write_auth_profiles(dir, &[("openai", REQUESTED_SENTINEL)]),
            || detect_credentials(None, Some("openai"), None),
        );

        let (provider, key, _model) = outcome.expect("test: the isolated store supplies the credential");
        assert_eq!(provider, "openai");
        assert_eq!(
            key, REQUESTED_SENTINEL,
            "the credential must come from the isolated store"
        );
        assert!(
            !ambient.iter().any(|value| value == &key),
            "no environment value may reach the result"
        );
    }

    /// Without `--provider` nothing is renamed and the behavior is unchanged.
    #[test]
    fn a_detected_credential_without_an_explicit_provider_is_unchanged() {
        let outcome = detect_in(
            |dir| write_auth_profile(dir, "anthropic"),
            || detect_credentials(None, None, None),
        );

        let (provider, key, model) = outcome.expect("test: a detected credential is used as detected");
        assert_eq!(provider, "anthropic");
        assert_eq!(key, DETECTED_SENTINEL);
        assert_eq!(model, default_model_for_provider("anthropic"));
    }
}
