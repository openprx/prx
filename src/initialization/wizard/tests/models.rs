#![cfg(test)]

use super::*;

// ── model helper coverage ───────────────────────────────────

#[test]
fn default_model_for_provider_uses_latest_defaults() {
    assert_eq!(default_model_for_provider("openrouter"), "anthropic/claude-sonnet-5");
    assert_eq!(default_model_for_provider("openai"), "gpt-5.6-sol");
    assert_eq!(default_model_for_provider("openai-codex"), "gpt-5.2-codex");
    assert_eq!(default_model_for_provider("anthropic"), "claude-sonnet-5");
    assert_eq!(default_model_for_provider("qwen"), "qwen3.7-plus");
    assert_eq!(default_model_for_provider("qwen-intl"), "qwen3.7-plus");
    assert_eq!(default_model_for_provider("qwen-code"), "qwen3-coder-plus");
    assert_eq!(default_model_for_provider("glm-cn"), "glm-5");
    assert_eq!(default_model_for_provider("minimax-cn"), "MiniMax-M3");
    assert_eq!(default_model_for_provider("zai-cn"), "glm-5");
    assert_eq!(default_model_for_provider("gemini"), "gemini-3.8-flash");
    assert_eq!(default_model_for_provider("google"), "gemini-3.8-flash");
    assert_eq!(default_model_for_provider("kimi-code"), "k3");
    assert_eq!(default_model_for_provider("bedrock"), "anthropic.claude-sonnet-5");
    assert_eq!(default_model_for_provider("google-gemini"), "gemini-3.8-flash");
    assert_eq!(default_model_for_provider("venice"), "zai-org-glm-5");
    assert_eq!(default_model_for_provider("moonshot"), "kimi-k3");
    assert_eq!(default_model_for_provider("nvidia"), "meta/llama-3.3-70b-instruct");
    assert_eq!(default_model_for_provider("nvidia-nim"), "meta/llama-3.3-70b-instruct");
    assert_eq!(default_model_for_provider("llamacpp"), "ggml-org/gpt-oss-20b-GGUF");
    assert_eq!(default_model_for_provider("astrai"), "anthropic/claude-sonnet-4.6");
}

#[test]
fn canonical_provider_name_normalizes_regional_aliases() {
    assert_eq!(canonical_provider_name("qwen-intl"), "qwen");
    assert_eq!(canonical_provider_name("dashscope-us"), "qwen");
    assert_eq!(canonical_provider_name("qwen-code"), "qwen-code");
    assert_eq!(canonical_provider_name("qwen-oauth"), "qwen-code");
    assert_eq!(canonical_provider_name("moonshot-intl"), "moonshot");
    assert_eq!(canonical_provider_name("kimi-cn"), "moonshot");
    assert_eq!(canonical_provider_name("kimi_coding"), "kimi-code");
    assert_eq!(canonical_provider_name("kimi_for_coding"), "kimi-code");
    assert_eq!(canonical_provider_name("glm-cn"), "glm");
    assert_eq!(canonical_provider_name("bigmodel"), "glm");
    assert_eq!(canonical_provider_name("minimax-cn"), "minimax");
    assert_eq!(canonical_provider_name("zai-cn"), "zai");
    assert_eq!(canonical_provider_name("z.ai-global"), "zai");
    assert_eq!(canonical_provider_name("nvidia-nim"), "nvidia");
    assert_eq!(canonical_provider_name("aws-bedrock"), "bedrock");
    assert_eq!(canonical_provider_name("build.nvidia.com"), "nvidia");
    assert_eq!(canonical_provider_name("llama.cpp"), "llamacpp");
    assert_eq!(canonical_provider_name("lite-llm"), "litellm");
    assert_eq!(canonical_provider_name("v-llm"), "vllm");
    assert_eq!(canonical_provider_name("hf"), "huggingface");
}

#[test]
fn catalog_models_for_openai_include_latest_choices() {
    let ids: Vec<String> = catalog_models_for_provider("openai")
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert!(ids.contains(&"gpt-5.6".to_string()));
    assert!(ids.contains(&"gpt-5.6-luna".to_string()));
}

#[test]
fn catalog_models_for_glm_removes_deprecated_flash_plus_aliases() {
    let ids: Vec<String> = catalog_models_for_provider("glm")
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert!(ids.contains(&"glm-5".to_string()));
    assert!(ids.contains(&"glm-4.7".to_string()));
    assert!(ids.contains(&"glm-4.5-air".to_string()));
    assert!(!ids.contains(&"glm-4-plus".to_string()));
    assert!(!ids.contains(&"glm-4-flash".to_string()));
}

#[test]
fn catalog_models_for_openai_codex_include_codex_family() {
    let ids: Vec<String> = catalog_models_for_provider("openai-codex")
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert!(ids.contains(&"gpt-5-codex".to_string()));
    assert!(ids.contains(&"gpt-5.2-codex".to_string()));
}

