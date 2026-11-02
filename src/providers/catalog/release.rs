//! The release-owned model catalog embedded in every PRX binary.
//!
//! `catalog/models.json` is the source of built-in model identities and facts.
//! It is parsed once, validated semantically, and then shared by every model
//! surface. The operator chose release-only updates, so there is no filesystem
//! overlay or network update path.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{DescriptorSource, FeatureSet, ModelDescriptor, ModelOperation, Support};
use crate::providers::ProviderId;

const EMBEDDED_CATALOG_JSON: &str = include_str!("../../../catalog/models.json");
static EMBEDDED_CATALOG: OnceLock<Result<Arc<ReleaseCatalog>, ReleaseCatalogError>> = OnceLock::new();
#[cfg(test)]
const EMBEDDED_CATALOG_SCHEMA: &str = include_str!("../../../catalog/models.schema.json");
const SUPPORTED_SCHEMA_MAJOR: u64 = 1;
const CATALOG_NOT_DECLARED: &str = "the embedded catalog does not declare this operation";

/// A catalog load or semantic-validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseCatalogError {
    message: Arc<str>,
}

impl ReleaseCatalogError {
    fn new(message: impl Into<Arc<str>>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn missing_embedded_catalog() -> Self {
        Self::new("this catalog view does not carry embedded release evidence")
    }
}

impl fmt::Display for ReleaseCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ReleaseCatalogError {}

/// The closed set of request parameters a release catalog may declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelParameter {
    Temperature,
    TopP,
    MaxOutputTokens,
    ReasoningEffort,
    ThinkingBudget,
    Verbosity,
    ServiceTier,
    Seed,
}

impl ModelParameter {
    /// Parse the catalog spelling. Unknown future names are intentionally
    /// ignored by this binary after a warning.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "temperature" => Some(Self::Temperature),
            "top_p" => Some(Self::TopP),
            "max_output_tokens" => Some(Self::MaxOutputTokens),
            "reasoning_effort" => Some(Self::ReasoningEffort),
            "thinking_budget" => Some(Self::ThinkingBudget),
            "verbosity" => Some(Self::Verbosity),
            "service_tier" => Some(Self::ServiceTier),
            "seed" => Some(Self::Seed),
            _ => None,
        }
    }

    /// The JSON and configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Temperature => "temperature",
            Self::TopP => "top_p",
            Self::MaxOutputTokens => "max_output_tokens",
            Self::ReasoningEffort => "reasoning_effort",
            Self::ThinkingBudget => "thinking_budget",
            Self::Verbosity => "verbosity",
            Self::ServiceTier => "service_tier",
            Self::Seed => "seed",
        }
    }
}

/// One supported operation in the JSON vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogOperation {
    Chat,
    Embedding,
    DecisionEndpoint,
    Rerank,
    ImageGeneration,
    AudioGeneration,
    VideoGeneration,
}

impl From<CatalogOperation> for ModelOperation {
    fn from(value: CatalogOperation) -> Self {
        match value {
            CatalogOperation::Chat => Self::Chat,
            CatalogOperation::Embedding => Self::Embedding,
            CatalogOperation::DecisionEndpoint => Self::DecisionEndpoint,
            CatalogOperation::Rerank => Self::Rerank,
            CatalogOperation::ImageGeneration => Self::ImageGeneration,
            CatalogOperation::AudioGeneration => Self::AudioGeneration,
            CatalogOperation::VideoGeneration => Self::VideoGeneration,
        }
    }
}

/// The lifecycle state maintained by releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelLifecycleStatus {
    Active,
    Deprecated,
    Retired,
}

/// Lifecycle and replacement metadata for a catalog model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLifecycle {
    pub status: ModelLifecycleStatus,
    #[serde(default)]
    pub deprecated_at: Option<String>,
    #[serde(default)]
    pub retire_at: Option<String>,
    #[serde(default)]
    pub replacement: Option<String>,
}

/// Model feature facts. `None` is unknown rather than false.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogFeatures {
    #[serde(default)]
    pub native_tools: Option<bool>,
    #[serde(default)]
    pub vision_input: Option<bool>,
    #[serde(default)]
    pub streaming: Option<bool>,
    #[serde(default)]
    pub structured_output: Option<bool>,
    #[serde(default)]
    pub prompt_cache: Option<bool>,
}

/// Token limits supplied by the release catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogLimits {
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

/// A price tier above an input-token threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPriceTier {
    pub above_input_tokens: u64,
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
}

/// How a catalog row is billed. Absent means a per-token tariff.
///
/// `subscription` is a plan the operator already pays for. It is not a zero
/// token price: a zero tariff would be recorded as a priced charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogBilling {
    #[default]
    Token,
    Subscription,
}

/// Per-million-token prices shipped by a release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPricing {
    pub unit: String,
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub tiers: Vec<CatalogPriceTier>,
}

/// A typed request-parameter declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CatalogParameter {
    Range {
        min: f64,
        max: f64,
        #[serde(default)]
        default: Option<Value>,
        #[serde(default)]
        step: Option<f64>,
        #[serde(default)]
        excludes: Vec<String>,
    },
    Levels {
        levels: Vec<String>,
        #[serde(default)]
        default: Option<String>,
        #[serde(default)]
        excludes: Vec<String>,
    },
    Bool {
        #[serde(default)]
        default: Option<bool>,
        #[serde(default)]
        excludes: Vec<String>,
    },
    Unsupported,
}

impl CatalogParameter {
    fn excludes(&self) -> &[String] {
        match self {
            Self::Range { excludes, .. } | Self::Levels { excludes, .. } | Self::Bool { excludes, .. } => excludes,
            Self::Unsupported => &[],
        }
    }

    fn accepts(&self, value: &Value) -> bool {
        match self {
            Self::Range { min, max, .. } => value.as_f64().is_some_and(|value| value >= *min && value <= *max),
            Self::Levels { levels, .. } => value
                .as_str()
                .is_some_and(|value| levels.iter().any(|level| level == value)),
            Self::Bool { .. } => value.is_boolean(),
            Self::Unsupported => false,
        }
    }
}

/// Router hints associated with one model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRouterHints {
    pub categories: Vec<String>,
    /// Router ranking cost per million tokens. This is deliberately separate
    /// from [`CatalogPricing`]: routing may use a stable comparative estimate,
    /// while billing and G6 admission require the full input/output tariff.
    pub cost_per_million_tokens: f64,
    pub latency_ms: u32,
    pub quality_rank: u32,
}

/// Hosted transport recommendations supplied by a model entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogTransport {
    #[serde(default)]
    pub recommended_timeout_ms: Option<u64>,
    #[serde(default)]
    pub extra_latency_budget_ms: Option<u64>,
}

/// One model record in `catalog/models.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseModel {
    pub id: String,
    pub provider: String,
    pub api_model: String,
    pub display_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub lifecycle: ModelLifecycle,
    pub operations: Vec<CatalogOperation>,
    #[serde(default)]
    pub features: CatalogFeatures,
    #[serde(default)]
    pub limits: CatalogLimits,
    #[serde(default)]
    pub tokenizer: Option<String>,
    /// `subscription` rows have no [`Self::pricing`] object.
    #[serde(default)]
    pub billing: CatalogBilling,
    #[serde(default)]
    pub pricing: Option<CatalogPricing>,
    #[serde(default)]
    pub parameters: BTreeMap<String, CatalogParameter>,
    #[serde(default)]
    pub levels: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(default)]
    pub default_level: Option<String>,
    #[serde(default)]
    pub router: Option<CatalogRouterHints>,
    #[serde(default)]
    pub pinned_snapshot: Option<String>,
    #[serde(default)]
    pub transport: CatalogTransport,
}

