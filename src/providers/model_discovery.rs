#![allow(clippy::print_stdout, clippy::print_stderr)]
use crate::auth::discovery::is_claude_code_oauth_setup_token;
use crate::auth::{profiles::AuthProfilesStore, select_profile_id};
use crate::config::Config;
use crate::providers::catalog::discovery::{self, DiscoveredModel};
use crate::providers::{
    canonical_china_provider_name, is_huggingface_alias, is_litellm_alias, is_qwen_oauth_alias, is_vllm_alias,
};
use anyhow::{Context, Result, bail};
use console::style;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const LIVE_MODEL_MAX_OPTIONS: usize = 120;
pub(crate) const MODEL_PREVIEW_LIMIT: usize = 20;
pub(crate) const MODEL_CACHE_TTL_SECS: u64 = 12 * 60 * 60;
pub(crate) const CUSTOM_MODEL_SENTINEL: &str = "__custom_model__";

pub(crate) fn canonical_provider_name(provider_name: &str) -> &str {
    if is_qwen_oauth_alias(provider_name) {
        return "qwen-code";
    }
    if is_litellm_alias(provider_name) {
        return "litellm";
    }
    if is_vllm_alias(provider_name) {
        return "vllm";
    }
    if is_huggingface_alias(provider_name) {
        return "huggingface";
    }

    if let Some(canonical) = canonical_china_provider_name(provider_name) {
        return canonical;
    }

    match provider_name {
        "grok" => "xai",
        "together" => "together-ai",
        "google" | "google-gemini" => "gemini",
        "kimi_coding" | "kimi_for_coding" => "kimi-code",
        "nvidia-nim" | "build.nvidia.com" => "nvidia",
        "aws-bedrock" => "bedrock",
        "llama.cpp" => "llamacpp",
        _ => provider_name,
    }
}

pub(crate) fn allows_unauthenticated_model_fetch(provider_name: &str) -> bool {
    matches!(
        canonical_provider_name(provider_name),
        "openrouter" | "ollama" | "llamacpp" | "litellm" | "vllm" | "compatible" | "venice" | "astrai" | "nvidia"
    )
}

/// Resolve a new default as an operator selection (D-034): negative
/// evidence refuses and nothing is written; no evidence is accepted and the
/// operator is told the model is unverified.
///
/// The evidence is the one every other surface resolves against
/// ([`crate::providers::selection::catalog_for`], D-078 T-8), so a default
/// onboarding accepts is judged the same way by the configuration API and
/// `config.patch` afterwards.
pub(crate) fn check_onboarding_selection(config: &Config, provider: &str, model: &str) -> Result<()> {
    let catalog = crate::providers::selection::catalog_for(config);
    let credential = crate::providers::selection::credential_for(config, provider);
    match crate::providers::selection::check_with(
        config,
        &catalog,
        provider,
        model,
        credential,
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::OperatorSelection,
    ) {
        Ok(resolved) => {
            if let crate::providers::catalog::Verification::Unverified { unproven } = resolved.verification() {
                println!(
                    "  {} model '{model}' is unverified for {}: PRX has no catalog evidence for it yet. It will be \
                     used; declare it in `[[providers.models]]` once you know what it supports.",
                    style("!").yellow().bold(),
                    unproven.join(", ")
                );
            }
            if let Ok(release) = crate::providers::catalog::ReleaseCatalog::embedded_ref()
                && let Some(model) = release.model(
                    &crate::providers::ProviderId::of(resolved.provider_label()),
                    resolved.model(),
                )
                && model_has_no_billing_tariff(resolved.provider_label(), model, &config.cost)
            {
                println!(
                    "  {} model '{}' has no published token tariff; add an exact `[cost.prices]` entry before \
                     cost-admitted use.",
                    style("!").yellow().bold(),
                    resolved.model()
                );
            }
            Ok(())
        }
        Err(refusal) => bail!("{refusal}. Nothing was written."),
    }
}

pub(crate) fn model_has_no_billing_tariff(
    provider: &str,
    model: &crate::providers::catalog::ReleaseModel,
    cost: &crate::config::schema::CostConfig,
) -> bool {
    matches!(
        crate::llm::route_decision::usage_cost_resolution(provider, &model.api_model, 1, 1, 0, 0, cost,),
        crate::llm::route_decision::UsagePrice::Missing | crate::llm::route_decision::UsagePrice::Invalid
    )
}

