//! The fail-fast model resolver.
//!
//! [`resolve`] decides, before any provider is constructed, whether a
//! selection may run the requested operation, which dialects it speaks, and
//! which credential it may use. It is pure and local: it reads the in-memory
//! [`CatalogView`] and a [`CredentialSource`], never the network, so a refusal
//! is always zero-transport.
//!
//! The order is fixed — classify, describe, operation, features, dialects,
//! credential — and the credential comes last, so a model or feature refusal
//! never reads a secret.

use std::fmt;
use std::sync::Arc;

use super::CatalogView;
use super::descriptor::{DescriptorSource, Dialects, Feature, ModelOperation, Requirement, Support};
use crate::providers::credentials::{CredentialLookupError, CredentialMaterial, CredentialRef, CredentialSource};
use crate::providers::{ProviderCapabilityError, ProviderId, pasteable_provider_label};

/// What a caller selected: a provider spelling, a model, and the credential
/// the selection may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub credential: CredentialRef,
}

/// What to do with a model the evidence neither proves nor refutes (D-018 Q1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownPolicy {
    /// Running an existing selection (a persisted default, a route, a
    /// command-line override): an unverified model runs, and the resolved
    /// model carries the warning to show once.
    Invocation,
    /// A durable selection the OPERATOR makes (onboarding, the wizard, the
    /// configuration API and Console, CLI flags; D-034): an unverified model
    /// is accepted and marked unverified, because a fresh install has no
    /// catalog yet. Only negative evidence refuses.
    OperatorSelection,
    /// A durable selection made by a MODEL (`config.patch`, any agent tool):
    /// an unverified model is refused until it is declared or its catalog
    /// metadata confirms it.
    DurableSelection,
}

/// Whether every requirement was proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// The evidence supports every requirement.
    Verified,
    /// Some requirements had no evidence either way and ran under
    /// [`UnknownPolicy::Invocation`]; they are listed by name.
    Unverified { unproven: Vec<&'static str> },
}

/// Why a selection was refused. Every message is one sentence that names the
/// provider (URL userinfo redacted), the model, what was asked, the evidence
/// and the fix. None carries a credential or names the configured default.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolutionError {
    #[error("provider '{provider}' is not one PRX can construct; use a name from `prx providers` or `custom:<url>`")]
    UnknownProvider { provider: String },
    #[error("{detail}")]
    MalformedProvider { provider: String, detail: String },
    #[error("no model is selected for provider '{provider}'; set `default_model` or pass `--model`")]
    EmptyModel { provider: String },
    #[error(
        "model '{model}' is an unresolved routing hint, not a model id; the router must resolve it before \
         provider '{provider}' is used"
    )]
    UnresolvedHint { provider: String, model: String },
    #[error("model 'hint:{hint}' names no route: the name {problem}; write it as 'hint:<name>'")]
    MalformedRouteHint {
        provider: String,
        hint: String,
        /// What is wrong with the name: "is empty" or "contains whitespace".
        problem: &'static str,
    },
    #[error("model 'hint:{hint}' names no configured `[[model_routes]]` entry; configured hints: {configured}")]
    UnknownRouteHint {
        provider: String,
        hint: String,
        configured: String,
    },
    #[error("{detail}")]
    ModelIncompatible {
        provider: String,
        model: String,
        detail: String,
    },
    #[error(
        "model '{model}' of provider '{provider}' cannot be used for {operation}: {reason} (evidence: {evidence}); \
         select a model that supports {operation}"
    )]
    OperationUnsupported {
        provider: String,
        model: String,
        operation: ModelOperation,
        reason: String,
        evidence: DescriptorSource,
    },
    #[error(
        "model '{model}' of provider '{provider}' is not verified for {operation} (evidence: {evidence}); declare \
         it in `[[providers.models]]` with provider = \"{pasteable}\", model = \"{model}\", operations = \
         [\"{operation}\"], or run `prx models refresh --provider {provider}`"
    )]
    OperationUnverified {
        provider: String,
        pasteable: String,
        model: String,
        operation: ModelOperation,
        evidence: DescriptorSource,
    },
    #[error(
        "model '{model}' of provider '{provider}' does not support {feature}: {reason} (evidence: {evidence}); \
         select a model with {feature}, or send the request without it"
    )]
    FeatureUnsupported {
        provider: String,
        model: String,
        feature: Feature,
        reason: String,
        evidence: DescriptorSource,
    },
    #[error(
        "model '{model}' of provider '{provider}' is not verified for {feature} (evidence: {evidence}); declare \
         `{feature} = true` for it in `[[providers.models]]` (provider = \"{pasteable}\", model = \"{model}\")"
    )]
    FeatureUnverified {
        provider: String,
        pasteable: String,
        model: String,
        feature: Feature,
        evidence: DescriptorSource,
    },
    #[error(
        "{reference} was issued for a different provider, so it is not released to provider '{provider}'; give \
         this provider a credential of its own"
    )]
    CredentialOwnerMismatch { provider: String, reference: String },
    #[error("{reference}, referenced for provider '{provider}', does not exist or is empty")]
    CredentialMissing { provider: String, reference: String },
    #[error(
        "provider '{provider}' needs a credential and the selection references none; reference an auth profile \
         or the provider's own credential"
    )]
    CredentialRequired { provider: String },
    #[error("the credential for provider '{provider}' could not be looked up: {reason}")]
    CredentialLookup { provider: String, reason: String },
    #[error(
        "model '{model}' of decision service '{provider}' is not pinned: a hosted decision backend needs an exact \
         model version, not an alias or an empty model; set `causal_tree.backend.model` to the version to pin"
    )]
    DecisionModelNotPinned { provider: String, model: String },
    #[error(
        "decision service '{provider}' is given {reference}: a hosted decision backend uses only an auth profile \
         (`causal_tree.backend.credential_ref = \"profile:…\"`), never an inline key or the provider's own store"
    )]
    DecisionCredentialNotProfile { provider: String, reference: String },
}

