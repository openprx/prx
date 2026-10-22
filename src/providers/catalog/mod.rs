//! The provider/model catalog: typed descriptors, the adapter table, and the
//! fail-fast resolver built on them.
//!
//! - [`descriptor`]: what one model of one provider can do, per operation and
//!   feature, as tri-state evidence.
//! - [`adapters`]: what each provider family can execute at all — the upper
//!   bound of every descriptor.
//! - [`discovery`]: provider catalog metadata as descriptor evidence, and the
//!   model cache.
//! - [`resolver`]: `resolve(selection, requirement, catalog, credentials,
//!   policy)`, pure and local, which either releases a model with its dialects
//!   and its owner-bound credential or refuses before any transport exists.
//!
//! [`CatalogView`] is the read-only evidence the resolver consults: the
//! adapter table, operator declarations from configuration, and discovered
//! catalog metadata.

pub mod adapters;
pub mod descriptor;
pub mod discovery;
pub mod parameters;
pub(crate) mod process_snapshot;
pub mod release;
pub mod resolver;

use std::sync::Arc;

pub use adapters::{AdapterBound, AdapterFamily, DecisionAdapterKind, adapter_bound, decision_adapter};
pub use descriptor::{
    DescriptorSource, Dialects, Feature, FeatureSet, ModelDescriptor, ModelOperation, RequiredFeatures, Requirement,
    Support,
};
pub use release::{
    CatalogFeatures, CatalogLimits, CatalogOperation, CatalogParameter, CatalogPriceTier, CatalogPricing,
    CatalogRouterHints, CatalogTransport, ModelLifecycle, ModelLifecycleStatus, ModelParameter, ReleaseCatalog,
    ReleaseCatalogError, ReleaseDefaults, ReleaseModel, ReleaseProvider,
};
pub use resolver::{
    ResolutionError, ResolvedCredential, ResolvedModel, Selection, UnknownPolicy, Verification, resolve,
};

use crate::providers::{ProviderId, ProviderNamespace, TOOL_DIALECT_DECLARABLE_CATALOG, ToolCallDialect};
use crate::tools::ToolSchemaDialect;

/// Built-in model-family schema restrictions, shared by the catalog and transport.
/// Model identity is authoritative; endpoint URLs never select a dialect.
pub(crate) fn builtin_tool_schema_dialect(provider: &ProviderId, model: &str) -> ToolSchemaDialect {
    if provider.catalog_name() == Some("openrouter")
        && model
            .trim()
            .split_once('/')
            .is_some_and(|(family, _)| family.eq_ignore_ascii_case("anthropic"))
    {
        ToolSchemaDialect::NoRootCombinators
    } else {
        adapter_bound(provider).dialects.tool_schema
    }
}

/// Why an operation is unsupported when a declaration lists others only.
const NOT_DECLARED: &str = "the operator declaration for this model does not list it";

/// One operator declaration about a provider's models.
///
/// `model: None` covers every model of the provider; a model-scoped
/// declaration overrides it field by field. `None` fields declare nothing.
#[derive(Clone, PartialEq, Eq)]
pub struct CuratedModel {
    pub provider: ProviderId,
    pub model: Option<Arc<str>>,
    /// The complete list of operations the model executes, when declared:
    /// every operation not listed is `Unsupported`.
    pub operations: Option<Vec<ModelOperation>>,
    pub native_tools: Option<bool>,
    pub vision_input: Option<bool>,
    pub streaming: Option<bool>,
    pub structured_output: Option<bool>,
    pub tool_schema: Option<ToolSchemaDialect>,
    /// Endpoint-wide: the response parser cannot tell models apart, so only a
    /// provider-wide declaration is read.
    pub tool_call: Option<ToolCallDialect>,
    pub context_window: Option<u32>,
}

impl CuratedModel {
    /// A declaration for `provider` (and `model`) that declares nothing yet.
    #[must_use]
    pub fn new(provider: ProviderId, model: Option<&str>) -> Self {
        Self {
            provider,
            model: model.map(str::trim).filter(|model| !model.is_empty()).map(Arc::from),
            operations: None,
            native_tools: None,
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema: None,
            tool_call: None,
            context_window: None,
        }
    }