pub(crate) fn onboarding_model_warning(
    provider: &str,
    model: &crate::providers::catalog::ReleaseModel,
) -> Option<String> {
    let lifecycle = model.selection_warning();
    let pricing = model_has_no_billing_tariff(provider, model, &crate::config::schema::CostConfig::default())
        .then_some("pricing unavailable; add [cost.prices] before cost-admitted use".to_owned());
    match (lifecycle, pricing) {
        (Some(lifecycle), Some(pricing)) => Some(format!("{lifecycle}; {pricing}")),
        (Some(warning), None) | (None, Some(warning)) => Some(warning),
        (None, None) => None,
    }
}

/// Pick a sensible default model for the given provider.
pub(crate) fn default_model_for_provider(provider: &str) -> String {
    let provider = canonical_provider_name(provider);
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

pub(crate) fn catalog_models_for_provider(provider_name: &str) -> Vec<(String, String)> {
    let provider = canonical_provider_name(provider_name);
    match crate::providers::catalog::ReleaseCatalog::embedded() {
        Ok(catalog) => catalog
            .listed_models(provider)
            .into_iter()
            .map(|model| {
                let label = onboarding_model_warning(provider, model).map_or_else(
                    || model.display_name.clone(),
                    |warning| format!("{} ({warning})", model.display_name),
                );
                (model.api_model.clone(), label)
            })
            .collect(),
        Err(error) => {
            tracing::error!(error = %error, "embedded model catalog is invalid");
            Vec::new()
        }
    }
}

pub(crate) fn prioritize_default_model(provider_name: &str, models: &mut Vec<(String, String)>) {
    let default = default_model_for_provider(provider_name);
    if let Some(index) = models.iter().position(|(model, _)| model == &default)
        && index != 0
    {
        let entry = models.remove(index);
        models.insert(0, entry);
    }
}

pub(crate) fn supports_live_model_fetch(provider_name: &str) -> bool {
    matches!(
        canonical_provider_name(provider_name),
        "openrouter"
            | "openai"
            | "anthropic"
            | "groq"
            | "mistral"
            | "deepseek"
            | "xai"
            | "together-ai"
            | "gemini"
            | "ollama"
            | "llamacpp"
            | "kimi-code"
            | "litellm"
            | "vllm"
            | "huggingface"
            | "compatible"
            | "astrai"
            | "venice"
            | "fireworks"
            | "cohere"
            | "moonshot"
            | "glm"
            | "zai"
            | "qwen"
            | "nvidia"
    )
}

pub(crate) fn models_endpoint_for_provider(provider_name: &str) -> Option<&'static str> {
    match provider_name {
        "qwen-intl" => Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/models"),
        "dashscope-us" => Some("https://dashscope-us.aliyuncs.com/compatible-mode/v1/models"),
        "moonshot-cn" | "kimi-cn" => Some("https://api.moonshot.cn/v1/models"),
        "glm-cn" | "bigmodel" => Some("https://open.bigmodel.cn/api/paas/v4/models"),
        "zai-cn" | "z.ai-cn" => Some("https://open.bigmodel.cn/api/coding/paas/v4/models"),
        _ => match canonical_provider_name(provider_name) {
            "openai" => Some("https://api.openai.com/v1/models"),
            "venice" => Some("https://api.venice.ai/api/v1/models"),
            "groq" => Some("https://api.groq.com/openai/v1/models"),
            "mistral" => Some("https://api.mistral.ai/v1/models"),
            "deepseek" => Some("https://api.deepseek.com/v1/models"),
            "xai" => Some("https://api.x.ai/v1/models"),
            "together-ai" => Some("https://api.together.xyz/v1/models"),
            "fireworks" => Some("https://api.fireworks.ai/inference/v1/models"),
            "cohere" => Some("https://api.cohere.com/compatibility/v1/models"),
            "moonshot" => Some("https://api.moonshot.ai/v1/models"),
            "glm" => Some("https://api.z.ai/api/paas/v4/models"),
            "zai" => Some("https://api.z.ai/api/coding/paas/v4/models"),
            "qwen" => Some("https://dashscope.aliyuncs.com/compatible-mode/v1/models"),
            "nvidia" => Some("https://integrate.api.nvidia.com/v1/models"),
            "astrai" => Some("https://as-trai.com/v1/models"),
            "llamacpp" => Some("http://localhost:8080/v1/models"),
            "kimi-code" => Some("https://api.kimi.com/coding/v1/models"),
            "litellm" => Some("http://localhost:4000/v1/models"),
            "vllm" => Some("http://localhost:8000/v1/models"),
            "huggingface" => Some("https://router.huggingface.co/v1/models"),
            _ => None,
        },
    }
}

