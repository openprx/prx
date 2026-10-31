//! Provider listing, availability summary and model-prefix matching.

use super::{
    ByoDialect, PathBuf, ProviderId, ProviderIdentity, ProviderNamespace, ProviderRuntimeOptions, ResolvedPrimary,
    classify_provider, create_provider_with_options, resolve_provider_credentials,
};

/// Information about a supported provider for display purposes.
pub struct ProviderInfo {
    /// Canonical name used in config (e.g. `"openrouter"`)
    pub name: &'static str,
    /// Human-readable display name
    pub display_name: &'static str,
    /// Alternative names accepted in config
    pub aliases: &'static [&'static str],
    /// Whether the provider runs locally (no API key required)
    pub local: bool,
    /// Whether this provider is a model aggregator that accepts any model ID
    /// regardless of the org-prefix in `org/model` format.
    /// Example: OpenRouter routes `mistralai/mistral-7b` even though its own
    /// canonical name is `"openrouter"`, not `"mistralai"`.
    pub accepts_any_model: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderAvailabilitySummary {
    pub configured: Vec<String>,
    pub available: Vec<String>,
    pub unavailable: Vec<(String, String)>,
    pub degraded: bool,
}

pub(super) fn provider_requires_explicit_credential(name: &str) -> bool {
    if matches!(name, "bedrock" | "aws-bedrock") {
        return false;
    }

    match classify_provider(name) {
        // A `custom:<url>` endpoint is an OpenAI-compatible server the
        // operator runs. Many need no credential at all, exactly like the
        // `local: true` catalog entries, and they cannot carry that flag
        // because they are not catalog rows. The exemption stops at that
        // dialect: `AnthropicProvider` has no keyless mode, so exempting
        // `anthropic-custom:<url>` would only move the refusal from this gate
        // to the first turn, and would advertise a fallback that can never
        // serve one. An unparseable BYO string classifies `Unknown` and keeps
        // the requirement.
        ProviderIdentity::ByoEndpoint {
            dialect: ByoDialect::OpenAiCompatible,
            ..
        } => false,
        // A catalog provider needs a credential unless its row is a local
        // runtime. Aliases fold to the row, so one pass over the catalog
        // answers for every spelling.
        ProviderIdentity::Catalog(canonical) => {
            !list_providers().iter().any(|info| info.local && info.name == canonical)
        }
        _ => true,
    }
}

pub(super) fn is_openai_codex_alias(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "openai-codex" | "openai_codex" | "codex"
    )
}

pub(super) fn default_openprx_state_dir() -> PathBuf {
    crate::config::default_openprx_dir().unwrap_or_else(|| PathBuf::from(".openprx"))
}

pub(super) fn openai_codex_auth_profile_available_for_state_dir(state_dir: &std::path::Path) -> bool {
    let auth = crate::auth::AuthService::new_with_codex_import(
        state_dir,
        false,
        Some(crate::auth::codex_auth::default_codex_auth_json_path()),
        true,
    );

    auth.get_provider_bearer_token("openai-codex", None)
        .ok()
        .flatten()
        .is_some_and(|token| !token.trim().is_empty())
}

pub(super) fn openai_codex_auth_profile_available() -> bool {
    openai_codex_auth_profile_available_for_state_dir(&default_openprx_state_dir())
}

