//! The decision backend factory (CDP 4.2 and 17; CDP-PLAN 1.5; SP-CDP-A
//! 5.6; D-040).
//!
//! One factory is built per configuration generation. It owns the ordered
//! backend slots, one circuit breaker per slot, the one decision cache and
//! the outbound guard, and it is the only caller of the response validator
//! and the only place a backend is built (AG-04).
//!
//! - **Order** ([`DecisionBackendFactory::backend_order`], pure, printed by
//!   `prx causal backends`): `enabled = false` is `[builtin]` and nothing
//!   leaves PRX; a loopback compatible endpoint needs no switch; a remote
//!   one, and every hosted backend, only with `remote_enabled`; the builtin
//!   is always last. A stored credential alone never enables a backend, and
//!   there is no implicit hosted fallback.
//! - **Selection** is strict (D-040 OQ-A2): the configured backend goes
//!   through `providers::selection::check_decision_backend` at every build,
//!   whether or not it takes part (D-079 L-5), so an undeclared model, an
//!   unpinned hosted model or a hosted backend without an auth-profile
//!   credential (D-074) fails the build, and the generation with it. While
//!   the plane is disabled only the rules the configuration alone decides
//!   run (`check_decision_model`): the credential store is never opened
//!   (D-082 N-9), and enabling the plane runs the whole check.
//! - **Decide**: the builtin shadow first (always); then, per slot, the
//!   cache (a hit carries the current request id, D-079 L-9), the breaker
//!   (a cancelled half-open probe is released, D-079 M-1), the remaining
//!   deadline, the outbound guard, whose checked bytes are exactly what the
//!   backend sends (D-079 M-3), the attempt bounded by the slot timeout and
//!   the request deadline (a timeout the turn's deadline forced is not the
//!   backend's failure, D-079 L-6), and validation. Any failure falls to the next slot; when every slot fails
//!   or time runs out, the builtin shadow is the answer. The factory has no
//!   API that changes a candidate, a state or a gate.
//! - An unavailable decision key (D-057) leaves only the builtin.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::backend::{DecisionBackend, locality_of};
use super::breaker::{BreakerState, BreakerTransition, CircuitBreaker};
use super::builtin::BuiltinDecisionBackend;
use super::cache::{CacheKey, DecisionCache};
use super::error::DecisionError;
use super::key::DecisionKey;
use super::outbound::{GuardedBody, OutboundSecrets, check_outbound};
use super::protocol::{
    BackendIdentity, BackendKind, BackendLocality, DecisionQuestion, DecisionRequest, GlobalQuestion, QuestionId,
    RequestLimits, RequestParts, StructuredValue,
};
use super::telemetry::{
    CausalEvent, CausalEventFields, CausalEventKind, CausalEventSink, CausalLineage, CircuitState, ServedLabel,
};
use super::validate::{DecisionResponseValidator, ModelVersionPolicy, ValidatedDecision, ValidationContext};
use super::wire::SystemOneCodec;
use crate::causal_tree::config::{BackendSelection, CausalTreeConfig, CredentialRefSetting};
use crate::config::Config;
use crate::providers::ProviderId;
use crate::providers::catalog::{CatalogView, ResolutionError, ResolvedModel};
use crate::providers::credentials::CredentialRef;

/// The shortest attempt worth starting.
const MIN_ATTEMPT: Duration = Duration::from_millis(20);

/// Why a configured backend is not in the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// The decision plane is disabled: nothing leaves PRX.
    Disabled,
    /// A remote backend while `remote_enabled` is false.
    RemoteDisabled,
    /// The backend's breaker is open.
    CircuitOpen,
    /// `kind = auto` with no endpoint.
    NotConfigured,
    /// The workspace decision key is unavailable (D-057).
    KeyUnavailable,
    /// The backend's last health probe failed.
    Unhealthy,
    /// The caller cannot bound the backend's bill for this decision chain.
    BudgetUnproven,
}

impl Exclusion {
    /// The stable reason name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::RemoteDisabled => "remote_disabled",
            Self::CircuitOpen => "circuit_open",
            Self::NotConfigured => "not_configured",
            Self::KeyUnavailable => "key_unavailable",
            Self::Unhealthy => "unhealthy",
            Self::BudgetUnproven => "budget_unproven",
        }
    }
}

/// One position of the backend order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedSlot {
    /// The backend kind.
    pub kind: BackendKind,
    /// Where it runs.
    pub locality: BackendLocality,
    /// Why it is left out, if it is.
    pub exclusion: Option<Exclusion>,
}

/// The live facts [`DecisionBackendFactory::backend_order`] reads besides
/// the configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HealthView {
    /// Whether the configured backend's breaker is open.
    pub circuit_open: bool,
    /// Whether the workspace decision key is unavailable.
    pub key_unavailable: bool,
    /// Whether the configured backend's last health probe failed.
    pub unhealthy: bool,
}

/// What a transport needs to build one configured backend.
#[derive(Debug)]
pub struct BackendSpec<'a> {
    /// The configured identity.
    pub identity: BackendIdentity,
    /// Where it runs.
    pub locality: BackendLocality,
    /// The configured endpoint, trimmed; empty for a hosted default.
    pub endpoint: &'a str,
    /// The exact model a pinned backend must answer with: the configured
    /// `pinned_snapshot`, or the configured model (D-043 C-8).
    pub expected_model: Arc<str>,
    /// The strict selection result, with the released credential.
    pub resolved: &'a ResolvedModel,
    /// The per-attempt transport timeout.
    pub timeout: Duration,
    /// The largest response body.
    pub max_response_bytes: u32,
    /// The workspace decision key (question-id salting).
    pub key: Arc<DecisionKey>,
}

/// Builds the backends of the configured kinds. The HTTP and hosted
/// adapters implement it (CDP-07, CDP-13); only the factory calls it.
pub trait DecisionTransports: Send + Sync {
    /// Build the backend `spec` describes. Building performs no I/O.
    ///
    /// # Errors
    ///
    /// When this build has no transport for the kind, or the spec is
    /// unusable.
    fn build(&self, spec: &BackendSpec<'_>) -> Result<Box<dyn DecisionBackend>, DecisionError>;
}

/// What a factory is built from, besides the configuration.
pub struct FactoryDeps<'a> {
    /// The strict selection's catalog.
    pub catalog: &'a CatalogView,
    /// The workspace decision key; `None` when it is unavailable, which
    /// leaves only the builtin (D-057).
    pub key: Option<Arc<DecisionKey>>,
    /// The transports of the configured kinds.
    pub transports: &'a dyn DecisionTransports,
    /// The configuration generation the factory serves (part of every cache
    /// key; a new generation builds a new factory, resetting breakers).
    pub config_generation: u64,
    /// Where the factory's causal events go (CDP 18; D-077).
    pub events: Arc<dyn CausalEventSink>,
}

impl FactoryDeps<'_> {
    /// Load the workspace decision key of `config`, or `None` when it is
    /// unavailable (the cause is logged by the key module, and the plane
    /// falls back to the builtin). While the plane is disabled the key is
    /// neither read nor created (D-079 L-14).
    #[must_use]
    pub fn load_key(config: &Config) -> Option<Arc<DecisionKey>> {
        if !config.causal_tree.enabled {
            return None;
        }
        DecisionKey::load_or_create(&config.workspace_dir)
            .inspect_err(|error| {
                tracing::warn!(error = %error, "decision key unavailable; only the builtin decision backend runs");
            })
            .ok()
    }
}

/// Why a factory could not be built. The generation that asked for it is
/// rejected; the running one stays.
#[derive(Debug, thiserror::Error)]
pub enum FactoryBuildError {
    /// The strict decision backend selection refused the configured backend.
    #[error("the configured decision backend is not selectable: {0}")]
    Selection(ResolutionError),
    /// No backend could be built for the configured kind.
    #[error("the configured decision backend could not be built: {0}")]
    Transport(DecisionError),
    /// The scripted test backend is never selected by configuration (D-057
    /// L-1).
    #[cfg(any(test, feature = "test-mock"))]
    #[error("the scripted test backend cannot be selected by configuration")]
    MockNotConfigurable,
}

/// Who served a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServedBy {
    /// The first backend in the order.
    Primary,
    /// A later backend in the order.
    Fallback,
    /// The decision cache.
    Cache,
    /// The builtin backend.
    Builtin,
}

/// Why the builtin served although another backend is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// Too little of the deadline was left for another attempt.
    DeadlineExhausted,
    /// Every configured backend failed.
    AllFailed,
    /// The decision plane is disabled.
    Disabled,
    /// A configured backend lacked a provable decision-chain budget bound.
    BudgetUnproven,
}

/// What happened to one slot during a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// It answered with a valid decision.
    Served,
    /// The cache answered for it.
    Cached,
    /// It failed with this class; no body is kept.
    Failed(DecisionError),
    /// It was not attempted.
    Skipped(DecisionError),
}

/// One slot's part in a decision: identity, outcome and latency, never a
/// body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    /// The configured identity of the slot.
    pub identity: BackendIdentity,
    /// What happened.
    pub outcome: AttemptOutcome,
    /// How long the attempt took.
    pub latency: Duration,
}

/// The result of one decision.
#[derive(Clone)]
pub struct PredictionResult {
    /// The decision that is used.
    pub served: ValidatedDecision,
    /// Who served it.
    pub served_by: ServedBy,
    /// Factory-owned locality of the backend that actually served.
    pub served_locality: BackendLocality,
    /// Its configured endpoint, including an empty hosted default. Builtin
    /// and test-only slots without a configured endpoint use `None`.
    pub served_endpoint: Option<Arc<str>>,
    /// The builtin's answer, always computed first; kept for regret.
    pub builtin_shadow: ValidatedDecision,
    /// Every slot that was considered, in order.
    pub attempts: Vec<AttemptRecord>,
    /// The order the factory tried.
    pub fallback_order: Vec<BackendIdentity>,
    /// Why the builtin served when another backend is configured.
    pub reason: Option<FallbackReason>,
}

impl std::fmt::Debug for PredictionResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PredictionResult")
            .field("served_by", &self.served_by)
            .field("served_locality", &self.served_locality)
            .field("has_served_endpoint", &self.served_endpoint.is_some())
            .field("attempt_count", &self.attempts.len())
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

/// The builtin could not answer: impossible by construction, typed anyway;
/// the caller maps it to a conservative directive (CDP 17).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the builtin decision backend failed: {0}")]
pub struct BuiltinFailure(pub DecisionError);

/// The limits of a test slot (`from_slots`).
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlotLimits {
    pub(crate) timeout: Duration,
    pub(crate) failure_threshold: u32,
    pub(crate) cooldown: Duration,
}

#[cfg(test)]
impl Default for SlotLimits {
    fn default() -> Self {
        let defaults = crate::causal_tree::config::DecisionBackendConfig::default();
        Self {
            timeout: Duration::from_millis(u64::from(defaults.timeout_ms)),
            failure_threshold: defaults.circuit_breaker.failure_threshold,
            cooldown: Duration::from_secs(u64::from(defaults.circuit_breaker.cooldown_secs)),
        }
    }
}

/// One configured backend in the order.
struct Slot {
    /// Factory-owned model selected by PMM before the adapter is built.
    request_model: Arc<str>,
    backend: Box<dyn DecisionBackend>,
    locality: BackendLocality,
    endpoint: Option<Arc<str>>,
    breaker: CircuitBreaker,
    healthy: AtomicBool,
    /// The class of the slot's last failure (a probe or an attempt), for
    /// status output; class names carry no data.
    last_failure: parking_lot::Mutex<Option<&'static str>>,
    timeout: Duration,
    max_request_bytes: u32,
    max_response_bytes: u32,
    key: Arc<DecisionKey>,
}

impl Slot {
    /// The shortest remaining deadline worth an attempt: the larger of 20 ms
    /// and a quarter of the slot timeout.
    fn min_attempt(&self) -> Duration {
        MIN_ATTEMPT.max(self.timeout / 4)
    }
}