pub(crate) fn build_model_fetch_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(4))
        .user_agent(crate::PRX_USER_AGENT)
        .build()
        .context("failed to build model-fetch HTTP client")
}

pub(crate) fn fetch_openai_compatible_models(
    endpoint: &str,
    api_key: Option<&str>,
    allow_unauthenticated: bool,
) -> Result<Vec<DiscoveredModel>> {
    let client = build_model_fetch_client()?;
    let mut request = client.get(endpoint);
    // An `api_url` may carry `user:password@`; errors name the endpoint only.
    let shown_endpoint = crate::providers::redact_endpoint_userinfo(endpoint);

    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    } else if !allow_unauthenticated {
        bail!("model fetch requires API key for endpoint {shown_endpoint}");
    }

    let payload: Value = request
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| anyhow::anyhow!("{}", crate::providers::redact_endpoint_userinfo(&error.to_string())))
        .with_context(|| format!("model fetch failed: GET {shown_endpoint}"))?
        .json()
        .context("failed to parse model list response")?;

    Ok(discovery::parse_openai_compatible_models(&payload))
}

pub(crate) fn fetch_openrouter_models(api_key: Option<&str>) -> Result<Vec<DiscoveredModel>> {
    let client = build_model_fetch_client()?;
    let mut request = client.get("https://openrouter.ai/api/v1/models");
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }

    let payload: Value = request
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .context("model fetch failed: GET https://openrouter.ai/api/v1/models")?
        .json()
        .context("failed to parse OpenRouter model list response")?;

    Ok(discovery::parse_openai_compatible_models(&payload))
}

pub(crate) fn fetch_anthropic_models(api_key: Option<&str>) -> Result<Vec<DiscoveredModel>> {
    let Some(api_key) = api_key else {
        bail!("Anthropic model fetch requires API key or OAuth token");
    };

    if is_claude_code_oauth_setup_token(api_key) {
        bail!("{}", crate::auth::terms::ANTHROPIC_SUBSCRIPTION_REMOVED);
    }
    let client = build_model_fetch_client()?;
    let request = client
        .get("https://api.anthropic.com/v1/models")
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", api_key);

    let response = request
        .send()
        .context("model fetch failed: GET https://api.anthropic.com/v1/models")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        bail!("Anthropic model list request failed (HTTP {status}): {body}");
    }

    let payload: Value = response
        .json()
        .context("failed to parse Anthropic model list response")?;

    Ok(discovery::parse_openai_compatible_models(&payload))
}

pub(crate) fn fetch_gemini_models(api_key: Option<&str>) -> Result<Vec<DiscoveredModel>> {
    let Some(api_key) = api_key else {
        bail!("Gemini model fetch requires API key");
    };

    let client = build_model_fetch_client()?;
    let payload: Value = client
        .get("https://generativelanguage.googleapis.com/v1beta/models")
        .query(&[("key", api_key), ("pageSize", "200")])
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .context("model fetch failed: GET Gemini models")?
        .json()
        .context("failed to parse Gemini model list response")?;

    Ok(discovery::parse_gemini_models(&payload))
}

pub(crate) fn fetch_ollama_models() -> Result<Vec<DiscoveredModel>> {
    let client = build_model_fetch_client()?;
    let payload: Value = client
        .get("http://localhost:11434/api/tags")
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .context("model fetch failed: GET http://localhost:11434/api/tags")?
        .json()
        .context("failed to parse Ollama model list response")?;

    Ok(discovery::parse_ollama_models(&payload))
}