#[test]
fn catalog_models_for_openrouter_use_valid_anthropic_id() {
    let ids: Vec<String> = catalog_models_for_provider("openrouter")
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert!(ids.contains(&"anthropic/claude-sonnet-5".to_string()));
}

#[test]
fn catalog_models_for_bedrock_include_verified_model_ids() {
    let ids: Vec<String> = catalog_models_for_provider("bedrock")
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert!(ids.contains(&"anthropic.claude-sonnet-5".to_string()));
    assert!(ids.contains(&"anthropic.claude-opus-4-8".to_string()));
    assert!(ids.contains(&"anthropic.claude-haiku-4-5-20251001-v1:0".to_string()));
    assert!(ids.contains(&"anthropic.claude-sonnet-4-5-20250929-v1:0".to_string()));
}

#[test]
fn catalog_models_for_moonshot_drop_deprecated_aliases() {
    let ids: Vec<String> = catalog_models_for_provider("moonshot")
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert!(ids.contains(&"kimi-k2.6".to_string()));
    assert!(!ids.contains(&"kimi-k2.5".to_string()));
    assert!(!ids.contains(&"kimi-k2-thinking".to_string()));
    assert!(!ids.contains(&"kimi-latest".to_string()));
    assert!(!ids.contains(&"kimi-thinking-preview".to_string()));
}

#[test]
fn allows_unauthenticated_model_fetch_for_public_catalogs() {
    assert!(allows_unauthenticated_model_fetch("openrouter"));
    assert!(allows_unauthenticated_model_fetch("venice"));
    assert!(allows_unauthenticated_model_fetch("nvidia"));
    assert!(allows_unauthenticated_model_fetch("nvidia-nim"));
    assert!(allows_unauthenticated_model_fetch("build.nvidia.com"));
    assert!(allows_unauthenticated_model_fetch("astrai"));
    assert!(allows_unauthenticated_model_fetch("ollama"));
    assert!(allows_unauthenticated_model_fetch("llamacpp"));
    assert!(allows_unauthenticated_model_fetch("llama.cpp"));
    assert!(!allows_unauthenticated_model_fetch("openai"));
    assert!(!allows_unauthenticated_model_fetch("deepseek"));
}

#[test]
fn catalog_models_for_kimi_code_include_official_agent_model() {
    let ids: Vec<String> = catalog_models_for_provider("kimi-code")
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert_eq!(
        ids,
        vec![
            "k3".to_string(),
            "kimi-for-coding-highspeed".to_string(),
            "kimi-for-coding".to_string(),
            "k3.1-pro".to_string(),
            "k3.1-pro-highspeed".to_string(),
            "k3-256k".to_string()
        ]
    );
}

#[test]
fn catalog_models_for_qwen_code_include_coding_plan_models() {
    let ids: Vec<String> = catalog_models_for_provider("qwen-code")
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert!(ids.contains(&"qwen3-coder-plus".to_string()));
    assert!(ids.contains(&"qwen3.5-plus".to_string()));
    assert!(ids.contains(&"qwen3-max-2026-01-23".to_string()));
}

#[test]
fn supports_live_model_fetch_for_supported_and_unsupported_providers() {
    assert!(supports_live_model_fetch("openai"));
    assert!(supports_live_model_fetch("anthropic"));
    assert!(supports_live_model_fetch("gemini"));
    assert!(supports_live_model_fetch("google"));
    assert!(supports_live_model_fetch("grok"));
    assert!(supports_live_model_fetch("together"));
    assert!(supports_live_model_fetch("nvidia"));
    assert!(supports_live_model_fetch("nvidia-nim"));
    assert!(supports_live_model_fetch("build.nvidia.com"));
    assert!(supports_live_model_fetch("ollama"));
    assert!(supports_live_model_fetch("llamacpp"));
    assert!(supports_live_model_fetch("llama.cpp"));
    assert!(supports_live_model_fetch("kimi-code"));
    assert!(supports_live_model_fetch("litellm"));
    assert!(supports_live_model_fetch("vllm"));
    assert!(supports_live_model_fetch("huggingface"));
    assert!(supports_live_model_fetch("compatible"));
    assert!(supports_live_model_fetch("astrai"));
    assert!(supports_live_model_fetch("venice"));
    assert!(supports_live_model_fetch("glm-cn"));
    assert!(supports_live_model_fetch("qwen-intl"));
    assert!(!supports_live_model_fetch("minimax-cn"));
    assert!(!supports_live_model_fetch("unknown-provider"));
}