impl ReleaseModel {
    /// Whether this row is eligible to anchor a release default.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.lifecycle.status == ModelLifecycleStatus::Active && self.api_model != "*"
    }

    /// A plan the operator already pays for, with no per-token tariff.
    #[must_use]
    pub fn is_subscription_billed(&self) -> bool {
        self.billing == CatalogBilling::Subscription
    }

    /// Whether this row can be offered for a new selection.
    #[must_use]
    pub fn is_listed(&self) -> bool {
        self.lifecycle.status != ModelLifecycleStatus::Retired && self.api_model != "*"
    }

    /// A lifecycle warning shown beside a still-selectable deprecated row.
    #[must_use]
    pub fn selection_warning(&self) -> Option<String> {
        (self.lifecycle.status == ModelLifecycleStatus::Deprecated).then(|| {
            self.lifecycle.replacement.as_deref().map_or_else(
                || "deprecated; select an active model when possible".to_owned(),
                |replacement| format!("deprecated; use {replacement}"),
            )
        })
    }

    /// Whether this model performs the requested operation.
    #[must_use]
    pub fn supports(&self, operation: ModelOperation) -> bool {
        self.operations
            .iter()
            .copied()
            .map(ModelOperation::from)
            .any(|item| item == operation)
    }
}

fn model_spelling_matches(catalog_model: &str, selected: &str) -> bool {
    catalog_model.eq_ignore_ascii_case(selected)
        || selected.get(..catalog_model.len()).is_some_and(|prefix| {
            prefix.eq_ignore_ascii_case(catalog_model)
                && selected
                    .get(catalog_model.len()..)
                    .is_some_and(|suffix| suffix.starts_with(':'))
        })
}

/// Provider display and default-selection metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseProvider {
    pub default_model: String,
    pub display_name: String,
}

/// The consolidated defaults shared by every surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDefaults {
    pub provider: String,
    pub model: String,
}

/// The validated embedded catalog document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseCatalog {
    #[serde(rename = "$schema", default)]
    schema: Option<String>,
    pub schema_version: String,
    pub revision: String,
    pub published_at: String,
    pub providers: BTreeMap<String, ReleaseProvider>,
    pub defaults: ReleaseDefaults,
    pub models: Vec<ReleaseModel>,
    #[serde(skip)]
    content_sha256: String,
    #[serde(skip)]
    warnings: Vec<String>,
}

impl ReleaseCatalog {
    /// Parse and semantically validate a catalog document.
    ///
    /// # Errors
    ///
    /// The JSON is malformed, its schema major is unsupported, or one of the
    /// cross-record invariants does not hold.
    pub fn from_json(input: &str) -> Result<Self, ReleaseCatalogError> {
        let raw: Value = serde_json::from_str(input)
            .map_err(|_| ReleaseCatalogError::new("the embedded model catalog is not valid JSON"))?;
        validate_secret_free(&raw)?;
        let mut catalog: Self = serde_json::from_value(raw)
            .map_err(|_| ReleaseCatalogError::new("the embedded model catalog does not match schema v1"))?;
        catalog.content_sha256 = format!("sha256:{:x}", Sha256::digest(input.as_bytes()));
        catalog.validate()?;
        Ok(catalog)
    }

    /// The one catalog compiled into this binary.
    ///
    /// # Errors
    ///
    /// The shipped document is invalid. Release CI treats that as a build
    /// defect; callers receive the typed failure instead of using partial data.
    pub fn embedded() -> Result<Arc<Self>, ReleaseCatalogError> {
        EMBEDDED_CATALOG
            .get_or_init(|| {
                Self::from_json(EMBEDDED_CATALOG_JSON).map(|catalog| {
                    for warning in catalog.warnings() {
                        tracing::warn!(warning, "embedded model catalog warning");
                    }
                    Arc::new(catalog)
                })
            })
            .clone()
    }