pub(crate) fn resolve_live_models_endpoint(provider_name: &str, provider_api_url: Option<&str>) -> Option<String> {
    // The configured endpoint wherever provider construction honours it
    // (D-095 M-1): the key goes to the host the provider itself addresses,
    // never to the vendor's public host behind an operator's gateway.
    let configured = crate::providers::honoured_base_url(provider_name, provider_api_url).or_else(|| {
        (canonical_provider_name(provider_name) == "compatible")
            .then_some(provider_api_url)
            .flatten()
            .map(str::trim)
            .filter(|url| !url.is_empty())
    });
    if let Some(url) = configured {
        let path_ends = |suffix: &str| {
            reqwest::Url::parse(url).is_ok_and(|parsed| parsed.path().trim_end_matches('/').ends_with(suffix))
        };
        if path_ends("/models") {
            return crate::providers::endpoint_url(url, "/models");
        }
        if canonical_provider_name(provider_name) == "litellm" && !path_ends("/v1") {
            return crate::providers::endpoint_url(url, "/v1/models");
        }
        return crate::providers::endpoint_url(url, "/models");
    }

    models_endpoint_for_provider(provider_name).map(str::to_string)
}

pub(crate) fn fetch_live_models_for_provider(
    provider_name: &str,
    api_key: &str,
    provider_api_url: Option<&str>,
) -> Result<Vec<DiscoveredModel>> {
    let requested_provider_name = provider_name;
    let provider_name = canonical_provider_name(provider_name);
    let api_key = if api_key.trim().is_empty() {
        provider_credential_env_var(provider_name)
            .and_then(|variable| std::env::var(variable).ok())
            .or_else(|| {
                // Anthropic also accepts OAuth setup-tokens via ANTHROPIC_OAUTH_TOKEN
                if provider_name == "anthropic" {
                    std::env::var("ANTHROPIC_OAUTH_TOKEN").ok()
                } else if provider_name == "minimax" {
                    std::env::var("MINIMAX_OAUTH_TOKEN").ok()
                } else {
                    None
                }
            })
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    } else {
        Some(api_key.trim().to_string())
    };

    let models = match provider_name {
        "openrouter" => fetch_openrouter_models(api_key.as_deref())?,
        "anthropic" => fetch_anthropic_models(api_key.as_deref())?,
        "gemini" => fetch_gemini_models(api_key.as_deref())?,
        "ollama" => {
            if api_key.as_deref().map_or(true, |k| k.trim().is_empty()) {
                // Key is None or empty, assume local Ollama
                fetch_ollama_models()?
            } else {
                // Ollama Cloud does not expose a model-list endpoint. Its
                // release-owned fallback comes from the same catalog as every
                // other offline model surface.
                let mut models = crate::providers::catalog::ReleaseCatalog::embedded()
                    .map_err(|error| anyhow::anyhow!("embedded model catalog is invalid: {error}"))?
                    .listed_models("ollama")
                    .into_iter()
                    .map(|model| DiscoveredModel::id_only(&model.api_model))
                    .collect::<Vec<_>>();
                // The credential means this is Ollama Cloud discovery. Keep
                // the release catalog as the source, but offer hosted ids
                // before local-only defaults.
                models.sort_by_key(|model| !model.id.ends_with(":cloud"));
                models
            }
        }
        _ => {
            if let Some(endpoint) = resolve_live_models_endpoint(requested_provider_name, provider_api_url) {
                let allow_unauthenticated = allows_unauthenticated_model_fetch(requested_provider_name);
                fetch_openai_compatible_models(&endpoint, api_key.as_deref(), allow_unauthenticated)?
            } else {
                Vec::new()
            }
        }
    };

    Ok(models)
}