pub fn provider_matches_model_prefix(provider_name: &str, model: &str) -> bool {
    #[cfg(feature = "wasm-plugins")]
    if provider_name.starts_with("wasm:") {
        return true;
    }
    let model = model.trim();
    if model.is_empty() {
        // An empty model means "use the provider's configured default", which is
        // always compatible with that provider.
        return true;
    }
    if model.starts_with("hint:") {
        // d04 §9.2 hint-injection hardening: a raw "hint:<name>" prefix is an
        // UNRESOLVED routing hint. It MUST be resolved upstream by the
        // RouterProvider into a concrete "<provider>:<model>" / "<provider>/<model>"
        // form before reaching provider matching. The previous wildcard
        // early-return (`return true`) let a crafted, unresolved hint match ANY
        // provider, bypassing the prefix check and allowing a model to be routed
        // to the wrong API. An unresolved hint reaching here is a non-match.
        return false;
    }

    let Some((prefix, _rest)) = model.split_once('/') else {
        return true;
    };

    let prefix = prefix.trim().to_ascii_lowercase();
    if prefix.is_empty() {
        return true;
    }

    let provider_name = provider_name.trim().to_ascii_lowercase();
    if provider_name == prefix {
        return true;
    }

    // A bring-your-own endpoint publishes its own model catalog, so PRX has
    // nothing to check an `org/model` id against and the endpoint is the
    // authority. This arm sits behind the `hint:` guard and the slash guard,
    // so neither of those is widened, and behind the catalog comparison for
    // every other identity, which is unchanged.
    if matches!(classify_provider(&provider_name), ProviderIdentity::ByoEndpoint { .. }) {
        return true;
    }
    // A decision service publishes its own model ids, like a BYO endpoint.
    if ProviderId::of(&provider_name).namespace() == ProviderNamespace::DecisionService {
        return true;
    }

    for info in list_providers() {
        if info.name.eq_ignore_ascii_case(&provider_name)
            || info
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(&provider_name))
        {
            // Aggregator providers accept any model regardless of the org-prefix.
            // OpenRouter routes to hundreds of models whose IDs use `org/model`
            // format (e.g. "mistralai/mistral-7b", "anthropic/claude-3-opus").
            // The prefix is the upstream org, not "openrouter" itself.
            if info.accepts_any_model {
                return true;
            }

            if info.name.eq_ignore_ascii_case(&prefix)
                || info.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(&prefix))
            {
                return true;
            }
        }
    }

    false
}

/// Providers that serve whatever model the operator loaded into them, named by
/// its Hugging Face `org/model` repository (`llama-server -hf …`). The
/// organisation says nothing about which provider serves it
/// (`google/gemma-…-GGUF` is a llama.cpp repository).
pub(super) const SELF_HOSTED_MODEL_SERVERS: &[&str] = &["llamacpp"];

/// Whether `provider_name` is a self-hosted model server (llama.cpp),
/// under its name or an alias.
pub fn is_self_hosted_model_server(provider_name: &str) -> bool {
    canonical_provider_info(provider_name).is_some_and(|info| SELF_HOSTED_MODEL_SERVERS.contains(&info.name))
}

/// Whether `provider_name` can serve `model` when the model is configured for
/// it (the default model, an onboarding choice, `/model`). A self-hosted model
/// server serves any repository it is configured for; every other provider
/// keeps [`provider_matches_model_prefix`]. Which provider answers a route is
/// decided by the route's provider identity, not by the model's organisation
/// (see `ReliableProvider`, D-075).
pub fn provider_serves_configured_model(provider_name: &str, model: &str) -> bool {
    if is_self_hosted_model_server(provider_name) {
        return !model.trim().starts_with("hint:");
    }
    provider_matches_model_prefix(provider_name, model)
}

/// Whether `model` explicitly names a model repository (`org/repository`)
/// that is not routed to another provider: bare ids go to the route's own
/// provider, and an organisation written as a PRX provider name or alias
/// (`anthropic/…`, `openai/…`) is that provider's model by PRX's
/// `<provider>/<model>` convention. A self-hosted model server serves such a
/// `model_fallbacks` entry as a fallback member (D-076 N-1).
pub fn names_model_repository(model: &str) -> bool {
    let model = model.trim();
    if model.starts_with("hint:") {
        return false;
    }
    let Some((organisation, repository)) = model.split_once('/') else {
        return false;
    };
    // Normalised as provider matching does elsewhere: trimmed, any case
    // (`OpenAI/gpt-4o` and `openai /gpt-4o` are OpenAI's; D-076 N-10).
    let organisation = organisation.trim();
    if organisation.is_empty() || repository.trim().is_empty() {
        return false;
    }
    !list_providers().iter().any(|provider| {
        provider.name.eq_ignore_ascii_case(organisation)
            || provider
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(organisation))
    })
}