    fn applies_to(&self, provider: &ProviderId, model: &str) -> bool {
        self.provider == *provider
            && self
                .model
                .as_deref()
                .is_none_or(|declared| declared.eq_ignore_ascii_case(model))
    }

    /// Whether this declaration says anything about operations or features.
    const fn describes_capabilities(&self) -> bool {
        self.operations.is_some()
            || self.native_tools.is_some()
            || self.vision_input.is_some()
            || self.streaming.is_some()
            || self.structured_output.is_some()
    }
}

impl std::fmt::Debug for CuratedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CuratedModel")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("operations", &self.operations)
            .field("tool_schema", &self.tool_schema)
            .field("tool_call", &self.tool_call)
            .finish_non_exhaustive()
    }
}

/// Whether the adapter for `provider` reads tool-dialect declarations: an
/// OpenAI-compatible `custom:` endpoint and the catalog servers the operator
/// runs. Every other adapter fixes its own dialect.
fn reads_dialect_declarations(provider: &ProviderId) -> bool {
    match provider.namespace() {
        ProviderNamespace::Custom => true,
        ProviderNamespace::Catalog => provider
            .catalog_name()
            .is_some_and(|name| TOOL_DIALECT_DECLARABLE_CATALOG.contains(&name)),
        ProviderNamespace::AnthropicCustom
        | ProviderNamespace::Wasm
        | ProviderNamespace::DecisionService
        | ProviderNamespace::Unknown => false,
    }
}

/// The read-only evidence the resolver consults.
///
/// Built from configuration (operator declarations) and, optionally,
/// discovered catalog metadata. It is local: building or reading it never
/// touches the network.
#[derive(Debug, Clone)]
pub struct CatalogView {
    curated: Vec<CuratedModel>,
    discovered: Vec<ModelDescriptor>,
    routes: Vec<RouteTarget>,
    release: Option<Arc<ReleaseCatalog>>,
    release_error: Option<ReleaseCatalogError>,
}

impl Default for CatalogView {
    fn default() -> Self {
        Self {
            curated: Vec::new(),
            discovered: Vec::new(),
            routes: Vec::new(),
            release: None,
            release_error: None,
        }
    }
}

/// One `[[model_routes]]` destination: what a `hint:<hint>` model names.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RouteTarget {
    pub hint: String,
    pub provider: String,
    pub model: String,
    /// Whether the route carries its own `api_key`.
    pub has_own_key: bool,
    /// The credential reference (`cred-<name>`) the route names instead of
    /// an inline key, if any.
    pub credential_ref: Option<String>,
}

impl std::fmt::Debug for RouteTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteTarget")
            .field("hint", &self.hint)
            .field("provider", &crate::providers::redact_endpoint_userinfo(&self.provider))
            .field("model", &self.model)
            .field("has_own_key", &self.has_own_key)
            .field("credential_ref", &self.credential_ref)
            .finish()
    }
}

impl CatalogView {
    /// Release evidence without operator declarations or discovery state.
    #[must_use]
    pub fn release_only() -> Self {
        let (release, release_error) = match ReleaseCatalog::embedded() {
            Ok(catalog) => (Some(catalog), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            curated: Vec::new(),
            discovered: Vec::new(),
            routes: Vec::new(),
            release,
            release_error,
        }
    }

    /// The operator declarations in `config`: `[[providers.models]]` and
    /// `[[providers.tool_dialects]]`.
    #[must_use]
    pub fn from_config(config: &crate::config::Config) -> Self {
        let (release, release_error) = match ReleaseCatalog::embedded() {
            Ok(catalog) => (Some(catalog), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            curated: curated_from_config(config),
            discovered: Vec::new(),
            routes: config
                .model_routes
                .iter()
                .map(|route| RouteTarget {
                    hint: route.hint.trim().to_string(),
                    provider: route.provider.trim().to_string(),
                    model: route.model.trim().to_string(),
                    has_own_key: route.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()),
                    credential_ref: route
                        .credential_ref
                        .as_deref()
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string),
                })
                .collect(),
            release,
            release_error,
        }
    }