/// Async boundary for the blocking model-discovery fan-out.
///
/// `reqwest::blocking` enters and drops a temporary Tokio runtime on the calling
/// thread, so running it on an async worker panics with "Cannot drop a runtime in
/// a context where blocking is not allowed". `prx models refresh` already routes
/// this exact fan-out through `runtime::blocking::spawn_blocking`; the
/// interactive wizard uses the same boundary.
///
/// Arguments are owned because the blocking closure must be `'static`.
pub(crate) async fn fetch_live_models_for_provider_off_runtime(
    provider_name: String,
    api_key: String,
    provider_api_url: Option<String>,
) -> Result<Vec<DiscoveredModel>> {
    crate::runtime::blocking::spawn_blocking(move || {
        fetch_live_models_for_provider(&provider_name, &api_key, provider_api_url.as_deref())
    })
    .await
    .map_err(|error| anyhow::anyhow!("model discovery task failed: {error}"))?
}

/// The credential and base URL model discovery may present to a provider.
pub(crate) struct DiscoveryConnection<'a> {
    /// Empty when the provider has no credential of its own.
    pub(crate) api_key: String,
    pub(crate) api_url: Option<&'a str>,
}

/// What `prx models refresh`, the read-only probe and `prx doctor models`
/// (which probes every catalog provider) send to `provider_name`.
///
/// The root `api_key` and `api_url` belong to the configured default provider
/// and are released only on a provider-identity match; any other provider is
/// probed with its own auth profile (or its own environment variable, read by
/// the fetcher), or without a credential.
pub(crate) fn model_discovery_connection<'a>(config: &'a Config, provider_name: &str) -> DiscoveryConnection<'a> {
    let configured_provider = crate::providers::selection::default_provider(config);
    let root = crate::providers::configured_connection_for(
        provider_name,
        configured_provider,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
    );
    let api_key = root
        .api_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map_or_else(|| model_discovery_api_key(config, provider_name), str::to_string);
    DiscoveryConnection {
        api_key,
        api_url: root.api_url,
    }
}

/// The selected provider's own auth-profile credential, or empty.
pub(crate) fn model_discovery_api_key(config: &Config, provider_name: &str) -> String {
    let state_dir = config
        .config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::config::default_openprx_dir().unwrap_or_else(|| PathBuf::from(".openprx")));
    let store = AuthProfilesStore::new(&state_dir, config.secrets.encrypt);
    let Ok(data) = store.load_read_only() else {
        return String::new();
    };
    let canonical = canonical_provider_name(provider_name);
    let Some(profile_id) = select_profile_id(&data, canonical, None) else {
        return String::new();
    };
    let Some(profile) = data.profiles.get(&profile_id) else {
        return String::new();
    };
    profile
        .token
        .as_deref()
        .or_else(|| profile.token_set.as_ref().map(|tokens| tokens.access_token.as_str()))
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .unwrap_or_default()
        .to_string()
}

#[derive(Debug, Clone)]
pub(crate) struct CachedModels {
    pub(crate) models: Vec<String>,
    pub(crate) age_secs: u64,
}

pub(crate) fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Record a fetched catalog, with every model's metadata, in the model cache.
pub(crate) fn cache_live_models_for_provider(
    workspace_dir: &Path,
    provider_name: &str,
    models: &[DiscoveredModel],
) -> Result<()> {
    discovery::record_discovered(workspace_dir, provider_name, models, now_unix_secs())
}

pub(crate) fn load_cached_models_for_provider_internal(
    workspace_dir: &Path,
    provider_name: &str,
    ttl_secs: Option<u64>,
) -> Result<Option<CachedModels>> {
    let state = discovery::load_model_cache_state(workspace_dir)?;
    let now = now_unix_secs();

    let Some(entry) = state.entries.into_iter().find(|entry| entry.provider == provider_name) else {
        return Ok(None);
    };

    if entry.models.is_empty() {
        return Ok(None);
    }

    let age_secs = now.saturating_sub(entry.fetched_at_unix);
    if ttl_secs.is_some_and(|ttl| age_secs > ttl) {
        return Ok(None);
    }

    Ok(Some(CachedModels {
        models: entry.models,
        age_secs,
    }))
}

pub(crate) fn load_cached_models_for_provider(
    workspace_dir: &Path,
    provider_name: &str,
    ttl_secs: u64,
) -> Result<Option<CachedModels>> {
    load_cached_models_for_provider_internal(workspace_dir, provider_name, Some(ttl_secs))
}

