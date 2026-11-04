//! Provider runtime options and tool dialect declarations.

use super::{ByoDialect, ConfiguredProviders, PathBuf, ProviderId, ProviderIdentity, classify_provider};

/// One operator declaration of what an endpoint accepts in a tool schema.
///
/// PRX cannot know what an arbitrary `custom:<url>` endpoint will accept, and a
/// URL is not evidence, so an undeclared endpoint keeps
/// [`ToolSchemaDialect::Raw`](crate::tools::ToolSchemaDialect::Raw) — the
/// strongest enforcement contract — and the first transport rejection carries
/// the diagnostic that says which declaration would fix it.
///
/// Filled from `[[providers.tool_dialects]]` by
/// [`provider_runtime_options_from_config`]. The declaration belongs to the
/// per-(provider, model) capability descriptor, and that descriptor replaces
/// this struct when it lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchemaDialectDeclaration {
    /// The provider it applies to. Compared through
    /// [`canonical_provider_key`], so every spelling of one provider identity
    /// shares the declaration.
    pub provider: String,
    /// The model the declaration applies to, or every model of that provider.
    pub model: Option<String>,
    /// What that endpoint accepts.
    pub dialect: crate::tools::ToolSchemaDialect,
}

/// Which tool-call grammars an endpoint was declared to emit.
///
/// A model served behind an OpenAI-compatible API answers a tool request in
/// the provider's native `tool_calls` array. Some chat templates *also* emit a
/// textual grammar in the content — `<function=NAME>…</function>` is the
/// Qwen/Llama spelling — and reading it is a capability, not a courtesy: it
/// decides whether prose an endpoint produced becomes an executed tool call.
/// So it is declared, and the default is that nothing textual is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ToolCallDialect {
    /// Only the provider's native `tool_calls` array is a tool call.
    #[default]
    Native,
    /// Additionally read `<function=NAME>{json}</function>` from the content.
    ///
    /// The tool name must be one this request registered and the body must
    /// parse as JSON; a block failing either is reported, never executed.
    XmlFunctionTags,
}

/// One operator declaration of a textual tool-call grammar an endpoint emits.
///
/// Like [`ToolSchemaDialectDeclaration`], this is filled from
/// `[[providers.tool_dialects]]` and is replaced by the per-(provider, model)
/// capability descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallDialectDeclaration {
    /// The provider it applies to, compared through [`canonical_provider_key`].
    pub provider: String,
    /// The model the declaration applies to, or every model of that provider.
    pub model: Option<String>,
    /// What that endpoint emits.
    pub dialect: ToolCallDialect,
}

#[derive(Debug, Clone)]
pub struct ProviderRuntimeOptions {
    /// The global baseline; invocation-specific temperatures remain higher precedence.
    pub default_temperature: Option<f64>,
    pub model_settings:
        Option<std::sync::Arc<std::collections::BTreeMap<String, crate::config::model_settings::ModelSettings>>>,
    pub auth_profile_override: Option<String>,
    pub openprx_dir: Option<PathBuf>,
    pub secrets_encrypt: bool,
    pub codex_auth_json_path: Option<PathBuf>,
    pub codex_auth_json_auto_import: bool,
    pub reasoning_enabled: Option<bool>,
    pub codex_stream_idle_timeout_secs: Option<u64>,
    pub codex_reasoning_effort: Option<String>,
    pub ollama_num_ctx: Option<usize>,
    pub ollama_model_num_ctx: Vec<(String, usize)>,
    /// What each declared endpoint accepts in a tool schema. Empty means every
    /// endpoint is offered the published document unchanged.
    pub tool_schema_dialects: Vec<ToolSchemaDialectDeclaration>,
    /// Which textual tool-call grammar each declared endpoint emits. Empty
    /// means only native `tool_calls` are read, anywhere.
    pub tool_call_dialects: Vec<ToolCallDialectDeclaration>,
}