#[test]
fn model_discovery_uses_active_auth_profile_when_config_key_is_absent() {
    let tmp = TempDir::new().unwrap();
    let mut config = Config::default();
    config.api_key = None;
    config.config_path = tmp.path().join("config.toml");
    config.secrets.encrypt = false;

    let store = AuthProfilesStore::new(tmp.path(), false);
    store
        .upsert_profile(
            crate::auth::profiles::AuthProfile::new_token("kimi-code", "default", "profile-secret".to_string()),
            true,
        )
        .unwrap();

    assert_eq!(model_discovery_api_key(&config, "kimi-code"), "profile-secret");
}

/// C0-D: `prx doctor models` probes every catalog provider through this
/// connection. The root key and base URL belong to the configured default
/// provider only; every other vendor gets its own credential or none.
#[test]
fn model_discovery_releases_the_root_connection_only_to_the_default_provider() {
    let temp = TempDir::new().unwrap();
    let config = discovery_config(&temp);
    let default_key = crate::providers::canonical_provider_key("openai");

    for info in crate::providers::list_providers() {
        if crate::providers::canonical_provider_key(info.name) == default_key {
            continue;
        }
        let connection = model_discovery_connection(&config, info.name);
        assert!(
            !connection.api_key.contains(DISCOVERY_ROOT_SENTINEL),
            "the root key would be sent to {}'s model catalog",
            info.name
        );
        assert_eq!(
            connection.api_url, None,
            "the root base URL would be used for {}",
            info.name
        );
    }

    let own = model_discovery_connection(&config, "OpenAI");
    assert_eq!(own.api_key, DISCOVERY_ROOT_SENTINEL);
    assert_eq!(own.api_url, Some("http://127.0.0.1:9/v1"));
}

/// A provider that is not the default still finds its own auth profile.
#[test]
fn model_discovery_uses_the_selected_provider_profile_despite_a_root_key() {
    let temp = TempDir::new().unwrap();
    let config = discovery_config(&temp);
    let store = AuthProfilesStore::new(temp.path(), false);
    store
        .upsert_profile(
            crate::auth::profiles::AuthProfile::new_token("kimi-code", "default", "kimi-profile".to_string()),
            true,
        )
        .unwrap();

    assert_eq!(model_discovery_connection(&config, "kimi-code").api_key, "kimi-profile");
}

/// Guard: the configured provider still refreshes with its own key and URL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_refresh_for_the_default_provider_keeps_its_connection() {
    use crate::providers::credential_probe::CredentialProbe;
    let probe = CredentialProbe::start().await;
    let temp = TempDir::new().unwrap();
    let mut config = discovery_config(&temp);
    config.default_provider = Some("compatible".to_string());
    config.api_url = Some(probe.base_url());

    let outcome = tokio::task::spawn_blocking(move || run_models_refresh(&config, Some("compatible"), true))
        .await
        .expect("test: refresh task joins");

    assert!(outcome.is_ok(), "{outcome:?}");
    let requests = probe.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests.first().and_then(|request| request.authorization()),
        Some(format!("Bearer {DISCOVERY_ROOT_SENTINEL}").as_str())
    );
}

/// C0-f F-7: model discovery reads a provider's own credential variable
/// only. The generic `API_KEY` belongs to no provider, so a provider
/// without a variable of its own reads nothing (and no test reads the
/// operator's ambient `$API_KEY`).
#[test]
fn discovery_reads_no_generic_api_key_variable() {
    for provider in ["litellm", "vllm", "huggingface", "compatible", "unknown-provider"] {
        assert_eq!(provider_credential_env_var(provider), None, "{provider}");
    }
    assert_eq!(provider_credential_env_var("openai"), Some("OPENAI_API_KEY"));
    assert_eq!(provider_credential_env_var("openrouter"), Some("OPENROUTER_API_KEY"));
    // INT-2B a36, RED before: the onboarding hint for such a provider was
    // "export API_KEY", a variable nothing reads.
    let hint = keyless_provider_hint("litellm");
    assert!(!hint.contains("API_KEY"), "{hint}");
    assert!(hint.contains("`api_key` in config.toml"), "{hint}");
    assert!(hint.contains("prx auth paste-token --provider litellm"), "{hint}");
    let custom = keyless_provider_hint("custom:http://alice:a36-pw@127.0.0.1:1/v1");
    assert!(!custom.contains("a36-pw"), "{custom}");
    // B2f F-8, RED before: the redacted label was printed, and pasted it
    // registers the key under a different identity (`[redacted]`).
    assert!(!custom.contains("[redacted]") && !custom.contains("alice"), "{custom}");
    assert!(custom.contains("default_provider value from config.toml"), "{custom}");
    // A label without userinfo is printed as a word the shell passes
    // through unchanged, quoted when it needs it.
    let plain = keyless_provider_hint("custom:http://127.0.0.1:1/v1");
    assert!(plain.contains("--provider custom:http://127.0.0.1:1/v1`"), "{plain}");
    let quoted = keyless_provider_hint("custom:http://127.0.0.1:1/v1?a=1&b='x'");
    assert!(
        quoted.contains(r"--provider 'custom:http://127.0.0.1:1/v1?a=1&b='\''x'\'''`"),
        "{quoted}"
    );
}