pub(crate) fn load_any_cached_models_for_provider(
    workspace_dir: &Path,
    provider_name: &str,
) -> Result<Option<CachedModels>> {
    load_cached_models_for_provider_internal(workspace_dir, provider_name, None)
}

pub(crate) fn humanize_age(age_secs: u64) -> String {
    if age_secs < 60 {
        format!("{age_secs}s")
    } else if age_secs < 60 * 60 {
        format!("{}m", age_secs / 60)
    } else {
        format!("{}h", age_secs / (60 * 60))
    }
}

pub(crate) fn build_model_options(provider_name: &str, model_ids: Vec<String>, source: &str) -> Vec<(String, String)> {
    let provider_name = canonical_provider_name(provider_name);
    let provider = crate::providers::ProviderId::of(provider_name);
    let release = crate::providers::catalog::ReleaseCatalog::embedded().ok();
    build_model_options_with_warnings(model_ids, source, |model_id| {
        release
            .as_ref()
            .and_then(|catalog| catalog.model(&provider, model_id))
            .and_then(|model| onboarding_model_warning(provider_name, model))
    })
}

pub(crate) fn build_model_options_with_warnings(
    model_ids: Vec<String>,
    source: &str,
    mut warning_for: impl FnMut(&str) -> Option<String>,
) -> Vec<(String, String)> {
    model_ids
        .into_iter()
        .map(|model_id| {
            let warning = warning_for(&model_id);
            let label = warning.map_or_else(
                || format!("{model_id} ({source})"),
                |warning| format!("{model_id} ({source}; {warning})"),
            );
            (model_id, label)
        })
        .collect()
}

#[derive(Debug)]
pub(crate) enum WizardModelFetchStatus {
    Fetched { fetched_count: usize, shown_count: usize },
    Empty,
    Failed { error: String, stale_age_secs: Option<u64> },
}

pub(crate) fn apply_wizard_model_fetch(
    workspace_dir: &Path,
    provider_name: &str,
    live_options: &mut Option<Vec<(String, String)>>,
    result: Result<Vec<DiscoveredModel>>,
) -> Result<WizardModelFetchStatus> {
    match result {
        Ok(live_models) if !live_models.is_empty() => {
            cache_live_models_for_provider(workspace_dir, provider_name, &live_models)?;
            let live_model_ids = discovery::chat_model_ids(&live_models);
            let fetched_count = live_model_ids.len();
            let shown_count = fetched_count.min(LIVE_MODEL_MAX_OPTIONS);
            *live_options = Some(build_model_options(
                provider_name,
                live_model_ids.into_iter().take(LIVE_MODEL_MAX_OPTIONS).collect(),
                "live",
            ));
            Ok(WizardModelFetchStatus::Fetched {
                fetched_count,
                shown_count,
            })
        }
        Ok(_) => Ok(WizardModelFetchStatus::Empty),
        Err(error) => {
            let mut stale_age_secs = None;
            if live_options.is_none()
                && let Some(stale) = load_any_cached_models_for_provider(workspace_dir, provider_name)?
            {
                stale_age_secs = Some(stale.age_secs);
                *live_options = Some(build_model_options(
                    provider_name,
                    stale.models.into_iter().take(LIVE_MODEL_MAX_OPTIONS).collect(),
                    "stale-cache",
                ));
            }
            Ok(WizardModelFetchStatus::Failed {
                error: error.to_string(),
                stale_age_secs,
            })
        }
    }
}

pub(crate) fn print_model_preview(models: &[String]) {
    for model in models.iter().take(MODEL_PREVIEW_LIMIT) {
        println!("  {} {model}", style("-"));
    }

    if models.len() > MODEL_PREVIEW_LIMIT {
        println!("  {} ... and {} more", style("-"), models.len() - MODEL_PREVIEW_LIMIT);
    }
}