impl ResolutionError {
    /// The capability the refusal is about, in `ProviderCapabilityError` terms.
    #[must_use]
    pub fn capability(&self) -> String {
        match self {
            Self::UnknownProvider { .. } | Self::MalformedProvider { .. } => "provider".to_string(),
            Self::EmptyModel { .. }
            | Self::UnresolvedHint { .. }
            | Self::UnknownRouteHint { .. }
            | Self::MalformedRouteHint { .. }
            | Self::ModelIncompatible { .. }
            | Self::DecisionModelNotPinned { .. } => "model".to_string(),
            Self::OperationUnsupported { operation, .. } | Self::OperationUnverified { operation, .. } => {
                format!("operation:{operation}")
            }
            Self::FeatureUnsupported { feature, .. } | Self::FeatureUnverified { feature, .. } => {
                format!("feature:{feature}")
            }
            Self::CredentialOwnerMismatch { .. }
            | Self::DecisionCredentialNotProfile { .. }
            | Self::CredentialMissing { .. }
            | Self::CredentialRequired { .. }
            | Self::CredentialLookup { .. } => "credential".to_string(),
        }
    }

    fn provider(&self) -> &str {
        match self {
            Self::UnknownProvider { provider }
            | Self::MalformedProvider { provider, .. }
            | Self::EmptyModel { provider }
            | Self::UnresolvedHint { provider, .. }
            | Self::UnknownRouteHint { provider, .. }
            | Self::MalformedRouteHint { provider, .. }
            | Self::ModelIncompatible { provider, .. }
            | Self::OperationUnsupported { provider, .. }
            | Self::OperationUnverified { provider, .. }
            | Self::FeatureUnsupported { provider, .. }
            | Self::FeatureUnverified { provider, .. }
            | Self::CredentialOwnerMismatch { provider, .. }
            | Self::CredentialMissing { provider, .. }
            | Self::CredentialRequired { provider }
            | Self::CredentialLookup { provider, .. }
            | Self::DecisionModelNotPinned { provider, .. }
            | Self::DecisionCredentialNotProfile { provider, .. } => provider,
        }
    }
}

impl From<ResolutionError> for ProviderCapabilityError {
    fn from(error: ResolutionError) -> Self {
        Self {
            provider: error.provider().to_string(),
            capability: error.capability(),
            message: error.to_string(),
        }
    }
}

