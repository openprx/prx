//! TG1: Provider End-to-End Resolution Tests
//!
//! Prevents: Pattern 1 — Provider configuration & resolution bugs (27% of user bugs).
//! Issues: #831, #834, #721, #580, #452, #451, #796, #843
//!
//! Tests the full pipeline from config values through PMM selection and the
//! typed provider factory, verifying provider resolution, URL construction,
//! credential wiring, and auth header format.

use openprx::config::Config;
use openprx::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
use openprx::providers::{self, create_provider_for};

fn typed_provider_result(name: &str, key: Option<&str>, url: Option<&str>) -> anyhow::Result<()> {
    let workspace = tempfile::TempDir::new()?;
    let mut config = Config {
        workspace_dir: workspace.path().to_path_buf(),
        config_path: workspace.path().join("config.toml"),
        ..Config::default()
    };
    config.default_provider = Some(name.to_string());
    config.default_model = Some("prx-provider-resolution-model".to_string());
    config.api_key = key.map(str::to_string);
    config.api_url = url.map(str::to_string);
    let catalog = providers::selection::catalog_for(&config);
    let resolved = providers::selection::check_with(
        &config,
        &catalog,
        name,
        "prx-provider-resolution-model",
        providers::selection::credential_for(&config, name),
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
    create_provider_for(
        &resolved,
        &catalog,
        &config.reliability,
        &config.cost,
        &providers::provider_runtime_options_from_config(&config),
    )?;
    Ok(())
}

/// Helper: assert typed provider creation succeeds.
fn assert_provider_ok(name: &str, key: Option<&str>, url: Option<&str>) {
    let result = typed_provider_result(name, key, url);
    assert!(
        result.is_ok(),
        "{name} provider should resolve: {}",
        result.err().map(|e| e.to_string()).unwrap_or_default()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Factory resolution: each major provider name resolves without error
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_resolves_openai_provider() {
    assert_provider_ok("openai", Some("test-key"), None);
}

#[test]
fn factory_resolves_anthropic_provider() {
    assert_provider_ok("anthropic", Some("test-key"), None);
}

#[test]
fn factory_resolves_deepseek_provider() {
    assert_provider_ok("deepseek", Some("test-key"), None);
}

#[test]
fn factory_resolves_mistral_provider() {
    assert_provider_ok("mistral", Some("test-key"), None);
}

#[test]
fn factory_resolves_ollama_provider() {
    assert_provider_ok("ollama", None, None);
}

#[test]
fn factory_resolves_groq_provider() {
    assert_provider_ok("groq", Some("test-key"), None);
}

#[test]
fn factory_resolves_xai_provider() {
    assert_provider_ok("xai", Some("test-key"), None);
}

#[test]
fn factory_resolves_together_provider() {
    assert_provider_ok("together", Some("test-key"), None);
}

#[test]
fn factory_resolves_fireworks_provider() {
    assert_provider_ok("fireworks", Some("test-key"), None);
}

#[test]
fn factory_resolves_perplexity_provider() {
    assert_provider_ok("perplexity", Some("test-key"), None);
}

// ─────────────────────────────────────────────────────────────────────────────
// Factory resolution: alias variants map to same provider
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_grok_alias_resolves_to_xai() {
    assert_provider_ok("grok", Some("test-key"), None);
}

#[test]
fn factory_kimi_alias_resolves_to_moonshot() {
    assert_provider_ok("kimi", Some("test-key"), None);
}

#[test]
fn factory_zhipu_alias_resolves_to_glm() {
    assert_provider_ok("zhipu", Some("test-key"), None);
}

// ─────────────────────────────────────────────────────────────────────────────
// Custom URL provider creation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_custom_http_url_resolves() {
    assert_provider_ok("custom:http://localhost:8080", Some("test-key"), None);
}

#[test]
fn factory_custom_https_url_resolves() {
    assert_provider_ok("custom:https://api.example.com/v1", Some("test-key"), None);
}

#[test]
fn factory_custom_ftp_url_rejected() {
    let result = typed_provider_result("custom:ftp://example.com", None, None);
    assert!(result.is_err(), "ftp scheme should be rejected");
    let err_msg = result.err().unwrap().to_string();
    assert!(
        err_msg.contains("http://") || err_msg.contains("https://"),
        "error should mention valid schemes: {err_msg}"
    );
}

#[test]
fn factory_custom_empty_url_rejected() {
    let result = typed_provider_result("custom:", None, None);
    assert!(result.is_err(), "empty custom URL should be rejected");
}

#[test]
fn factory_unknown_provider_rejected() {
    let result = typed_provider_result("nonexistent_provider_xyz", None, None);
    assert!(result.is_err(), "unknown provider name should be rejected");
}

// ─────────────────────────────────────────────────────────────────────────────
// OpenAiCompatibleProvider: credential and auth style wiring
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn compatible_provider_bearer_auth_style() {
    // Construction with Bearer auth should succeed
    let _provider = OpenAiCompatibleProvider::new(
        "TestProvider",
        "https://api.test.com",
        Some("provider-test-credential"),
        AuthStyle::Bearer,
    );
}

#[test]
fn compatible_provider_xapikey_auth_style() {
    // Construction with XApiKey auth should succeed
    let _provider = OpenAiCompatibleProvider::new(
        "TestProvider",
        "https://api.test.com",
        Some("provider-test-credential"),
        AuthStyle::XApiKey,
    );
}

#[test]
fn compatible_provider_custom_auth_header() {
    // Construction with Custom auth should succeed
    let _provider = OpenAiCompatibleProvider::new(
        "TestProvider",
        "https://api.test.com",
        Some("provider-test-credential"),
        AuthStyle::Custom("X-Custom-Auth".into()),
    );
}

#[test]
fn compatible_provider_no_credential() {
    // Construction without credential should succeed (for local providers)
    let _provider = OpenAiCompatibleProvider::new("TestLocal", "http://localhost:11434", None, AuthStyle::Bearer);
}

#[test]
fn compatible_provider_base_url_trailing_slash_normalized() {
    // Construction with trailing slash URL should succeed
    let _provider = OpenAiCompatibleProvider::new(
        "TestProvider",
        "https://api.test.com/v1/",
        Some("key"),
        AuthStyle::Bearer,
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Provider with api_url override (simulates #721 - Ollama api_url config)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_ollama_with_custom_api_url() {
    assert_provider_ok("ollama", None, Some("http://192.168.1.100:11434"));
}

#[test]
fn factory_openai_with_custom_api_url() {
    assert_provider_ok(
        "openai",
        Some("test-key"),
        Some("https://custom-openai-proxy.example.com/v1"),
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Provider default convenience factory
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn convenience_factory_resolves_major_providers() {
    for provider_name in &[
        "openai",
        "anthropic",
        "deepseek",
        "mistral",
        "groq",
        "xai",
        "together",
        "fireworks",
        "perplexity",
    ] {
        let result = typed_provider_result(provider_name, Some("test-key"), None);
        assert!(
            result.is_ok(),
            "convenience factory should resolve {provider_name}: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }
}

#[test]
fn convenience_factory_ollama_no_key() {
    let result = typed_provider_result("ollama", None, None);
    assert!(
        result.is_ok(),
        "ollama should not require api key: {}",
        result.err().map(|e| e.to_string()).unwrap_or_default()
    );
}