impl ProviderRuntimeOptions {
    /// The `(model, dialect)` declarations that apply to `provider`.
    ///
    /// Matched by provider identity, not by spelling: a padded, differently
    /// cased or trailing-slash label is the provider C1's identity says it is,
    /// and a different URL path is a different endpoint.
    #[must_use]
    pub fn declared_tool_schema_dialects(
        &self,
        provider: &str,
    ) -> Vec<(Option<String>, crate::tools::ToolSchemaDialect)> {
        if self.tool_schema_dialects.is_empty() {
            return Vec::new();
        }
        let id = ProviderId::of(provider);
        self.tool_schema_dialects
            .iter()
            .filter(|declaration| ProviderId::of(&declaration.provider) == id)
            .map(|declaration| (declaration.model.clone(), declaration.dialect))
            .collect()
    }

    /// The `(model, dialect)` textual tool-call declarations for `provider`,
    /// matched by provider identity like
    /// [`declared_tool_schema_dialects`](Self::declared_tool_schema_dialects).
    #[must_use]
    pub fn declared_tool_call_dialects(&self, provider: &str) -> Vec<(Option<String>, ToolCallDialect)> {
        if self.tool_call_dialects.is_empty() {
            return Vec::new();
        }
        let id = ProviderId::of(provider);
        self.tool_call_dialects
            .iter()
            .filter(|declaration| ProviderId::of(&declaration.provider) == id)
            .map(|declaration| (declaration.model.clone(), declaration.dialect))
            .collect()
    }
}

impl Default for ProviderRuntimeOptions {
    fn default() -> Self {
        Self {
            default_temperature: None,
            model_settings: Default::default(),
            auth_profile_override: None,
            openprx_dir: None,
            secrets_encrypt: true,
            codex_auth_json_path: None,
            codex_auth_json_auto_import: false,
            reasoning_enabled: None,
            codex_stream_idle_timeout_secs: None,
            codex_reasoning_effort: None,
            ollama_num_ctx: None,
            ollama_model_num_ctx: Vec::new(),
            tool_schema_dialects: Vec::new(),
            tool_call_dialects: Vec::new(),
        }
    }
}

