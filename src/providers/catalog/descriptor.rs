//! Typed, operation-aware descriptions of what one model of one provider can do.
//!
//! A [`ModelDescriptor`] answers, for a `(ProviderId, model)` pair, which
//! [`ModelOperation`]s it can execute and which chat [`Feature`]s it has, each
//! as a tri-state [`Support`]: `Unknown` never proves support. It also carries
//! the tool dialects that model speaks, resolved for that model rather than
//! folded across the endpoint.
//!
//! The types here are data only. Which evidence produces a descriptor and how
//! evidence combines is the catalog view's concern; what a caller may do
//! with one is the resolver's.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::providers::ProviderId;
use crate::providers::ToolCallDialect;
use crate::tools::ToolSchemaDialect;

/// Something a model can be asked to do, as a whole request.
///
/// "Tool calling", "vision" and "streaming" are not operations: they are
/// [`Feature`]s of [`Chat`](Self::Chat). `DecisionEndpoint` is an upstream
/// classification endpoint (OpenRouter style); it is unrelated to the router's
/// own `RouteDecision`, which is why it is not called `Decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub enum ModelOperation {
    /// Generative chat and the tool loop: the `Provider` trait.
    Chat,
    /// Text embeddings: the `EmbeddingProvider` trait.
    Embedding,
    /// A typed classification endpoint.
    DecisionEndpoint,
    /// Document reranking.
    Rerank,
    /// Image generation.
    ImageGeneration,
    /// Audio generation.
    AudioGeneration,
    /// Video generation.
    VideoGeneration,
}

impl ModelOperation {
    /// Every operation, in a stable order.
    pub const ALL: [Self; 7] = [
        Self::Chat,
        Self::Embedding,
        Self::DecisionEndpoint,
        Self::Rerank,
        Self::ImageGeneration,
        Self::AudioGeneration,
        Self::VideoGeneration,
    ];

    /// The configuration and display spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Embedding => "embedding",
            Self::DecisionEndpoint => "decision_endpoint",
            Self::Rerank => "rerank",
            Self::ImageGeneration => "image_generation",
            Self::AudioGeneration => "audio_generation",
            Self::VideoGeneration => "video_generation",
        }
    }
}

impl fmt::Display for ModelOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether something is supported, as far as the evidence goes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Support {
    /// Positive evidence.
    Supported,
    /// Negative evidence, with the reason in words an operator can act on.
    Unsupported(Cow<'static, str>),
    /// No evidence either way. Never treated as support.
    Unknown,
}

impl Support {
    /// Negative evidence with a static reason.
    #[must_use]
    pub const fn unsupported(reason: &'static str) -> Self {
        Self::Unsupported(Cow::Borrowed(reason))
    }

    /// Whether this is positive evidence.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        matches!(self, Self::Supported)
    }

    /// Whether this is negative evidence.
    #[must_use]
    pub const fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported(_))
    }

    /// `Some(true)`/`Some(false)` as positive/negative evidence, `None` as none.
    #[must_use]
    pub const fn from_declared(declared: Option<bool>, negative: &'static str) -> Self {
        match declared {
            Some(true) => Self::Supported,
            Some(false) => Self::unsupported(negative),
            None => Self::Unknown,
        }
    }

    /// Combine two pieces of evidence about the same fact.
    ///
    /// Negative evidence from any source wins over positive evidence, and
    /// positive evidence wins over none: a provider-wide "tools: yes" never
    /// outweighs a model record that says it takes no tools. When both sides
    /// are negative the first reason is kept.
    #[must_use]
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unsupported(reason), _) | (_, Self::Unsupported(reason)) => Self::Unsupported(reason),
            (Self::Supported, _) | (_, Self::Supported) => Self::Supported,
            (Self::Unknown, Self::Unknown) => Self::Unknown,
        }
    }

    /// Cap this evidence at what an adapter can execute.
    ///
    /// An adapter that has no transport for something makes it unsupported
    /// whatever any metadata claims; an adapter that has one proves nothing
    /// about a particular model, so the evidence stands.
    #[must_use]
    pub fn within(self, bound: &Self) -> Self {
        match bound {
            Self::Unsupported(reason) => Self::Unsupported(reason.clone()),
            Self::Supported | Self::Unknown => self,
        }
    }

    /// A short word for tables and messages.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported(_) => "unsupported",
            Self::Unknown => "unverified",
        }
    }
}