pub fn run_models_list(config: &Config, provider_override: Option<&str>) -> Result<()> {
    let provider_name = provider_override
        .or(config.default_provider.as_deref())
        .unwrap_or("openrouter")
        .trim()
        .to_string();

    if provider_name.is_empty() {
        anyhow::bail!("Provider name cannot be empty");
    }
    // Printed and quoted in errors: a `custom:` URL may carry `user:password@`.
    let shown = crate::security::redaction::redact_provider_text(&provider_name);

    let canonical = canonical_provider_name(&provider_name).to_string();
    let cached = load_any_cached_models_for_provider(&config.workspace_dir, &canonical)?.or_else(|| {
        load_any_cached_models_for_provider(&config.workspace_dir, &provider_name)
            .ok()
            .flatten()
    });
    let release_catalog = crate::providers::catalog::ReleaseCatalog::embedded()
        .map_err(|error| anyhow::anyhow!("embedded model catalog is invalid: {error}"))?;
    let catalog_models = catalog_models_for_provider(&canonical);

    println!("Model catalog for '{}':", shown);
    if provider_override.is_none() {
        match crate::providers::selection::default_status(config) {
            Ok((_, line)) | Err(line) => println!("  {line}"),
        }
    }
    if let Some(cached) = cached {
        println!(
            "  Source: cached live catalog (updated {} ago)",
            humanize_age(cached.age_secs)
        );
        println!("  Models: {}", cached.models.len());
        print_model_preview(&cached.models);
        if cached.models.len() > MODEL_PREVIEW_LIMIT {
            println!("  Tip: use `prx models refresh --provider {shown} --force` to refresh this cache.");
        }
    } else if !catalog_models.is_empty() {
        println!(
            "  Source: embedded release catalog {} (no live cache found)",
            release_catalog.revision
        );
        println!("  Models: {}", catalog_models.len());
        for (model, label) in catalog_models.iter().take(MODEL_PREVIEW_LIMIT) {
            println!("  {} {model} — {label}", style("-"));
        }
        println!("  Tip: run `prx models refresh --provider {shown}` to cache a live catalog when supported.");
    } else {
        println!("  No cached or embedded models found.");
        println!(
            "  Tip: run `prx providers` to see supported providers, then `prx models refresh --provider <provider>`."
        );
    }

    Ok(())
}

pub fn run_models_refresh(config: &Config, provider_override: Option<&str>, force: bool) -> Result<()> {
    let provider_name = provider_override
        .or(config.default_provider.as_deref())
        .unwrap_or("openrouter")
        .trim()
        .to_string();

    if provider_name.is_empty() {
        anyhow::bail!("Provider name cannot be empty");
    }
    // Printed and quoted in errors: a `custom:` URL may carry `user:password@`.
    let shown = crate::security::redaction::redact_provider_text(&provider_name);

    if !supports_live_model_fetch(&provider_name) {
        anyhow::bail!("Provider '{shown}' does not support live model discovery yet");
    }

    if !force {
        if let Some(cached) =
            load_cached_models_for_provider(&config.workspace_dir, &provider_name, MODEL_CACHE_TTL_SECS)?
        {
            println!(
                "Using cached model list for '{}' (updated {} ago):",
                shown,
                humanize_age(cached.age_secs)
            );
            print_model_preview(&cached.models);
            println!();
            println!(
                "Tip: run `prx models refresh --force --provider {}` to fetch latest now.",
                shown
            );
            return Ok(());
        }
    }

    let connection = model_discovery_connection(config, &provider_name);

    match fetch_live_models_for_provider(&provider_name, &connection.api_key, connection.api_url) {
        Ok(models) if !models.is_empty() => {
            cache_live_models_for_provider(&config.workspace_dir, &provider_name, &models)?;
            println!("Refreshed '{}' model cache with {} models.", shown, models.len());
            print_model_preview(&discovery::chat_model_ids(&models));
            Ok(())
        }
        Ok(_) => {
            if let Some(stale_cache) = load_any_cached_models_for_provider(&config.workspace_dir, &provider_name)? {
                println!(
                    "Provider returned no models; using stale cache (updated {} ago):",
                    humanize_age(stale_cache.age_secs)
                );
                print_model_preview(&stale_cache.models);
                return Ok(());
            }

            anyhow::bail!("Provider '{}' returned an empty model list", shown)
        }
        Err(error) => {
            if let Some(stale_cache) = load_any_cached_models_for_provider(&config.workspace_dir, &provider_name)? {
                println!(
                    "Live refresh failed ({}). Falling back to stale cache (updated {} ago):",
                    error,
                    humanize_age(stale_cache.age_secs)
                );
                print_model_preview(&stale_cache.models);
                return Ok(());
            }

            Err(error).with_context(|| format!("failed to refresh models for provider '{shown}'"))
        }
    }
}