pub(super) fn canonical_provider_info(provider_name: &str) -> Option<ProviderInfo> {
    let provider_name = provider_name.trim();
    list_providers().into_iter().find(|info| {
        info.name.eq_ignore_ascii_case(provider_name)
            || info
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(provider_name))
    })
}

pub(super) const KIMI_CODE_SUPPORTED_MODELS: &[&str] = &["k3", "kimi-for-coding", "kimi-for-coding-highspeed"];

/// Validate model IDs that PRX can safely validate locally.
///
/// Most OpenAI-compatible providers own model catalog changes, so PRX does not
/// generally reject bare model IDs. Kimi Code is an exception in practice: the
/// coding endpoint can return success even for an invalid model string, which
/// makes CLI validation evidence misleading.
pub fn validate_provider_model(provider_name: &str, model: &str) -> anyhow::Result<()> {
    let model = model.trim();
    if model.is_empty() || model.starts_with("hint:") {
        return Ok(());
    }

    if !provider_serves_configured_model(provider_name, model) {
        anyhow::bail!("model '{model}' is not compatible with provider '{provider_name}'");
    }

    if canonical_provider_info(provider_name).is_some_and(|info| info.name == "kimi-code")
        && !KIMI_CODE_SUPPORTED_MODELS.iter().any(|known| *known == model)
    {
        anyhow::bail!(
            "invalid model '{model}' for provider 'kimi-code'. Supported models: {}",
            KIMI_CODE_SUPPORTED_MODELS.join(", ")
        );
    }

    Ok(())
}

pub fn summarize_provider_availability(
    primary_name: &str,
    api_key: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    options: &ProviderRuntimeOptions,
) -> ProviderAvailabilitySummary {
    summarize_provider_availability_inner(primary_name, api_key, reliability, options, None)
}

pub(super) fn summarize_provider_availability_inner(
    primary_name: &str,
    api_key: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    options: &ProviderRuntimeOptions,
    validated_primary: Option<ResolvedPrimary<'_>>,
) -> ProviderAvailabilitySummary {
    let mut configured = vec![primary_name.to_string()];
    for fallback in &reliability.fallback_providers {
        if !configured.iter().any(|name| name == fallback) {
            configured.push(fallback.clone());
        }
    }

    let mut available = Vec::new();
    let mut unavailable = Vec::new();

    for provider in &configured {
        if provider == primary_name && validated_primary.is_some() {
            // The same resolver-owned primary has already passed its real constructor.
            available.push(provider.clone());
            continue;
        }
        if let Err(error) = create_provider_with_options(provider, None, options) {
            unavailable.push((provider.clone(), format!("invalid provider: {error}")));
            continue;
        }

        if is_openai_codex_alias(provider) {
            if openai_codex_auth_profile_available() {
                available.push(provider.clone());
            } else {
                unavailable.push((provider.clone(), "missing OpenAI Codex auth profile/token".to_string()));
            }
            continue;
        }

        let explicit_for_provider = (provider == primary_name).then_some(api_key).flatten();
        let resolved = resolve_provider_credentials(provider, explicit_for_provider, options);
        if provider_requires_explicit_credential(provider) && !resolved.is_usable_for(provider) {
            unavailable.push((provider.clone(), "missing credential/api key".to_string()));
            continue;
        }

        available.push(provider.clone());
    }

    ProviderAvailabilitySummary {
        configured,
        degraded: available.len() < 2,
        available,
        unavailable,
    }
}