#[test]
fn catalog_provider_aliases_share_the_same_release_rows() {
    assert_eq!(catalog_models_for_provider("xai"), catalog_models_for_provider("grok"));
    assert_eq!(
        catalog_models_for_provider("together-ai"),
        catalog_models_for_provider("together")
    );
    assert_eq!(
        catalog_models_for_provider("gemini"),
        catalog_models_for_provider("google")
    );
    assert_eq!(
        catalog_models_for_provider("gemini"),
        catalog_models_for_provider("google-gemini")
    );
    assert_eq!(
        catalog_models_for_provider("qwen"),
        catalog_models_for_provider("qwen-intl")
    );
    assert_eq!(
        catalog_models_for_provider("qwen"),
        catalog_models_for_provider("dashscope-us")
    );
    assert_eq!(
        catalog_models_for_provider("minimax"),
        catalog_models_for_provider("minimax-cn")
    );
    assert_eq!(
        catalog_models_for_provider("zai"),
        catalog_models_for_provider("zai-cn")
    );
    assert_eq!(
        catalog_models_for_provider("nvidia"),
        catalog_models_for_provider("nvidia-nim")
    );
    assert_eq!(
        catalog_models_for_provider("nvidia"),
        catalog_models_for_provider("build.nvidia.com")
    );
    assert_eq!(
        catalog_models_for_provider("llamacpp"),
        catalog_models_for_provider("llama.cpp")
    );
    assert_eq!(
        catalog_models_for_provider("bedrock"),
        catalog_models_for_provider("aws-bedrock")
    );
}

#[test]
fn catalog_and_live_options_preselect_the_release_default() {
    for (provider, expected) in [
        ("anthropic", "claude-sonnet-5"),
        ("openai", "gpt-5.6-sol"),
        ("gemini", "gemini-3.8-flash"),
        ("xai", "grok-4.20-0309-reasoning"),
        ("mistral", "mistral-medium-latest"),
        ("ollama", "llama3.2"),
    ] {
        let catalog = catalog_models_for_provider(provider);
        assert_eq!(catalog.first().map(|(model, _)| model.as_str()), Some(expected));

        let mut live = vec![
            ("alphabetical-first".to_string(), "First".to_string()),
            (expected.to_string(), "Provider default".to_string()),
        ];
        prioritize_default_model(provider, &mut live);
        assert_eq!(live.first().map(|(model, _)| model.as_str()), Some(expected));
    }
}

#[test]
fn ollama_cloud_fallback_is_served_from_active_release_rows() {
    let discovered = fetch_live_models_for_provider("ollama", "fixture-key", None)
        .expect("test: embedded Ollama fallback must load");
    let ids: Vec<_> = discovered.iter().map(|model| model.id.as_str()).collect();
    assert!(ids.first().is_some_and(|model| model.ends_with(":cloud")));
    assert!(ids.contains(&"glm-5:cloud"));
    assert!(ids.contains(&"qwen2.5-coder:3b"));
    assert!(!ids.contains(&"*"));
}

#[test]
fn model_options_label_deprecated_rows_without_hiding_them() {
    let options = build_model_options_with_warnings(
        vec!["prx-current".to_owned(), "prx-legacy".to_owned()],
        "live",
        |model| (model == "prx-legacy").then(|| "deprecated; use ollama/prx-current".to_owned()),
    );
    assert_eq!(
        options,
        vec![
            ("prx-current".to_owned(), "prx-current (live)".to_owned()),
            (
                "prx-legacy".to_owned(),
                "prx-legacy (live; deprecated; use ollama/prx-current)".to_owned(),
            ),
        ]
    );
}