/// The credential a resolved model may use: only material whose owner is the
/// resolved provider. Opaque — `Debug` redacts, and there is no `Display` and
/// no `Serialize`.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedCredential {
    reference: CredentialRef,
    material: Option<CredentialMaterial>,
}

impl ResolvedCredential {
    /// The reference this credential was resolved from.
    #[must_use]
    pub const fn reference(&self) -> &CredentialRef {
        &self.reference
    }

    /// The secret released to the provider, if any.
    #[must_use]
    pub fn api_key(&self) -> Option<&str> {
        self.material.as_ref().and_then(CredentialMaterial::api_key)
    }

    /// The base URL that travels with the secret, if any.
    #[must_use]
    pub fn api_url(&self) -> Option<&str> {
        self.material.as_ref().and_then(CredentialMaterial::api_url)
    }

    /// The rotation keys that travel with the secret.
    #[must_use]
    pub fn rotation_keys(&self) -> &[String] {
        self.material.as_ref().map_or(&[], CredentialMaterial::rotation_keys)
    }
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("reference", &self.reference)
            .field("material", &self.material)
            .finish()
    }
}

/// A model the resolver released for one operation.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    route_settings: Option<crate::config::model_settings::ModelSettings>,
    provider: ProviderId,
    label: Arc<str>,
    model: Arc<str>,
    operation: ModelOperation,
    requirement: Requirement,
    policy: UnknownPolicy,
    dialects: Dialects,
    source: DescriptorSource,
    verification: Verification,
    credential: ResolvedCredential,
}

impl ResolvedModel {
    pub(crate) fn set_route_settings(&mut self, settings: crate::config::model_settings::ModelSettings) {
        self.route_settings = (settings.level.is_some() || !settings.overrides.is_empty()).then_some(settings);
    }

    pub(crate) const fn route_settings(&self) -> Option<&crate::config::model_settings::ModelSettings> {
        self.route_settings.as_ref()
    }
    /// The provider identity.
    #[must_use]
    pub const fn provider(&self) -> &ProviderId {
        &self.provider
    }

    /// The provider spelling the factory constructs. It may carry URL
    /// userinfo: pass it to the factory, never print it.
    #[must_use]
    pub fn provider_label(&self) -> &str {
        &self.label
    }

    /// The model id, as selected (trimmed).
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The operation this model was released for.
    #[must_use]
    pub const fn operation(&self) -> ModelOperation {
        self.operation
    }

    /// The requirement this model was released for.
    #[must_use]
    pub const fn requirement(&self) -> &Requirement {
        &self.requirement
    }

    /// The policy this model was released under.
    #[must_use]
    pub const fn policy(&self) -> UnknownPolicy {
        self.policy
    }

    /// The dialects of this model, resolved for it alone.
    #[must_use]
    pub const fn dialects(&self) -> Dialects {
        self.dialects
    }

    /// Where the evidence came from.
    #[must_use]
    pub fn source(&self) -> DescriptorSource {
        self.source.clone()
    }

    /// Whether every requirement was proven.
    #[must_use]
    pub const fn verification(&self) -> &Verification {
        &self.verification
    }

    /// The owner-bound credential.
    #[must_use]
    pub const fn credential(&self) -> &ResolvedCredential {
        &self.credential
    }

    /// The one-line warning for an unverified model, or `None` when verified.
    #[must_use]
    pub fn warning(&self) -> Option<String> {
        let Verification::Unverified { unproven } = &self.verification else {
            return None;
        };
        let provider = crate::security::redaction::redact_provider_text(&self.label);
        Some(format!(
            "model '{}' of provider '{provider}' is unverified for {} (evidence: {}); it runs, but PRX cannot \
             confirm it supports the request — declare it in `[[providers.models]]` or run `prx models refresh`",
            self.model,
            unproven.join(", "),
            self.source
        ))
    }
}

impl fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("operation", &self.operation)
            .field("dialects", &self.dialects)
            .field("source", &self.source)
            .field("verification", &self.verification)
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