/// A capability of a [`ModelOperation::Chat`] request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    /// The provider's native tool-calling primitive.
    NativeTools,
    /// Image input.
    VisionInput,
    /// Incremental (streamed) responses.
    Streaming,
    /// A response constrained to a JSON schema.
    StructuredOutput,
}

impl Feature {
    /// Every feature, in a stable order.
    pub const ALL: [Self; 4] = [
        Self::NativeTools,
        Self::VisionInput,
        Self::Streaming,
        Self::StructuredOutput,
    ];

    /// The configuration and display spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeTools => "native_tools",
            Self::VisionInput => "vision_input",
            Self::Streaming => "streaming",
            Self::StructuredOutput => "structured_output",
        }
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The chat features of one model, each tri-state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeatureSet {
    pub native_tools: Support,
    pub vision_input: Support,
    pub streaming: Support,
    pub structured_output: Support,
}

impl Default for FeatureSet {
    fn default() -> Self {
        Self::unknown()
    }
}

impl FeatureSet {
    /// No evidence about any feature.
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            native_tools: Support::Unknown,
            vision_input: Support::Unknown,
            streaming: Support::Unknown,
            structured_output: Support::Unknown,
        }
    }

    /// The evidence about `feature`.
    #[must_use]
    pub const fn get(&self, feature: Feature) -> &Support {
        match feature {
            Feature::NativeTools => &self.native_tools,
            Feature::VisionInput => &self.vision_input,
            Feature::Streaming => &self.streaming,
            Feature::StructuredOutput => &self.structured_output,
        }
    }

    pub(crate) const fn slot(&mut self, feature: Feature) -> &mut Support {
        match feature {
            Feature::NativeTools => &mut self.native_tools,
            Feature::VisionInput => &mut self.vision_input,
            Feature::Streaming => &mut self.streaming,
            Feature::StructuredOutput => &mut self.structured_output,
        }
    }

    /// Combine `other`'s evidence into this set, feature by feature, with
    /// [`Support::and`].
    #[must_use]
    pub fn and(mut self, other: Self) -> Self {
        for feature in Feature::ALL {
            let slot = self.slot(feature);
            let current = std::mem::replace(slot, Support::Unknown);
            *slot = current.and(other.get(feature).clone());
        }
        self
    }

    /// Cap every feature at `bound` with [`Support::within`].
    #[must_use]
    pub fn within(mut self, bound: &Self) -> Self {
        for feature in Feature::ALL {
            let slot = self.slot(feature);
            let current = std::mem::replace(slot, Support::Unknown);
            *slot = current.within(bound.get(feature));
        }
        self
    }
}

/// The tool dialects one model speaks.
///
/// Resolved per `(provider, model)`: a declaration for another model of the
/// same endpoint does not apply, which is what the endpoint-wide `widest` fold
/// could not express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Dialects {
    /// What the model is shown of a tool's published schema. Default `Raw`;
    /// never inferred from a URL.
    pub tool_schema: ToolSchemaDialect,
    /// Which tool-call grammars are read from its responses. Default `Native`.
    pub tool_call: ToolCallDialect,
}

/// Where a descriptor's evidence came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DescriptorSource {
    /// Only the adapter table: what the provider family can execute.
    AdapterStatic,
    /// An operator declaration in configuration.
    Curated,
    /// A provider catalog fetched at `fetched_at_unix`, with metadata.
    Discovery { fetched_at_unix: u64 },
    /// The release-owned catalog embedded in the binary.
    Catalog { revision: Arc<str> },
    /// A model known by id only: no metadata at all.
    LegacyIdOnly,
}

impl DescriptorSource {
    /// A short phrase for messages.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::AdapterStatic => "provider adapter only",
            Self::Curated => "operator declaration",
            Self::Discovery { .. } => "discovered catalog metadata",
            Self::Catalog { .. } => "embedded release catalog",
            Self::LegacyIdOnly => "model id only (unverified)",
        }
    }
}