#[test]
fn onboarding_labels_catalog_rows_that_have_no_billing_tariff() {
    for (provider, model) in [
        ("openrouter", "deepseek/deepseek-v3.2"),
        ("openrouter", "google/gemini-3.5-flash"),
        ("openrouter", "meta-llama/llama-4-maverick"),
        ("openrouter", "openai/gpt-5-mini"),
        ("gemini", "gemini-3.5-flash"),
    ] {
        let option = catalog_models_for_provider(provider)
            .into_iter()
            .find(|(id, _)| id == model)
            .unwrap_or_else(|| panic!("missing onboarding row {provider}/{model}"));
        assert!(option.1.contains("pricing unavailable"), "{}: {}", option.0, option.1);
    }

    for model in ["anthropic/claude-sonnet-5", "openai/gpt-5.6", "x-ai/grok-4.20"] {
        let option = catalog_models_for_provider("openrouter")
            .into_iter()
            .find(|(id, _)| id == model)
            .unwrap_or_else(|| panic!("missing priced upstream row {model}"));
        assert!(!option.1.contains("pricing unavailable"), "{}: {}", option.0, option.1);
    }

    let release = crate::providers::catalog::ReleaseCatalog::embedded_ref().expect("release catalog");
    let row = release
        .model(
            &crate::providers::ProviderId::of("openrouter"),
            "deepseek/deepseek-v3.2",
        )
        .expect("unpriced onboarding row");
    let mut cost = crate::config::schema::CostConfig::default();
    cost.prices.insert(
        "deepseek/deepseek-v3.2".to_owned(),
        crate::config::schema::ModelPricing {
            input: 0.1,
            output: 0.2,
            cache_write: 0.1,
            cache_read: 0.01,
        },
    );
    assert!(
        !model_has_no_billing_tariff("openrouter", row, &cost),
        "an exact operator override must clear the selected-model warning"
    );
}

#[test]
fn catalog_models_for_nvidia_include_nim_entries() {
    let ids: Vec<String> = catalog_models_for_provider("nvidia")
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert!(ids.contains(&"meta/llama-3.3-70b-instruct".to_string()));
    assert!(ids.contains(&"deepseek-ai/deepseek-v3.2".to_string()));
    assert!(ids.contains(&"nvidia/llama-3.3-nemotron-super-49b-v1.5".to_string()));
}

#[test]
fn models_endpoint_for_provider_handles_region_aliases() {
    assert_eq!(
        models_endpoint_for_provider("glm-cn"),
        Some("https://open.bigmodel.cn/api/paas/v4/models")
    );
    assert_eq!(
        models_endpoint_for_provider("zai-cn"),
        Some("https://open.bigmodel.cn/api/coding/paas/v4/models")
    );
    assert_eq!(
        models_endpoint_for_provider("qwen-intl"),
        Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/models")
    );
}

#[test]
fn models_endpoint_for_provider_supports_additional_openai_compatible_providers() {
    assert_eq!(
        models_endpoint_for_provider("venice"),
        Some("https://api.venice.ai/api/v1/models")
    );
    assert_eq!(
        models_endpoint_for_provider("cohere"),
        Some("https://api.cohere.com/compatibility/v1/models")
    );
    assert_eq!(
        models_endpoint_for_provider("moonshot"),
        Some("https://api.moonshot.ai/v1/models")
    );
    assert_eq!(
        models_endpoint_for_provider("llamacpp"),
        Some("http://localhost:8080/v1/models")
    );
    assert_eq!(
        models_endpoint_for_provider("llama.cpp"),
        Some("http://localhost:8080/v1/models")
    );
    assert_eq!(
        models_endpoint_for_provider("kimi-code"),
        Some("https://api.kimi.com/coding/v1/models")
    );
    assert_eq!(
        models_endpoint_for_provider("huggingface"),
        Some("https://router.huggingface.co/v1/models")
    );
    assert_eq!(models_endpoint_for_provider("perplexity"), None);
    assert_eq!(models_endpoint_for_provider("unknown-provider"), None);
}

#[test]
fn resolve_live_models_endpoint_prefers_compatible_custom_url() {
    assert_eq!(
        resolve_live_models_endpoint("llamacpp", Some("http://127.0.0.1:8033/v1")),
        Some("http://127.0.0.1:8033/v1/models".to_string())
    );
    assert_eq!(
        resolve_live_models_endpoint("llama.cpp", Some("http://127.0.0.1:8033/v1/")),
        Some("http://127.0.0.1:8033/v1/models".to_string())
    );
    assert_eq!(
        resolve_live_models_endpoint("llamacpp", Some("http://127.0.0.1:8033/v1/models")),
        Some("http://127.0.0.1:8033/v1/models".to_string())
    );
    assert_eq!(
        resolve_live_models_endpoint("litellm", Some("http://127.0.0.1:4000/v1")),
        Some("http://127.0.0.1:4000/v1/models".to_string())
    );
    assert_eq!(
        resolve_live_models_endpoint("litellm", Some("http://127.0.0.1:4000")),
        Some("http://127.0.0.1:4000/v1/models".to_string())
    );
}

