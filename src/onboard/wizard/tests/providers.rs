#![cfg(test)]

use super::*;

/// `prx models refresh --provider compatible` while the root connection is
/// configured for `openai`: the loopback stands in for the root base URL.
/// Before, the refresh sent the root key there; now it reaches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_refresh_for_another_provider_sends_no_root_credential() {
    use crate::providers::credential_probe::CredentialProbe;
    let probe = CredentialProbe::start().await;
    let temp = TempDir::new().unwrap();
    let mut config = discovery_config(&temp);
    config.api_url = Some(probe.base_url());

    let outcome = tokio::task::spawn_blocking(move || run_models_refresh(&config, Some("compatible"), true))
        .await
        .expect("test: refresh task joins");

    assert!(
        !probe.saw(DISCOVERY_ROOT_SENTINEL),
        "the root key reached another provider's catalog"
    );
    assert!(
        probe.requests().is_empty(),
        "the root base URL must not serve another provider"
    );
    assert!(outcome.is_err(), "no endpoint of its own means nothing to refresh");
}

/// ONBOARD-HINT (D-062): onboarding points every provider at the
/// supported mechanism (`api_key` in config.toml, or the auth store via
/// `prx auth paste-token`) and names an environment variable only when
/// the daemon reads it. It used to print `export OPENROUTER_API_KEY=…`,
/// `export ANTHROPIC_API_KEY=…` and the like, which the daemon never
/// reads, and it reported a set `ANTHROPIC_API_KEY` as "detected".
#[test]
fn onboarding_credential_hints_name_only_what_the_daemon_reads() {
    for provider in [
        "openrouter",
        "openai",
        "anthropic",
        "groq",
        "mistral",
        "deepseek",
        "xai",
        "glm",
        "minimax",
        "moonshot",
        "qwen",
        "ollama",
        "llamacpp",
        "litellm",
    ] {
        let hint = credential_setup_hint(provider);
        assert!(
            !hint.contains("export") && !hint.contains("_API_KEY"),
            "{provider}: {hint}"
        );
        assert!(hint.contains("`api_key` in config.toml"), "{provider}: {hint}");
        assert!(
            hint.contains(&format!("prx auth paste-token --provider {provider}")),
            "{provider}: {hint}"
        );
    }
    for (provider, variable) in [("gemini", "GEMINI_API_KEY"), ("qwen-code", "QWEN_OAUTH_TOKEN")] {
        let hint = credential_setup_hint(provider);
        assert!(hint.contains(variable), "{provider}: {hint}");
        assert!(hint.contains("`api_key` in config.toml"), "{provider}: {hint}");
    }

    // No production text of the wizard suggests exporting a variable the
    // daemon does not read (the needles are split so this test does not
    // match itself).
    let sources = [
        include_str!("../../wizard.rs"),
        include_str!("../quick.rs"),
        include_str!("../models.rs"),
        include_str!("../workspace.rs"),
        include_str!("../provider.rs"),
        include_str!("../preferences.rs"),
        include_str!("../channels.rs"),
        include_str!("../tunnel.rs"),
        include_str!("../summary.rs"),
        include_str!("../persistence.rs"),
    ];
    assert!(sources.len() > 1);
    assert!(sources.iter().all(|source| !source.trim().is_empty()));
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/onboard/wizard");
    for entry in std::fs::read_dir(directory).expect("read wizard sources") {
        let path = entry.expect("wizard source entry").path();
        if path.is_file() && path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            let source = std::fs::read_to_string(&path).expect("read wizard source");
            assert!(
                sources.contains(&source.as_str()),
                "unscanned production wizard source: {}",
                path.display()
            );
        }
    }
    let production = sources
        .iter()
        .map(|source| source.split("#[cfg(test)]\nmod tests").next().unwrap_or(source))
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        concat!("export ", "OPENROUTER_API_KEY"),
        concat!("export ", "ANTHROPIC_API_KEY"),
        concat!("\"export {", "env_var}"),
        concat!("via env var ", "or config file"),
        concat!("ANTHROPIC_API_KEY environment ", "variable detected"),
        concat!("ANTHROPIC_OAUTH_TOKEN environment ", "variable detected"),
        concat!("style(\"OLLAMA", "_API_KEY\")"),
        concat!("style(\"LLAMACPP", "_API_KEY\")"),
    ] {
        assert!(!production.contains(needle), "the wizard still prints `{needle}`");
    }
}

// ── provider_credential_env_var ────────────────────────────────────────