    /// Retain this view's immutable release evidence for terminal provenance.
    pub(crate) fn release_pin(&self) -> Option<Arc<ReleaseCatalog>> {
        self.release.clone()
    }

    /// The release catalog this view pins.
    ///
    /// # Errors
    ///
    /// The catalog embedded in this binary did not pass semantic validation.
    pub fn release_catalog(&self) -> Result<&ReleaseCatalog, ReleaseCatalogError> {
        self.release.as_deref().ok_or_else(|| {
            self.release_error
                .clone()
                .unwrap_or_else(ReleaseCatalogError::missing_embedded_catalog)
        })
    }

    /// The embedded revision pinned by this view.
    #[must_use]
    pub fn release_revision(&self) -> Option<&str> {
        self.release.as_ref().map(|catalog| catalog.revision.as_str())
    }

    /// Replace the release evidence for a focused test.
    #[cfg(test)]
    #[must_use]
    pub fn with_release_catalog(mut self, catalog: ReleaseCatalog) -> Self {
        self.release = Some(Arc::new(catalog));
        self.release_error = None;
        self
    }

    /// The `[[model_routes]]` destination `hint` names.
    #[must_use]
    pub fn route(&self, hint: &str) -> Option<&RouteTarget> {
        let hint = hint.trim();
        self.routes.iter().find(|route| route.hint == hint)
    }

    /// Every configured route hint, in configuration order.
    #[must_use]
    pub fn route_hints(&self) -> Vec<&str> {
        self.routes.iter().map(|route| route.hint.as_str()).collect()
    }

    /// Add operator declarations.
    #[must_use]
    pub fn with_curated(mut self, curated: impl IntoIterator<Item = CuratedModel>) -> Self {
        self.curated.extend(curated);
        self
    }

    /// Add the metadata cached by `prx models refresh` and onboarding for
    /// `workspace_dir`.
    ///
    /// # Errors
    ///
    /// When the cache file exists but cannot be read.
    pub fn with_discovery_cache(self, workspace_dir: &std::path::Path) -> anyhow::Result<Self> {
        Ok(self.with_discovered(discovery::cached_descriptors(workspace_dir)?))
    }

    /// Add discovered descriptors (metadata evidence from a provider catalog).
    #[must_use]
    pub fn with_discovered(mut self, discovered: impl IntoIterator<Item = ModelDescriptor>) -> Self {
        self.discovered.extend(discovered);
        self
    }