/// RED before PMME-h (D-095 M-1): `prx models refresh` for `openai` with
/// a configured `api_url` (an operator's gateway, where provider
/// construction sends the key) listed models at `api.openai.com` with
/// that key. It now addresses the configured endpoint, query kept.
#[test]
fn model_discovery_addresses_the_configured_endpoint_construction_uses() {
    assert_eq!(
        resolve_live_models_endpoint("openai", Some("http://127.0.0.1:19071/v1")),
        Some("http://127.0.0.1:19071/v1/models".to_string())
    );
    assert_eq!(
        resolve_live_models_endpoint("OpenAI", Some("https://gateway.corp.example/v1?tenant=t1")),
        Some("https://gateway.corp.example/v1/models?tenant=t1".to_string())
    );
    // An adapter with a fixed endpoint ignores `api_url` in construction,
    // so discovery does too.
    assert_eq!(
        resolve_live_models_endpoint("venice", Some("http://localhost:9999/v1")),
        Some("https://api.venice.ai/api/v1/models".to_string())
    );
}

#[test]
fn resolve_live_models_endpoint_falls_back_to_provider_defaults() {
    assert_eq!(
        resolve_live_models_endpoint("llamacpp", None),
        Some("http://localhost:8080/v1/models".to_string())
    );
    assert_eq!(
        resolve_live_models_endpoint("venice", Some("http://localhost:9999/v1")),
        Some("https://api.venice.ai/api/v1/models".to_string())
    );
    assert_eq!(resolve_live_models_endpoint("unknown-provider", None), None);
}

#[test]
fn parse_openai_model_ids_supports_data_array_payload() {
    let payload = json!({
        "data": [
            {"id": "  gpt-5.1  "},
            {"id": "gpt-5-mini"},
            {"id": "gpt-5.1"},
            {"id": ""}
        ]
    });

    let ids = discovery::chat_model_ids(&discovery::parse_openai_compatible_models(&payload));
    assert_eq!(ids, vec!["gpt-5-mini".to_string(), "gpt-5.1".to_string()]);
}

#[test]
fn parse_openai_model_ids_supports_root_array_payload() {
    let payload = json!([
        {"id": "alpha"},
        {"id": "beta"},
        {"id": "alpha"}
    ]);

    let ids = discovery::chat_model_ids(&discovery::parse_openai_compatible_models(&payload));
    assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);
}

#[test]
fn parse_gemini_model_ids_filters_for_generate_content() {
    let payload = json!({
        "models": [
            {
                "name": "models/gemini-2.5-pro",
                "supportedGenerationMethods": ["generateContent", "countTokens"]
            },
            {
                "name": "models/text-embedding-004",
                "supportedGenerationMethods": ["embedContent"]
            },
            {
                "name": "models/gemini-2.5-flash",
                "supportedGenerationMethods": ["generateContent"]
            }
        ]
    });

    let ids = discovery::chat_model_ids(&discovery::parse_gemini_models(&payload));
    assert_eq!(ids, vec!["gemini-2.5-flash".to_string(), "gemini-2.5-pro".to_string()]);
}

#[test]
fn parse_ollama_model_ids_extracts_and_deduplicates_names() {
    let payload = json!({
        "models": [
            {"name": "llama3.2:latest"},
            {"name": "mistral:latest"},
            {"name": "llama3.2:latest"}
        ]
    });

    let ids = discovery::chat_model_ids(&discovery::parse_ollama_models(&payload));
    assert_eq!(ids, vec!["llama3.2:latest".to_string(), "mistral:latest".to_string()]);
}

#[test]
fn model_cache_round_trip_returns_fresh_entry() {
    let tmp = TempDir::new().unwrap();
    let models = vec!["gpt-5.1".to_string(), "gpt-5-mini".to_string()];

    cache_live_models_for_provider(tmp.path(), "openai", &id_only(&models)).unwrap();

    let cached = load_cached_models_for_provider(tmp.path(), "openai", MODEL_CACHE_TTL_SECS).unwrap();
    let cached = cached.expect("expected fresh cached models");

    assert_eq!(cached.models.len(), 2);
    assert!(cached.models.contains(&"gpt-5.1".to_string()));
    assert!(cached.models.contains(&"gpt-5-mini".to_string()));
}

#[test]
fn model_cache_ttl_filters_stale_entries() {
    let tmp = TempDir::new().unwrap();
    let stale = discovery::ModelCacheState {
        version: 1,
        entries: vec![discovery::ModelCacheEntry {
            provider: "openai".to_string(),
            fetched_at_unix: now_unix_secs().saturating_sub(MODEL_CACHE_TTL_SECS + 120),
            models: vec!["gpt-5.1".to_string()],
            descriptors: Vec::new(),
        }],
    };

    discovery::save_model_cache_state(tmp.path(), &stale).unwrap();

    let fresh = load_cached_models_for_provider(tmp.path(), "openai", MODEL_CACHE_TTL_SECS).unwrap();
    assert!(fresh.is_none());

    let stale_any = load_any_cached_models_for_provider(tmp.path(), "openai").unwrap();
    assert!(stale_any.is_some());
}