/// Return the list of all known providers for display in `prx providers list`.
///
/// This is intentionally separate from the factory match in `create_provider`
/// (display concern vs. construction concern).
pub fn list_providers() -> Vec<ProviderInfo> {
    vec![
        // ── Primary providers ────────────────────────────────
        ProviderInfo {
            name: "openrouter",
            display_name: "OpenRouter",
            aliases: &[],
            local: false,
            // OpenRouter is a model aggregator: it routes requests to hundreds of
            // upstream models whose IDs use `org/model` format (e.g.
            // "mistralai/mistral-7b", "anthropic/claude-3-opus").  The prefix is
            // the upstream vendor, not "openrouter", so any model name is valid.
            accepts_any_model: true,
        },
        ProviderInfo {
            name: "anthropic",
            display_name: "Anthropic",
            aliases: &["claude-code", "claude-cli"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "openai",
            display_name: "OpenAI",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "openai-codex",
            display_name: "OpenAI Codex (OAuth)",
            aliases: &["openai_codex", "codex"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "ollama",
            display_name: "Ollama",
            aliases: &[],
            local: true,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "gemini",
            display_name: "Google Gemini",
            aliases: &["google", "google-gemini"],
            local: false,
            accepts_any_model: false,
        },
        // ── OpenAI-compatible providers ──────────────────────
        ProviderInfo {
            name: "venice",
            display_name: "Venice",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "vercel",
            display_name: "Vercel AI Gateway",
            aliases: &["vercel-ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "cloudflare",
            display_name: "Cloudflare AI",
            aliases: &["cloudflare-ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "moonshot",
            display_name: "Moonshot",
            aliases: &["kimi"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "kimi-code",
            display_name: "Kimi Code",
            aliases: &["kimi_coding", "kimi_for_coding"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "synthetic",
            display_name: "Synthetic",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "opencode",
            display_name: "OpenCode Zen",
            aliases: &["opencode-zen"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "zai",
            display_name: "Z.AI",
            aliases: &["z.ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "glm",
            display_name: "GLM (Zhipu)",
            aliases: &["zhipu"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "minimax",
            display_name: "MiniMax",
            aliases: &[
                "minimax-intl",
                "minimax-io",
                "minimax-global",
                "minimax-cn",
                "minimaxi",
                "minimax-oauth",
                "minimax-oauth-cn",
                "minimax-portal",
                "minimax-portal-cn",
            ],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "bedrock",
            display_name: "Amazon Bedrock",
            aliases: &["aws-bedrock"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "qianfan",
            display_name: "Qianfan (Baidu)",
            aliases: &["baidu"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "qwen",
            display_name: "Qwen (DashScope)",
            aliases: &["dashscope", "qwen-intl", "dashscope-intl", "qwen-us", "dashscope-us"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "groq",
            display_name: "Groq",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "mistral",
            display_name: "Mistral",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "xai",
            display_name: "xAI (Grok)",
            aliases: &["grok"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "deepseek",
            display_name: "DeepSeek",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "together",
            display_name: "Together AI",
            aliases: &["together-ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "fireworks",
            display_name: "Fireworks AI",
            aliases: &["fireworks-ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "perplexity",
            display_name: "Perplexity",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "cohere",
            display_name: "Cohere",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "copilot",
            display_name: "GitHub Copilot",
            aliases: &["github-copilot"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "lmstudio",
            display_name: "LM Studio",
            aliases: &["lm-studio"],
            local: true,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "llamacpp",
            display_name: "llama.cpp server",
            aliases: &["llama.cpp"],
            local: true,
            // Not an aggregator: a fallback llama.cpp member never answers for
            // another provider family's model. The model it is configured for
            // is accepted by `provider_serves_configured_model` (D-072 L-9).
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "litellm",
            display_name: "LiteLLM",
            aliases: &["lite-llm"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "vllm",
            display_name: "vLLM",
            aliases: &["v-llm"],
            local: true,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "huggingface",
            display_name: "Hugging Face Inference",
            aliases: &["hf", "hf-inference"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "nvidia",
            display_name: "NVIDIA NIM",
            aliases: &["nvidia-nim", "build.nvidia.com"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "ovhcloud",
            display_name: "OVHcloud AI Endpoints",
            aliases: &["ovh"],
            local: false,
            accepts_any_model: false,
        },
        // Test-only mock provider. `local: true` ensures
        // `provider_requires_explicit_credential` returns false so the
        // availability check passes without any API key.
        // Compiled-in only when `test-mock` feature is enabled (or under
        // `#[cfg(test)]` for the in-crate provider tests).
        #[cfg(any(test, feature = "test-mock"))]
        ProviderInfo {
            name: "mock",
            display_name: "Mock (test)",
            aliases: &[],
            local: true,
            accepts_any_model: true,
        },
    ]
}