    /// The one catalog compiled into this binary, borrowed for process-wide
    /// default and policy lookups that must not copy model facts into Rust.
    pub fn embedded_ref() -> Result<&'static Self, ReleaseCatalogError> {
        EMBEDDED_CATALOG
            .get_or_init(|| {
                Self::from_json(EMBEDDED_CATALOG_JSON).map(|catalog| {
                    for warning in catalog.warnings() {
                        tracing::warn!(warning, "embedded model catalog warning");
                    }
                    Arc::new(catalog)
                })
            })
            .as_ref()
            .map(Arc::as_ref)
            .map_err(Clone::clone)
    }

    /// Forward-compatibility warnings collected while loading.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Digest of the exact JSON bytes used to build this catalog.
    #[must_use]
    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }

    /// Find one model under the canonical provider identity.
    #[must_use]
    pub fn model(&self, provider: &ProviderId, selected: &str) -> Option<&ReleaseModel> {
        let provider = provider.catalog_name()?;
        let selected = selected.trim();
        self.models.iter().find(|model| {
            model.provider.eq_ignore_ascii_case(provider)
                && (model_spelling_matches(&model.api_model, selected)
                    || model_spelling_matches(&model.id, selected)
                    || model
                        .aliases
                        .iter()
                        .any(|alias| model_spelling_matches(alias, selected)))
        })
    }

    /// Active and deprecated chat models offered for one provider, with its
    /// release default first. Retired rows remain hidden.
    #[must_use]
    pub fn listed_models(&self, provider: &str) -> Vec<&ReleaseModel> {
        let Some(provider) = self.provider_name(provider) else {
            return Vec::new();
        };
        let default = self.providers.get(provider).map(|entry| entry.default_model.as_str());
        let mut models: Vec<_> = self
            .models
            .iter()
            .filter(|model| {
                model.provider.eq_ignore_ascii_case(provider)
                    && model.is_listed()
                    && model.supports(ModelOperation::Chat)
            })
            .collect();
        models.sort_by_key(|model| !default.is_some_and(|default| model.id.eq_ignore_ascii_case(default)));
        models
    }

    /// The provider's release default, returned in the API-model spelling
    /// existing configuration expects.
    #[must_use]
    pub fn default_model(&self, provider: &str) -> Option<&str> {
        let provider_name = self.provider_name(provider)?;
        let default = self
            .providers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(provider_name))?
            .1
            .default_model
            .as_str();
        self.models
            .iter()
            .find(|model| model.id.eq_ignore_ascii_case(default))
            .map(|model| model.api_model.as_str())
    }

    /// The release-wide default provider.
    #[must_use]
    pub fn default_provider(&self) -> &str {
        &self.defaults.provider
    }

    /// Active model spellings for one provider: `api_model` plus aliases.
    ///
    /// `None` when the catalog does not cover `provider`. An active row whose
    /// `api_model` is `default` stays in the set. A wildcard `api_model` of
    /// `*` is not active and is left out. The set is not every id the server
    /// can run.
    #[must_use]
    pub fn active_allowlist(&self, provider: &str) -> Option<BTreeSet<String>> {
        let provider_name = self.provider_name(provider)?;
        let mut spellings = BTreeSet::new();
        for model in &self.models {
            if !model.provider.eq_ignore_ascii_case(provider_name) || !model.is_active() {
                continue;
            }
            spellings.insert(model.api_model.clone());
            for alias in &model.aliases {
                let alias = alias.trim();
                if !alias.is_empty() {
                    spellings.insert(alias.to_string());
                }
            }
        }
        (!spellings.is_empty()).then_some(spellings)
    }

    /// The release-wide default in the API-model spelling used by config.
    #[must_use]
    pub fn global_default_model(&self) -> Option<&str> {
        self.models
            .iter()
            .find(|model| model.id.eq_ignore_ascii_case(&self.defaults.model))
            .map(|model| model.api_model.as_str())
    }

    fn provider_name(&self, requested: &str) -> Option<&str> {
        let requested = requested.trim();
        if let Some(provider) = self
            .providers
            .keys()
            .find(|provider| provider.eq_ignore_ascii_case(requested))
        {
            return Some(provider.as_str());
        }
        let identity = ProviderId::of(requested);
        let canonical = identity.catalog_name()?;
        self.providers
            .keys()
            .find(|provider| provider.eq_ignore_ascii_case(canonical))
            .map(String::as_str)
    }

    /// Convert a release row into resolver evidence.
    #[must_use]
    pub fn descriptor(&self, provider: &ProviderId, selected: &str) -> Option<ModelDescriptor> {
        let model = self.model(provider, selected)?;
        if model.lifecycle.status == ModelLifecycleStatus::Retired {
            return None;
        }
        let mut descriptor = ModelDescriptor::id_only(provider.clone(), selected.trim());
        for operation in ModelOperation::ALL {
            let support = if model.supports(operation) {
                Support::Supported
            } else {
                Support::unsupported(CATALOG_NOT_DECLARED)
            };
            descriptor.operations.insert(operation, support);
        }
        descriptor.features = FeatureSet {
            native_tools: Support::from_declared(model.features.native_tools, "catalog declares no native tools"),
            vision_input: Support::from_declared(model.features.vision_input, "catalog declares no image input"),
            streaming: Support::from_declared(model.features.streaming, "catalog declares no streaming"),
            structured_output: Support::from_declared(
                model.features.structured_output,
                "catalog declares no structured output",
            ),
        };
        descriptor.context_window = model.limits.context_window;
        descriptor.source = DescriptorSource::Catalog {
            revision: Arc::from(self.revision.as_str()),
        };
        Some(descriptor)
    }

    fn validate(&mut self) -> Result<(), ReleaseCatalogError> {
        let (major, minor) = parse_schema_version(&self.schema_version)?;
        if major != SUPPORTED_SCHEMA_MAJOR {
            return Err(ReleaseCatalogError::new(
                "the embedded model catalog schema major is unsupported",
            ));
        }
        if minor > 0 {
            self.warnings.push(format!(
                "catalog schema minor {minor} is newer than this binary's schema 1.0"
            ));
        }
        if self.revision.trim().is_empty() || self.revision.len() > 64 || !self.revision.is_ascii() {
            return Err(ReleaseCatalogError::new(
                "the embedded model catalog revision is invalid",
            ));
        }
        chrono::DateTime::parse_from_rfc3339(&self.published_at)
            .map_err(|_| ReleaseCatalogError::new("the embedded model catalog published_at is invalid"))?;
        if self.providers.is_empty() || self.models.is_empty() {
            return Err(ReleaseCatalogError::new(
                "the embedded model catalog must contain providers and models",
            ));
        }

        let mut identities_by_provider: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut canonical_ids = BTreeSet::new();
        for model in &self.models {
            validate_model_shape(model)?;
            let wire_id = format!("{}/{}", model.provider, model.api_model);
            if model.id != wire_id && !stable_settings_id(model) {
                return Err(ReleaseCatalogError::new(format!(
                    "catalog model '{}' is not the canonical provider/api_model id",
                    model.id
                )));
            }
            let id = model.id.to_ascii_lowercase();
            if !canonical_ids.insert(id.clone()) {
                return Err(ReleaseCatalogError::new(
                    "the embedded model catalog has a duplicate model id",
                ));
            }
            let identities = identities_by_provider
                .entry(model.provider.to_ascii_lowercase())
                .or_default();
            if !identities.insert(id) || !identities.insert(model.api_model.to_ascii_lowercase()) {
                return Err(ReleaseCatalogError::new(
                    "the embedded model catalog has a colliding model identity",
                ));
            }
            for alias in &model.aliases {
                let alias = alias.trim().to_ascii_lowercase();
                if alias.is_empty() {
                    return Err(ReleaseCatalogError::new(
                        "the embedded model catalog has a colliding alias",
                    ));
                }
                // The wire name is already this row's identity. Listing it again
                // records the dated spelling without creating a second key.
                if alias == model.api_model.to_ascii_lowercase() {
                    continue;
                }
                if !identities.insert(alias) {
                    return Err(ReleaseCatalogError::new(
                        "the embedded model catalog has a colliding alias",
                    ));
                }
            }
        }

        for model in &self.models {
            if let Some(replacement) = model.lifecycle.replacement.as_deref()
                && !canonical_ids.contains(&replacement.to_ascii_lowercase())
            {
                return Err(ReleaseCatalogError::new(
                    "a catalog lifecycle replacement does not exist",
                ));
            }
            validate_model_parameters(model, &mut self.warnings)?;
        }

        for (name, provider) in &self.providers {
            let default_model = self
                .models
                .iter()
                .find(|model| model.id.eq_ignore_ascii_case(&provider.default_model));
            if name.trim().is_empty()
                || provider.display_name.trim().is_empty()
                || default_model.is_none()
                || default_model.is_some_and(|model| !model.is_active() || !model.supports(ModelOperation::Chat))
                || !provider.default_model.starts_with(&format!("{name}/"))
            {
                return Err(ReleaseCatalogError::new(
                    "a catalog provider has an invalid default model",
                ));
            }
        }
        let global_default = self
            .models
            .iter()
            .find(|model| model.id.eq_ignore_ascii_case(&self.defaults.model));
        if !self.providers.contains_key(&self.defaults.provider)
            || global_default.is_none()
            || global_default.is_some_and(|model| !model.is_active() || !model.supports(ModelOperation::Chat))
            || !self.defaults.model.starts_with(&format!("{}/", self.defaults.provider))
        {
            return Err(ReleaseCatalogError::new(
                "the catalog-wide provider/model default is invalid",
            ));
        }
        Ok(())
    }
}

/// A saved settings key may keep its historical id after the wire name changes.
/// The id tail has to be an alias so the previous short name still resolves.
fn stable_settings_id(model: &ReleaseModel) -> bool {
    let prefix = format!("{}/", model.provider);
    let Some(tail) = model.id.strip_prefix(&prefix) else {
        return false;
    };
    !tail.is_empty()
        && !tail.eq_ignore_ascii_case(&model.api_model)
        && model.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(tail))
}

fn validate_model_parameters(model: &ReleaseModel, warnings: &mut Vec<String>) -> Result<(), ReleaseCatalogError> {
    for (name, declaration) in &model.parameters {
        let Some(parameter) = ModelParameter::parse(name) else {
            warnings.push(format!(
                "catalog model '{}' declares unknown parameter '{name}'",
                model.id
            ));
            continue;
        };
        validate_parameter_declaration(parameter, declaration)?;
        for excluded in declaration.excludes() {
            if ModelParameter::parse(excluded).is_none() {
                warnings.push(format!(
                    "catalog model '{}' excludes unknown parameter '{excluded}'",
                    model.id
                ));
            }
        }
    }

    if model.supports(ModelOperation::Chat) {
        for required in ["low", "medium", "high"] {
            if !model.levels.contains_key(required) {
                return Err(ReleaseCatalogError::new(
                    "every catalog chat model must define low, medium and high levels",
                ));
            }
        }
        let Some(default) = model.default_level.as_deref() else {
            return Err(ReleaseCatalogError::new(
                "every catalog chat model must define default_level",
            ));
        };
        if !model.levels.contains_key(default) {
            return Err(ReleaseCatalogError::new(
                "a catalog default_level does not name one of its levels",
            ));
        }
    }

    for values in model.levels.values() {
        for (name, value) in values {
            let Some(parameter) = ModelParameter::parse(name) else {
                warnings.push(format!(
                    "catalog model '{}' level uses unknown parameter '{name}'",
                    model.id
                ));
                continue;
            };
            let Some(declaration) = model.parameters.get(parameter.as_str()) else {
                return Err(ReleaseCatalogError::new("a catalog level uses an undeclared parameter"));
            };
            if !declaration.accepts(value) || !valid_integer_parameter(parameter, value) {
                return Err(ReleaseCatalogError::new(
                    "a catalog level parameter value is outside its declaration",
                ));
            }
        }
    }
    Ok(())
}