#[test]
fn run_models_refresh_uses_fresh_cache_without_network() {
    let tmp = TempDir::new().unwrap();

    cache_live_models_for_provider(tmp.path(), "openai", &id_only(&["gpt-5.1".to_string()])).unwrap();

    let config = Config {
        workspace_dir: tmp.path().to_path_buf(),
        default_provider: Some("openai".to_string()),
        ..Config::default()
    };

    run_models_refresh(&config, None, false).unwrap();
}

#[test]
fn run_models_probe_read_only_uses_cache_without_mutation() {
    let tmp = TempDir::new().unwrap();
    cache_live_models_for_provider(tmp.path(), "openai", &id_only(&["gpt-5.1".to_string()])).unwrap();
    let cache_path = discovery::model_cache_path(tmp.path());
    let before = std::fs::read(&cache_path).unwrap();
    let config = Config {
        workspace_dir: tmp.path().to_path_buf(),
        default_provider: Some("openai".to_string()),
        ..Config::default()
    };

    run_models_probe_read_only(&config, None, true).unwrap();

    assert_eq!(before, std::fs::read(cache_path).unwrap());
}

#[test]
fn run_models_refresh_rejects_unsupported_provider() {
    let tmp = TempDir::new().unwrap();

    let config = Config {
        workspace_dir: tmp.path().to_path_buf(),
        // Use a non-provider channel key to keep this test deterministic and offline.
        default_provider: Some("imessage".to_string()),
        ..Config::default()
    };

    let err = run_models_refresh(&config, None, true).unwrap_err();
    assert!(err.to_string().contains("does not support live model discovery"));
}

/// The wizard's discovery path must run on a blocking thread, not on the
/// async worker that `run_wizard` is awaited on.
///
/// Before the fix this call was made synchronously inside the runtime and
/// panicked with "Cannot drop a runtime in a context where blocking is not
/// allowed", because `reqwest::blocking` enters and drops a temporary
/// runtime on the calling thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboard_model_discovery_runs_off_the_async_worker() {
    let workspace = TempDir::new().unwrap();
    // An empty cache means discovery is genuinely attempted, exactly as in
    // a first-run wizard session.
    assert!(
        load_cached_models_for_provider(workspace.path(), "compatible", MODEL_CACHE_TTL_SECS)
            .unwrap()
            .is_none()
    );

    let body = r#"{"data":[{"id":"prx-model-alpha"},{"id":"prx-model-beta"}]}"#;
    let (addr, server) = spawn_canned_discovery_endpoint("HTTP/1.1 200 OK", body);
    let endpoint = format!("http://{addr}/v1/models");

    let models = fetch_live_models_for_provider_off_runtime(
        "compatible".to_string(),
        "prx_test_key".to_string(),
        Some(endpoint),
    )
    .await
    .unwrap();

    assert_eq!(
        discovery::chat_model_ids(&models),
        vec!["prx-model-alpha".to_string(), "prx-model-beta".to_string()]
    );

    cache_live_models_for_provider(workspace.path(), "compatible", &models).unwrap();
    let cached = load_cached_models_for_provider(workspace.path(), "compatible", MODEL_CACHE_TTL_SECS)
        .unwrap()
        .expect("test: discovery result must be cached for the next wizard run");
    assert_eq!(cached.models, discovery::chat_model_ids(&models));

    server.join().unwrap();
}

/// A failing discovery surfaces a typed, contextual error and leaves the
/// runtime healthy, so the wizard can report it and fall back to cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboard_model_discovery_failure_runs_wizard_cache_fallback() {
    let workspace = TempDir::new().unwrap();
    let (addr, server) = spawn_canned_discovery_endpoint(
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"discovery unavailable"}"#,
    );
    let endpoint = format!("http://{addr}/v1/models");

    let fetch = fetch_live_models_for_provider_off_runtime(
        "compatible".to_string(),
        "prx_test_key".to_string(),
        Some(endpoint.clone()),
    )
    .await;

    cache_live_models_for_provider(
        workspace.path(),
        "compatible",
        &id_only(&["prx-model-cached".to_string()]),
    )
    .unwrap();
    let mut live_options = None;
    let status = apply_wizard_model_fetch(workspace.path(), "compatible", &mut live_options, fetch).unwrap();
    let WizardModelFetchStatus::Failed { error, stale_age_secs } = status else {
        panic!("test: a failed fetch must take the wizard fallback branch")
    };
    assert!(
        error.contains(&format!("model fetch failed: GET {endpoint}")),
        "discovery failure must stay reportable, got: {error}"
    );
    assert!(
        stale_age_secs.is_some(),
        "the wizard must report loading its stale cache"
    );
    assert_eq!(
        live_options,
        Some(vec![(
            "prx-model-cached".to_string(),
            "prx-model-cached (stale-cache)".to_string()
        )]),
        "the wizard's model-source selector must receive stale-cache options"
    );

    // The property the panic used to destroy: the runtime still works.
    tokio::task::yield_now().await;
    assert_eq!(tokio::spawn(async { 1_u8 }).await.unwrap(), 1);

    server.join().unwrap();
}

