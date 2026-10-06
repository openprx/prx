//! What each provider family's adapter can execute, whatever a model claims.
//!
//! This is the upper bound of every descriptor: an operation with no concrete
//! transport in PRX is `Unsupported("no adapter …")` however a catalog or an
//! operator describes the model, and a chat feature the adapter never puts on
//! the wire cannot be switched on by metadata. A transport that exists proves
//! nothing about a particular model, so it bounds as `Unknown`, not as
//! `Supported`.
//!
//! The table is keyed by [`ProviderId`], so every spelling of one provider
//! lands on one row. `adapter_bounds_agree_with_the_constructed_providers`
//! pins it against what the factory actually builds.

use std::collections::BTreeMap;

use super::descriptor::{Dialects, FeatureSet, ModelOperation, Support};
use crate::providers::{ProviderId, ProviderNamespace, ToolCallDialect};
use crate::tools::ToolSchemaDialect;

/// The adapter implementation a provider's factory arm builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterFamily {
    /// `OpenAiCompatibleProvider`: `custom:` endpoints and most catalog rows.
    OpenAiCompatible,
    /// `OpenAiProvider` (`openai`, `ovhcloud`).
    OpenAi,
    /// `OpenRouterProvider`.
    OpenRouter,
    /// `AnthropicProvider` (`anthropic`, its aliases, `anthropic-custom:`).
    Anthropic,
    /// `GeminiProvider`.
    Gemini,
    /// `OllamaProvider`.
    Ollama,
    /// `BedrockProvider`.
    Bedrock,
    /// `OpenAiCodexProvider`.
    OpenAiCodex,
    /// `CopilotProvider`.
    Copilot,
    /// The test-only mock provider.
    Mock,
    /// A WASM plugin provider.
    WasmPlugin,
    /// A decision-only service (`decision:typesafe`): no chat, embedding or
    /// media transport, only the decision endpoint (D-040 OQ-A3).
    TypeSafeDecision,
}

/// The decision adapter a provider identity's decision factory builds
/// (D-036 item 4): an OpenAI-compatible `custom:` endpoint speaks System One
/// through the compatible adapter, `catalog:openrouter` has its own decision
/// adapter, and `decision:typesafe` is the hosted TypeSafe service. No other
/// identity has a decision transport, whatever a descriptor says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionAdapterKind {
    /// The System One compatible HTTP adapter.
    CompatibleHttp,
    /// The hosted TypeSafe adapter.
    TypeSafe,
    /// The hosted OpenRouter decision adapter.
    OpenRouter,
}

impl DecisionAdapterKind {
    /// Whether the adapter talks to a hosted service, whose model must be
    /// pinned (CDP 13.1; `crate::decision::model_pin`).
    #[must_use]
    pub const fn is_hosted(self) -> bool {
        matches!(self, Self::TypeSafe | Self::OpenRouter)
    }
}

/// The decision adapter for `provider`, or `None` when no decision
/// transport exists for it.
#[must_use]
pub fn decision_adapter(provider: &ProviderId) -> Option<DecisionAdapterKind> {
    match provider.namespace() {
        ProviderNamespace::Custom => Some(DecisionAdapterKind::CompatibleHttp),
        ProviderNamespace::DecisionService => {
            (provider.as_str() == "decision:typesafe").then_some(DecisionAdapterKind::TypeSafe)
        }
        ProviderNamespace::Catalog => {
            (provider.catalog_name() == Some("openrouter")).then_some(DecisionAdapterKind::OpenRouter)
        }
        ProviderNamespace::AnthropicCustom | ProviderNamespace::Wasm | ProviderNamespace::Unknown => None,
    }
}

/// Why an operation has no adapter anywhere in this release.
const NO_ADAPTER: &str = "no adapter executes this operation in this release";
/// Why streaming is refused for an adapter without a streaming transport.
const NO_STREAMING_TRANSPORT: &str = "this provider's adapter has no streaming transport";
/// Why embeddings are refused for a provider the embedding factory does not build.
const NO_EMBEDDING_ADAPTER: &str = "the embedding factory has no adapter for this provider";
/// Why a decision-only service refuses every other operation and feature.
const DECISION_ONLY: &str = "a decision-only service";