/// Decide whether `selection` may run `requirement`.
///
/// Pure and local: no provider is constructed and no network is used. On
/// refusal nothing has been sent anywhere; on success the caller constructs
/// the provider from the [`ResolvedModel`] alone
/// ([`crate::providers::create_provider_for`]).
///
/// # Errors
///
/// A [`ResolutionError`] naming the first failed step.
pub fn resolve(
    selection: &Selection<'_>,
    requirement: &Requirement,
    catalog: &CatalogView,
    credentials: &dyn CredentialSource,
    policy: UnknownPolicy,
) -> Result<ResolvedModel, ResolutionError> {
    // 1. Classify; 2. describe; 3. operation; 4. features.
    let label = selection.provider.trim();
    let shown = crate::security::redaction::redact_provider_text(label);
    let model = selection.model.trim();
    let (provider, descriptor, unproven) = resolve_model(label, model, requirement, catalog, policy)?;

    // 5. Dialects: this model's own, never folded across the endpoint.
    let dialects = descriptor.dialects;

    // 6. Credential, last.
    let credential = release_credential(&provider, label, &shown, &selection.credential, credentials)?;

    Ok(ResolvedModel {
        route_settings: None,
        provider,
        label: Arc::from(label),
        model: Arc::from(model),
        operation: requirement.operation,
        requirement: *requirement,
        policy,
        dialects,
        source: descriptor.source,
        verification: if unproven.is_empty() {
            Verification::Verified
        } else {
            Verification::Unverified { unproven }
        },
        credential,
    })
}

/// Steps 1 to 4 of [`resolve`] without the credential: classify the
/// provider, check the model spelling, describe the pair and check it against
/// `requirement` under `policy`. Returns the identity, the descriptor and the
/// requirements that ran unproven.
///
/// For a surface whose credential is decided elsewhere by the same owner
/// rule (a delegate agent's own key), so it can still refuse a model before
/// any provider exists.
///
/// # Errors
///
/// The first refused step.
pub fn resolve_model(
    provider_label: &str,
    model: &str,
    requirement: &Requirement,
    catalog: &CatalogView,
    policy: UnknownPolicy,
) -> Result<(ProviderId, super::descriptor::ModelDescriptor, Vec<&'static str>), ResolutionError> {
    let label = provider_label.trim();
    let shown = crate::security::redaction::redact_provider_text(label);
    let provider = ProviderId::of(label);
    if !provider.is_resolvable() {
        if let Some((dialect, raw_url)) = crate::providers::identity::split_byo_provider(label) {
            if let Err(error) =
                crate::providers::parse_custom_provider_url(raw_url, dialect.label(), dialect.format_hint())
            {
                return Err(ResolutionError::MalformedProvider {
                    provider: shown.clone(),
                    detail: format!(
                        "{error}; provider '{shown}' is not one PRX can construct; use an http:// or https:// URL"
                    ),
                });
            }
        }
        return Err(ResolutionError::UnknownProvider { provider: shown });
    }
    let model = model.trim();
    if model.is_empty() {
        return Err(ResolutionError::EmptyModel { provider: shown });
    }
    if model.starts_with("hint:") {
        return Err(ResolutionError::UnresolvedHint {
            provider: shown,
            model: model.to_string(),
        });
    }
    if let Err(error) = crate::providers::validate_provider_model(label, model) {
        return Err(ResolutionError::ModelIncompatible {
            detail: crate::security::redaction::redact_provider_text(&error.to_string()),
            provider: shown,
            model: model.to_string(),
        });
    }
    if policy != UnknownPolicy::Invocation
        && let Ok(release) = catalog.release_catalog()
        && let Some(row) = release.model(&provider, model)
        && row.lifecycle.status == super::ModelLifecycleStatus::Retired
    {
        return Err(ResolutionError::ModelIncompatible {
            provider: shown,
            model: model.to_owned(),
            detail: row.lifecycle.replacement.as_deref().map_or_else(
                || "retired model cannot be selected; choose an active model".to_owned(),
                |replacement| format!("retired model cannot be selected; use {replacement}"),
            ),
        });
    }
    let descriptor = catalog.describe(&provider, model);
    let unproven = check_descriptor(&descriptor, &shown, model, requirement, policy)?;
    Ok((provider, descriptor, unproven))
}