fn parse_schema_version(version: &str) -> Result<(u64, u64), ReleaseCatalogError> {
    let Some((major, minor)) = version.split_once('.') else {
        return Err(ReleaseCatalogError::new(
            "the embedded model catalog schema version is invalid",
        ));
    };
    let major = major
        .parse::<u64>()
        .map_err(|_| ReleaseCatalogError::new("the embedded model catalog schema version is invalid"))?;
    let minor = minor
        .parse::<u64>()
        .map_err(|_| ReleaseCatalogError::new("the embedded model catalog schema version is invalid"))?;
    Ok((major, minor))
}

fn validate_model_shape(model: &ReleaseModel) -> Result<(), ReleaseCatalogError> {
    if model.id.trim().is_empty()
        || model.provider.trim().is_empty()
        || model.api_model.trim().is_empty()
        || model.display_name.trim().is_empty()
        || model.operations.is_empty()
        || !model.id.is_ascii()
        || !model.provider.is_ascii()
    {
        return Err(ReleaseCatalogError::new(
            "an embedded catalog model identity is invalid",
        ));
    }
    if model.lifecycle.status == ModelLifecycleStatus::Deprecated && model.lifecycle.deprecated_at.is_none() {
        return Err(ReleaseCatalogError::new(
            "a deprecated catalog model has no deprecated_at timestamp",
        ));
    }
    if model.lifecycle.status == ModelLifecycleStatus::Retired && model.lifecycle.retire_at.is_none() {
        return Err(ReleaseCatalogError::new(
            "a retired catalog model has no retire_at timestamp",
        ));
    }
    for timestamp in [
        model.lifecycle.deprecated_at.as_deref(),
        model.lifecycle.retire_at.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .map_err(|_| ReleaseCatalogError::new("a catalog lifecycle timestamp is invalid"))?;
    }
    if model.limits.context_window == Some(0) || model.limits.max_output_tokens == Some(0) {
        return Err(ReleaseCatalogError::new(
            "catalog token limits must be greater than zero",
        ));
    }
    if model.billing == CatalogBilling::Subscription && model.pricing.is_some() {
        return Err(ReleaseCatalogError::new(
            "a subscription catalog row must not carry a per-token price",
        ));
    }
    if let Some(pricing) = &model.pricing {
        if pricing.unit != "usd_per_1m_tokens"
            || ![pricing.input, pricing.output, pricing.cache_write, pricing.cache_read]
                .into_iter()
                .all(|price| price.is_finite() && price >= 0.0)
            || pricing.tiers.iter().any(|tier| {
                tier.above_input_tokens == 0
                    || !tier.input.is_finite()
                    || tier.input < 0.0
                    || !tier.output.is_finite()
                    || tier.output < 0.0
                    || tier.cache_write.is_some_and(|price| !price.is_finite() || price < 0.0)
                    || tier.cache_read.is_some_and(|price| !price.is_finite() || price < 0.0)
            })
        {
            return Err(ReleaseCatalogError::new("catalog pricing is invalid"));
        }
    }
    if let Some(router) = &model.router
        && (router.categories.is_empty()
            || !router.cost_per_million_tokens.is_finite()
            || router.cost_per_million_tokens < 0.0
            || router.latency_ms == 0)
    {
        return Err(ReleaseCatalogError::new("catalog router hints are invalid"));
    }
    Ok(())
}

fn validate_parameter_declaration(
    parameter: ModelParameter,
    declaration: &CatalogParameter,
) -> Result<(), ReleaseCatalogError> {
    if integer_parameter(parameter)
        && !matches!(
            declaration,
            CatalogParameter::Range { .. } | CatalogParameter::Unsupported
        )
    {
        return Err(ReleaseCatalogError::new(
            "integer catalog parameters require numeric ranges",
        ));
    }
    match declaration {
        CatalogParameter::Range {
            min,
            max,
            default,
            step,
            ..
        } => {
            if !min.is_finite()
                || !max.is_finite()
                || min > max
                || step.is_some_and(|step| !step.is_finite() || step <= 0.0)
                || default
                    .as_ref()
                    .is_some_and(|value| !declaration.accepts(value) || !valid_integer_parameter(parameter, value))
                || (integer_parameter(parameter)
                    && (min.fract() != 0.0 || max.fract() != 0.0 || step.is_some_and(|step| step.fract() != 0.0)))
            {
                return Err(ReleaseCatalogError::new(format!(
                    "catalog parameter '{}' has an invalid range",
                    parameter.as_str()
                )));
            }
        }
        CatalogParameter::Levels { levels, default, .. } => {
            let unique: BTreeSet<&str> = levels.iter().map(String::as_str).collect();
            if levels.is_empty()
                || unique.len() != levels.len()
                || levels.iter().any(|level| level.trim().is_empty())
                || default.as_ref().is_some_and(|value| !levels.contains(value))
            {
                return Err(ReleaseCatalogError::new(format!(
                    "catalog parameter '{}' has invalid levels",
                    parameter.as_str()
                )));
            }
        }
        CatalogParameter::Bool { .. } | CatalogParameter::Unsupported => {}
    }
    Ok(())
}