#[must_use]
pub fn provider_runtime_options_from_config(config: &crate::config::Config) -> ProviderRuntimeOptions {
    ProviderRuntimeOptions {
        default_temperature: Some(config.default_temperature),
        model_settings: (!config.model_settings.is_empty()).then(|| std::sync::Arc::new(config.model_settings.clone())),
        auth_profile_override: None,
        openprx_dir: config.config_path.parent().map(PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        codex_auth_json_path: Some(config.auth.codex_auth_json_path.clone()),
        codex_auth_json_auto_import: config.auth.codex_auth_json_auto_import,
        reasoning_enabled: config.runtime.reasoning_enabled,
        codex_stream_idle_timeout_secs: config.runtime.codex_stream_idle_timeout_secs,
        codex_reasoning_effort: config.runtime.codex_reasoning_effort.clone(),
        ollama_num_ctx: config.providers.ollama.num_ctx,
        ollama_model_num_ctx: ollama_model_num_ctx(config),
        // `[[providers.tool_dialects]]`. An endpoint with no entry keeps the
        // published document (`Raw`) and native tool calls only; nothing is
        // inferred from its URL.
        tool_schema_dialects: tool_schema_dialects_from_config(&config.providers),
        tool_call_dialects: tool_call_dialects_from_config(&config.providers),
    }
}

/// The schema dialects declared in `[[providers.tool_dialects]]` and
/// `[[providers.models]]`. The loader refuses a `(provider, model)` that
/// declares one in both tables, so the two lists never disagree.
pub(super) fn tool_schema_dialects_from_config(
    providers: &crate::config::ProvidersConfig,
) -> Vec<ToolSchemaDialectDeclaration> {
    let from_key = providers
        .tool_dialects
        .iter()
        .filter_map(|entry| Some((entry.provider.as_str(), entry.model.as_deref(), entry.tool_schema?)));
    let from_models = providers.models.iter().filter_map(|entry| {
        Some((
            entry.provider.as_str(),
            entry.model.as_deref(),
            entry.tool_schema_dialect?,
        ))
    });
    from_key
        .chain(from_models)
        .map(|(provider, model, setting)| ToolSchemaDialectDeclaration {
            provider: provider.trim().to_string(),
            model: declared_model(model),
            dialect: tool_schema_dialect_setting(setting),
        })
        .collect()
}

/// The textual tool-call grammars declared in `[[providers.tool_dialects]]`
/// and `[[providers.models]]`.
pub(super) fn tool_call_dialects_from_config(
    providers: &crate::config::ProvidersConfig,
) -> Vec<ToolCallDialectDeclaration> {
    let from_key = providers
        .tool_dialects
        .iter()
        .filter_map(|entry| Some((entry.provider.as_str(), entry.model.as_deref(), entry.tool_call?)));
    let from_models = providers.models.iter().filter_map(|entry| {
        Some((
            entry.provider.as_str(),
            entry.model.as_deref(),
            entry.tool_call_dialect?,
        ))
    });
    from_key
        .chain(from_models)
        .map(|(provider, model, setting)| ToolCallDialectDeclaration {
            provider: provider.trim().to_string(),
            model: declared_model(model),
            dialect: tool_call_dialect_setting(setting),
        })
        .collect()
}

/// The tool-schema dialect a configuration setting names.
pub(crate) const fn tool_schema_dialect_setting(
    setting: crate::config::schema::ToolSchemaDialectSetting,
) -> crate::tools::ToolSchemaDialect {
    match setting {
        crate::config::schema::ToolSchemaDialectSetting::Raw => crate::tools::ToolSchemaDialect::Raw,
        crate::config::schema::ToolSchemaDialectSetting::NoRootCombinators => {
            crate::tools::ToolSchemaDialect::NoRootCombinators
        }
    }
}

/// The tool-call grammar a configuration setting names.
pub(crate) const fn tool_call_dialect_setting(
    setting: crate::config::schema::ToolCallDialectSetting,
) -> ToolCallDialect {
    match setting {
        crate::config::schema::ToolCallDialectSetting::Native => ToolCallDialect::Native,
        crate::config::schema::ToolCallDialectSetting::XmlFunctionTags => ToolCallDialect::XmlFunctionTags,
    }
}

pub(super) fn declared_model(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
}

/// The indices of `[[providers.tool_dialects]]` entries whose provider is none
/// of the providers the configuration uses: the default, the fallbacks, the
/// model routes, the agents and the router's candidate models.
///
/// Such an entry is harmless, but it is usually a typo'd port or path, and the
/// endpoint the operator meant then keeps failing with its own HTTP 400.
pub(crate) fn unused_tool_dialect_entries(config: &crate::config::Config) -> Vec<usize> {
    if config.providers.tool_dialects.is_empty() {
        return Vec::new();
    }
    let used = ConfiguredProviders::from_config(config)
        .with_providers(config.router.models.iter().map(|model| model.provider.as_str()));
    config
        .providers
        .tool_dialects
        .iter()
        .enumerate()
        .filter(|(_, entry)| !used.permits(&entry.provider))
        .map(|(index, _)| index)
        .collect()
}

/// The catalog entries whose constructor builds an `OpenAiCompatibleProvider`
/// against a server the operator runs, and therefore reads
/// `[[providers.tool_dialects]]` (D-020 Q3). Canonical catalog names.
pub(crate) const TOOL_DIALECT_DECLARABLE_CATALOG: [&str; 4] = ["vllm", "llamacpp", "lmstudio", "litellm"];

/// Whether the constructor `name` selects reads a tool-dialect declaration.
///
/// A `custom:<url>` endpoint with a URL that parses, and the catalog entries
/// in [`TOOL_DIALECT_DECLARABLE_CATALOG`] under any of their spellings. Every
/// other provider either has a fixed dialect (`gemini`, `anthropic`,
/// `anthropic-custom:`) or is a hosted catalog vendor whose constructor
/// ignores declarations, so accepting one would be a silently ignored key.
pub(crate) fn accepts_tool_dialect_declarations(name: &str) -> bool {
    match classify_provider(name) {
        ProviderIdentity::ByoEndpoint {
            dialect: ByoDialect::OpenAiCompatible,
            ..
        } => true,
        ProviderIdentity::Catalog(canonical) => TOOL_DIALECT_DECLARABLE_CATALOG.contains(&canonical),
        _ => false,
    }
}

pub(super) fn ollama_model_num_ctx(config: &crate::config::Config) -> Vec<(String, usize)> {
    // Runtime options are built before a generation catalog is available at
    // many call sites. Key canonicalization here uses the release catalog;
    // exact keys do not depend on catalog reloads (see docs/model-catalog.md).
    let catalog = super::catalog::CatalogView::release_only();
    let mut windows = std::collections::BTreeMap::new();
    for (key, settings) in &config.model_settings {
        let Some((provider, model)) = key.split_once('/') else {
            continue;
        };
        if super::ProviderId::of(provider).catalog_name() == Some("ollama")
            && super::context_window::model_settings_window_key(&catalog, &config.model_settings, provider, model)
                == *key
            && let Some(window) = settings.context_window
        {
            windows.insert(model.to_owned(), usize::try_from(window).unwrap_or(usize::MAX));
        }
    }
    const ROUTER_DEFAULT_MAX_CONTEXT_SENTINEL: usize = 1_000_000;
    for model in config.router.models.iter().filter(|model| {
        model.provider.eq_ignore_ascii_case("ollama")
            && model.max_context > 0
            && model.max_context < ROUTER_DEFAULT_MAX_CONTEXT_SENTINEL
    }) {
        let key = super::context_window::model_settings_window_key(
            &catalog,
            &config.model_settings,
            &model.provider,
            &model.model_id,
        );
        let window = config
            .model_settings
            .get(&key)
            .and_then(|settings| settings.context_window)
            .map(|window| usize::try_from(window).unwrap_or(usize::MAX))
            .unwrap_or(model.max_context);
        windows.entry(model.model_id.clone()).or_insert(window);
    }
    windows.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_model_windows_prefer_model_settings_and_keep_legacy_sentinel() {
        let config: crate::config::Config = toml::from_str(
            r#"
[model_settings."ollama/my-local-model:latest"]
context_window = 32768
[[router.models]]
provider = "ollama"
model_id = "my-local-model:latest"
max_context = 16384
[[router.models]]
provider = "OLLAMA"
model_id = "prx_router_only"
max_context = 8192
[[router.models]]
provider = "ollama"
model_id = "prx_default_window"
[[router.models]]
provider = "openai"
model_id = "prx_other_provider"
max_context = 8192
"#,
        )
        .unwrap();
        let windows = provider_runtime_options_from_config(&config).ollama_model_num_ctx;
        assert_eq!(windows.len(), 2);
        assert!(windows.contains(&("my-local-model:latest".into(), 32_768)));
        assert!(windows.contains(&("prx_router_only".into(), 8_192)));
        assert!(!windows.iter().any(|(model, _)| model == "prx_default_window"));
    }
    async fn factory_request_num_ctx(config: &crate::config::Config, model: &str) -> usize {
        use axum::{Json, Router, routing::post};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let app = Router::new().route(
            "/api/chat",
            post(move |Json(request): Json<serde_json::Value>| {
                let sender = sender.clone();
                async move {
                    sender.send(request).await.unwrap();
                    Json(serde_json::json!({"message": {"role": "assistant", "content": "ok"}, "done": true}))
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let options = provider_runtime_options_from_config(config);
        let provider = super::super::create_provider_with_url_and_options(
            "ollama",
            None.into(),
            Some(&format!("http://{address}")),
            &options,
        )
        .unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.chat_with_system(None, "hello", model, 0.0),
        )
        .await;
        server.abort();
        let _ = server.await;
        response.unwrap().unwrap();
        let request = receiver.recv().await.unwrap();
        usize::try_from(
            request
                .pointer("/options/num_ctx")
                .and_then(serde_json::Value::as_u64)
                .unwrap(),
        )
        .unwrap()
    }

    fn window(config: &crate::config::Config, model: &str) -> usize {
        super::super::context_window::resolve_effective_compaction_config(
            &config.agent.compaction,
            "ollama",
            model,
            super::super::context_window::ContextWindowSources::from_config(config),
            &[],
        )
        .config
        .max_context_tokens
    }

    #[tokio::test]
    async fn tagged_exact_key_window_matches_num_ctx() {
        let config = toml::from_str(
            r#"[model_settings."ollama/llama3.2:latest"]
context_window = 32768
[model_settings."ollama/llama3.2"]
context_window = 16384"#,
        )
        .unwrap();
        assert_eq!(window(&config, "llama3.2:latest"), 32_768, "exact-key window");
        assert_eq!(
            window(&config, "llama3.2:latest"),
            factory_request_num_ctx(&config, "llama3.2:latest").await,
            "exact-key factory num_ctx agrees with resolver"
        );
    }

    #[tokio::test]
    async fn row_id_key_window_matches_num_ctx_for_tagged_model() {
        let mut config: crate::config::Config = toml::from_str(
            r#"[model_settings."ollama/llama3.2"]
context_window = 32768"#,
        )
        .unwrap();
        assert_eq!(window(&config, "llama3.2:latest"), 32_768, "row-ID window");
        assert_eq!(
            window(&config, "llama3.2:latest"),
            factory_request_num_ctx(&config, "llama3.2:latest").await,
            "row-ID factory num_ctx agrees with resolver"
        );
        config.router.models.push(
            toml::from_str(
                r#"provider = "ollama"
model_id = "llama3.2:latest"
max_context = 16384"#,
            )
            .unwrap(),
        );
        assert_eq!(
            window(&config, "llama3.2:latest"),
            factory_request_num_ctx(&config, "llama3.2:latest").await,
            "row-ID settings still beat a tagged legacy router window"
        );
    }

    #[tokio::test]
    async fn warn_suggestion_for_tagged_row_resolves_to_same_window() {
        let legacy: crate::config::Config = toml::from_str(
            r#"[[router.models]]
provider = "ollama"
model_id = "llama3.2:latest"
max_context = 32768"#,
        )
        .unwrap();
        let warning = super::super::context_window::legacy_router_window_warning(&legacy).unwrap();
        let suggested = warning
            .split(" -> [model_settings.\"")
            .nth(1)
            .unwrap()
            .split("\"]")
            .next()
            .unwrap();
        assert_eq!(suggested, "ollama/llama3.2:latest");
        let migrated = toml::from_str(&format!("[model_settings.\"{suggested}\"]\ncontext_window = 32768")).unwrap();
        assert_eq!(window(&migrated, "llama3.2:latest"), window(&legacy, "llama3.2:latest"));
        assert_eq!(
            window(&migrated, "llama3.2:latest"),
            factory_request_num_ctx(&migrated, "llama3.2:latest").await,
            "suggested exact key preserves the legacy factory num_ctx"
        );
    }

    #[tokio::test]
    async fn two_tags_with_exact_keys_get_distinct_windows() {
        let config = toml::from_str(
            r#"[model_settings."ollama/llama3.2:latest"]
context_window = 32768
[model_settings."ollama/llama3.2:70b"]
context_window = 49152
[model_settings."ollama/llama3.2"]
context_window = 16384"#,
        )
        .unwrap();
        for (model, expected) in [("llama3.2:latest", 32768), ("llama3.2:70b", 49152), ("llama3.2", 16384)] {
            assert_eq!(window(&config, model), expected, "exact selected tag precedes row ID");
            assert_eq!(
                factory_request_num_ctx(&config, model).await,
                expected,
                "factory preserves each selected tag"
            );
        }
    }
}