/// The upper bound for one provider: what its adapter can put on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterBound {
    pub family: AdapterFamily,
    pub operations: BTreeMap<ModelOperation, Support>,
    pub features: FeatureSet,
    /// The dialects the adapter itself fixes. A declaration can change them
    /// only where the adapter reads declarations.
    pub dialects: Dialects,
}

impl AdapterFamily {
    /// The family a provider identity's factory arm builds, or `None` when the
    /// identity names nothing this build can construct.
    #[must_use]
    pub fn of(provider: &ProviderId) -> Option<Self> {
        match provider.namespace() {
            ProviderNamespace::Custom => Some(Self::OpenAiCompatible),
            ProviderNamespace::AnthropicCustom => Some(Self::Anthropic),
            ProviderNamespace::Wasm => Some(Self::WasmPlugin),
            ProviderNamespace::DecisionService => Some(Self::TypeSafeDecision),
            ProviderNamespace::Unknown => None,
            ProviderNamespace::Catalog => Some(match provider.catalog_name().unwrap_or_default() {
                "openai" | "ovhcloud" => Self::OpenAi,
                "openrouter" => Self::OpenRouter,
                "anthropic" => Self::Anthropic,
                "gemini" => Self::Gemini,
                "ollama" => Self::Ollama,
                "bedrock" => Self::Bedrock,
                "openai-codex" => Self::OpenAiCodex,
                "copilot" => Self::Copilot,
                "mock" => Self::Mock,
                _ => Self::OpenAiCompatible,
            }),
        }
    }

    /// Whether the adapter has a streaming transport.
    const fn streams(self) -> bool {
        !matches!(self, Self::WasmPlugin | Self::TypeSafeDecision)
    }

    /// The tool-schema dialect the adapter fixes.
    const fn tool_schema_dialect(self) -> ToolSchemaDialect {
        match self {
            Self::Gemini => ToolSchemaDialect::Gemini,
            Self::Anthropic => ToolSchemaDialect::Anthropic,
            _ => ToolSchemaDialect::Raw,
        }
    }
}

/// Whether the embedding factory builds an adapter for `provider`.
///
/// Mirrors `create_embedding_provider_with_base_url`: `openai`, `openrouter`
/// and an OpenAI-compatible `custom:` endpoint.
fn embeds(provider: &ProviderId) -> bool {
    matches!(provider.namespace(), ProviderNamespace::Custom)
        || matches!(provider.catalog_name(), Some("openai" | "openrouter"))
}