    /// The combined descriptor for `model` of `provider`.
    ///
    /// 1. The adapter table is the upper bound: nothing lifts an adapter
    ///    `Unsupported`.
    /// 2. Negative discovery evidence outranks an operator declaration.
    /// 3. An operator declaration outranks positive discovery and the release
    ///    catalog, and sets dialects where the adapter reads them.
    /// 4. Positive discovery outranks the release catalog.
    /// 5. With no evidence at all, every operation and feature is `Unknown`
    ///    and the source is [`DescriptorSource::LegacyIdOnly`].
    #[must_use]
    pub fn describe(&self, provider: &ProviderId, model: &str) -> ModelDescriptor {
        let model = model.trim();
        let bound = adapter_bound(provider);
        let mut descriptor = self
            .release
            .as_ref()
            .and_then(|catalog| catalog.descriptor(provider, model))
            .unwrap_or_else(|| ModelDescriptor::id_only(provider.clone(), model));

        // Several records can describe one model (refreshes under different
        // spellings of one provider); the newest is the evidence.
        let found = self
            .discovered
            .iter()
            .filter(|found| found.provider == *provider && found.model_id.eq_ignore_ascii_case(model))
            .max_by_key(|found| match &found.source {
                DescriptorSource::Discovery { fetched_at_unix } => *fetched_at_unix,
                DescriptorSource::AdapterStatic
                | DescriptorSource::Catalog { .. }
                | DescriptorSource::Curated
                | DescriptorSource::LegacyIdOnly => 0,
            });

        // Provider-wide declarations first, then the model's own, so a
        // model-scoped field overrides the provider-wide one.
        let mut declared: Vec<&CuratedModel> = self
            .curated
            .iter()
            .filter(|curated| curated.applies_to(provider, model))
            .collect();
        declared.sort_by_key(|curated| curated.model.is_some());
        let mut curated = CuratedModel::new(provider.clone(), Some(model));
        let mut provider_wide_tool_call = None;
        for entry in &declared {
            if entry.model.is_none() && entry.tool_call.is_some() {
                provider_wide_tool_call = entry.tool_call;
            }
            curated.operations = entry.operations.clone().or(curated.operations);
            curated.native_tools = entry.native_tools.or(curated.native_tools);
            curated.vision_input = entry.vision_input.or(curated.vision_input);
            curated.streaming = entry.streaming.or(curated.streaming);
            curated.structured_output = entry.structured_output.or(curated.structured_output);
            curated.tool_schema = entry.tool_schema.or(curated.tool_schema);
            curated.context_window = entry.context_window.or(curated.context_window);
        }
        let has_capability_declaration = declared.iter().any(|entry| entry.describes_capabilities());
        let declared_features = FeatureSet {
            native_tools: Support::from_declared(curated.native_tools, "declared without native tools"),
            vision_input: Support::from_declared(curated.vision_input, "declared without image input"),
            streaming: Support::from_declared(curated.streaming, "declared without streaming"),
            structured_output: Support::from_declared(curated.structured_output, "declared without structured output"),
        };
        for operation in ModelOperation::ALL {
            let catalog = descriptor.operation(operation);
            let discovered = found.map_or(Support::Unknown, |found| found.operation(operation));
            let declared = curated.operations.as_ref().map_or(Support::Unknown, |listed| {
                if listed.contains(&operation) {
                    Support::Supported
                } else {
                    Support::unsupported(NOT_DECLARED)
                }
            });
            descriptor
                .operations
                .insert(operation, choose_capability(catalog, discovered, declared));
        }
        for feature in Feature::ALL {
            let catalog = descriptor.features.get(feature).clone();
            let discovered = found.map_or(Support::Unknown, |found| found.features.get(feature).clone());
            let declared = declared_features.get(feature).clone();
            *descriptor.features.slot(feature) = choose_capability(catalog, discovered, declared);
        }
        descriptor.context_window = curated
            .context_window
            .or_else(|| found.and_then(|found| found.context_window))
            .or(descriptor.context_window);

        let discovery_has_negative = found.is_some_and(|found| {
            ModelOperation::ALL
                .into_iter()
                .any(|operation| found.operation(operation).is_unsupported())
                || Feature::ALL
                    .into_iter()
                    .any(|feature| found.features.get(feature).is_unsupported())
        });
        let discovery_has_evidence = found.is_some_and(|found| {
            found.context_window.is_some()
                || ModelOperation::ALL
                    .into_iter()
                    .any(|operation| !matches!(found.operation(operation), Support::Unknown))
                || Feature::ALL
                    .into_iter()
                    .any(|feature| !matches!(found.features.get(feature), Support::Unknown))
        });
        if discovery_has_negative {
            descriptor.source = found.map_or(DescriptorSource::LegacyIdOnly, |found| found.source.clone());
        } else if has_capability_declaration {
            descriptor.source = DescriptorSource::Curated;
        } else if discovery_has_evidence {
            descriptor.source = found.map_or(DescriptorSource::LegacyIdOnly, |found| found.source.clone());
        }

        for operation in ModelOperation::ALL {
            let current = descriptor.operation(operation);
            let capped = match bound.operations.get(&operation) {
                Some(limit) => current.within(limit),
                None => current,
            };
            descriptor.operations.insert(operation, capped);
        }
        descriptor.features = descriptor.features.within(&bound.features);

        descriptor.dialects = bound.dialects;
        descriptor.dialects.tool_schema = builtin_tool_schema_dialect(provider, model);
        if reads_dialect_declarations(provider) {
            if let Some(tool_schema) = curated.tool_schema {
                descriptor.dialects.tool_schema = tool_schema;
            }
            if let Some(tool_call) = provider_wide_tool_call {
                descriptor.dialects.tool_call = tool_call;
            }
        }
        descriptor
    }