/// Engine-facing borrowed adapter. The original sealed accounting owner and
/// physical attempt construction stay in the decision/runtime boundary.
#[derive(Clone, Copy)]
pub(crate) struct AccountedPrediction<'a> {
    factory: &'a DecisionBackendFactory,
    accounting: Option<&'a dyn super::budget::DecisionAttemptAccounting>,
}
impl crate::causal_tree::decision_engine::prediction_sealed::Producer for AccountedPrediction<'_> {}
impl crate::causal_tree::decision_engine::PredictionPort for AccountedPrediction<'_> {
    fn remaining_budget(&self) -> Option<(u64, u64)> {
        self.accounting.map(|owner| owner.remaining_budget())
    }
    async fn predict<'a>(
        &'a self,
        request: &'a crate::decision::DecisionRequest,
        lineage: &'a CausalLineage,
    ) -> Result<PredictionResult, crate::decision::DecisionError> {
        self.factory
            .decide_with_accounting(request, lineage, self.accounting)
            .await
            .map_err(|BuiltinFailure(error)| error)
    }
}

/// A configured backend's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotStatus {
    /// The configured identity.
    pub identity: BackendIdentity,
    /// Where it runs.
    pub locality: BackendLocality,
    /// Its breaker.
    pub breaker: BreakerState,
    /// Whether its last health probe passed.
    pub healthy: bool,
    /// The class of its last failure, if any.
    pub last_failure: Option<&'static str>,
}

/// The factory's status, for `prx causal status` and `backends`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendPlaneStatus {
    /// Whether the decision plane is enabled.
    pub enabled: bool,
    /// The configuration generation served.
    pub config_generation: u64,
    /// The configured backends in order; the builtin always follows.
    pub order: Vec<SlotStatus>,
    /// Configured backends left out, with the reason.
    pub excluded: Vec<(BackendKind, BackendLocality, Exclusion)>,
    /// How many decisions are cached.
    pub cached: usize,
}

/// The decision backend factory of one configuration generation.
pub struct DecisionBackendFactory {
    enabled: bool,
    budget_unproven: bool,
    events: Arc<dyn CausalEventSink>,
    slots: Vec<Slot>,
    excluded: Vec<(BackendKind, BackendLocality, Exclusion)>,
    cache: DecisionCache,
    secrets: OutboundSecrets,
    config_generation: u64,
}

impl std::fmt::Debug for DecisionBackendFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionBackendFactory")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// The configured non-builtin backend kind, if any: `auto` with an
/// endpoint is the compatible backend, `auto` without one and `builtin` are
/// none. The one reading of the `kind` setting, shared with the
/// configuration's risk rule (D-082 N-5).
#[must_use]
pub fn configured_kind(section: &CausalTreeConfig) -> Option<BackendKind> {
    match section.backend.kind {
        BackendSelection::Auto if section.backend.endpoint.trim().is_empty() => None,
        BackendSelection::Auto => Some(BackendKind::CompatibleHttp),
        BackendSelection::Fixed(BackendKind::Builtin) => None,
        BackendSelection::Fixed(kind) => Some(kind),
    }
}

/// The provider a configured backend belongs to.
fn provider_of(kind: BackendKind, endpoint: &str) -> String {
    match kind {
        BackendKind::CompatibleHttp => format!("custom:{endpoint}"),
        other => other.as_str().to_string(),
    }
}

/// The credential reference the strict selection checks (D-040 OQ-A4,
/// D-074): an explicit `"none"` is anonymous; an unset reference is
/// anonymous only on a literal loopback endpoint, and otherwise names the
/// provider's own store, which a hosted backend refuses (it takes an auth
/// profile only).
fn credential_ref_for(setting: &CredentialRefSetting, locality: BackendLocality) -> CredentialRef {
    match (setting, locality) {
        (CredentialRefSetting::None, _) | (CredentialRefSetting::Unset, BackendLocality::Loopback) => {
            CredentialRef::None
        }
        (CredentialRefSetting::Unset, BackendLocality::Builtin | BackendLocality::Remote) => {
            CredentialRef::ProviderStore
        }
        (CredentialRefSetting::Profile(id), _) => CredentialRef::Profile { id: id.clone() },
    }
}

/// The backend a `[causal_tree]` section configures, as the strict
/// selection sees it; `None` for the builtin, or `auto` without an
/// endpoint. The factory builds from it and the configuration check
/// (`config::manage::validate`) checks it, so both read one rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredSelection {
    pub kind: BackendKind,
    pub locality: BackendLocality,
    /// The provider id the backend belongs to (the owner of its credential).
    pub provider: String,
    /// The credential the strict selection checks.
    pub credential: CredentialRef,
}

/// The configured selection of `section`.
#[must_use]
pub fn configured_selection(section: &CausalTreeConfig) -> Option<ConfiguredSelection> {
    let kind = configured_kind(section)?;
    let endpoint = section.backend.endpoint.trim();
    let locality = locality_of(kind, endpoint);
    Some(ConfiguredSelection {
        kind,
        locality,
        provider: provider_of(kind, endpoint),
        credential: credential_ref_for(&section.backend.credential_ref, locality),
    })
}

fn builtin_shadow(request: &DecisionRequest) -> Result<ValidatedDecision, BuiltinFailure> {
    let response = BuiltinDecisionBackend::answer(request).map_err(BuiltinFailure)?;
    DecisionResponseValidator::validate(
        request,
        response,
        &ValidationContext {
            received_at: Instant::now(),
            max_response_bytes: u32::MAX,
            version_policy: ModelVersionPolicy::ExactPinned(BuiltinDecisionBackend::identity().configured_model),
            local_backend: BuiltinDecisionBackend::identity(),
        },
    )
    .map_err(BuiltinFailure)
}

/// `identity` as shown in a status: a provider label that carries URL user
/// information is redacted (AG-12; the one helper
/// `providers::redact_endpoint_userinfo`).
fn shown_identity(identity: &BackendIdentity) -> BackendIdentity {
    let provider = identity.provider.as_ref().map(|provider| {
        match crate::providers::redact_endpoint_userinfo(provider.as_str()) {
            std::borrow::Cow::Borrowed(_) => provider.clone(),
            std::borrow::Cow::Owned(redacted) => ProviderId::of(&redacted),
        }
    });
    BackendIdentity {
        provider,
        ..identity.clone()
    }
}

/// The breaker transition as an event state.
const fn circuit_state(transition: BreakerTransition) -> CircuitState {
    match transition {
        BreakerTransition::Opened { .. } => CircuitState::Open,
        BreakerTransition::OpenedForGeneration => CircuitState::OpenForGeneration,
        BreakerTransition::Closed => CircuitState::Closed,
    }
}

/// A breaker opening is logged at WARN whatever the observer backend is
/// (D-086 F-3): backend and duration only, no lineage.
fn warn_opened(kind: BackendKind, transition: BreakerTransition) {
    match transition {
        BreakerTransition::Opened { until } => tracing::warn!(
            backend = kind.as_str(),
            open_for_ms =
                u64::try_from(until.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(u64::MAX),
            "decision backend circuit opened"
        ),
        BreakerTransition::OpenedForGeneration => tracing::warn!(
            backend = kind.as_str(),
            "decision backend circuit opened until the next configuration generation"
        ),
        BreakerTransition::Closed => {}
    }
}

/// A request refused before it was sent is logged at WARN whatever the
/// observer backend is (D-086 F-3): backend, class and rule only; never the
/// body or a value from it.
fn warn_refused(kind: BackendKind, error: &DecisionError) {
    tracing::warn!(
        backend = kind.as_str(),
        class = error.class(),
        rule = error.rule().unwrap_or(""),
        "decision request refused before it was sent"
    );
}

/// Emits one decision's events with its lineage.
struct Emitter<'a> {
    sink: &'a dyn CausalEventSink,
    lineage: CausalLineage,
}

impl Emitter<'_> {
    fn emit(&self, kind: CausalEventKind, fields: CausalEventFields) {
        self.sink.emit(CausalEvent {
            kind,
            lineage: self.lineage.clone(),
            fields,
        });
    }

    /// The fields that name `slot`'s backend.
    fn slot_fields(slot: &Slot) -> CausalEventFields {
        let identity = slot.backend.identity();
        CausalEventFields {
            backend: Some(identity.kind),
            locality: Some(slot.locality),
            configured_model: Some(Arc::clone(&identity.configured_model)),
            ..CausalEventFields::default()
        }
    }

    fn failed(&self, slot: &Slot, error: &DecisionError, latency: Option<Duration>) {
        self.emit(
            CausalEventKind::PredictionFailed,
            CausalEventFields {
                error_class: Some(error.class()),
                error_rule: error.rule(),
                latency_ms: latency.map(|latency| u64::try_from(latency.as_millis()).unwrap_or(u64::MAX)),
                ..Self::slot_fields(slot)
            },
        );
    }

    fn circuit(&self, slot: &Slot, transition: Option<BreakerTransition>) {
        if let Some(transition) = transition {
            warn_opened(slot.backend.identity().kind, transition);
            self.emit(
                CausalEventKind::BackendCircuitOpened,
                CausalEventFields {
                    circuit: Some(circuit_state(transition)),
                    ..Self::slot_fields(slot)
                },
            );
        }
    }

    fn fallback(&self, slot: &Slot, to: BackendKind) {
        self.emit(
            CausalEventKind::BackendFallback,
            CausalEventFields {
                to_backend: Some(to),
                ..Self::slot_fields(slot)
            },
        );
    }

    fn served_by_builtin(&self) {
        self.emit(
            CausalEventKind::PredictionSucceeded,
            CausalEventFields {
                backend: Some(BackendKind::Builtin),
                locality: Some(BackendLocality::Builtin),
                configured_model: Some(BuiltinDecisionBackend::identity().configured_model),
                served: Some(ServedLabel::Builtin),
                ..CausalEventFields::default()
            },
        );
    }
}

