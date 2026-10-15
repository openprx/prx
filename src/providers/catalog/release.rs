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

use super::{DescriptorSource, FeatureSet, ModelDescriptor, ModelOperation, Support};
use crate::providers::ProviderId;

const EMBEDDED_CATALOG_JSON: &str = include_str!("../../../catalog/models.json");
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
pub struct CatalogLimits {
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

/// A price tier above an input-token threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogPriceTier {
    pub above_input_tokens: u64,
    pub input: f64,
    pub output: f64,
}

/// Per-million-token prices shipped by a release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[serde(tag = "kind", rename_all = "snake_case")]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRouterHints {
    pub categories: Vec<String>,
    pub latency_ms: u32,
    pub quality_rank: u32,
}

/// Hosted transport recommendations supplied by a model entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogTransport {
    #[serde(default)]
    pub recommended_timeout_ms: Option<u64>,
    #[serde(default)]
    pub extra_latency_budget_ms: Option<u64>,
}

/// One model record in `catalog/models.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Whether this row can be offered for a new selection.
    #[must_use]
    pub fn is_listed(&self) -> bool {
        self.lifecycle.status != ModelLifecycleStatus::Retired && self.api_model != "*"
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

/// Provider display and default-selection metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseProvider {
    pub default_model: String,
    pub display_name: String,
}

/// The consolidated defaults shared by every surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseDefaults {
    pub provider: String,
    pub model: String,
}

/// The validated embedded catalog document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        static CATALOG: OnceLock<Result<Arc<ReleaseCatalog>, ReleaseCatalogError>> = OnceLock::new();
        CATALOG
            .get_or_init(|| Self::from_json(EMBEDDED_CATALOG_JSON).map(Arc::new))
            .clone()
    }

    /// Forward-compatibility warnings collected while loading.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Find one model under the canonical provider identity.
    #[must_use]
    pub fn model(&self, provider: &ProviderId, selected: &str) -> Option<&ReleaseModel> {
        let provider = provider.catalog_name()?;
        let selected = selected.trim();
        self.models.iter().find(|model| {
            model.provider.eq_ignore_ascii_case(provider)
                && (model.api_model.eq_ignore_ascii_case(selected)
                    || model.id.eq_ignore_ascii_case(selected)
                    || model.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(selected)))
        })
    }

    /// Models offered for one provider, in catalog order.
    #[must_use]
    pub fn listed_models(&self, provider: &str) -> Vec<&ReleaseModel> {
        let Some(provider) = self.provider_name(provider) else {
            return Vec::new();
        };
        self.models
            .iter()
            .filter(|model| model.provider.eq_ignore_ascii_case(provider) && model.is_listed())
            .collect()
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

        let mut identities = BTreeSet::new();
        let mut canonical_ids = BTreeSet::new();
        for model in &self.models {
            validate_model_shape(model)?;
            let canonical = format!("{}/{}", model.provider, model.api_model);
            if model.id != canonical {
                return Err(ReleaseCatalogError::new(format!(
                    "catalog model '{}' is not the canonical provider/api_model id",
                    model.id
                )));
            }
            let id = model.id.to_ascii_lowercase();
            if !canonical_ids.insert(id.clone()) || !identities.insert(id) {
                return Err(ReleaseCatalogError::new(
                    "the embedded model catalog has a duplicate model id",
                ));
            }
            for alias in &model.aliases {
                let alias = alias.trim().to_ascii_lowercase();
                if alias.is_empty() || !identities.insert(alias) {
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
            if name.trim().is_empty()
                || provider.display_name.trim().is_empty()
                || !canonical_ids.contains(&provider.default_model.to_ascii_lowercase())
                || !provider.default_model.starts_with(&format!("{name}/"))
            {
                return Err(ReleaseCatalogError::new(
                    "a catalog provider has an invalid default model",
                ));
            }
        }
        if !self.providers.contains_key(&self.defaults.provider)
            || !canonical_ids.contains(&self.defaults.model.to_ascii_lowercase())
            || !self.defaults.model.starts_with(&format!("{}/", self.defaults.provider))
        {
            return Err(ReleaseCatalogError::new(
                "the catalog-wide provider/model default is invalid",
            ));
        }
        Ok(())
    }
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
            if !declaration.accepts(value) {
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
            })
        {
            return Err(ReleaseCatalogError::new("catalog pricing is invalid"));
        }
    }
    if let Some(router) = &model.router
        && (router.categories.is_empty() || router.latency_ms == 0)
    {
        return Err(ReleaseCatalogError::new("catalog router hints are invalid"));
    }
    Ok(())
}

fn validate_parameter_declaration(
    parameter: ModelParameter,
    declaration: &CatalogParameter,
) -> Result<(), ReleaseCatalogError> {
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
                || default.as_ref().is_some_and(|value| !declaration.accepts(value))
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
        assert_eq!(catalog.revision, "2026-10-16.1");
        assert_eq!(catalog.schema.as_deref(), Some("./models.schema.json"));
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
    fn catalog_view_applies_discovery_operator_and_catalog_precedence() {
        let json =
            minimal_catalog("gpt-fixture").replace("\"operations\":[\"chat\"]", "\"operations\":[\"embedding\"]");
        let catalog = ReleaseCatalog::from_json(&json).expect("test: fixture catalog validates");
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