    /// Intersect a concrete adapter's ceiling with the effective model facts.
    #[must_use]
    pub fn effective_provider_capabilities(
        &self,
        provider: &str,
        model: &str,
        adapter: crate::providers::ProviderCapabilities,
    ) -> crate::providers::ProviderCapabilities {
        let descriptor = self.describe(&ProviderId::of(provider), model);
        let effective = |feature: Feature, ceiling: bool| match descriptor.features.get(feature) {
            Support::Supported => ceiling,
            Support::Unsupported(_) => false,
            Support::Unknown => ceiling,
        };
        crate::providers::ProviderCapabilities {
            native_tool_calling: effective(Feature::NativeTools, adapter.native_tool_calling),
            vision: effective(Feature::VisionInput, adapter.vision),
        }
    }
}

/// Apply the catalog evidence precedence for one operation or feature.
fn choose_capability(catalog: Support, discovered: Support, declared: Support) -> Support {
    match discovered {
        Support::Unsupported(reason) => Support::Unsupported(reason),
        Support::Supported => match declared {
            Support::Unknown => Support::Supported,
            declared => declared,
        },
        Support::Unknown => match declared {
            Support::Unknown => catalog,
            declared => declared,
        },
    }
}

/// The operation a configuration setting names.
const fn model_operation_setting(setting: crate::config::schema::ModelOperationSetting) -> ModelOperation {
    use crate::config::schema::ModelOperationSetting as Setting;
    match setting {
        Setting::Chat => ModelOperation::Chat,
        Setting::Embedding => ModelOperation::Embedding,
        Setting::DecisionEndpoint => ModelOperation::DecisionEndpoint,
        Setting::Rerank => ModelOperation::Rerank,
        Setting::ImageGeneration => ModelOperation::ImageGeneration,
        Setting::AudioGeneration => ModelOperation::AudioGeneration,
        Setting::VideoGeneration => ModelOperation::VideoGeneration,
    }
}

/// The declarations `config` makes about models, as curated descriptors:
/// every `[[providers.models]]` entry, then every
/// `[[providers.tool_dialects]]` entry (dialects only).
fn curated_from_config(config: &crate::config::Config) -> Vec<CuratedModel> {
    let models = config.providers.models.iter().map(|entry| CuratedModel {
        operations: entry
            .operations
            .as_ref()
            .map(|listed| listed.iter().copied().map(model_operation_setting).collect()),
        native_tools: entry.native_tools,
        vision_input: entry.vision_input,
        streaming: entry.streaming,
        structured_output: entry.structured_output,
        tool_schema: entry
            .tool_schema_dialect
            .map(crate::providers::tool_schema_dialect_setting),
        tool_call: entry.tool_call_dialect.map(crate::providers::tool_call_dialect_setting),
        context_window: entry.context_window,
        ..CuratedModel::new(ProviderId::of(&entry.provider), entry.model.as_deref())
    });
    let dialects = config.providers.tool_dialects.iter().map(|entry| {
        let mut curated = CuratedModel::new(ProviderId::of(&entry.provider), entry.model.as_deref());
        curated.tool_schema = entry.tool_schema.map(crate::providers::tool_schema_dialect_setting);
        curated.tool_call = entry.tool_call.map(crate::providers::tool_call_dialect_setting);
        curated
    });
    models.chain(dialects).collect()
}

#[cfg(test)]
mod curated_config_tests;
#[cfg(test)]
mod resolver_tests;