impl fmt::Display for DescriptorSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What one model of one provider can do.
///
/// `Debug` shows the provider redacted (a `custom:` URL may carry
/// `user:password@`); there is no `Display`, because a descriptor is not a
/// message.
#[derive(Clone, PartialEq, Eq)]
pub struct ModelDescriptor {
    pub provider: ProviderId,
    pub model_id: Arc<str>,
    /// Missing operations are `Unknown`.
    pub operations: BTreeMap<ModelOperation, Support>,
    /// Chat features.
    pub features: FeatureSet,
    pub dialects: Dialects,
    pub context_window: Option<u32>,
    pub source: DescriptorSource,
}

impl ModelDescriptor {
    /// A model known by id only: every operation and feature `Unknown`.
    #[must_use]
    pub fn id_only(provider: ProviderId, model_id: &str) -> Self {
        Self {
            provider,
            model_id: Arc::from(model_id),
            operations: BTreeMap::new(),
            features: FeatureSet::unknown(),
            dialects: Dialects::default(),
            context_window: None,
            source: DescriptorSource::LegacyIdOnly,
        }
    }

    /// The evidence about `operation`.
    #[must_use]
    pub fn operation(&self, operation: ModelOperation) -> Support {
        self.operations.get(&operation).cloned().unwrap_or(Support::Unknown)
    }

    /// Whether the only operation this model is known to execute is the
    /// decision endpoint.
    #[must_use]
    pub fn is_decision_only(&self) -> bool {
        let supported: Vec<ModelOperation> = self
            .operations
            .iter()
            .filter(|(_, support)| support.is_supported())
            .map(|(operation, _)| *operation)
            .collect();
        supported == [ModelOperation::DecisionEndpoint]
    }
}

impl fmt::Debug for ModelDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelDescriptor")
            .field("provider", &self.provider)
            .field("model_id", &self.model_id)
            .field("operations", &self.operations)
            .field("features", &self.features)
            .field("dialects", &self.dialects)
            .field("context_window", &self.context_window)
            .field("source", &self.source)
            .finish()
    }
}

/// The chat features a request needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequiredFeatures {
    native_tools: bool,
    vision_input: bool,
    streaming: bool,
    structured_output: bool,
}

impl RequiredFeatures {
    /// No feature required.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            native_tools: false,
            vision_input: false,
            streaming: false,
            structured_output: false,
        }
    }

    /// Also require `feature`.
    #[must_use]
    pub const fn with(mut self, feature: Feature) -> Self {
        match feature {
            Feature::NativeTools => self.native_tools = true,
            Feature::VisionInput => self.vision_input = true,
            Feature::Streaming => self.streaming = true,
            Feature::StructuredOutput => self.structured_output = true,
        }
        self
    }

    /// Whether `feature` is required.
    #[must_use]
    pub const fn requires(self, feature: Feature) -> bool {
        match feature {
            Feature::NativeTools => self.native_tools,
            Feature::VisionInput => self.vision_input,
            Feature::Streaming => self.streaming,
            Feature::StructuredOutput => self.structured_output,
        }
    }

    /// The required features, in [`Feature::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Feature> {
        Feature::ALL.into_iter().filter(move |feature| self.requires(*feature))
    }
}

/// What a caller is about to ask of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirement {
    pub operation: ModelOperation,
    /// Only meaningful for [`ModelOperation::Chat`].
    pub features: RequiredFeatures,
}

impl Requirement {
    /// A chat request with no particular feature.
    #[must_use]
    pub const fn chat() -> Self {
        Self::operation(ModelOperation::Chat)
    }

    /// A request for `operation` with no particular feature.
    #[must_use]
    pub const fn operation(operation: ModelOperation) -> Self {
        Self {
            operation,
            features: RequiredFeatures::none(),
        }
    }