impl DecisionBackendFactory {
    pub(crate) fn prediction_port<'a>(
        &'a self,
        accounting: Option<&'a dyn super::budget::DecisionAttemptAccounting>,
    ) -> AccountedPrediction<'a> {
        AccountedPrediction {
            factory: self,
            accounting,
        }
    }

    /// The backend order for `section` (SP-CDP-A 5.6 under D-040). Pure:
    /// the same configuration and health always give the same order. The
    /// builtin is always the last included slot.
    #[must_use]
    pub fn backend_order(section: &CausalTreeConfig, health: &HealthView) -> Vec<PlannedSlot> {
        let builtin = PlannedSlot {
            kind: BackendKind::Builtin,
            locality: BackendLocality::Builtin,
            exclusion: None,
        };
        let backend = &section.backend;
        let Some(kind) = configured_kind(section) else {
            let mut order = Vec::new();
            if backend.kind == BackendSelection::Auto {
                order.push(PlannedSlot {
                    kind: BackendKind::CompatibleHttp,
                    locality: BackendLocality::Loopback,
                    exclusion: Some(Exclusion::NotConfigured),
                });
            }
            order.push(builtin);
            return order;
        };
        let locality = locality_of(kind, backend.endpoint.trim());
        let exclusion = if !section.enabled {
            Some(Exclusion::Disabled)
        } else if locality == BackendLocality::Remote && !backend.remote_enabled {
            Some(Exclusion::RemoteDisabled)
        } else if health.key_unavailable {
            Some(Exclusion::KeyUnavailable)
        } else if health.circuit_open {
            Some(Exclusion::CircuitOpen)
        } else if health.unhealthy {
            Some(Exclusion::Unhealthy)
        } else {
            None
        };
        vec![
            PlannedSlot {
                kind,
                locality,
                exclusion,
            },
            builtin,
        ]
    }

    /// Build the factory for `config`'s `[causal_tree]` section.
    ///
    /// Nothing is built for an excluded backend: a disabled plane, a remote
    /// backend without `remote_enabled` and an unavailable key never reach
    /// a transport.
    ///
    /// # Errors
    ///
    /// [`FactoryBuildError::Selection`] when the strict selection refuses
    /// the configured backend, [`FactoryBuildError::Transport`] when it
    /// cannot be built.
    pub fn from_config(config: &Config, deps: FactoryDeps<'_>) -> Result<Self, FactoryBuildError> {
        let section = &config.causal_tree;
        #[cfg(any(test, feature = "test-mock"))]
        if section.backend.kind == BackendSelection::Fixed(BackendKind::Mock) {
            return Err(FactoryBuildError::MockNotConfigurable);
        }
        let health = HealthView {
            circuit_open: false,
            key_unavailable: deps.key.is_none(),
            unhealthy: false,
        };
        let mut secrets = OutboundSecrets::of_config(config);
        let backend = &section.backend;
        let endpoint = backend.endpoint.trim();
        // Strict selection of the configured backend at every build, whether
        // or not it takes part (D-040 OQ-A2, D-079 L-5): a configuration that
        // would fail once enabled fails now. It reads local state only.
        let selected = match configured_selection(section) {
            None => None,
            // A disabled plane builds no backend; it never opens the
            // credential store (D-082 N-9).
            Some(selection) if !section.enabled => {
                crate::providers::selection::check_decision_model(
                    deps.catalog,
                    &selection.provider,
                    backend.model.trim(),
                    &selection.credential,
                )
                .map_err(FactoryBuildError::Selection)?;
                None
            }
            Some(selection) => {
                let resolved = crate::providers::selection::check_decision_backend(
                    config,
                    deps.catalog,
                    &selection.provider,
                    backend.model.trim(),
                    selection.credential,
                )
                .map_err(FactoryBuildError::Selection)?;
                Some((selection.provider, resolved))
            }
        };
        let mut slots = Vec::new();
        let mut excluded = Vec::new();
        for planned in Self::backend_order(section, &health) {
            if planned.kind == BackendKind::Builtin {
                continue;
            }
            if let Some(exclusion) = planned.exclusion {
                excluded.push((planned.kind, planned.locality, exclusion));
                continue;
            }
            let (Some(key), Some((provider, resolved))) = (deps.key.clone(), selected.as_ref()) else {
                excluded.push((planned.kind, planned.locality, Exclusion::KeyUnavailable));
                continue;
            };
            if let Some(material) = resolved.credential().api_key() {
                secrets.add("causal_tree.backend.credential_ref", material);
            }
            for rotation in resolved.credential().rotation_keys() {
                secrets.add("causal_tree.backend.credential_ref", rotation);
            }
            let request_model: Arc<str> = Arc::from(resolved.model());
            let identity = BackendIdentity {
                kind: planned.kind,
                provider: Some(ProviderId::of(provider)),
                configured_model: Arc::clone(&request_model),
            };
            let built = deps
                .transports
                .build(&BackendSpec {
                    identity,
                    locality: planned.locality,
                    endpoint,
                    expected_model: Arc::from(match backend.pinned_snapshot.trim() {
                        "" => backend.model.trim(),
                        snapshot => snapshot,
                    }),
                    resolved,
                    timeout: Duration::from_millis(u64::from(backend.timeout_ms)),
                    max_response_bytes: backend.max_response_bytes,
                    key: Arc::clone(&key),
                })
                .map_err(FactoryBuildError::Transport)?;
            if built.identity().configured_model != request_model {
                return Err(FactoryBuildError::Transport(DecisionError::InvalidRequest(
                    super::RequestRule::ModelInvalid,
                )));
            }
            slots.push(Slot {
                request_model,
                backend: built,
                locality: planned.locality,
                endpoint: Some(Arc::from(endpoint)),
                healthy: AtomicBool::new(true),
                last_failure: parking_lot::Mutex::new(None),
                breaker: CircuitBreaker::new(
                    backend.circuit_breaker.failure_threshold,
                    Duration::from_secs(u64::from(backend.circuit_breaker.cooldown_secs)),
                ),
                timeout: Duration::from_millis(u64::from(backend.timeout_ms)),
                max_request_bytes: backend.max_request_bytes,
                max_response_bytes: backend.max_response_bytes,
                key,
            });
        }
        Ok(Self {
            enabled: section.enabled,
            budget_unproven: false,
            events: deps.events,
            slots,
            excluded,
            cache: DecisionCache::new(),
            secrets,
            config_generation: deps.config_generation,
        })
    }

    /// A factory of `config` with only the builtin: every decision is the
    /// builtin's and nothing is sent (`prx causal test --backend builtin`).
    #[must_use]
    pub fn builtin_only(config: &Config, events: Arc<dyn CausalEventSink>) -> Self {
        Self {
            enabled: true,
            budget_unproven: false,
            events,
            slots: Vec::new(),
            excluded: Vec::new(),
            cache: DecisionCache::new(),
            secrets: OutboundSecrets::of_config(config),
            config_generation: 0,
        }
    }

    /// Exclude configured slots for a caller that cannot prove a priced,
    /// transport-enforced upper bound before the attempt. Configuration was
    /// still validated by `from_config`; this changes only call admission.
    pub(crate) fn with_unproven_budget_gate(mut self) -> Self {
        self.budget_unproven = !self.slots.is_empty();
        for slot in self.slots.drain(..) {
            self.excluded
                .push((slot.backend.identity().kind, slot.locality, Exclusion::BudgetUnproven));
        }
        self
    }

    /// The one configured model of retained eligible slots, before sealing.
    /// Builtin/default, disabled, keyless and excluded slots supply none.
    /// A divergent test-only slot order cannot define one compiler model.
    pub(crate) fn request_model(&self) -> Option<&str> {
        if !self.enabled {
            return None;
        }
        let model = self.slots.first()?.request_model.as_ref();
        self.slots
            .iter()
            .all(|slot| slot.request_model.as_ref() == model)
            .then_some(model)
    }

    /// A factory over `backends` in order (SP-CDP-C V2.07 needs two
    /// non-builtin backends, which configuration never yields), each with
    /// `limits`.
    #[cfg(test)]
    pub(crate) fn from_slots(
        backends: Vec<Box<dyn DecisionBackend>>,
        key: Arc<DecisionKey>,
        secrets: OutboundSecrets,
        limits: SlotLimits,
    ) -> Self {
        let defaults = crate::causal_tree::config::DecisionBackendConfig::default();
        Self {
            enabled: true,
            budget_unproven: false,
            events: Arc::new(super::telemetry::testing::RecordingSink::default()),
            slots: backends
                .into_iter()
                .map(|backend| Slot {
                    request_model: Arc::clone(&backend.identity().configured_model),
                    locality: backend.locality(),
                    endpoint: None,
                    backend,
                    healthy: AtomicBool::new(true),
                    last_failure: parking_lot::Mutex::new(None),
                    breaker: CircuitBreaker::new(limits.failure_threshold, limits.cooldown),
                    timeout: limits.timeout,
                    max_request_bytes: defaults.max_request_bytes,
                    max_response_bytes: defaults.max_response_bytes,
                    key: Arc::clone(&key),
                })
                .collect(),
            excluded: Vec::new(),
            cache: DecisionCache::new(),
            secrets,
            config_generation: 0,
        }
    }

    /// The same factory emitting to `events` (tests read what it emits).
    #[cfg(test)]
    pub(crate) fn with_events(mut self, events: Arc<dyn CausalEventSink>) -> Self {
        self.events = events;
        self
    }

    /// Decide `request` before its manifest deadline, emitting its causal
    /// events with `lineage` (its decision id defaults to the request id). Cancel-safe: dropping
    /// the future drops the attempt in flight, and nothing is recorded as
    /// served. The factory writes nothing; recording is the caller's.
    ///
    /// # Errors
    ///
    /// [`BuiltinFailure`] only when the builtin shadow itself fails, which
    /// is impossible by construction.
    pub async fn decide(
        &self,
        request: &DecisionRequest,
        lineage: &CausalLineage,
    ) -> Result<PredictionResult, BuiltinFailure> {
        self.decide_with_accounting(request, lineage, None).await
    }

    /// The same decision path with a borrowed, source-verified deferred owner.
    /// Builtin and cache hits never begin an accounting attempt.
    pub async fn decide_with_accounting(
        &self,
        request: &DecisionRequest,
        lineage: &CausalLineage,
        accounting: Option<&dyn super::budget::DecisionAttemptAccounting>,
    ) -> Result<PredictionResult, BuiltinFailure> {
        let deadline = request.manifest().expires_at();
        let shadow = builtin_shadow(request)?;
        let events = Emitter {
            sink: self.events.as_ref(),
            lineage: CausalLineage {
                decision_id: lineage
                    .decision_id
                    .clone()
                    .or_else(|| Some(Arc::from(request.request_id()))),
                ..lineage.clone()
            },
        };
        let fallback_order: Vec<BackendIdentity> = self
            .slots
            .iter()
            .map(|slot| slot.backend.identity().clone())
            .chain(std::iter::once(BuiltinDecisionBackend::identity()))
            .collect();
        let builtin = |attempts: Vec<AttemptRecord>, reason: Option<FallbackReason>| {
            events.served_by_builtin();
            PredictionResult {
                served: shadow.clone(),
                served_by: ServedBy::Builtin,
                served_locality: BackendLocality::Builtin,
                served_endpoint: None,
                builtin_shadow: shadow.clone(),
                attempts,
                fallback_order: fallback_order.clone(),
                reason,
            }
        };
        if !self.enabled {
            return Ok(builtin(Vec::new(), Some(FallbackReason::Disabled)));
        }
        if self.budget_unproven {
            return Ok(builtin(Vec::new(), Some(FallbackReason::BudgetUnproven)));
        }
        // The backend a failed slot falls back to: the next slot, or the
        // builtin.
        let next_kind = |index: usize| {
            self.slots
                .get(index + 1)
                .map_or(BackendKind::Builtin, |next| next.backend.identity().kind)
        };
        let mut attempts = Vec::new();
        let mut reason = None;
        for (index, slot) in self.slots.iter().enumerate() {
            let now = Instant::now();
            let identity = slot.backend.identity().clone();
            // Model binding is checked before cache, circuit admission and I/O.
            // Neither the sealed request nor its guarded bytes are rewritten.
            if request.model() != Some(slot.request_model.as_ref()) || identity.configured_model != slot.request_model {
                let error = DecisionError::InvalidRequest(super::RequestRule::ModelInvalid);
                warn_refused(identity.kind, &error);
                events.failed(slot, &error, None);
                events.fallback(slot, next_kind(index));
                attempts.push(AttemptRecord {
                    identity,
                    outcome: AttemptOutcome::Skipped(error),
                    latency: Duration::ZERO,
                });
                continue;
            }
            let cache_key = request
                .manifest()
                .cacheable()
                .then(|| CacheKey::new(request, &identity, self.config_generation));
            if let Some(hit) = cache_key.as_ref().and_then(|key| self.cache.get(key, now)) {
                attempts.push(AttemptRecord {
                    identity,
                    outcome: AttemptOutcome::Cached,
                    latency: Duration::ZERO,
                });
                events.emit(
                    CausalEventKind::PredictionSucceeded,
                    CausalEventFields {
                        served: Some(ServedLabel::Cache),
                        ..Emitter::slot_fields(slot)
                    },
                );
                return Ok(PredictionResult {
                    served: hit.served_from_cache(request.request_id()),
                    served_by: ServedBy::Cache,
                    served_locality: slot.locality,
                    served_endpoint: slot.endpoint.clone(),
                    builtin_shadow: shadow,
                    attempts,
                    fallback_order,
                    reason: None,
                });
            }
            let skip = |error: DecisionError| {
                events.failed(slot, &error, None);
                events.fallback(slot, next_kind(index));
                AttemptRecord {
                    identity: identity.clone(),
                    outcome: AttemptOutcome::Skipped(error),
                    latency: Duration::ZERO,
                }
            };
            if !slot.healthy.load(Ordering::Relaxed) {
                attempts.push(skip(DecisionError::Unavailable));
                continue;
            }
            let Some(admission) = slot.breaker.admit(now) else {
                attempts.push(skip(DecisionError::CircuitOpen));
                continue;
            };
            if deadline.saturating_duration_since(now) < slot.min_attempt() {
                // Dropping the admission releases a half-open probe; no time
                // is left for any other backend.
                drop(admission);
                events.failed(slot, &DecisionError::Timeout, None);
                events.fallback(slot, BackendKind::Builtin);
                attempts.push(AttemptRecord {
                    identity: identity.clone(),
                    outcome: AttemptOutcome::Skipped(DecisionError::Timeout),
                    latency: Duration::ZERO,
                });
                reason = Some(FallbackReason::DeadlineExhausted);
                break;
            }
            let guarded = SystemOneCodec::new(request, &slot.key)
                .and_then(|codec| codec.encode_request())
                .and_then(|body| check_outbound(body, slot.max_request_bytes, &self.secrets));
            let body = match guarded {
                Ok(body) => body,
                Err(error) => {
                    // A refusal before sending is never the backend's fault.
                    drop(admission);
                    warn_refused(identity.kind, &error);
                    attempts.push(skip(error));
                    continue;
                }
            };
            #[cfg(test)]
            let mut material_boundary = crate::causal_tree::decision_engine::original_observation::material::evaluation(
                request.request_id(),
                body.bytes(),
            );
            events.emit(CausalEventKind::PredictionStarted, Emitter::slot_fields(slot));
            // The attempt is bounded by the slot timeout, or by the turn's
            // deadline when that comes first.
            let slot_bound = now + slot.timeout <= deadline;
            let attempt_deadline = deadline.min(now + slot.timeout);
            let observed_accounting = accounting.map(super::budget::ObservedAccounting::new);
            let outcome = tokio::time::timeout_at(
                attempt_deadline.into(),
                slot.backend.evaluate_with_accounting(
                    request,
                    &body,
                    attempt_deadline,
                    observed_accounting
                        .as_ref()
                        .map(|owner| owner as &dyn super::budget::DecisionAttemptAccounting),
                ),
            )
            .await
            .unwrap_or_else(|_| {
                Err(observed_accounting
                    .as_ref()
                    .map_or(DecisionError::Timeout, super::budget::ObservedAccounting::timeout_error))
            })
            .and_then(|response| {
                DecisionResponseValidator::validate(
                    request,
                    response,
                    &ValidationContext {
                        received_at: Instant::now(),
                        max_response_bytes: slot.max_response_bytes,
                        version_policy: slot.backend.version_policy(),
                        local_backend: identity.clone(),
                    },
                )
            });
            #[cfg(test)]
            material_boundary.returned();
            let latency = now.elapsed();
            match outcome {
                Ok(decision) => {
                    events.circuit(slot, admission.succeeded());
                    if let Some(key) = cache_key {
                        self.cache.put(key, decision.clone(), Instant::now());
                    }
                    attempts.push(AttemptRecord {
                        identity,
                        outcome: AttemptOutcome::Served,
                        latency,
                    });
                    let served_by = if index == 0 {
                        ServedBy::Primary
                    } else {
                        ServedBy::Fallback
                    };
                    events.emit(
                        CausalEventKind::PredictionSucceeded,
                        CausalEventFields {
                            served: Some(if index == 0 {
                                ServedLabel::Primary
                            } else {
                                ServedLabel::Fallback
                            }),
                            latency_ms: Some(u64::try_from(latency.as_millis()).unwrap_or(u64::MAX)),
                            ..Emitter::slot_fields(slot)
                        },
                    );
                    return Ok(PredictionResult {
                        served: decision,
                        served_by,
                        served_locality: slot.locality,
                        served_endpoint: slot.endpoint.clone(),
                        builtin_shadow: shadow,
                        attempts,
                        fallback_order,
                        reason: None,
                    });
                }
                Err(DecisionError::Timeout) if !slot_bound => {
                    // The turn's deadline cut the attempt short: not the
                    // backend's failure (D-079 L-6), and no time is left.
                    drop(admission);
                    events.failed(slot, &DecisionError::Timeout, Some(latency));
                    *slot.last_failure.lock() = Some(DecisionError::Timeout.class());
                    events.fallback(slot, BackendKind::Builtin);
                    attempts.push(AttemptRecord {
                        identity,
                        outcome: AttemptOutcome::Failed(DecisionError::Timeout),
                        latency,
                    });
                    reason = Some(FallbackReason::DeadlineExhausted);
                    break;
                }
                Err(error @ (DecisionError::BudgetExceeded | DecisionError::Cancelled)) => {
                    // Local accounting/cancellation is not evidence of an
                    // unhealthy backend and cannot consume a breaker failure.
                    drop(admission);
                    attempts.push(AttemptRecord {
                        identity,
                        outcome: AttemptOutcome::Failed(error),
                        latency,
                    });
                }
                Err(error) => {
                    events.circuit(slot, admission.failed(&error, Instant::now()));
                    events.failed(slot, &error, Some(latency));
                    *slot.last_failure.lock() = Some(error.class());
                    events.fallback(slot, next_kind(index));
                    attempts.push(AttemptRecord {
                        identity,
                        outcome: AttemptOutcome::Failed(error),
                        latency,
                    });
                }
            }
        }
        if reason.is_none() && !self.slots.is_empty() {
            reason = Some(FallbackReason::AllFailed);
        }
        Ok(builtin(attempts, reason))
    }

    /// Probe every configured backend's health (SP-CDP-A 5.6): a loopback
    /// backend answers a neutral request, a remote one reports readiness
    /// without I/O. A failed probe keeps the slot out of every decision until
    /// the next probe passes. Called after the factory is built and again
    /// by the runtime when it re-checks.
    pub async fn check_health(&self) {
        let _ = self.check_health_with_accounting(None).await;
    }

    /// Only an admitted operator-owned scope may fund a physical health probe.
    /// Returns false if any probe was locally unfunded/cancelled; true means
    /// observations were made, not that all observed backends were healthy.
    pub async fn check_health_with_accounting(
        &self,
        accounting: Option<&dyn super::budget::DecisionAttemptAccounting>,
    ) -> bool {
        let mut observed = true;
        for slot in &self.slots {
            let deadline = Instant::now() + slot.timeout;
            let result = match self.health_probe(slot, deadline) {
                Ok((probe, body)) => {
                    slot.backend
                        .health_with_accounting(&probe, &body, deadline, accounting)
                        .await
                }
                Err(error) => Err(error),
            };
            if matches!(&result, Err(DecisionError::BudgetExceeded | DecisionError::Cancelled)) {
                // No physical health evidence: preserve the previous state,
                // but tell an explicit probe caller that it was not observed.
                observed = false;
                continue;
            }
            if let Err(error) = &result {
                tracing::warn!(
                    backend = slot.backend.identity().kind.as_str(),
                    class = error.class(),
                    "decision backend health probe failed; it is left out until the next probe passes"
                );
                *slot.last_failure.lock() = Some(error.class());
            }
            slot.healthy.store(result.is_ok(), Ordering::Relaxed);
        }
        observed
    }

    /// The neutral health-probe request for `slot` (no user state; never
    /// cached) and its body, checked by the same outbound guard as every
    /// decision (D-082 N-2).
    fn health_probe(&self, slot: &Slot, deadline: Instant) -> Result<(DecisionRequest, GuardedBody), DecisionError> {
        if slot.backend.identity().configured_model != slot.request_model {
            return Err(DecisionError::InvalidRequest(super::RequestRule::ModelInvalid));
        }
        let text = |value: &str| StructuredValue::try_from(serde_json::Value::from(value));
        let mut questions = std::collections::BTreeMap::new();
        questions.insert(
            QuestionId::global(GlobalQuestion::EvidenceSufficient),
            DecisionQuestion::Noul {
                instructions: text("Health check: is the evidence sufficient?")?,
                criteria: None,
            },
        );
        let probe = DecisionRequest::seal(
            RequestParts {
                state: StructuredValue::try_from(serde_json::json!({"health_check": true}))?,
                questions,
                model: Some(slot.request_model.to_string()),
                state_fingerprint: "health-check".to_string(),
                candidate_set_hash: "health-check".to_string(),
                policy_version: Arc::from("health-check"),
                catalog_generation: "health-check".to_string(),
                omitted_questions: Vec::new(),
                approval_pending: true,
            },
            &slot.key,
            deadline,
            &RequestLimits::default(),
        )?;
        let body = check_outbound(
            SystemOneCodec::new(&probe, &slot.key)?.encode_request()?,
            slot.max_request_bytes,
            &self.secrets,
        )?;
        Ok((probe, body))
    }

    /// The factory's status: the order with each breaker, the exclusions
    /// and the cache size.
    #[must_use]
    pub fn status(&self) -> BackendPlaneStatus {
        BackendPlaneStatus {
            enabled: self.enabled,
            config_generation: self.config_generation,
            order: self
                .slots
                .iter()
                .map(|slot| SlotStatus {
                    identity: shown_identity(slot.backend.identity()),
                    locality: slot.locality,
                    breaker: slot.breaker.state(),
                    healthy: slot.healthy.load(Ordering::Relaxed),
                    last_failure: *slot.last_failure.lock(),
                })
                .collect(),
            excluded: self.excluded.clone(),
            cached: self.cache.len(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    mod model_binding;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::decision::mock::{MockDecisionBackend, MockOutcome, MockStep};
    use crate::decision::protocol::{
        DecisionQuestion, DecisionResponse, GlobalQuestion, QuestionId, RequestLimits, RequestParts, StructuredValue,
    };
    use crate::decision::{PolicyRule, ValidationRule};
    use crate::providers::catalog::{CuratedModel, ModelOperation};

    const LOOPBACK: &str = "http://127.0.0.1:8900/v1";
    const REMOTE: &str = "https://decisions.example.com/v1";
    const MODEL: &str = "decider-2026-09";

    fn key(temp: &tempfile::TempDir) -> Arc<DecisionKey> {
        DecisionKey::load_or_create(temp.path()).expect("test: key")
    }

    fn request_with(
        key: &DecisionKey,
        state: serde_json::Value,
        expires_in: Duration,
        approval_pending: bool,
    ) -> DecisionRequest {
        let mut questions = BTreeMap::new();
        questions.insert(
            QuestionId::global(GlobalQuestion::EvidenceSufficient),
            DecisionQuestion::Noul {
                instructions: StructuredValue::try_from(serde_json::Value::from("Is the evidence sufficient?"))
                    .expect("test: text"),
                criteria: None,
            },
        );
        DecisionRequest::seal(
            RequestParts {
                state: StructuredValue::try_from(state).expect("test: state"),
                questions,
                model: Some(MODEL.to_string()),
                state_fingerprint: "state".to_string(),
                candidate_set_hash: "candidates".to_string(),
                policy_version: Arc::from("policy"),
                catalog_generation: "catalog".to_string(),
                omitted_questions: Vec::new(),
                approval_pending,
            },
            key,
            Instant::now() + expires_in,
            &RequestLimits::default(),
        )
        .expect("test: seal")
    }

    fn request(key: &DecisionKey) -> DecisionRequest {
        request_with(
            key,
            serde_json::json!({"goal": "list files"}),
            Duration::from_secs(10),
            false,
        )
    }

    /// A valid answer that names `backend`, so a test can tell who served.
    fn answer_as(request: &DecisionRequest, backend: &str) -> DecisionResponse {
        let mut response = BuiltinDecisionBackend::answer(request).expect("test: answer");
        response.backend = backend.to_string();
        response.resolved_model = MODEL.to_string();
        response
    }

    /// A scripted non-builtin backend with its own identity; it records the
    /// bytes it was handed.
    struct Scripted {
        mock: Arc<MockDecisionBackend>,
        identity: BackendIdentity,
        received: Received,
    }

    type Received = Arc<parking_lot::Mutex<Vec<Vec<u8>>>>;

    #[async_trait]
    impl DecisionBackend for Scripted {
        fn identity(&self) -> &BackendIdentity {
            &self.identity
        }

        fn locality(&self) -> BackendLocality {
            BackendLocality::Loopback
        }

        fn version_policy(&self) -> ModelVersionPolicy {
            ModelVersionPolicy::Unpinned
        }

        async fn evaluate(
            &self,
            request: &DecisionRequest,
            body: &GuardedBody,
            deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.received.lock().push(body.bytes().to_vec());
            self.mock.evaluate(request, deadline).await
        }
    }

    fn recorded(
        key: &Arc<DecisionKey>,
        name: &str,
        script: Vec<MockStep>,
    ) -> (Box<dyn DecisionBackend>, Arc<MockDecisionBackend>, Received) {
        let mock = Arc::new(MockDecisionBackend::new(Arc::clone(key), script));
        let received = Received::default();
        let backend = Scripted {
            mock: Arc::clone(&mock),
            identity: BackendIdentity {
                kind: BackendKind::CompatibleHttp,
                provider: Some(ProviderId::of(&format!("custom:http://127.0.0.1:1/{name}"))),
                configured_model: Arc::from(MODEL),
            },
            received: Arc::clone(&received),
        };
        (Box::new(backend), mock, received)
    }

    #[tokio::test]
    async fn unproven_budget_gate_excludes_configured_slot_before_wire_or_cache() {
        let key = Arc::new(DecisionKey::ephemeral());
        let (backend, mock, received) = recorded(&key, "prx-budget-model", Vec::new());
        let factory = DecisionBackendFactory::from_slots(
            vec![backend],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        )
        .with_unproven_budget_gate();
        let status = factory.status();
        assert!(status.order.is_empty(), "expected an empty value");
        assert!(
            status
                .excluded
                .iter()
                .any(|(_, _, reason)| *reason == Exclusion::BudgetUnproven)
        );
        let result = factory
            .decide(&request(&key), &CausalLineage::default())
            .await
            .expect("local builtin fallback");
        assert_eq!(result.served_by, ServedBy::Builtin);
        assert_eq!(result.served_locality, BackendLocality::Builtin);
        assert_eq!(result.served_endpoint, None);
        assert_eq!(result.reason, Some(FallbackReason::BudgetUnproven));
        assert!(result.attempts.is_empty(), "expected an empty value");
        assert_eq!(mock.calls(), 0);
        assert!(received.lock().is_empty());
    }

    #[tokio::test]
    async fn unproven_budget_gate_ignores_a_primed_decision_cache() {
        let key = Arc::new(DecisionKey::ephemeral());
        let initial = request(&key);
        let (backend, mock) = scripted(
            &key,
            "prx-budget-model",
            vec![MockStep::now(MockOutcome::Respond(answer_as(
                &initial,
                "prx-budget-model",
            )))],
        );
        let factory = DecisionBackendFactory::from_slots(
            vec![backend],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        let first = factory
            .decide(&initial, &CausalLineage::default())
            .await
            .expect("priming response");
        assert_eq!(first.served_by, ServedBy::Primary);
        assert_eq!(factory.status().cached, 1);
        let gated = factory.with_unproven_budget_gate();
        let repeated = request(&key);
        let result = gated
            .decide(&repeated, &CausalLineage::default())
            .await
            .expect("builtin bypasses primed cache");
        assert_eq!(result.served_by, ServedBy::Builtin);
        assert_eq!(result.reason, Some(FallbackReason::BudgetUnproven));
        assert_eq!(mock.calls(), 1, "the gated decision did not send");
    }

    fn scripted(
        key: &Arc<DecisionKey>,
        name: &str,
        script: Vec<MockStep>,
    ) -> (Box<dyn DecisionBackend>, Arc<MockDecisionBackend>) {
        let (backend, mock, _) = recorded(key, name, script);
        (backend, mock)
    }

    /// Transports that count builds and build a scripted backend answering
    /// `script` (or refuse, when there is none).
    struct CountingTransports {
        builds: AtomicUsize,
        script: parking_lot::Mutex<Option<Vec<MockStep>>>,
    }

    impl CountingTransports {
        fn refusing() -> Self {
            Self {
                builds: AtomicUsize::new(0),
                script: parking_lot::Mutex::new(None),
            }
        }

        fn answering(script: Vec<MockStep>) -> Self {
            Self {
                builds: AtomicUsize::new(0),
                script: parking_lot::Mutex::new(Some(script)),
            }
        }

        fn builds(&self) -> usize {
            self.builds.load(Ordering::SeqCst)
        }
    }

    impl DecisionTransports for CountingTransports {
        fn build(&self, spec: &BackendSpec<'_>) -> Result<Box<dyn DecisionBackend>, DecisionError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            let script = self.script.lock().take().ok_or(DecisionError::Unavailable)?;
            Ok(Box::new(Scripted {
                mock: Arc::new(MockDecisionBackend::new(Arc::clone(&spec.key), script)),
                identity: spec.identity.clone(),
                received: Received::default(),
            }))
        }
    }

    fn config(temp: &tempfile::TempDir, change: impl FnOnce(&mut CausalTreeConfig)) -> Config {
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.config_path = temp.path().join("config.toml");
        config.api_key = Some("sk-ROOT-FACTORY-SENTINEL".to_string());
        config.causal_tree.enabled = true;
        change(&mut config.causal_tree);
        config
    }

    fn catalog(config: &Config, provider: &str) -> CatalogView {
        CatalogView::from_config(config).with_curated([CuratedModel {
            operations: Some(vec![ModelOperation::DecisionEndpoint]),
            ..CuratedModel::new(ProviderId::of(provider), Some(MODEL))
        }])
    }

    fn build(
        config: &Config,
        catalog: &CatalogView,
        key: Option<Arc<DecisionKey>>,
        transports: &CountingTransports,
    ) -> Result<DecisionBackendFactory, FactoryBuildError> {
        DecisionBackendFactory::from_config(
            config,
            FactoryDeps {
                catalog,
                key,
                transports,
                config_generation: 7,
                events: Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
            },
        )
    }

    fn compatible(endpoint: &'static str, remote: bool) -> impl FnOnce(&mut CausalTreeConfig) {
        move |section| {
            section.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
            section.backend.endpoint = endpoint.to_string();
            section.backend.model = MODEL.to_string();
            section.backend.remote_enabled = remote;
        }
    }

    /// The order table (SP-CDP-A 5.6 under D-040): pure, builtin last.
    #[test]
    fn the_backend_order_follows_the_table() {
        let order = |change: &dyn Fn(&mut CausalTreeConfig), health: HealthView| {
            let mut section = CausalTreeConfig::default();
            section.enabled = true;
            change(&mut section);
            DecisionBackendFactory::backend_order(&section, &health)
                .into_iter()
                .map(|slot| (slot.kind, slot.locality, slot.exclusion))
                .collect::<Vec<_>>()
        };
        let builtin = (BackendKind::Builtin, BackendLocality::Builtin, None);
        let set = |kind: BackendSelection, endpoint: &'static str, remote: bool| {
            move |section: &mut CausalTreeConfig| {
                section.backend.kind = kind;
                section.backend.endpoint = endpoint.to_string();
                section.backend.remote_enabled = remote;
            }
        };
        let healthy = HealthView::default();
        let compat = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        assert_eq!(
            order(&set(BackendSelection::Fixed(BackendKind::Builtin), "", true), healthy),
            [builtin]
        );
        assert_eq!(
            order(&set(BackendSelection::Auto, "", true), healthy),
            [
                (
                    BackendKind::CompatibleHttp,
                    BackendLocality::Loopback,
                    Some(Exclusion::NotConfigured)
                ),
                builtin
            ]
        );
        for kind in [BackendSelection::Auto, compat] {
            assert_eq!(
                order(&set(kind, LOOPBACK, false), healthy),
                [(BackendKind::CompatibleHttp, BackendLocality::Loopback, None), builtin],
                "loopback needs no remote switch"
            );
            assert_eq!(
                order(&set(kind, REMOTE, false), healthy),
                [
                    (
                        BackendKind::CompatibleHttp,
                        BackendLocality::Remote,
                        Some(Exclusion::RemoteDisabled)
                    ),
                    builtin
                ]
            );
            assert_eq!(
                order(&set(kind, REMOTE, true), healthy),
                [(BackendKind::CompatibleHttp, BackendLocality::Remote, None), builtin]
            );
            assert_eq!(
                order(
                    &set(kind, LOOPBACK, false),
                    HealthView {
                        circuit_open: true,
                        key_unavailable: false,
                        unhealthy: false,
                    }
                ),
                [
                    (
                        BackendKind::CompatibleHttp,
                        BackendLocality::Loopback,
                        Some(Exclusion::CircuitOpen)
                    ),
                    builtin
                ]
            );
        }
        assert_eq!(
            order(
                &set(compat, LOOPBACK, false),
                HealthView {
                    circuit_open: false,
                    key_unavailable: true,
                    unhealthy: false,
                }
            ),
            [
                (
                    BackendKind::CompatibleHttp,
                    BackendLocality::Loopback,
                    Some(Exclusion::KeyUnavailable)
                ),
                builtin
            ],
            "`prx causal backends` shows an unavailable key"
        );
        for hosted in [BackendKind::TypeSafe, BackendKind::OpenRouter] {
            assert_eq!(
                order(&set(BackendSelection::Fixed(hosted), LOOPBACK, false), healthy),
                [
                    (hosted, BackendLocality::Remote, Some(Exclusion::RemoteDisabled)),
                    builtin
                ],
                "COORD-6: a hosted backend is remote even on a loopback override"
            );
            assert_eq!(
                order(&set(BackendSelection::Fixed(hosted), "", true), healthy),
                [(hosted, BackendLocality::Remote, None), builtin]
            );
        }
        let disabled = |section: &mut CausalTreeConfig| {
            section.enabled = false;
            section.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
            section.backend.endpoint = LOOPBACK.to_string();
        };
        assert_eq!(
            order(&disabled, healthy),
            [
                (
                    BackendKind::CompatibleHttp,
                    BackendLocality::Loopback,
                    Some(Exclusion::Disabled)
                ),
                builtin
            ]
        );
    }

    /// `disabled_builds_builtin_only_order_and_no_transport` and V2.09
    /// (`default_config_performs_no_decision_network_io`): with the plane
    /// explicitly disabled, the CDP-31 enabled default with no endpoint, or
    /// `auto` without an endpoint, no transport is built and the builtin serves.
    #[tokio::test]
    async fn disabled_builds_builtin_only_order_and_no_transport() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let transports = CountingTransports::refusing();
        let mut default = Config::default();
        default.workspace_dir = temp.path().to_path_buf();
        let mut disabled = config(&temp, compatible(LOOPBACK, false));
        disabled.causal_tree.enabled = false;
        let auto_without_endpoint = config(&temp, |_| {});
        for (config, reason) in [
            (&default, None),
            (&disabled, Some(FallbackReason::Disabled)),
            (&auto_without_endpoint, None),
        ] {
            let factory = build(
                config,
                &catalog(config, &format!("custom:{LOOPBACK}")),
                Some(Arc::clone(&key)),
                &transports,
            )
            .expect("test: build");
            let result = factory
                .decide(&request(&key), &CausalLineage::default())
                .await
                .expect("test: decide");
            assert_eq!(result.served_by, ServedBy::Builtin);
            assert_eq!(result.reason, reason);
            assert!(result.attempts.is_empty(), "expected an empty value");
            assert_eq!(result.served, result.builtin_shadow);
            assert!(factory.status().order.is_empty(), "expected an empty value");
        }
        assert_eq!(transports.builds(), 0, "no transport is ever constructed");
        let disabled_status = build(
            &disabled,
            &catalog(&disabled, &format!("custom:{LOOPBACK}")),
            Some(key),
            &transports,
        )
        .expect("test: build")
        .status();
        assert_eq!(
            disabled_status.excluded,
            [(
                BackendKind::CompatibleHttp,
                BackendLocality::Loopback,
                Exclusion::Disabled
            )]
        );
    }

    /// V2.04 with D-074 and D-079 L-5: a hosted backend needs an
    /// auth-profile credential, which this release cannot configure, so it
    /// is refused at every build, with remote off or on and whatever root
    /// key is configured. No transport is built. (A remote backend left out
    /// while remote is off is shown with a compatible endpoint.)
    #[tokio::test]
    async fn hosted_backend_refused_when_remote_disabled_even_with_key() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let transports = CountingTransports::refusing();
        for remote in [false, true] {
            let hosted = config(&temp, |section| {
                section.backend.kind = BackendSelection::Fixed(BackendKind::TypeSafe);
                section.backend.model = MODEL.to_string();
                section.backend.remote_enabled = remote;
            });
            let error = build(
                &hosted,
                &catalog(&hosted, "typesafe"),
                Some(Arc::clone(&key)),
                &transports,
            )
            .expect_err("test: refused");
            assert!(
                matches!(
                    error,
                    FactoryBuildError::Selection(ResolutionError::DecisionCredentialNotProfile { .. })
                ),
                "{error}"
            );
            assert!(!error.to_string().contains("sk-ROOT-FACTORY-SENTINEL"), "{error}");
        }
        assert_eq!(transports.builds(), 0);
    }

    /// Strict selection at build (D-040 OQ-A2): an undeclared model fails
    /// the build, and so the generation; a declared loopback model is built
    /// and serves as the primary; a remote endpoint without the remote
    /// switch is left out and never built.
    #[tokio::test]
    async fn a_configured_backend_is_built_only_when_selectable_and_allowed() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let loopback = config(&temp, compatible(LOOPBACK, false));
        let undeclared = CatalogView::from_config(&loopback);
        let refusing = CountingTransports::refusing();
        assert!(matches!(
            build(&loopback, &undeclared, Some(Arc::clone(&key)), &refusing),
            Err(FactoryBuildError::Selection(
                ResolutionError::OperationUnverified { .. }
            ))
        ));
        let remote_off = config(&temp, compatible(REMOTE, false));
        let factory = build(
            &remote_off,
            &catalog(&remote_off, &format!("custom:{REMOTE}")),
            Some(Arc::clone(&key)),
            &refusing,
        )
        .expect("test: build");
        assert_eq!(
            factory.status().excluded,
            [(
                BackendKind::CompatibleHttp,
                BackendLocality::Remote,
                Exclusion::RemoteDisabled
            )]
        );
        assert_eq!(refusing.builds(), 0);

        let request = request(&key);
        let transports = CountingTransports::answering(vec![MockStep::now(MockOutcome::Respond(answer_as(
            &request,
            "local-decider",
        )))]);
        let factory = build(
            &loopback,
            &catalog(&loopback, &format!("custom:{LOOPBACK}")),
            Some(Arc::clone(&key)),
            &transports,
        )
        .expect("test: build");
        assert_eq!(transports.builds(), 1);
        let status = factory.status();
        assert_eq!(status.order.len(), 1);
        assert_eq!(status.order[0].identity.kind, BackendKind::CompatibleHttp);
        assert_eq!(
            status.order[0].identity.provider,
            Some(ProviderId::of(&format!("custom:{LOOPBACK}")))
        );
        let result = factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(result.served_by, ServedBy::Primary);
        assert_eq!(result.served_locality, BackendLocality::Loopback);
        assert_eq!(result.served_endpoint.as_deref(), Some(LOOPBACK));
        let mut private_path_result = result.clone();
        private_path_result.served_endpoint = Some(Arc::from("http://127.0.0.1/prx_private_route"));
        assert!(!format!("{private_path_result:?}").contains("prx_private_route"));
        assert_eq!(result.attempts.len(), 1);
        assert_eq!(result.attempts[0].outcome, AttemptOutcome::Served);
        assert_eq!(result.fallback_order.last(), Some(&BuiltinDecisionBackend::identity()));
    }

    /// D-040 OQ-A4, D-074: anonymous only when asked for, or on a literal
    /// loopback endpoint; otherwise the provider's own store, which a hosted
    /// backend refuses.
    #[test]
    fn an_unset_credential_is_anonymous_only_on_loopback() {
        assert_eq!(
            credential_ref_for(&CredentialRefSetting::Unset, BackendLocality::Loopback),
            CredentialRef::None
        );
        assert_eq!(
            credential_ref_for(&CredentialRefSetting::Unset, BackendLocality::Remote),
            CredentialRef::ProviderStore
        );
        for locality in [BackendLocality::Loopback, BackendLocality::Remote] {
            assert_eq!(
                credential_ref_for(&CredentialRefSetting::None, locality),
                CredentialRef::None
            );
        }
    }

    /// D-077: a backend whose health probe failed is left out of every
    /// decision until a probe passes; the order shows it as unhealthy.
    #[tokio::test]
    async fn an_unhealthy_backend_is_left_out_until_a_probe_passes() {
        struct Sick {
            identity: BackendIdentity,
            healthy: Arc<std::sync::atomic::AtomicBool>,
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl DecisionBackend for Sick {
            fn identity(&self) -> &BackendIdentity {
                &self.identity
            }
            fn locality(&self) -> BackendLocality {
                BackendLocality::Loopback
            }
            fn version_policy(&self) -> ModelVersionPolicy {
                ModelVersionPolicy::Unpinned
            }
            async fn evaluate(
                &self,
                _: &DecisionRequest,
                _: &GuardedBody,
                _: Instant,
            ) -> Result<DecisionResponse, DecisionError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(DecisionError::Unavailable)
            }
            async fn health(&self, _: &DecisionRequest, _: &GuardedBody, _: Instant) -> Result<(), DecisionError> {
                if self.healthy.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(DecisionError::Unavailable)
                }
            }
        }
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let healthy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let sick = Sick {
            identity: BackendIdentity {
                configured_model: Arc::from(MODEL),
                ..BuiltinDecisionBackend::identity()
            },
            healthy: Arc::clone(&healthy),
            calls: Arc::clone(&calls),
        };
        let factory = DecisionBackendFactory::from_slots(
            vec![Box::new(sick)],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        factory.check_health().await;
        assert!(!factory.status().order[0].healthy);
        let result = factory
            .decide(&request(&key), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Skipped(DecisionError::Unavailable)
        );
        assert_eq!(result.served_by, ServedBy::Builtin);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "an unhealthy backend is not called");
        healthy.store(true, Ordering::SeqCst);
        factory.check_health().await;
        assert!(factory.status().order[0].healthy);
        factory
            .decide(&request(&key), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let mut section = CausalTreeConfig::default();
        section.enabled = true;
        section.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        section.backend.endpoint = LOOPBACK.to_string();
        let order = DecisionBackendFactory::backend_order(
            &section,
            &HealthView {
                circuit_open: false,
                key_unavailable: false,
                unhealthy: true,
            },
        );
        assert_eq!(order[0].exclusion, Some(Exclusion::Unhealthy));
    }

    /// D-057 L-1: configuration can never select the scripted test backend.
    #[test]
    fn configuration_can_never_select_the_mock() {
        assert!(BackendSelection::try_from("mock".to_string()).is_err());
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let config = config(&temp, |section| {
            section.backend.kind = BackendSelection::Fixed(BackendKind::Mock)
        });
        let transports = CountingTransports::refusing();
        assert!(matches!(
            build(
                &config,
                &CatalogView::from_config(&config),
                Some(key(&temp)),
                &transports
            ),
            Err(FactoryBuildError::MockNotConfigurable)
        ));
        assert_eq!(transports.builds(), 0);
    }

    /// D-057 N-5: an unavailable decision key leaves only the builtin; the
    /// configured backend is left out and never built.
    #[tokio::test]
    async fn an_unavailable_key_falls_back_to_the_builtin() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key_dir = temp.path().join("self").join("causal_tree");
        std::fs::create_dir_all(&key_dir).expect("test: key dir");
        std::fs::write(key_dir.join(".decision_key"), b"short").expect("test: a malformed key");
        let loopback = config(&temp, compatible(LOOPBACK, false));
        assert!(
            FactoryDeps::load_key(&loopback).is_none(),
            "a malformed key is refused, not replaced"
        );
        let transports = CountingTransports::refusing();
        let factory = build(
            &loopback,
            &catalog(&loopback, &format!("custom:{LOOPBACK}")),
            None,
            &transports,
        )
        .expect("test: build");
        assert_eq!(
            factory.status().excluded,
            [(
                BackendKind::CompatibleHttp,
                BackendLocality::Loopback,
                Exclusion::KeyUnavailable
            )]
        );
        assert_eq!(transports.builds(), 0);
        let other = tempfile::TempDir::new().expect("test: temp dir");
        let result = factory
            .decide(&request(&key(&other)), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(result.served_by, ServedBy::Builtin);
    }

    /// D-079 L-5: a disabled plane still validates its configured backend,
    /// so enabling it later cannot fail the generation; nothing is built.
    #[test]
    fn a_disabled_plane_still_validates_its_backend() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut disabled = config(&temp, compatible(LOOPBACK, false));
        disabled.causal_tree.enabled = false;
        let transports = CountingTransports::refusing();
        assert!(matches!(
            build(&disabled, &CatalogView::from_config(&disabled), None, &transports),
            Err(FactoryBuildError::Selection(
                ResolutionError::OperationUnverified { .. }
            ))
        ));
        assert_eq!(transports.builds(), 0);
    }

    /// Every path under `dir`, sorted.
    fn listing(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in std::fs::read_dir(&next).expect("test: read dir").filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path.clone());
                }
                found.push(path);
            }
        }
        found.sort();
        found
    }

    /// D-082 N-9: a disabled plane never opens the credential store. A store
    /// another process holds neither delays nor fails the build or the
    /// configuration check, and no file appears; the configuration-only
    /// rules still hold, and enabled, the same configuration resolves its
    /// credential.
    #[test]
    fn a_disabled_plane_never_opens_the_credential_store() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut disabled = config(&temp, compatible(LOOPBACK, false));
        disabled.causal_tree.enabled = false;
        disabled.causal_tree.backend.credential_ref = CredentialRefSetting::Profile("cred-decision-held".to_string());
        // Declared in the configuration, which the configuration check reads.
        disabled.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": format!("custom:{LOOPBACK}"),
                "model": MODEL,
                "operations": ["decision_endpoint"],
            }))
            .expect("test: declaration"),
        );
        let lock = temp.path().join("auth-profiles.lock");
        std::fs::write(&lock, "pid=1\n").expect("test: held lock");
        let before = listing(temp.path());
        let transports = CountingTransports::refusing();
        let declared = catalog(&disabled, &format!("custom:{LOOPBACK}"));
        let started = Instant::now();
        build(&disabled, &declared, None, &transports).expect("test: a disabled plane builds");
        assert_eq!(
            crate::config::manage::validate::check_decision_plane(None, &disabled),
            Ok(None)
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the held store was never waited for"
        );
        assert_eq!(listing(temp.path()), before, "no file was created");
        assert_eq!(transports.builds(), 0);
        // The configuration-only rules still hold while disabled: an
        // undeclared model is refused.
        let mut undeclared = disabled.clone();
        undeclared.providers.models.clear();
        assert!(matches!(
            build(&undeclared, &CatalogView::from_config(&undeclared), None, &transports),
            Err(FactoryBuildError::Selection(
                ResolutionError::OperationUnverified { .. }
            ))
        ));
        // Enabled, the credential is resolved: a free store without it
        // refuses the reference.
        std::fs::remove_file(&lock).expect("test: release");
        let mut enabled = disabled;
        enabled.causal_tree.enabled = true;
        assert!(matches!(
            build(&enabled, &declared, Some(key(&temp)), &transports),
            Err(FactoryBuildError::Selection(ResolutionError::CredentialMissing { .. }))
        ));
    }

    /// D-079 L-14: while the plane is disabled the decision key is neither
    /// read nor created.
    #[test]
    fn a_disabled_plane_creates_no_key() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut disabled = config(&temp, compatible(LOOPBACK, false));
        disabled.causal_tree.enabled = false;
        assert!(FactoryDeps::load_key(&disabled).is_none());
        assert!(!temp.path().join("self").exists(), "no key file was written");
        let enabled = config(&temp, compatible(LOOPBACK, false));
        assert!(FactoryDeps::load_key(&enabled).is_some());
        assert!(
            temp.path()
                .join("self")
                .join("causal_tree")
                .join(".decision_key")
                .exists()
        );
    }

    /// V2.07 (and D-079 L-13): breakers are per backend. The first
    /// backend's failures open its breaker only; the second keeps serving;
    /// after the recovery interval one probe goes to the first again, and it
    /// serves.
    #[tokio::test]
    async fn circuit_opens_per_backend_not_globally() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request_with(
            &key,
            serde_json::json!({"goal": "list files"}),
            Duration::from_secs(10),
            true,
        );
        let (first, first_mock) = scripted(
            &key,
            "a",
            vec![
                MockStep::now(MockOutcome::Fail(DecisionError::Transport)),
                MockStep::now(MockOutcome::Fail(DecisionError::Transport)),
                MockStep::now(MockOutcome::Respond(answer_as(&request, "a"))),
            ],
        );
        let (second, second_mock) = scripted(
            &key,
            "b",
            vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "b"))); 3],
        );
        let limits = SlotLimits {
            failure_threshold: 2,
            cooldown: Duration::from_millis(100),
            ..SlotLimits::default()
        };
        let factory = DecisionBackendFactory::from_slots(
            vec![first, second],
            Arc::clone(&key),
            OutboundSecrets::default(),
            limits,
        );
        for _ in 0..2 {
            assert_eq!(
                factory
                    .decide(&request, &CausalLineage::default())
                    .await
                    .expect("test: decide")
                    .served_by,
                ServedBy::Fallback
            );
        }
        assert!(matches!(factory.status().order[0].breaker, BreakerState::Open { .. }));
        assert_eq!(
            factory.status().order[1].breaker,
            BreakerState::Closed { consecutive: 0 }
        );
        let result = factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Skipped(DecisionError::CircuitOpen)
        );
        assert_eq!(result.served_by, ServedBy::Fallback);
        assert_eq!(first_mock.calls(), 2, "an open breaker is not called");
        tokio::time::sleep(Duration::from_millis(120)).await;
        let result = factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(result.served_by, ServedBy::Primary, "the recovery probe served");
        assert_eq!(first_mock.calls(), 3);
        assert_eq!(second_mock.calls(), 3);
        assert_eq!(
            factory.status().order[0].breaker,
            BreakerState::Closed { consecutive: 0 }
        );
    }

    /// V2.08 (D-079 L-12, deterministic): a fallback is attempted only
    /// while the remaining deadline allows one. The first backend hangs for
    /// its whole slot timeout (180 ms) inside a 200 ms deadline, leaving less
    /// than the second backend's minimum attempt (45 ms); the builtin serves.
    #[tokio::test]
    async fn fallback_only_if_remaining_deadline_allows() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request_with(
            &key,
            serde_json::json!({"goal": "list files"}),
            Duration::from_millis(200),
            false,
        );
        let (first, _) = scripted(&key, "a", vec![MockStep::now(MockOutcome::Hang)]);
        let (second, second_mock) = scripted(
            &key,
            "b",
            vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "b")))],
        );
        let limits = SlotLimits {
            timeout: Duration::from_millis(180),
            ..SlotLimits::default()
        };
        let factory = DecisionBackendFactory::from_slots(
            vec![first, second],
            Arc::clone(&key),
            OutboundSecrets::default(),
            limits,
        );
        let started = Instant::now();
        let result = factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: decide");
        assert!(started.elapsed() < Duration::from_secs(2), "one total deadline");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::Timeout)
        );
        assert_eq!(result.reason, Some(FallbackReason::DeadlineExhausted));
        assert_eq!(result.served_by, ServedBy::Builtin);
        assert_eq!(second_mock.calls(), 0);
    }

    /// D-079 L-6: a timeout forced by the turn's deadline is not the
    /// backend's failure: its breaker does not count it. The 300 ms slot
    /// timeout outlasts the 100 ms turn deadline, which bounds the attempt
    /// (the minimum attempt, 75 ms, still lets it start).
    #[tokio::test]
    async fn a_turn_deadline_timeout_is_not_counted_against_the_backend() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request_with(
            &key,
            serde_json::json!({"goal": "list files"}),
            Duration::from_millis(100),
            false,
        );
        let (first, _) = scripted(&key, "a", vec![MockStep::now(MockOutcome::Hang)]);
        let limits = SlotLimits {
            timeout: Duration::from_millis(300),
            failure_threshold: 1,
            ..SlotLimits::default()
        };
        let factory =
            DecisionBackendFactory::from_slots(vec![first], Arc::clone(&key), OutboundSecrets::default(), limits);
        let result = factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::Timeout)
        );
        assert_eq!(result.reason, Some(FallbackReason::DeadlineExhausted));
        assert_eq!(
            factory.status().order[0].breaker,
            BreakerState::Closed { consecutive: 0 }
        );
    }

    /// D-079 M-3: a backend is handed exactly the bytes the outbound guard
    /// checked, which are the System One encoding of the request.
    #[tokio::test]
    async fn a_backend_sends_exactly_the_guarded_bytes() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request(&key);
        let (first, _, received) = recorded(
            &key,
            "a",
            vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "a")))],
        );
        let factory = DecisionBackendFactory::from_slots(
            vec![first],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        assert_eq!(
            factory
                .decide(&request, &CausalLineage::default())
                .await
                .expect("test: decide")
                .served_by,
            ServedBy::Primary
        );
        let expected = SystemOneCodec::new(&request, &key)
            .and_then(|codec| codec.encode_request())
            .expect("test: encode");
        assert_eq!(received.lock().as_slice(), [expected]);
    }

    /// CDP 17: an invalid response is void; the next backend serves, and the
    /// request (state and candidates) is unchanged.
    #[tokio::test]
    async fn invalid_response_falls_to_the_next_backend_and_never_changes_candidates() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request(&key);
        let state_before = request.state().as_json().clone();
        let mut invalid = answer_as(&request, "a");
        invalid.answers.clear();
        let (first, _) = scripted(&key, "a", vec![MockStep::now(MockOutcome::Respond(invalid))]);
        let (second, _) = scripted(
            &key,
            "b",
            vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "b")))],
        );
        let factory = DecisionBackendFactory::from_slots(
            vec![first, second],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        let result = factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::InvalidResponse(ValidationRule::AnswerMissing))
        );
        assert_eq!(result.served_by, ServedBy::Fallback);
        assert_eq!(result.served.claimed_backend(), "b");
        let question = QuestionId::global(GlobalQuestion::EvidenceSufficient);
        assert_eq!(result.served.answer(&question), result.builtin_shadow.answer(&question));
        assert_eq!(request.state().as_json(), &state_before);
    }

    /// Invariant 16.2: a request whose state carries a configured secret is
    /// never handed to a non-builtin backend.
    /// D-086 F-3: a refused request and a breaker opening reach the log at
    /// WARN whatever the observer backend is, with the backend, class, rule
    /// and duration only: never the refused value.
    #[tokio::test]
    async fn refusals_and_breaker_openings_are_logged_at_warn() {
        #[derive(Clone, Default)]
        struct Sink(Arc<parking_lot::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        // The callsite interest cache is global: a rebuild racing on another
        // thread can leave a callsite disabled after ours, so each run below
        // is retried, after a rebuild, until its warning is captured.
        let captured = |needle: &str| String::from_utf8_lossy(&sink.0.lock()).contains(needle);
        let mut live = Config::default();
        live.api_key = Some("sk-WARN-LOG-SENTINEL".to_string());
        let secret = request_with(
            &key,
            serde_json::json!({"goal": "use sk-WARN-LOG-SENTINEL please"}),
            Duration::from_secs(10),
            false,
        );
        for _ in 0..20 {
            tracing::callsite::rebuild_interest_cache();
            let (first, _) = scripted(&key, "a", Vec::new());
            let factory = DecisionBackendFactory::from_slots(
                vec![first],
                Arc::clone(&key),
                OutboundSecrets::of_config(&live),
                SlotLimits::default(),
            );
            factory
                .decide(&secret, &CausalLineage::default())
                .await
                .expect("test: decide");
            if captured("decision request refused before it was sent") {
                break;
            }
        }

        let plain = request(&key);
        for _ in 0..20 {
            tracing::callsite::rebuild_interest_cache();
            let (failing, _) = scripted(
                &key,
                "a",
                vec![MockStep::now(MockOutcome::Fail(DecisionError::Timeout))],
            );
            let factory = DecisionBackendFactory::from_slots(
                vec![failing],
                Arc::clone(&key),
                OutboundSecrets::default(),
                SlotLimits {
                    failure_threshold: 1,
                    ..SlotLimits::default()
                },
            );
            factory
                .decide(&plain, &CausalLineage::default())
                .await
                .expect("test: decide");
            if captured("decision backend circuit opened") {
                break;
            }
        }

        let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
        assert!(
            logs.contains("decision request refused before it was sent")
                && logs.contains("class=\"policy_rejection\"")
                && logs.contains("rule=\"secret_in_request\""),
            "{logs}"
        );
        assert!(
            logs.contains("decision backend circuit opened") && logs.contains("open_for_ms="),
            "{logs}"
        );
        assert!(logs.contains("WARN"), "{logs}");
        assert!(!logs.contains("WARN-LOG-SENTINEL"), "{logs}");
    }

    #[tokio::test]
    async fn a_request_carrying_a_registered_secret_is_never_sent() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let mut live = Config::default();
        live.api_key = Some("sk-OUTBOUND-STATE-SENTINEL".to_string());
        let request = request_with(
            &key,
            serde_json::json!({"goal": "use sk-OUTBOUND-STATE-SENTINEL please"}),
            Duration::from_secs(10),
            false,
        );
        let (first, mock) = scripted(
            &key,
            "a",
            vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "a")))],
        );
        let factory = DecisionBackendFactory::from_slots(
            vec![first],
            Arc::clone(&key),
            OutboundSecrets::of_config(&live),
            SlotLimits::default(),
        );
        let result = factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Skipped(DecisionError::PolicyRejection(PolicyRule::SecretInRequest))
        );
        assert_eq!(mock.calls(), 0, "nothing was sent");
        assert_eq!(result.served_by, ServedBy::Builtin);
        assert_eq!(result.reason, Some(FallbackReason::AllFailed));
    }

    /// V7.06: a repeated cacheable request is served from the cache; a
    /// different request, a pending approval or another generation misses.
    #[tokio::test]
    async fn decision_cache_hit_and_invalidation() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request(&key);
        let other = request_with(
            &key,
            serde_json::json!({"goal": "read a file"}),
            Duration::from_secs(10),
            false,
        );
        let pending = request_with(
            &key,
            serde_json::json!({"goal": "list files"}),
            Duration::from_secs(10),
            true,
        );
        let (first, mock) = scripted(
            &key,
            "a",
            vec![
                MockStep::now(MockOutcome::Respond(answer_as(&request, "a"))),
                MockStep::now(MockOutcome::Respond(answer_as(&other, "a"))),
                MockStep::now(MockOutcome::Respond(answer_as(&pending, "a"))),
                MockStep::now(MockOutcome::Respond(answer_as(&pending, "a"))),
            ],
        );
        let factory = DecisionBackendFactory::from_slots(
            vec![first],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        assert_eq!(
            factory
                .decide(&request, &CausalLineage::default())
                .await
                .expect("test: first")
                .served_by,
            ServedBy::Primary
        );
        let again = request_with(
            &key,
            serde_json::json!({"goal": "list files"}),
            Duration::from_secs(10),
            false,
        );
        assert_ne!(again.request_id(), request.request_id());
        let cached = factory
            .decide(&again, &CausalLineage::default())
            .await
            .expect("test: again");
        assert_eq!(cached.served_by, ServedBy::Cache);
        assert_eq!(cached.served_locality, BackendLocality::Loopback);
        assert_eq!(cached.served_endpoint, None, "test slots have no configured endpoint");
        // D-079 L-9: a cache hit carries the current request's id and no
        // usage of its own.
        assert_eq!(cached.served.request_id(), again.request_id());
        assert_eq!(cached.served.usage(), &crate::decision::DecisionUsage::default());
        assert_eq!(cached.served.provider_request_id(), None);
        assert_eq!(mock.calls(), 1);
        assert_eq!(
            factory
                .decide(&other, &CausalLineage::default())
                .await
                .expect("test: other")
                .served_by,
            ServedBy::Primary
        );
        for _ in 0..2 {
            assert_eq!(
                factory
                    .decide(&pending, &CausalLineage::default())
                    .await
                    .expect("test: pending")
                    .served_by,
                ServedBy::Primary
            );
        }
        assert_eq!(mock.calls(), 4, "a pending approval is never cached");
        assert_eq!(factory.status().cached, 2);

        let identity = BuiltinDecisionBackend::identity();
        let base = CacheKey::new(&request, &identity, 1);
        assert_ne!(base, CacheKey::new(&request, &identity, 2), "generation");
        assert_ne!(base, CacheKey::new(&other, &identity, 1), "request fingerprint");
        let mut model = identity.clone();
        model.configured_model = Arc::from("other-model");
        assert_ne!(base, CacheKey::new(&request, &model, 1), "configured model");
        let mut kind = identity;
        kind.kind = BackendKind::CompatibleHttp;
        assert_ne!(base, CacheKey::new(&request, &kind, 1), "backend kind");
    }

    #[test]
    fn cache_entries_expire_and_the_least_recent_is_evicted() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request(&key);
        let decision = builtin_shadow(&request).expect("test: shadow");
        let cache = DecisionCache::new();
        let now = Instant::now();
        let identity = BuiltinDecisionBackend::identity();
        let first = CacheKey::new(&request, &identity, 0);
        cache.put(first, decision.clone(), now);
        assert!(cache.get(&first, now + crate::decision::cache::CACHE_TTL / 2).is_some());
        assert!(
            cache.get(&first, now + crate::decision::cache::CACHE_TTL).is_none(),
            "expired"
        );
        assert!(cache.is_empty(), "an expired entry is removed");
        for generation in 0..=crate::decision::cache::CACHE_CAPACITY as u64 {
            cache.put(CacheKey::new(&request, &identity, generation), decision.clone(), now);
        }
        assert_eq!(cache.len(), crate::decision::cache::CACHE_CAPACITY);
        assert!(
            cache.get(&CacheKey::new(&request, &identity, 0), now).is_none(),
            "the oldest was evicted"
        );
    }

    /// Cancel-safety: dropping a decision mid-attempt records nothing as
    /// served and leaves the factory usable.
    #[tokio::test]
    async fn a_dropped_decision_records_nothing() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request(&key);
        let (first, mock) = scripted(
            &key,
            "a",
            vec![
                MockStep::now(MockOutcome::Hang),
                MockStep::now(MockOutcome::Respond(answer_as(&request, "a"))),
            ],
        );
        let factory = DecisionBackendFactory::from_slots(
            vec![first],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                factory.decide(&request, &CausalLineage::default())
            )
            .await
            .is_err()
        );
        assert_eq!(factory.status().cached, 0);
        assert_eq!(
            factory.status().order[0].breaker,
            BreakerState::Closed { consecutive: 0 }
        );
        let result = factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(result.served_by, ServedBy::Primary);
        assert_eq!(mock.calls(), 2);
    }

    /// D-079 M-1 (and L-13): a half-open probe that is cancelled (the
    /// decision dropped) does not wedge the breaker; the next decision
    /// probes the backend again and it serves.
    #[tokio::test]
    async fn a_cancelled_half_open_probe_does_not_wedge_the_breaker() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request(&key);
        let mut loopback = config(&temp, compatible(LOOPBACK, false));
        loopback.causal_tree.backend.circuit_breaker.failure_threshold = 1;
        loopback.causal_tree.backend.circuit_breaker.cooldown_secs = 1;
        let transports = CountingTransports::answering(vec![
            MockStep::now(MockOutcome::Fail(DecisionError::Transport)),
            MockStep::now(MockOutcome::Hang),
            MockStep::now(MockOutcome::Respond(answer_as(&request, "a"))),
        ]);
        let factory = build(
            &loopback,
            &catalog(&loopback, &format!("custom:{LOOPBACK}")),
            Some(Arc::clone(&key)),
            &transports,
        )
        .expect("test: build");
        assert_eq!(
            factory
                .decide(&request, &CausalLineage::default())
                .await
                .expect("test: first")
                .served_by,
            ServedBy::Builtin
        );
        assert!(matches!(factory.status().order[0].breaker, BreakerState::Open { .. }));
        tokio::time::sleep(Duration::from_millis(1050)).await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                factory.decide(&request, &CausalLineage::default())
            )
            .await
            .is_err(),
            "the probe was cancelled"
        );
        let result = factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: after the cancelled probe");
        assert_eq!(result.served_by, ServedBy::Primary, "{:?}", result.attempts);
    }

    fn lineage() -> CausalLineage {
        CausalLineage {
            workspace_id: Arc::from("workspace-1"),
            session_key: Some(Arc::from("SESSION-SENTINEL-8f3")),
            run_id: Some(Arc::from("run-1")),
            ..CausalLineage::default()
        }
    }

    /// V8.03: every event of a decision carries its lineage, and the
    /// sequence says what happened: A started, its breaker opened, it
    /// failed and fell back to B; B started and served as the fallback.
    #[tokio::test]
    async fn every_causal_event_emitted_with_lineage() {
        use crate::decision::telemetry::{CausalEventKind as Kind, CircuitState, ServedLabel};
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request(&key);
        let (first, _) = scripted(
            &key,
            "a",
            vec![MockStep::now(MockOutcome::Fail(DecisionError::Transport))],
        );
        let (second, _) = scripted(
            &key,
            "b",
            vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "b")))],
        );
        let sink = Arc::new(crate::decision::telemetry::testing::RecordingSink::default());
        let limits = SlotLimits {
            failure_threshold: 1,
            ..SlotLimits::default()
        };
        let factory = DecisionBackendFactory::from_slots(
            vec![first, second],
            Arc::clone(&key),
            OutboundSecrets::default(),
            limits,
        )
        .with_events(Arc::clone(&sink) as Arc<dyn CausalEventSink>);
        let result = factory.decide(&request, &lineage()).await.expect("test: decide");
        assert_eq!(result.served_by, ServedBy::Fallback);
        assert_eq!(result.served_locality, BackendLocality::Loopback);
        assert_eq!(result.served_endpoint, None);
        let events = std::mem::take(&mut *sink.events.lock());
        let kinds: Vec<Kind> = events.iter().map(|event| event.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::PredictionStarted,
                Kind::BackendCircuitOpened,
                Kind::PredictionFailed,
                Kind::BackendFallback,
                Kind::PredictionStarted,
                Kind::PredictionSucceeded,
            ]
        );
        for event in &events {
            assert_eq!(&*event.lineage.workspace_id, "workspace-1");
            assert_eq!(event.lineage.session_key.as_deref(), Some("SESSION-SENTINEL-8f3"));
            assert_eq!(event.lineage.decision_id.as_deref(), Some(request.request_id()));
            assert_eq!(event.fields.backend, Some(BackendKind::CompatibleHttp));
        }
        assert_eq!(events[1].fields.circuit, Some(CircuitState::Open));
        assert_eq!(events[2].fields.error_class, Some("transport"));
        assert_eq!(events[3].fields.to_backend, Some(BackendKind::CompatibleHttp));
        assert_eq!(events[5].fields.served, Some(ServedLabel::Fallback));

        // The builtin serving is an event too.
        let sink = Arc::new(crate::decision::telemetry::testing::RecordingSink::default());
        let (only, _) = scripted(
            &key,
            "a",
            vec![MockStep::now(MockOutcome::Fail(DecisionError::Unavailable))],
        );
        let factory = DecisionBackendFactory::from_slots(
            vec![only],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        )
        .with_events(Arc::clone(&sink) as Arc<dyn CausalEventSink>);
        factory.decide(&request, &lineage()).await.expect("test: decide");
        let events = sink.events.lock();
        let last = events.last().expect("test: events");
        assert_eq!(last.kind, Kind::PredictionSucceeded);
        assert_eq!(last.fields.served, Some(ServedLabel::Builtin));
        assert_eq!(events[events.len() - 2].fields.to_backend, Some(BackendKind::Builtin));
    }

    /// SP-CDP-A 5.11 `metric_labels_are_bounded_allowlist`: after a decision
    /// run with a sentinel session, user text in the state and a hostile
    /// model claimed by the backend, the Prometheus text holds none of them,
    /// and the decision metrics hold bounded labels only.
    #[tokio::test]
    async fn metric_labels_are_bounded_allowlist() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let request = request_with(
            &key,
            serde_json::json!({"goal": "USER-TEXT-SENTINEL please list files"}),
            Duration::from_secs(10),
            false,
        );
        let mut hostile = answer_as(&request, "a");
        hostile.resolved_model = "HOSTILE-CLAIMED-MODEL".to_string();
        let (first, _) = scripted(&key, "a", vec![MockStep::now(MockOutcome::Respond(hostile))]);
        let observer = Arc::new(crate::observability::PrometheusObserver::try_new().expect("test: observer"));
        let sink = crate::decision::telemetry::ObserverCausalSink::new(
            Arc::clone(&observer) as Arc<dyn crate::observability::Observer>
        );
        let factory = DecisionBackendFactory::from_slots(
            vec![first],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        )
        .with_events(Arc::new(sink));
        assert_eq!(
            factory
                .decide(&request, &lineage())
                .await
                .expect("test: decide")
                .served_by,
            ServedBy::Primary
        );
        let text = observer.encode();
        assert!(
            text.contains(
                r#"prx_causal_decisions_total{backend="compatible_http",locality="loopback",served_by="primary"} 1"#
            ),
            "{text}"
        );
        for sentinel in [
            "SESSION-SENTINEL",
            "USER-TEXT-SENTINEL",
            "HOSTILE-CLAIMED-MODEL",
            "workspace-1",
            "run-1",
            request.request_id(),
        ] {
            assert!(!text.contains(sentinel), "{sentinel} reached a metric: {text}");
        }
    }

    /// AG-12: a decision error and the factory's status never show URL user
    /// information, even for a provider label that carries it.
    #[tokio::test]
    async fn decision_errors_and_status_carry_no_userinfo() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let key = key(&temp);
        let mock = Arc::new(MockDecisionBackend::new(Arc::clone(&key), Vec::new()));
        let leaky = Scripted {
            mock,
            identity: BackendIdentity {
                kind: BackendKind::CompatibleHttp,
                provider: Some(ProviderId::of("custom:http://decider:USERINFO-SENTINEL@127.0.0.1:1/v1")),
                configured_model: Arc::from(MODEL),
            },
            received: Received::default(),
        };
        let factory = DecisionBackendFactory::from_slots(
            vec![Box::new(leaky)],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        // `ProviderId`'s Debug redacts by itself; the raw key is what a
        // serialized status would carry, so it is checked directly.
        let shown = factory.status().order[0]
            .identity
            .provider
            .as_ref()
            .map(|provider| provider.as_str().to_string())
            .unwrap_or_default();
        assert!(!shown.contains("USERINFO-SENTINEL"), "{shown}");
        assert!(shown.contains("127.0.0.1"), "{shown}");
        let status = format!("{:?} {:?}", factory.status(), factory);
        assert!(!status.contains("USERINFO-SENTINEL"), "{status}");
        assert!(status.contains("127.0.0.1"), "the endpoint stays readable: {status}");
        let refused = config(
            &temp,
            compatible("http://decider:USERINFO-SENTINEL@127.0.0.1:1/v1", false),
        );
        let transports = CountingTransports::refusing();
        let error =
            build(&refused, &CatalogView::from_config(&refused), Some(key), &transports).expect_err("test: refused");
        assert!(!error.to_string().contains("USERINFO-SENTINEL"), "{error}");
        assert!(!format!("{error:?}").contains("USERINFO-SENTINEL"), "{error:?}");
    }
}