fn validate_secret_free(value: &Value) -> Result<(), ReleaseCatalogError> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let normalized = key.to_ascii_lowercase();
                if [
                    "api_key",
                    "token",
                    "access_token",
                    "refresh_token",
                    "secret",
                    "client_secret",
                    "credential",
                    "password",
                ]
                .contains(&normalized.as_str())
                    && !value.is_null()
                {
                    return Err(ReleaseCatalogError::new(
                        "the embedded model catalog contains a credential field",
                    ));
                }
                validate_secret_free(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_secret_free(value)?;
            }
        }
        Value::String(value) => {
            let looks_like_secret = value.starts_with(concat!("s", "k-"))
                || value.starts_with(concat!("g", "hp_"))
                || (value.starts_with(concat!("AK", "IA")) && value.len() >= 16);
            if looks_like_secret {
                return Err(ReleaseCatalogError::new(
                    "the embedded model catalog contains credential material",
                ));
            }
            if let Some((_, remainder)) = value.split_once("://") {
                let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
                if authority.contains('@') {
                    return Err(ReleaseCatalogError::new(
                        "the embedded model catalog URL contains userinfo",
                    ));
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

const fn integer_parameter(parameter: ModelParameter) -> bool {
    matches!(
        parameter,
        ModelParameter::MaxOutputTokens | ModelParameter::ThinkingBudget | ModelParameter::Seed
    )
}

fn valid_integer_parameter(parameter: ModelParameter, value: &Value) -> bool {
    !integer_parameter(parameter) || value.as_i64().is_some() || value.as_u64().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::catalog::{CatalogView, CuratedModel};

    fn minimal_catalog(model_tail: &str) -> String {
        format!(
            r#"{{
                "schema_version":"1.0",
                "revision":"fixture.1",
                "published_at":"2026-10-16T00:00:00Z",
                "providers":{{"openai":{{"default_model":"openai/{model_tail}","display_name":"OpenAI"}}}},
                "defaults":{{"provider":"openai","model":"openai/{model_tail}"}},
                "models":[{{
                    "id":"openai/{model_tail}","provider":"openai","api_model":"{model_tail}",
                    "display_name":"Fixture","aliases":[],"lifecycle":{{"status":"active"}},
                    "operations":["chat"],"features":{{"native_tools":true}},"limits":{{"context_window":8192}},
                    "parameters":{{"reasoning_effort":{{"kind":"levels","levels":["low","medium","high"],"default":"medium"}}}},
                    "levels":{{"low":{{"reasoning_effort":"low"}},"medium":{{"reasoning_effort":"medium"}},"high":{{"reasoning_effort":"high"}}}},
                    "default_level":"medium"
                }}]
            }}"#
        )
    }

    #[test]
    fn embedded_catalog_is_valid_and_every_chat_model_has_common_levels() {
        let Ok(catalog) = ReleaseCatalog::embedded() else {
            assert!(false, "the embedded release catalog must validate");
            return;
        };
        assert_eq!(catalog.schema_version, "1.0");
        assert_eq!(catalog.revision, "2026-10-31.3");
        assert_eq!(catalog.schema.as_deref(), Some("./models.schema.json"));
        assert!(catalog.content_sha256().starts_with("sha256:"));
        assert!(catalog.models.len() >= 100);
        assert!(
            catalog.warnings().is_empty(),
            "unexpected warnings: {:?}",
            catalog.warnings()
        );
        for model in catalog
            .models
            .iter()
            .filter(|model| model.supports(ModelOperation::Chat))
        {
            assert!(model.levels.contains_key("low"), "{} lacks low", model.id);
            assert!(model.levels.contains_key("medium"), "{} lacks medium", model.id);
            assert!(model.levels.contains_key("high"), "{} lacks high", model.id);
        }
    }

    fn independent_active_allowlist(raw: &Value, provider: &str) -> BTreeSet<String> {
        let mut spellings = BTreeSet::new();
        let Some(models) = raw.get("models").and_then(Value::as_array) else {
            return spellings;
        };
        for model in models {
            if model.get("provider").and_then(Value::as_str) != Some(provider) {
                continue;
            }
            if model.pointer("/lifecycle/status").and_then(Value::as_str) != Some("active") {
                continue;
            }
            let Some(api_model) = model.get("api_model").and_then(Value::as_str) else {
                continue;
            };
            if api_model == "*" {
                continue;
            }
            spellings.insert(api_model.to_string());
            if let Some(aliases) = model.get("aliases").and_then(Value::as_array) {
                for alias in aliases {
                    let Some(alias) = alias.as_str().map(str::trim).filter(|alias| !alias.is_empty()) else {
                        continue;
                    };
                    spellings.insert(alias.to_string());
                }
            }
        }
        spellings
    }

    #[test]
    fn provider_allowlists_equal_the_catalog_active_set() {
        let raw: Value = serde_json::from_str(EMBEDDED_CATALOG_JSON).expect("test: catalog json");
        let catalog = ReleaseCatalog::embedded().expect("test: catalog");
        let providers = raw
            .get("providers")
            .and_then(Value::as_object)
            .expect("test: providers");
        for provider in providers.keys() {
            let expected = independent_active_allowlist(&raw, provider);
            assert_eq!(
                catalog.active_allowlist(provider).unwrap_or_default(),
                expected,
                "{provider}"
            );
        }
        let kimi = independent_active_allowlist(&raw, "kimi-code");
        for spelling in &kimi {
            for provider in ["kimi-code", "kimi_coding"] {
                crate::providers::validate_provider_model(provider, spelling).unwrap_or_else(|error| {
                    panic!("{provider}/{spelling} is an active catalog model: {error}");
                });
            }
        }
        for id in [
            "k3",
            "k3-256k",
            "k3.1-pro",
            "k3.1-pro-highspeed",
            "kimi-for-coding",
            "kimi-for-coding-highspeed",
        ] {
            assert!(kimi.contains(id), "{id}");
        }
        let refused = crate::providers::validate_provider_model("kimi-code", "definitely-not-a-real-model")
            .expect_err("test: unknown kimi-code model")
            .to_string();
        let listed = refused
            .split_once("Supported models: ")
            .expect("test: supported-model list")
            .1;
        let listed: BTreeSet<String> = listed.split(", ").map(str::to_string).collect();
        assert_eq!(listed, kimi);
        assert!(crate::providers::validate_provider_model("kimi-code", "K3").is_err());
        assert!(crate::providers::validate_provider_model("openai", "prx-unlisted-model").is_ok());

        for info in crate::providers::list_providers() {
            let expected = std::iter::once(info.name)
                .chain(info.aliases.iter().copied())
                .filter(|name| providers.contains_key(*name))
                .flat_map(|name| independent_active_allowlist(&raw, name))
                .collect::<BTreeSet<_>>();
            assert_eq!(
                crate::providers::provider_model_allowlist(info.name).unwrap_or_default(),
                expected,
                "{}",
                info.name
            );
        }
        for uncovered in [
            "vercel",
            "cloudflare",
            "synthetic",
            "opencode",
            "qianfan",
            "lmstudio",
            "ovhcloud",
            "mock",
        ] {
            assert!(
                crate::providers::provider_model_allowlist(uncovered).is_none(),
                "{uncovered}"
            );
        }
    }

    #[test]
    fn subscription_rows_have_no_contradictory_token_price() {
        let raw: Value = serde_json::from_str(EMBEDDED_CATALOG_JSON).expect("test: catalog json");
        let mut marked = 0_usize;
        for row in raw.get("models").and_then(Value::as_array).expect("test: models") {
            if row.get("billing").and_then(Value::as_str) != Some("subscription") {
                continue;
            }
            marked += 1;
            assert!(
                row.get("pricing").is_none(),
                "{} carries a token price",
                row.get("id").and_then(Value::as_str).unwrap_or("<missing>")
            );
        }
        assert!(marked > 0, "the release marks subscription rows");
        let catalog = ReleaseCatalog::embedded().expect("test: catalog");
        assert!(catalog.models.iter().any(|model| {
            model.provider == "kimi-code" && model.api_model == "kimi-for-coding" && model.is_subscription_billed()
        }));
        assert!(
            catalog.models.iter().any(|model| {
                model.provider == "openai-codex" && model.is_active() && model.is_subscription_billed()
            })
        );
        assert!(
            catalog
                .models
                .iter()
                .any(|model| { model.provider == "copilot" && model.is_active() && model.is_subscription_billed() })
        );
        assert!(
            catalog
                .models
                .iter()
                .filter(|model| model.provider == "xai" || model.provider == "qwen-code" || model.provider == "zai")
                .all(|model| !model.is_subscription_billed())
        );

        let mut contradictory = serde_json::from_str::<Value>(&minimal_catalog("fixture")).expect("test: fixture");
        let row = contradictory
            .pointer_mut("/models/0")
            .and_then(Value::as_object_mut)
            .expect("test: fixture row");
        row.insert("billing".into(), Value::from("subscription"));
        row.insert(
            "pricing".into(),
            serde_json::json!({"unit":"usd_per_1m_tokens","input":1.0,"output":2.0}),
        );
        let error = ReleaseCatalog::from_json(&contradictory.to_string()).expect_err("test: subscription plus price");
        assert!(
            error.to_string().contains("must not carry a per-token price"),
            "{error}"
        );
    }

    #[test]
    fn active_router_ranking_hints_match_the_published_input_baseline() {
        let catalog = ReleaseCatalog::embedded().expect("test: catalog");
        for row in catalog.models.iter().filter(|row| row.is_active()) {
            if let (Some(router), Some(pricing)) = (&row.router, &row.pricing) {
                assert!(
                    (router.cost_per_million_tokens - pricing.input).abs() < f64::EPSILON,
                    "{} ranking input tariff",
                    row.id
                );
            }
        }
        let legacy = catalog.model(&ProviderId::of("openai"), "gpt-4o").unwrap();
        assert!(
            (legacy.router.as_ref().unwrap().cost_per_million_tokens - legacy.pricing.as_ref().unwrap().input).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn every_priced_release_row_has_a_tariff_citation_or_the_documented_zero_rule() {
        let catalog = ReleaseCatalog::embedded().expect("test: release catalog");
        let docs = include_str!("../../../docs/providers.md");
        let table = docs
            .split_once("<!-- catalog-tariff-citations:start -->")
            .and_then(|(_, rest)| rest.split_once("<!-- catalog-tariff-citations:end -->"))
            .map(|(table, _)| table)
            .expect("test: citation table exists");
        let mut citations = BTreeMap::new();
        for line in table.lines() {
            let mut columns = line.split('|').skip(1);
            let Some(id) = columns
                .next()
                .and_then(|column| column.trim().strip_prefix('`'))
                .and_then(|id| id.strip_suffix('`'))
            else {
                continue;
            };
            let source = columns.next().expect("test: citation source column").trim();
            assert!(citations.insert(id, source).is_none(), "duplicate citation: {id}");
            assert!(
                catalog.models.iter().any(|row| row.id == id && row.pricing.is_some()),
                "stale citation: {id}"
            );
        }
        for row in catalog.models.iter().filter(|row| row.pricing.is_some()) {
            let pricing = row.pricing.as_ref().expect("test: priced row");
            if row.provider == "ollama" {
                assert_eq!(
                    citations.get("ollama/*"),
                    Some(&"[Documented Ollama zero rule](#ollama-zero-cost-rule)")
                );
                assert!(docs.contains("### Ollama zero-cost rule"));
                for rate in [pricing.input, pricing.output, pricing.cache_write, pricing.cache_read] {
                    assert_eq!(
                        rate.to_bits(),
                        0.0_f64.to_bits(),
                        "{} is covered by the zero rule",
                        row.id
                    );
                }
                assert!(
                    pricing.tiers.is_empty(),
                    "{} has no priced tier under the zero rule",
                    row.id
                );
            } else {
                let source = citations.get(row.id.as_str());
                assert!(source.is_some(), "uncited tariff: {}", row.id);
                let source = source.expect("test: every priced model has a citation");
                let url = source
                    .split_once("](https://")
                    .and_then(|(_, tail)| tail.split_once(')'))
                    .map(|(tail, _)| format!("https://{tail}"))
                    .expect("test: published HTTPS tariff link");
                let parsed = reqwest::Url::parse(&url).expect("test: public source URL");
                assert!(parsed.host_str().is_some() && parsed.username().is_empty() && parsed.password().is_none());
            }
        }
    }

    #[test]
    fn stable_catalog_ids_keep_settings_keys_when_the_wire_name_changes() {
        let catalog = ReleaseCatalog::embedded().expect("test: release catalog");
        for (provider, stable_id, wire, short) in [
            ("xai", "xai/grok-4.20", "grok-4.20-0309-reasoning", "grok-4.20"),
            (
                "xai",
                "xai/grok-4.20-non-reasoning-latest",
                "grok-4.20-0309-non-reasoning",
                "grok-4.20-non-reasoning-latest",
            ),
            (
                "mistral",
                "mistral/codestral-latest",
                "codestral-2508",
                "codestral-latest",
            ),
        ] {
            let provider_id = ProviderId::of(provider);
            let by_id = catalog
                .model(&provider_id, stable_id)
                .expect("test: stable settings id");
            let by_wire = catalog.model(&provider_id, wire).expect("test: dated wire name");
            let by_short = catalog.model(&provider_id, short).expect("test: short alias");
            assert_eq!(by_id.id, stable_id);
            assert_eq!(by_id.api_model, wire);
            assert_eq!(by_wire.id, stable_id);
            assert_eq!(by_short.id, stable_id);
            assert!(by_id.aliases.iter().any(|alias| alias == wire));
            assert!(by_id.aliases.iter().any(|alias| alias == short));
        }
        assert_eq!(
            catalog.providers.get("xai").expect("test: xai provider").default_model,
            "xai/grok-4.20"
        );
        assert_eq!(catalog.default_model("xai"), Some("grok-4.20-0309-reasoning"));
        assert_eq!(
            catalog
                .model(&ProviderId::of("xai"), "grok-4")
                .expect("test: retired grok-4")
                .lifecycle
                .replacement
                .as_deref(),
            Some("xai/grok-4.20")
        );
        assert_eq!(
            catalog
                .model(&ProviderId::of("gemini"), "gemini-3.5-flash")
                .expect("test: deprecated gemini flash")
                .lifecycle
                .replacement
                .as_deref(),
            Some("gemini/gemini-3.8-flash")
        );
    }

    #[test]
    fn openai_release_tariffs_include_published_cached_and_long_context_rates() {
        let catalog = ReleaseCatalog::embedded().expect("test: embedded release catalog");
        let provider = ProviderId::of("openai");
        for (model, input, output, write, read) in [
            ("gpt-4o", 2.5_f64, 10.0, 0.0, 1.25),
            ("gpt-4o-mini", 0.15, 0.6, 0.0, 0.075),
            ("gpt-5.2-codex", 1.75, 14.0, 0.0, 0.175),
            ("gpt-5.6", 4.0, 20.0, 0.0, 0.4),
            ("gpt-5.6-sol", 4.0, 20.0, 0.0, 0.4),
            ("gpt-5.6-luna", 0.2, 1.2, 0.0, 0.02),
            ("gpt-5.6-terra", 2.0, 12.0, 0.0, 0.2),
            ("o1-preview", 15.0, 60.0, 0.0, 7.5),
        ] {
            let pricing = catalog
                .model(&provider, model)
                .expect("test: published OpenAI model")
                .pricing
                .as_ref()
                .expect("test: published OpenAI tariff");
            for (actual, expected) in [
                (pricing.input, input),
                (pricing.output, output),
                (pricing.cache_write, write),
                (pricing.cache_read, read),
            ] {
                assert!((actual - expected).abs() < 1e-9, "{model}: {actual} != {expected}");
            }
            if model.starts_with("gpt-5.6") {
                assert_eq!(pricing.tiers.len(), 1);
                let tier = pricing.tiers.first().expect("test: published long-context tier");
                assert_eq!(tier.above_input_tokens, 272_000);
                for (actual, expected) in [
                    (tier.input, input * 2.0),
                    (tier.output, output * 1.5),
                    (
                        tier.cache_write.expect("test: long-context cache-write rate"),
                        write * 2.0,
                    ),
                    (
                        tier.cache_read.expect("test: long-context cached-input rate"),
                        read * 2.0,
                    ),
                ] {
                    assert!(
                        (actual - expected).abs() < 1e-9,
                        "{model} long context: {actual} != {expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn gemini_release_tariffs_pin_cached_input_and_the_long_context_threshold() {
        let catalog = ReleaseCatalog::embedded().expect("test: embedded release catalog");
        let provider = ProviderId::of("gemini");
        let lite = catalog
            .model(&provider, "gemini-3.1-flash-lite")
            .and_then(|model| model.pricing.as_ref())
            .expect("test: published Flash-Lite tariff");
        assert!((lite.input - 0.25).abs() < 1e-9);
        assert!((lite.output - 1.5).abs() < 1e-9);
        assert!((lite.cache_read - 0.025).abs() < 1e-9);
        let pro = catalog
            .model(&provider, "gemini-3.1-pro-preview")
            .and_then(|model| model.pricing.as_ref())
            .expect("test: published Pro tariff");
        assert!((pro.input - 2.0).abs() < 1e-9);
        assert!((pro.output - 12.0).abs() < 1e-9);
        assert!((pro.cache_read - 0.2).abs() < 1e-9);
        assert_eq!(pro.tiers.len(), 1);
        let long = pro.tiers.first().expect("test: published long-context tariff");
        assert_eq!(long.above_input_tokens, 200_000);
        assert!((long.input - 4.0).abs() < 1e-9);
        assert!((long.output - 18.0).abs() < 1e-9);
        assert!((long.cache_read.expect("test: long-context cached input") - 0.4).abs() < 1e-9);
    }

    #[test]
    fn unverified_legacy_tariffs_are_not_replaced_with_other_models_prices() {
        let catalog = ReleaseCatalog::embedded().expect("test: embedded release catalog");
        for (provider, model) in [
            ("anthropic", "claude-3-haiku"),
            ("anthropic", "claude-3.5-sonnet"),
            ("anthropic", "claude-haiku-3"),
            ("gemini", "gemini-1.5-pro"),
            ("gemini", "gemini-2.0-flash"),
            ("moonshot", "kimi-k2.5"),
            ("openrouter", "moonshotai/kimi-k2.5"),
            ("xai", "grok-3"),
        ] {
            assert!(
                catalog
                    .model(&ProviderId::of(provider), model)
                    .expect("test: retained model identity")
                    .pricing
                    .is_none(),
                "{provider}/{model}: unverified tariff must remain absent"
            );
        }
    }

    #[test]
    fn embedded_schema_is_json_schema_2020_12_for_catalog_v1() {
        let schema: Value = serde_json::from_str(EMBEDDED_CATALOG_SCHEMA).expect("test: catalog schema is valid JSON");
        assert_eq!(
            schema.get("$schema").and_then(Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema")
        );
        assert_eq!(
            schema
                .pointer("/properties/schema_version/pattern")
                .and_then(Value::as_str),
            Some(r"^1\.[0-9]+$")
        );
        assert!(schema.pointer("/$defs/model").is_some());
        assert!(schema.pointer("/$defs/parameter").is_some());
    }

    #[test]
    fn semantic_validation_rejects_alias_level_and_secret_regressions() {
        let mut duplicate = minimal_catalog("gpt-fixture");
        duplicate = duplicate.replace("\"aliases\":[]", "\"aliases\":[\"openai/gpt-fixture\"]");
        assert!(ReleaseCatalog::from_json(&duplicate).is_err());

        let missing_level = minimal_catalog("gpt-fixture").replace(",\"high\":{\"reasoning_effort\":\"high\"}", "");
        assert!(ReleaseCatalog::from_json(&missing_level).is_err());

        let secret_value = format!("{}-fixture-secret", "sk");
        let secret = minimal_catalog("gpt-fixture").replace(
            "\"display_name\":\"Fixture\"",
            &format!("\"display_name\":\"{secret_value}\""),
        );
        assert!(ReleaseCatalog::from_json(&secret).is_err());

        let unknown_field = minimal_catalog("gpt-fixture").replace(
            "\"context_window\":8192",
            "\"context_window\":8192,\"context_windw\":4096",
        );
        assert!(
            ReleaseCatalog::from_json(&unknown_field).is_err(),
            "a misspelled schema-v1 field must not be silently dropped"
        );

        let mut alias_collision: Value =
            serde_json::from_str(&minimal_catalog("gpt-fixture")).expect("test: fixture JSON");
        let models = alias_collision
            .get_mut("models")
            .and_then(Value::as_array_mut)
            .expect("test: fixture models array");
        let first = models
            .first_mut()
            .and_then(Value::as_object_mut)
            .expect("test: fixture first model object");
        *first.get_mut("aliases").expect("test: aliases field") = serde_json::json!(["gpt-other"]);
        let mut other = Value::Object(first.clone());
        let other_fields = other.as_object_mut().expect("test: cloned model object");
        *other_fields.get_mut("id").expect("test: id field") = Value::String("openai/gpt-other".to_string());
        *other_fields.get_mut("api_model").expect("test: API model field") = Value::String("gpt-other".to_string());
        *other_fields.get_mut("aliases").expect("test: aliases field") = serde_json::json!([]);
        models.push(other);
        assert!(
            ReleaseCatalog::from_json(&alias_collision.to_string()).is_err(),
            "an alias must not collide with another row's API model"
        );
    }

    #[test]
    fn listed_models_are_selectable_chat_rows_with_the_provider_default_first() {
        let catalog = ReleaseCatalog::embedded().expect("test: embedded catalog validates");
        let anthropic = catalog.listed_models("anthropic");
        assert_eq!(
            anthropic.first().map(|model| model.id.as_str()),
            Some("anthropic/claude-sonnet-5")
        );
        assert_eq!(
            anthropic.len(),
            12,
            "retired price/context rows stay out of new selections"
        );
        assert!(
            anthropic
                .iter()
                .all(|model| model.lifecycle.status != ModelLifecycleStatus::Retired)
        );
        assert!(anthropic.iter().all(|model| model.supports(ModelOperation::Chat)));

        let openrouter = catalog.listed_models("openrouter");
        assert_eq!(
            openrouter.first().map(|model| model.id.as_str()),
            Some("openrouter/anthropic/claude-sonnet-5")
        );
        assert!(
            openrouter.iter().all(|model| model.api_model != "typesafe/jev-1.13"),
            "a decision-only model must not be offered as a chat selection"
        );
    }

    #[test]
    fn deprecated_chat_models_remain_selectable_with_a_replacement_warning() {
        let document = minimal_catalog("gpt-fixture");
        let mut value: Value = serde_json::from_str(&document).expect("test: catalog fixture JSON");
        let models = value
            .get_mut("models")
            .and_then(Value::as_array_mut)
            .expect("test: models array");
        let mut deprecated = models.first().cloned().expect("test: first model");
        let fields = deprecated.as_object_mut().expect("test: model object");
        fields.insert("id".to_owned(), Value::String("openai/gpt-legacy".to_owned()));
        fields.insert("api_model".to_owned(), Value::String("gpt-legacy".to_owned()));
        fields.insert("display_name".to_owned(), Value::String("Legacy fixture".to_owned()));
        fields.insert(
            "lifecycle".to_owned(),
            serde_json::json!({
                "status": "deprecated",
                "deprecated_at": "2026-10-16T00:00:00Z",
                "replacement": "openai/gpt-fixture"
            }),
        );
        models.push(deprecated);
        let catalog = ReleaseCatalog::from_json(&value.to_string()).expect("test: deprecated row validates");
        let listed = catalog.listed_models("openai");
        let model = listed
            .into_iter()
            .find(|model| model.id == "openai/gpt-legacy")
            .expect("test: deprecated row stays listed");
        assert_eq!(model.lifecycle.status, ModelLifecycleStatus::Deprecated);
        assert_eq!(
            model.selection_warning().as_deref(),
            Some("deprecated; use openai/gpt-fixture")
        );
    }

    #[test]
    fn deprecated_models_cannot_anchor_provider_or_global_defaults() {
        let document = minimal_catalog("gpt-fixture");
        let mut value: Value = serde_json::from_str(&document).expect("test: catalog fixture JSON");
        let models = value
            .get_mut("models")
            .and_then(Value::as_array_mut)
            .expect("test: models array");
        let mut deprecated = models.first().cloned().expect("test: first model");
        let fields = deprecated.as_object_mut().expect("test: model object");
        fields.insert("id".to_owned(), Value::String("openai/gpt-legacy".to_owned()));
        fields.insert("api_model".to_owned(), Value::String("gpt-legacy".to_owned()));
        fields.insert(
            "lifecycle".to_owned(),
            serde_json::json!({
                "status": "deprecated",
                "deprecated_at": "2026-10-16T00:00:00Z"
            }),
        );
        models.push(deprecated);

        let mut provider_default = value.clone();
        provider_default
            .get_mut("providers")
            .and_then(Value::as_object_mut)
            .and_then(|providers| providers.get_mut("openai"))
            .and_then(Value::as_object_mut)
            .expect("test: openai provider object")
            .insert(
                "default_model".to_owned(),
                Value::String("openai/gpt-legacy".to_owned()),
            );
        let refusal =
            ReleaseCatalog::from_json(&provider_default.to_string()).expect_err("provider default must be active");
        assert!(refusal.to_string().contains("provider has an invalid default model"));

        value
            .get_mut("defaults")
            .and_then(Value::as_object_mut)
            .expect("test: defaults object")
            .insert("model".to_owned(), Value::String("openai/gpt-legacy".to_owned()));
        let refusal = ReleaseCatalog::from_json(&value.to_string()).expect_err("global default must be active");
        assert!(refusal.to_string().contains("catalog-wide provider/model default"));
    }

    #[test]
    fn catalog_descriptor_carries_revision_and_adapter_ready_facts() {
        let Ok(catalog) = ReleaseCatalog::from_json(&minimal_catalog("gpt-fixture")) else {
            assert!(false, "fixture catalog must validate");
            return;
        };
        let provider = ProviderId::of("openai");
        let Some(descriptor) = catalog.descriptor(&provider, "gpt-fixture") else {
            assert!(false, "catalog descriptor must exist");
            return;
        };
        assert!(descriptor.operation(ModelOperation::Chat).is_supported());
        assert!(descriptor.features.native_tools.is_supported());
        assert_eq!(descriptor.context_window, Some(8192));
        assert_eq!(
            descriptor.source,
            DescriptorSource::Catalog {
                revision: Arc::from("fixture.1")
            }
        );
    }

    #[test]
    fn model_matching_accepts_vendor_and_variant_spellings_without_wildcard_verification() {
        let catalog = ReleaseCatalog::embedded().expect("test: embedded catalog validates");

        assert!(
            catalog
                .model(&ProviderId::of("openrouter"), "moonshotai/kimi-k2.5:free")
                .is_some(),
            "an aggregator suffix keeps the base model's catalog facts"
        );
        assert!(
            catalog
                .model(&ProviderId::of("anthropic"), "anthropic/claude-sonnet-5")
                .is_some(),
            "a vendor/model spelling matches a direct-provider API model"
        );
        assert!(
            catalog
                .model(&ProviderId::of("ollama"), "prx-unknown-local-model")
                .is_none(),
            "the router-only Ollama wildcard must not verify every local model"
        );
    }

    #[test]
    fn catalog_view_applies_discovery_operator_and_catalog_precedence() {
        let mut value: Value =
            serde_json::from_str(&minimal_catalog("gpt-default")).expect("test: fixture catalog is JSON");
        let models = value
            .get_mut("models")
            .and_then(Value::as_array_mut)
            .expect("test: fixture has models");
        let mut embedding = models.first().expect("test: fixture has a default model").clone();
        let embedding_fields = embedding.as_object_mut().expect("test: cloned model object");
        *embedding_fields.get_mut("id").expect("test: id field") = Value::String("openai/gpt-fixture".to_string());
        *embedding_fields.get_mut("api_model").expect("test: API model field") =
            Value::String("gpt-fixture".to_string());
        *embedding_fields.get_mut("operations").expect("test: operations field") = serde_json::json!(["embedding"]);
        models.push(embedding);
        let catalog = ReleaseCatalog::from_json(&value.to_string()).expect("test: fixture catalog validates");
        let provider = ProviderId::of("openai");

        let catalog_only = CatalogView::default().with_release_catalog(catalog);
        assert!(
            catalog_only
                .describe(&provider, "gpt-fixture")
                .operation(ModelOperation::Chat)
                .is_unsupported(),
            "the release catalog supplies the lowest-priority capability fact"
        );

        let mut positive_discovery = ModelDescriptor::id_only(provider.clone(), "gpt-fixture");
        positive_discovery
            .operations
            .insert(ModelOperation::Chat, Support::Supported);
        positive_discovery.source = DescriptorSource::Discovery { fetched_at_unix: 1 };
        let discovered = catalog_only.clone().with_discovered([positive_discovery]);
        assert!(
            discovered
                .describe(&provider, "gpt-fixture")
                .operation(ModelOperation::Chat)
                .is_supported(),
            "positive discovery outranks a negative release-catalog fact"
        );

        let declared_positive = catalog_only.with_curated([CuratedModel {
            operations: Some(vec![ModelOperation::Chat]),
            ..CuratedModel::new(provider.clone(), Some("gpt-fixture"))
        }]);
        assert!(
            declared_positive
                .describe(&provider, "gpt-fixture")
                .operation(ModelOperation::Chat)
                .is_supported(),
            "an operator declaration outranks a negative release-catalog fact"
        );

        let declared_negative = discovered.with_curated([CuratedModel {
            operations: Some(Vec::new()),
            ..CuratedModel::new(provider.clone(), Some("gpt-fixture"))
        }]);
        assert!(
            declared_negative
                .describe(&provider, "gpt-fixture")
                .operation(ModelOperation::Chat)
                .is_unsupported(),
            "an operator declaration outranks positive discovery"
        );

        let mut negative_discovery = ModelDescriptor::id_only(provider.clone(), "gpt-fixture");
        negative_discovery.operations.insert(
            ModelOperation::Chat,
            Support::unsupported("provider discovery reports no chat operation"),
        );
        negative_discovery.source = DescriptorSource::Discovery { fetched_at_unix: 2 };
        let discovery_denial = declared_positive.with_discovered([negative_discovery]);
        assert!(
            discovery_denial
                .describe(&provider, "gpt-fixture")
                .operation(ModelOperation::Chat)
                .is_unsupported(),
            "negative discovery outranks an operator declaration"
        );
    }
}

#[cfg(test)]
mod integer_parameter_tests {
    use super::*;
    #[test]
    fn fractional_integer_ranges_defaults_and_levels_are_rejected() {
        for pointer in [
            "/parameters/max_output_tokens/min",
            "/parameters/max_output_tokens/max",
            "/parameters/max_output_tokens/step",
            "/parameters/max_output_tokens/default",
            "/levels/high/max_output_tokens",
        ] {
            let mut raw: Value = serde_json::from_str(EMBEDDED_CATALOG_JSON).unwrap();
            let row = raw
                .get_mut("models")
                .unwrap()
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|row| row.get("id").and_then(Value::as_str) == Some("anthropic/claude-sonnet-5"))
                .unwrap();
            if pointer.ends_with("/step") {
                row.pointer_mut("/parameters/max_output_tokens")
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert("step".into(), Value::from(0.5));
            } else {
                let value = if pointer.ends_with("/min") {
                    0.5
                } else if pointer.ends_with("/max") {
                    128_000.5
                } else {
                    1024.5
                };
                *row.pointer_mut(pointer).unwrap() = Value::from(value);
            }
            assert!(ReleaseCatalog::from_json(&raw.to_string()).is_err(), "{pointer}");
        }
    }
    #[test]
    fn integer_parameter_declarations_cannot_supply_boolean_or_string_defaults() {
        for declaration in [
            serde_json::json!({"kind":"bool","default":true}),
            serde_json::json!({"kind":"levels","levels":["2048"],"default":"2048"}),
        ] {
            let mut raw: Value = serde_json::from_str(EMBEDDED_CATALOG_JSON).unwrap();
            let row = raw
                .get_mut("models")
                .unwrap()
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|row| row.get("id").and_then(Value::as_str) == Some("anthropic/claude-sonnet-5"))
                .unwrap();
            row.get_mut("parameters")
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("max_output_tokens".into(), declaration);
            // Isolate the declaration itself: no selected-level value should
            // cause this rejection before the invalid default is inspected.
            for level in row.get_mut("levels").unwrap().as_object_mut().unwrap().values_mut() {
                level.as_object_mut().unwrap().remove("max_output_tokens");
            }
            assert!(ReleaseCatalog::from_json(&raw.to_string()).is_err());
        }
    }
}