    /// Also require `feature`.
    #[must_use]
    pub const fn with(mut self, feature: Feature) -> Self {
        self.features = self.features.with(feature);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_evidence_wins_and_unknown_never_proves_support() {
        let no = Support::unsupported("model record lists no tools");
        assert_eq!(Support::Supported.and(no.clone()), no);
        assert_eq!(no.clone().and(Support::Supported), no);
        assert_eq!(Support::Unknown.and(Support::Supported), Support::Supported);
        assert_eq!(Support::Unknown.and(Support::Unknown), Support::Unknown);
        assert!(!Support::Unknown.is_supported());
        let first = Support::unsupported("first");
        assert_eq!(first.clone().and(Support::unsupported("second")), first);
    }

    #[test]
    fn an_adapter_bound_caps_evidence_but_never_lifts_it() {
        let no_adapter = Support::unsupported("no adapter");
        assert_eq!(Support::Supported.within(&no_adapter), no_adapter);
        assert_eq!(Support::Unknown.within(&Support::Supported), Support::Unknown);
        assert_eq!(Support::Supported.within(&Support::Unknown), Support::Supported);
        let declared_no = Support::unsupported("declared");
        assert_eq!(declared_no.clone().within(&Support::Supported), declared_no);
    }

    #[test]
    fn a_feature_set_combines_feature_by_feature() {
        let provider_wide = FeatureSet {
            native_tools: Support::Supported,
            vision_input: Support::Supported,
            ..FeatureSet::unknown()
        };
        let model_record = FeatureSet {
            native_tools: Support::unsupported("no tools"),
            streaming: Support::Supported,
            ..FeatureSet::unknown()
        };
        let combined = provider_wide.and(model_record);
        assert!(combined.native_tools.is_unsupported());
        assert!(combined.vision_input.is_supported());
        assert!(combined.streaming.is_supported());
        assert_eq!(combined.structured_output, Support::Unknown);
    }

    #[test]
    fn decision_only_means_only_the_decision_endpoint_is_supported() {
        let provider = ProviderId::of("openrouter");
        let mut descriptor = ModelDescriptor::id_only(provider, "jev-decision-test");
        assert!(!descriptor.is_decision_only());
        descriptor
            .operations
            .insert(ModelOperation::DecisionEndpoint, Support::Supported);
        descriptor
            .operations
            .insert(ModelOperation::Chat, Support::unsupported("decision only"));
        assert!(descriptor.is_decision_only());
        assert!(descriptor.operation(ModelOperation::Chat).is_unsupported());
        assert_eq!(descriptor.operation(ModelOperation::Embedding), Support::Unknown);
        descriptor.operations.insert(ModelOperation::Chat, Support::Supported);
        assert!(!descriptor.is_decision_only());
    }

    #[test]
    fn requirements_list_exactly_the_features_asked_for() {
        let requirement = Requirement::chat()
            .with(Feature::VisionInput)
            .with(Feature::NativeTools);
        assert_eq!(requirement.operation, ModelOperation::Chat);
        assert_eq!(
            requirement.features.iter().collect::<Vec<_>>(),
            vec![Feature::NativeTools, Feature::VisionInput]
        );
        assert_eq!(
            Requirement::operation(ModelOperation::Embedding)
                .features
                .iter()
                .count(),
            0
        );
    }

    #[test]
    fn spellings_are_stable_snake_case() {
        let operations: Vec<&str> = ModelOperation::ALL.iter().map(|operation| operation.as_str()).collect();
        assert_eq!(
            operations,
            [
                "chat",
                "embedding",
                "decision_endpoint",
                "rerank",
                "image_generation",
                "audio_generation",
                "video_generation"
            ]
        );
        let features: Vec<&str> = Feature::ALL.iter().map(|feature| feature.as_str()).collect();
        assert_eq!(
            features,
            ["native_tools", "vision_input", "streaming", "structured_output"]
        );
    }

    #[test]
    fn a_descriptor_debug_never_shows_url_userinfo() {
        let provider = ProviderId::of("custom:http://alice:pmmb-fixture-pw@127.0.0.1:9/v1");
        let descriptor = ModelDescriptor::id_only(provider, "m");
        let debug = format!("{descriptor:?}");
        assert!(!debug.contains("pmmb-fixture-pw"), "{debug}");
        assert!(debug.contains("127.0.0.1"), "{debug}");
    }
}