/// Probe a provider model catalog without creating or updating the workspace
/// cache. A fresh or stale cache may be read as fallback evidence only.
pub fn run_models_probe_read_only(config: &Config, provider_override: Option<&str>, prefer_cache: bool) -> Result<()> {
    let provider_name = provider_override
        .or(config.default_provider.as_deref())
        .unwrap_or("openrouter")
        .trim()
        .to_string();
    if provider_name.is_empty() {
        anyhow::bail!("Provider name cannot be empty");
    }
    // Printed and quoted in errors: a `custom:` URL may carry `user:password@`.
    let shown = crate::security::redaction::redact_provider_text(&provider_name);
    if !supports_live_model_fetch(&provider_name) {
        anyhow::bail!("Provider '{shown}' does not support live model discovery yet");
    }

    if prefer_cache
        && let Some(cached) =
            load_cached_models_for_provider(&config.workspace_dir, &provider_name, MODEL_CACHE_TTL_SECS)?
    {
        println!(
            "Using cached model list for '{}' as read-only evidence (updated {} ago):",
            shown,
            humanize_age(cached.age_secs)
        );
        print_model_preview(&cached.models);
        return Ok(());
    }

    let connection = model_discovery_connection(config, &provider_name);
    match fetch_live_models_for_provider(&provider_name, &connection.api_key, connection.api_url) {
        Ok(models) if !models.is_empty() => {
            println!(
                "Read-only live probe returned {} model(s) for '{}'; cache was not updated.",
                models.len(),
                shown
            );
            print_model_preview(&discovery::chat_model_ids(&models));
            Ok(())
        }
        Ok(_) => anyhow::bail!("Provider '{shown}' returned an empty model list"),
        Err(error) => {
            if let Some(stale_cache) = load_any_cached_models_for_provider(&config.workspace_dir, &provider_name)? {
                println!(
                    "Live read-only probe failed ({error}); stale cache exists as evidence (updated {} ago):",
                    humanize_age(stale_cache.age_secs)
                );
                print_model_preview(&stale_cache.models);
                return Ok(());
            }
            Err(error).with_context(|| format!("failed to probe models for provider '{shown}'"))
        }
    }
}

/// Map provider name to its conventional env var
/// The environment variable that holds `name`'s own credential, if the
/// provider has one. Model discovery reads only this.
pub(crate) fn provider_credential_env_var(name: &str) -> Option<&'static str> {
    Some(match canonical_provider_name(name) {
        "openrouter" => "OPENROUTER_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "ollama" => "OLLAMA_API_KEY",
        "llamacpp" => "LLAMACPP_API_KEY",
        "venice" => "VENICE_API_KEY",
        "groq" => "GROQ_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "xai" => "XAI_API_KEY",
        "together-ai" => "TOGETHER_API_KEY",
        "fireworks" | "fireworks-ai" => "FIREWORKS_API_KEY",
        "perplexity" => "PERPLEXITY_API_KEY",
        "cohere" => "COHERE_API_KEY",
        "kimi-code" => "KIMI_CODE_API_KEY",
        "moonshot" => "MOONSHOT_API_KEY",
        "glm" => "GLM_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "qwen" => "DASHSCOPE_API_KEY",
        "qianfan" => "QIANFAN_API_KEY",
        "zai" => "ZAI_API_KEY",
        "synthetic" => "SYNTHETIC_API_KEY",
        "opencode" | "opencode-zen" => "OPENCODE_API_KEY",
        "vercel" | "vercel-ai" => "VERCEL_API_KEY",
        "cloudflare" | "cloudflare-ai" => "CLOUDFLARE_API_KEY",
        "bedrock" | "aws-bedrock" => "AWS_ACCESS_KEY_ID",
        "gemini" => "GEMINI_API_KEY",
        "nvidia" | "nvidia-nim" | "build.nvidia.com" => "NVIDIA_API_KEY",
        "astrai" => "ASTRAI_API_KEY",
        _ => return None,
    })
}