/// The adapter bound for `provider`.
///
/// An identity this build cannot construct gets every operation
/// `Unsupported`, so it can never resolve.
#[must_use]
pub fn adapter_bound(provider: &ProviderId) -> AdapterBound {
    let Some(family) = AdapterFamily::of(provider) else {
        let reason = "PRX has no adapter for this provider";
        return AdapterBound {
            family: AdapterFamily::OpenAiCompatible,
            operations: ModelOperation::ALL
                .into_iter()
                .map(|operation| (operation, Support::unsupported(reason)))
                .collect(),
            features: FeatureSet {
                native_tools: Support::unsupported(reason),
                vision_input: Support::unsupported(reason),
                streaming: Support::unsupported(reason),
                structured_output: Support::unsupported(reason),
            },
            dialects: Dialects::default(),
        };
    };
    if family == AdapterFamily::TypeSafeDecision {
        return AdapterBound {
            family,
            operations: ModelOperation::ALL
                .into_iter()
                .map(|operation| {
                    let bound = if operation == ModelOperation::DecisionEndpoint {
                        Support::Unknown
                    } else {
                        Support::unsupported(DECISION_ONLY)
                    };
                    (operation, bound)
                })
                .collect(),
            features: FeatureSet {
                native_tools: Support::unsupported(DECISION_ONLY),
                vision_input: Support::unsupported(DECISION_ONLY),
                streaming: Support::unsupported(DECISION_ONLY),
                structured_output: Support::unsupported(DECISION_ONLY),
            },
            dialects: Dialects::default(),
        };
    }
    let decides = decision_adapter(provider).is_some();
    let operations = ModelOperation::ALL
        .into_iter()
        .map(|operation| {
            let bound = match operation {
                ModelOperation::Chat => Support::Unknown,
                ModelOperation::Embedding if embeds(provider) => Support::Unknown,
                ModelOperation::Embedding => Support::unsupported(NO_EMBEDDING_ADAPTER),
                ModelOperation::DecisionEndpoint if decides => Support::Unknown,
                ModelOperation::DecisionEndpoint
                | ModelOperation::Rerank
                | ModelOperation::ImageGeneration
                | ModelOperation::AudioGeneration
                | ModelOperation::VideoGeneration => Support::unsupported(NO_ADAPTER),
            };
            (operation, bound)
        })
        .collect();
    AdapterBound {
        family,
        operations,
        // Pinned against the constructed providers by
        // `adapter_bounds_agree_with_the_constructed_providers`; a WASM plugin
        // provider keeps the trait's non-streaming default.
        features: FeatureSet {
            streaming: if family.streams() {
                Support::Unknown
            } else {
                Support::unsupported(NO_STREAMING_TRANSPORT)
            },
            ..FeatureSet::unknown()
        },
        dialects: Dialects {
            tool_schema: family.tool_schema_dialect(),
            tool_call: ToolCallDialect::Native,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ByoDialect, ProviderRuntimeOptions, create_provider_with_options, list_providers};

    fn catalog_and_byo_names() -> Vec<String> {
        let mut names: Vec<String> = list_providers()
            .into_iter()
            .flat_map(|info| std::iter::once(info.name).chain(info.aliases.iter().copied()))
            .map(ToString::to_string)
            .collect();
        for dialect in [ByoDialect::OpenAiCompatible, ByoDialect::AnthropicCompatible] {
            names.push(format!("{}http://127.0.0.1:9/v1", dialect.prefix()));
        }
        names
    }

    #[test]
    fn every_catalog_row_alias_and_byo_dialect_has_an_adapter() {
        for name in catalog_and_byo_names() {
            let id = ProviderId::of(&name);
            assert!(
                AdapterFamily::of(&id).is_some(),
                "'{name}' ({id}) has no adapter family"
            );
        }
    }

    /// Every operation without a transport is unsupported. The decision
    /// endpoint has a transport exactly where `decision_adapter` names one
    /// (narrowed by CDP-02; see `the_decision_bound_agrees_with_the_decision_factory`).
    #[test]
    fn operations_without_a_transport_are_unsupported_everywhere() {
        for name in catalog_and_byo_names() {
            let id = ProviderId::of(&name);
            let bound = adapter_bound(&id);
            if decision_adapter(&id).is_none() {
                assert!(
                    bound
                        .operations
                        .get(&ModelOperation::DecisionEndpoint)
                        .is_some_and(Support::is_unsupported),
                    "'{name}' has no decision transport"
                );
            }
            for operation in [
                ModelOperation::Rerank,
                ModelOperation::ImageGeneration,
                ModelOperation::AudioGeneration,
                ModelOperation::VideoGeneration,
            ] {
                assert!(
                    bound.operations.get(&operation).is_some_and(Support::is_unsupported),
                    "'{name}' must not execute {operation}"
                );
            }
            assert!(
                !bound
                    .operations
                    .get(&ModelOperation::Chat)
                    .is_some_and(Support::is_unsupported),
                "'{name}' has a chat adapter"
            );
            assert!(
                !bound.operations.values().any(Support::is_supported),
                "an adapter proves no model's support: '{name}'"
            );
        }
    }

    /// The decision bound agrees with the decision adapter table: the decision
    /// endpoint is `Unknown` (a transport exists, no model is proven) exactly
    /// for the identities `decision_adapter` names, and `Unsupported`
    /// everywhere else. CDP-05 completes this guard by asserting that the
    /// decision factory builds an adapter for exactly these identities.
    #[test]
    fn the_decision_bound_agrees_with_the_decision_factory() {
        let mut names = catalog_and_byo_names();
        names.extend(["typesafe", "decision:typesafe", "DECISION:TypeSafe", "no-such-provider"].map(String::from));
        let mut with_adapter = Vec::new();
        for name in &names {
            let id = ProviderId::of(name);
            let decides = !adapter_bound(&id)
                .operations
                .get(&ModelOperation::DecisionEndpoint)
                .is_some_and(Support::is_unsupported);
            assert_eq!(decides, decision_adapter(&id).is_some(), "'{name}' ({id})");
            if decides {
                with_adapter.push(id);
            }
        }
        with_adapter.sort();
        with_adapter.dedup();
        let kinds: Vec<_> = with_adapter.iter().filter_map(decision_adapter).collect();
        assert!(kinds.contains(&DecisionAdapterKind::CompatibleHttp));
        assert!(kinds.contains(&DecisionAdapterKind::OpenRouter));
        assert!(kinds.contains(&DecisionAdapterKind::TypeSafe));
        assert_eq!(
            decision_adapter(&ProviderId::of("anthropic-custom:http://127.0.0.1:9/v1")),
            None,
            "an Anthropic-compatible endpoint has no decision transport"
        );
    }

    /// D-040 OQ-A3: `decision:typesafe` is a decision-only service. Every
    /// other operation and feature is unsupported, and it is not a provider
    /// any chat surface lists or builds.
    #[test]
    fn decision_typesafe_is_refused_for_chat_embedding_and_is_not_listed() {
        let id = ProviderId::of("typesafe");
        assert_eq!(id.as_str(), "decision:typesafe");
        assert_eq!(ProviderId::of("decision:typesafe"), id);
        assert_eq!(ProviderId::of(" Decision:TypeSafe "), id);
        assert_eq!(id.namespace(), ProviderNamespace::DecisionService);
        let bound = adapter_bound(&id);
        assert_eq!(bound.family, AdapterFamily::TypeSafeDecision);
        for operation in ModelOperation::ALL {
            let support = bound
                .operations
                .get(&operation)
                .expect("test: every operation is bounded");
            if operation == ModelOperation::DecisionEndpoint {
                assert_eq!(support, &Support::Unknown);
            } else {
                assert!(support.is_unsupported(), "{operation}");
            }
        }
        assert!(bound.features.streaming.is_unsupported());
        assert!(bound.features.native_tools.is_unsupported());
        assert!(
            !list_providers()
                .into_iter()
                .flat_map(|info| std::iter::once(info.name).chain(info.aliases.iter().copied()))
                .any(|name| ProviderId::of(name) == id),
            "no chat provider spelling names the decision service"
        );
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let options = ProviderRuntimeOptions {
            openprx_dir: Some(temp.path().to_path_buf()),
            codex_auth_json_path: Some(temp.path().join("no-codex-auth.json")),
            codex_auth_json_auto_import: false,
            ..ProviderRuntimeOptions::default()
        };
        assert!(
            create_provider_with_options("typesafe", Some("k"), &options).is_err(),
            "the chat factory builds no provider for a decision service"
        );
    }

    #[test]
    fn embeddings_are_bounded_by_the_embedding_factory() {
        for (name, embeds) in [
            ("openai", true),
            ("OpenRouter", true),
            ("custom:http://127.0.0.1:9/v1", true),
            ("anthropic", false),
            ("anthropic-custom:http://127.0.0.1:9/v1", false),
            ("gemini", false),
            ("ollama", false),
            ("groq", false),
        ] {
            let bound = adapter_bound(&ProviderId::of(name));
            assert_eq!(
                !bound
                    .operations
                    .get(&ModelOperation::Embedding)
                    .is_some_and(Support::is_unsupported),
                embeds,
                "{name}"
            );
        }
    }

    #[test]
    fn an_unknown_provider_resolves_nothing() {
        let bound = adapter_bound(&ProviderId::of("no-such-provider"));
        assert!(bound.operations.values().all(Support::is_unsupported));
        let bound = adapter_bound(&ProviderId::of("custom:not a url"));
        assert!(bound.operations.values().all(Support::is_unsupported));
    }

    /// The table must agree with what the factory builds, both ways: a
    /// provider with no streaming (or native tool) transport is bounded
    /// `Unsupported`, and one with a transport is not. The fixed tool-schema
    /// dialect is the one the constructed provider reports with no
    /// declaration. Catalog rows and both bring-your-own dialects are built.
    #[test]
    fn adapter_bounds_agree_with_the_constructed_providers() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let options = ProviderRuntimeOptions {
            openprx_dir: Some(temp.path().to_path_buf()),
            codex_auth_json_path: Some(temp.path().join("no-codex-auth.json")),
            codex_auth_json_auto_import: false,
            ..ProviderRuntimeOptions::default()
        };
        let names: Vec<String> = list_providers()
            .into_iter()
            .map(|info| info.name.to_string())
            .chain(
                [ByoDialect::OpenAiCompatible, ByoDialect::AnthropicCompatible]
                    .into_iter()
                    .map(|dialect| format!("{}http://127.0.0.1:9/v1", dialect.prefix())),
            )
            .collect();
        for name in &names {
            let Ok(provider) = create_provider_with_options(name, Some("pmmb-adapter-test-key"), &options) else {
                panic!("'{name}' must construct");
            };
            let bound = adapter_bound(&ProviderId::of(name));
            assert_eq!(
                bound.features.streaming.is_unsupported(),
                !provider.supports_streaming(),
                "'{name}': the streaming bound must be Unsupported exactly when the adapter has no streaming \
                 transport"
            );
            assert_eq!(
                bound.features.native_tools.is_unsupported(),
                !provider.supports_native_tools(),
                "'{name}': the native-tools bound must be Unsupported exactly when the adapter has no native \
                 tool transport"
            );
            assert_eq!(
                bound.dialects.tool_schema,
                provider.tool_schema_dialect(),
                "fixed dialect for '{name}'"
            );
            let described = super::super::CatalogView::default().describe(&ProviderId::of(name), "any-model");
            assert_eq!(
                described.dialects.tool_schema,
                provider.tool_schema_dialect(),
                "the undeclared descriptor's dialect for '{name}'"
            );
        }
        assert!(names.len() > 20, "the catalog was walked");
    }

    /// A WASM plugin provider has no streaming transport (`RuntimeWasmProvider`
    /// keeps the trait default), so no declaration can make it stream.
    #[cfg(feature = "wasm-plugins")]
    #[test]
    fn a_wasm_plugin_has_no_streaming_transport() {
        let bound = adapter_bound(&ProviderId::of("wasm:Echo"));
        assert_eq!(bound.family, AdapterFamily::WasmPlugin);
        assert!(bound.features.streaming.is_unsupported(), "{bound:?}");
        assert!(!bound.features.native_tools.is_unsupported(), "{bound:?}");
        let declared = super::super::CatalogView::default().with_curated([super::super::CuratedModel {
            streaming: Some(true),
            ..super::super::CuratedModel::new(ProviderId::of("wasm:Echo"), None)
        }]);
        assert!(
            declared
                .describe(&ProviderId::of("wasm:Echo"), "m")
                .features
                .streaming
                .is_unsupported(),
            "a declaration cannot give a WASM plugin a streaming transport"
        );
    }

    /// The embedding bound mirrors `create_embedding_provider_with_base_url`
    /// for the canonical spellings: an adapter exactly where the factory builds
    /// a real embedder rather than its no-op.
    #[test]
    fn the_embedding_bound_agrees_with_the_embedding_factory() {
        let names = list_providers()
            .into_iter()
            .map(|info| info.name.to_string())
            .chain(["custom:http://127.0.0.1:9/v1".to_string()]);
        for name in names {
            let embedder = crate::memory::embeddings::create_embedding_provider(&name, Some("k"), "m", 8);
            let bound = adapter_bound(&ProviderId::of(&name));
            assert_eq!(
                !bound
                    .operations
                    .get(&ModelOperation::Embedding)
                    .is_some_and(Support::is_unsupported),
                embedder.name() != "none",
                "embedding bound for '{name}' (factory built '{}')",
                embedder.name()
            );
        }
    }
}