#[test]
fn provider_credential_env_var_known_providers() {
    assert_eq!(provider_credential_env_var("openrouter"), Some("OPENROUTER_API_KEY"));
    assert_eq!(provider_credential_env_var("anthropic"), Some("ANTHROPIC_API_KEY"));
    assert_eq!(provider_credential_env_var("openai"), Some("OPENAI_API_KEY"));
    assert_eq!(provider_credential_env_var("ollama"), Some("OLLAMA_API_KEY"));
    assert_eq!(provider_credential_env_var("llamacpp"), Some("LLAMACPP_API_KEY"));
    assert_eq!(provider_credential_env_var("llama.cpp"), Some("LLAMACPP_API_KEY"));
    assert_eq!(provider_credential_env_var("xai"), Some("XAI_API_KEY"));
    assert_eq!(provider_credential_env_var("grok"), Some("XAI_API_KEY")); // alias
    assert_eq!(provider_credential_env_var("together"), Some("TOGETHER_API_KEY")); // alias
    assert_eq!(provider_credential_env_var("together-ai"), Some("TOGETHER_API_KEY"));
    assert_eq!(provider_credential_env_var("google"), Some("GEMINI_API_KEY")); // alias
    assert_eq!(provider_credential_env_var("google-gemini"), Some("GEMINI_API_KEY")); // alias
    assert_eq!(provider_credential_env_var("gemini"), Some("GEMINI_API_KEY"));
    assert_eq!(provider_credential_env_var("qwen"), Some("DASHSCOPE_API_KEY"));
    assert_eq!(provider_credential_env_var("qwen-intl"), Some("DASHSCOPE_API_KEY"));
    assert_eq!(provider_credential_env_var("dashscope-us"), Some("DASHSCOPE_API_KEY"));
    assert_eq!(provider_credential_env_var("qwen-code"), Some("QWEN_OAUTH_TOKEN"));
    assert_eq!(provider_credential_env_var("qwen-oauth"), Some("QWEN_OAUTH_TOKEN"));
    assert_eq!(provider_credential_env_var("glm-cn"), Some("GLM_API_KEY"));
    assert_eq!(provider_credential_env_var("minimax-cn"), Some("MINIMAX_API_KEY"));
    assert_eq!(provider_credential_env_var("kimi-code"), Some("KIMI_CODE_API_KEY"));
    assert_eq!(provider_credential_env_var("kimi_coding"), Some("KIMI_CODE_API_KEY"));
    assert_eq!(
        provider_credential_env_var("kimi_for_coding"),
        Some("KIMI_CODE_API_KEY")
    );
    assert_eq!(provider_credential_env_var("minimax-oauth"), Some("MINIMAX_API_KEY"));
    assert_eq!(provider_credential_env_var("minimax-oauth-cn"), Some("MINIMAX_API_KEY"));
    assert_eq!(provider_credential_env_var("moonshot-intl"), Some("MOONSHOT_API_KEY"));
    assert_eq!(provider_credential_env_var("zai-cn"), Some("ZAI_API_KEY"));
    assert_eq!(provider_credential_env_var("nvidia"), Some("NVIDIA_API_KEY"));
    assert_eq!(provider_credential_env_var("nvidia-nim"), Some("NVIDIA_API_KEY")); // alias
    assert_eq!(provider_credential_env_var("build.nvidia.com"), Some("NVIDIA_API_KEY")); // alias
    assert_eq!(provider_credential_env_var("astrai"), Some("ASTRAI_API_KEY"));
}

#[test]
fn provider_supports_keyless_local_usage_for_local_providers() {
    assert!(provider_supports_keyless_local_usage("ollama"));
    assert!(provider_supports_keyless_local_usage("llamacpp"));
    assert!(provider_supports_keyless_local_usage("llama.cpp"));
    assert!(!provider_supports_keyless_local_usage("openai"));
}

#[test]
fn provider_env_var_unknown_has_no_variable() {
    assert_eq!(provider_credential_env_var("some-new-provider"), None);
}

#[test]
fn onboarding_summaries_name_the_provider_without_url_userinfo() {
    let config = Config {
        default_provider: Some(USERINFO_LABEL.to_string()),
        ..Config::default()
    };
    for line in [
        setup_provider_line(USERINFO_LABEL),
        selected_provider_line(USERINFO_LABEL, "fixture-model"),
        quick_summary_provider_line(&config),
    ] {
        assert!(line.contains("custom:http://[redacted]@127.0.0.1:1/v1"), "{line}");
        assert!(!line.contains("SECRET-fixture-pw"), "{line}");
    }
}

/// RED before PMMDtail-j (D-084 E-3): the onboarding model-discovery
/// profile read loaded through the migrating load, rewriting
/// auth-profiles.json.
#[test]
fn the_onboarding_profile_read_never_rewrites_the_profiles_file() {
    let tmp = TempDir::new().expect("test: temp dir");
    let path = crate::auth::profiles::write_legacy_token_profile(tmp.path(), "moonshot", "sentinel-moonshot");
    let before = std::fs::read_to_string(&path).expect("test: read");
    let mut config = Config::default();
    config.config_path = tmp.path().join("config.toml");
    config.secrets.encrypt = true;

    let key = model_discovery_api_key(&config, "moonshot");

    assert_eq!(key, "sentinel-moonshot");
    assert_eq!(
        std::fs::read_to_string(&path).expect("test: read"),
        before,
        "the file was rewritten"
    );
}