/// Check a descriptor against a requirement under a policy: the operation,
/// then (for chat) every required feature. `Unsupported` always refuses;
/// `Unknown` is listed as unproven under [`UnknownPolicy::Invocation`] and
/// refused under [`UnknownPolicy::DurableSelection`].
///
/// `shown` is the provider label with URL userinfo already redacted.
///
/// # Errors
///
/// The first refused step.
pub fn check_descriptor(
    descriptor: &super::descriptor::ModelDescriptor,
    shown: &str,
    model: &str,
    requirement: &Requirement,
    policy: UnknownPolicy,
) -> Result<Vec<&'static str>, ResolutionError> {
    let pasteable = || pasteable_provider_label(shown).0.into_owned();
    let mut unproven = Vec::new();
    match descriptor.operation(requirement.operation) {
        Support::Supported => {}
        Support::Unsupported(reason) => {
            return Err(ResolutionError::OperationUnsupported {
                provider: shown.to_string(),
                model: model.to_string(),
                operation: requirement.operation,
                reason: reason.into_owned(),
                evidence: descriptor.source.clone(),
            });
        }
        Support::Unknown => match policy {
            UnknownPolicy::Invocation | UnknownPolicy::OperatorSelection => {
                unproven.push(requirement.operation.as_str());
            }
            UnknownPolicy::DurableSelection => {
                return Err(ResolutionError::OperationUnverified {
                    pasteable: pasteable(),
                    provider: shown.to_string(),
                    model: model.to_string(),
                    operation: requirement.operation,
                    evidence: descriptor.source.clone(),
                });
            }
        },
    }
    if requirement.operation == ModelOperation::Chat {
        for feature in requirement.features.iter() {
            match descriptor.features.get(feature) {
                Support::Supported => {}
                Support::Unsupported(reason) => {
                    return Err(ResolutionError::FeatureUnsupported {
                        provider: shown.to_string(),
                        model: model.to_string(),
                        feature,
                        reason: reason.to_string(),
                        evidence: descriptor.source.clone(),
                    });
                }
                Support::Unknown => match policy {
                    UnknownPolicy::Invocation | UnknownPolicy::OperatorSelection => unproven.push(feature.as_str()),
                    UnknownPolicy::DurableSelection => {
                        return Err(ResolutionError::FeatureUnverified {
                            pasteable: pasteable(),
                            provider: shown.to_string(),
                            model: model.to_string(),
                            feature,
                            evidence: descriptor.source.clone(),
                        });
                    }
                },
            }
        }
    }
    Ok(unproven)
}

/// Release the referenced credential to `provider` if and only if `provider`
/// is the identity it was issued for. There is no fallback arm.
fn release_credential(
    provider: &ProviderId,
    label: &str,
    shown: &str,
    reference: &CredentialRef,
    credentials: &dyn CredentialSource,
) -> Result<ResolvedCredential, ResolutionError> {
    let lookup = |error: CredentialLookupError| ResolutionError::CredentialLookup {
        provider: shown.to_string(),
        reason: error.to_string(),
    };
    match reference {
        CredentialRef::ProviderStore => {
            return Ok(ResolvedCredential {
                reference: reference.clone(),
                material: None,
            });
        }
        CredentialRef::None => {
            if crate::providers::provider_requires_explicit_credential(label) {
                return Err(ResolutionError::CredentialRequired {
                    provider: shown.to_string(),
                });
            }
            return Ok(ResolvedCredential {
                reference: reference.clone(),
                material: None,
            });
        }
        CredentialRef::Profile { .. } | CredentialRef::LegacyInline(_) => {}
    }
    let missing = || ResolutionError::CredentialMissing {
        provider: shown.to_string(),
        reference: reference.to_string(),
    };
    let owner = credentials.owner_of(reference).map_err(lookup)?.ok_or_else(missing)?;
    if owner != *provider {
        return Err(ResolutionError::CredentialOwnerMismatch {
            provider: shown.to_string(),
            reference: reference.to_string(),
        });
    }
    let material = credentials
        .material(reference)
        .map_err(lookup)?
        .filter(|material| !material.is_empty())
        .ok_or_else(missing)?;
    Ok(ResolvedCredential {
        reference: reference.clone(),
        material: Some(material),
    })
}