/// Cancelling the discovery task is observable to the caller and does not
/// wedge or poison the runtime.
///
/// Limitation, stated deliberately: `spawn_blocking` closures are not
/// cancellable. Aborting the handle detaches the caller; the blocking HTTP
/// call itself still runs to completion or to its timeout. This asserts the
/// caller-visible contract only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboard_model_discovery_cancellation_leaves_the_runtime_healthy() {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (addr, server) = spawn_silent_discovery_endpoint(std::sync::Arc::clone(&stop));
    let endpoint = format!("http://{addr}/v1/models");

    let handle = tokio::spawn(fetch_live_models_for_provider_off_runtime(
        "compatible".to_string(),
        "prx_test_key".to_string(),
        Some(endpoint),
    ));

    handle.abort();
    let join_error = handle
        .await
        .expect_err("test: an aborted discovery task must report cancellation");
    assert!(join_error.is_cancelled());

    // The runtime is still healthy after the cancellation.
    tokio::task::yield_now().await;
    assert_eq!(tokio::spawn(async { 2_u8 }).await.unwrap(), 2);

    // Release the detached blocking call so the runtime can shut down promptly.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
}

#[test]
fn model_discovery_errors_name_the_provider_and_endpoint_without_url_userinfo() {
    let tmp = TempDir::new().unwrap();
    let config = Config {
        workspace_dir: tmp.path().to_path_buf(),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    let errors = [
        run_models_refresh(&config, Some(USERINFO_LABEL), true).expect_err("test: custom has no live discovery"),
        run_models_probe_read_only(&config, Some(USERINFO_LABEL), false)
            .expect_err("test: custom has no live discovery"),
        fetch_openai_compatible_models("http://u:SECRET-fixture-pw@127.0.0.1:1/v1/models", None, false)
            .expect_err("test: no key and no anonymous access"),
        fetch_openai_compatible_models("http://u:SECRET-fixture-pw@127.0.0.1:1/v1/models", None, true)
            .expect_err("test: port 1 refuses connections"),
    ];
    for error in errors {
        let rendered = format!("{error:#}");
        assert!(rendered.contains("[redacted]@127.0.0.1:1"), "{rendered}");
        assert!(!rendered.contains("SECRET-fixture-pw"), "{rendered}");
    }
}

/// PMM-B-6: `prx models refresh` keeps what the catalog said about each
/// model. It used to keep the ids only, so a no-tools or image-only model
/// was indistinguishable from a chat model afterwards.
#[test]
fn a_models_refresh_keeps_the_catalog_metadata() {
    let workspace = TempDir::new().unwrap();
    let body = r#"{"data":[
            {"id":"vendor/chat-no-tools","context_length":32768,
             "architecture":{"input_modalities":["text"],"output_modalities":["text"]},
             "supported_parameters":["temperature","max_tokens"]},
            {"id":"vendor/image-maker",
             "architecture":{"input_modalities":["text"],"output_modalities":["image"]},
             "supported_parameters":[]},
            {"id":"vendor/bare"}
        ]}"#;
    let (addr, server) = spawn_canned_discovery_endpoint("HTTP/1.1 200 OK", body);
    let config = Config {
        workspace_dir: workspace.path().to_path_buf(),
        config_path: workspace.path().join("config.toml"),
        default_provider: Some("compatible".to_string()),
        api_url: Some(format!("http://{addr}/v1")),
        ..Config::default()
    };

    run_models_refresh(&config, None, true).expect("test: refresh from the loopback catalog");
    server.join().unwrap();

    let raw = std::fs::read_to_string(workspace.path().join("state").join("models_cache.json")).unwrap();
    let cache: Value = serde_json::from_str(&raw).unwrap();
    let entry = &cache["entries"][0];
    assert_eq!(
        entry["models"],
        serde_json::json!(["vendor/bare", "vendor/chat-no-tools"]),
        "the chat list offers a model the catalog says has no text output: {raw}"
    );
    let described = entry["descriptors"]
        .as_array()
        .and_then(|models| models.iter().find(|model| model["id"] == "vendor/chat-no-tools"))
        .unwrap_or_else(|| panic!("the refresh dropped the catalog metadata: {raw}"));
    assert_eq!(
        described["supported_parameters"],
        serde_json::json!(["temperature", "max_tokens"])
    );
    assert_eq!(described["output_modalities"], serde_json::json!(["text"]));
    assert_eq!(described["context_window"], serde_json::json!(32768));
    assert_eq!(cache["version"], serde_json::json!(2));
}
