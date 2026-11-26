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
//!   backend sends (D-079 M-3), and validation. A backend without its own
//!   transport bound has its whole attempt bounded by the slot timeout and
//!   request deadline. The HTTP adapter instead gives source-owned accounting
//!   the request deadline and retains the slot timeout on its client I/O. A
//!   timeout the turn's deadline forced is not the backend's failure (D-079
//!   L-6). Any failure falls to the next slot; when every slot fails or time
//!   runs out, the builtin shadow is the answer. The factory has no API that
//!   changes a candidate, a state or a gate.
//! - An unavailable decision key (D-057) leaves only the builtin.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::config::schema::decision_engine::{BackendKind, BackendLocality};
use crate::config::schema::decision_engine::{BackendSelection, DecisionBackendConfig};
use crate::config::schema::decision_engine::{configured_kind, configured_selection, locality_of};
use crate::cost::decision_attempt::BackendIdentity;
use crate::cost::decision_attempt::error::DecisionError;
use crate::decision_engine::prediction::backend::DecisionBackend;
use crate::decision_engine::prediction::breaker::{BreakerState, BreakerTransition, CircuitBreaker};
use crate::decision_engine::prediction::builtin::BuiltinDecisionBackend;
use crate::decision_engine::prediction::cache::{CacheKey, DecisionCache};
use crate::decision_engine::prediction::outbound::{GuardedBody, OutboundSecrets, check_outbound};
use crate::decision_engine::prediction::protocol::{
    DecisionQuestion, DecisionRequest, GlobalQuestion, QuestionId, RequestLimits, RequestParts, StructuredValue,
};
use crate::decision_engine::prediction::validate::{
    DecisionResponseValidator, ModelVersionPolicy, ValidatedDecision, ValidationContext,
};
use crate::decision_engine::prediction::wire::SystemOneCodec;
use crate::observability::causal_event::{
    CausalEvent, CausalEventFields, CausalEventKind, CausalEventSink, CausalLineage, CircuitState, ServedLabel,
};
use crate::providers::ProviderId;
use crate::providers::catalog::{CatalogView, ResolutionError, ResolvedModel};
use crate::security::runtime_key::DecisionKey;

/// The shortest attempt worth starting.
const MIN_ATTEMPT: Duration = Duration::from_millis(20);

/// Why a configured backend is not in the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// The advisory engine is off, so no physical backend is constructed
    /// and nothing leaves PRX.
    EngineDisabled,
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
            Self::EngineDisabled => "engine_disabled",
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
        let defaults = crate::config::schema::decision_engine::DecisionBackendConfig::default();
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
    accounting: Option<&'a dyn crate::cost::decision_attempt::DecisionAttemptAccounting>,
}
impl crate::decision_engine::prediction_port::prediction_sealed::Producer for AccountedPrediction<'_> {}
impl crate::decision_engine::prediction_port::PredictionPort for AccountedPrediction<'_> {
    async fn predict<'a>(
        &'a self,
        request: &'a crate::decision_engine::prediction::DecisionRequest,
        lineage: &'a CausalLineage,
    ) -> Result<PredictionResult, crate::cost::decision_attempt::DecisionError> {
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
    #[cfg(test)]
    prediction_port_calls: AtomicUsize,
    #[cfg(test)]
    workspace: std::path::PathBuf,
}

impl std::fmt::Debug for DecisionBackendFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionBackendFactory")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
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
        accounting: Option<&'a dyn crate::cost::decision_attempt::DecisionAttemptAccounting>,
    ) -> AccountedPrediction<'a> {
        #[cfg(test)]
        self.prediction_port_calls.fetch_add(1, Ordering::SeqCst);
        #[cfg(test)]
        crate::decision_engine::access_spy::record_factory(&self.workspace);
        AccountedPrediction {
            factory: self,
            accounting,
        }
    }

    #[cfg(test)]
    pub(crate) fn prediction_port_calls(&self) -> usize {
        self.prediction_port_calls.load(Ordering::SeqCst)
    }

    /// The backend order for `backend` (SP-CDP-A 5.6 under D-040) with the
    /// engine switch `enabled`. Pure: the same configuration and health always
    /// give the same order. The builtin is always the last included slot.
    #[must_use]
    pub fn backend_order(backend: &DecisionBackendConfig, enabled: bool, health: &HealthView) -> Vec<PlannedSlot> {
        let builtin = PlannedSlot {
            kind: BackendKind::Builtin,
            locality: BackendLocality::Builtin,
            exclusion: None,
        };
        let Some(kind) = configured_kind(backend) else {
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
        let exclusion = if !enabled {
            Some(Exclusion::EngineDisabled)
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

    /// Build the factory for `config`'s effective engine backend (R55).
    ///
    /// Nothing is built for an excluded backend: a disabled engine, a remote
    /// backend without `remote_enabled` and an unavailable key never reach
    /// a transport.
    ///
    /// # Errors
    ///
    /// [`FactoryBuildError::Selection`] when the strict selection refuses
    /// the configured backend, [`FactoryBuildError::Transport`] when it
    /// cannot be built.
    pub fn from_config(config: &Config, deps: FactoryDeps<'_>) -> Result<Self, FactoryBuildError> {
        let backend = crate::config::schema::decision_engine::effective_engine_backend(config);
        let enabled = config.decision_engine.enabled;
        #[cfg(any(test, feature = "test-mock"))]
        if backend.kind == BackendSelection::Fixed(BackendKind::Mock) {
            return Err(FactoryBuildError::MockNotConfigurable);
        }
        let health = HealthView {
            circuit_open: false,
            key_unavailable: deps.key.is_none(),
            unhealthy: false,
        };
        let mut secrets = OutboundSecrets::of_config(config);
        let endpoint = backend.endpoint.trim();
        // Strict selection of the configured backend at every build, whether
        // or not it takes part (D-040 OQ-A2, D-079 L-5): a configuration that
        // would fail once enabled fails now. It reads local state only.
        let selected = match configured_selection(backend) {
            None => None,
            // A disabled decision engine builds no slot, including public
            // loopback HTTP. No engine-off slot may resolve a stored
            // credential (D-082 N-9).
            Some(selection) if !enabled => {
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
        for planned in Self::backend_order(backend, enabled, &health) {
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
                secrets.add("decision_engine.backend.credential_ref", material);
            }
            for rotation in resolved.credential().rotation_keys() {
                secrets.add("decision_engine.backend.credential_ref", rotation);
            }
            let request_model: Arc<str> = Arc::from(resolved.model());
            let identity = BackendIdentity {
                kind: planned.kind,
                provider: Some(ProviderId::of(provider)),
                configured_model: Arc::clone(&request_model),
            };
            #[cfg(test)]
            crate::decision_engine::access_spy::record_backend_slot(&config.workspace_dir);
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
                    crate::cost::decision_attempt::error::RequestRule::ModelInvalid,
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
            enabled,
            budget_unproven: false,
            events: deps.events,
            slots,
            excluded,
            cache: DecisionCache::new(),
            secrets,
            config_generation: deps.config_generation,
            #[cfg(test)]
            prediction_port_calls: AtomicUsize::new(0),
            #[cfg(test)]
            workspace: config.workspace_dir.clone(),
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
            #[cfg(test)]
            prediction_port_calls: AtomicUsize::new(0),
            #[cfg(test)]
            workspace: config.workspace_dir.clone(),
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
        let defaults = crate::config::schema::decision_engine::DecisionBackendConfig::default();
        Self {
            enabled: true,
            budget_unproven: false,
            events: Arc::new(crate::observability::causal_event::testing::RecordingSink::default()),
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
            prediction_port_calls: AtomicUsize::new(0),
            workspace: std::path::PathBuf::new(),
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
        accounting: Option<&dyn crate::cost::decision_attempt::DecisionAttemptAccounting>,
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
                let error =
                    DecisionError::InvalidRequest(crate::cost::decision_attempt::error::RequestRule::ModelInvalid);
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
            events.emit(CausalEventKind::PredictionStarted, Emitter::slot_fields(slot));
            // Ordinarily the whole attempt is bounded by the slot timeout or
            // the turn deadline, whichever comes first.
            let slot_bound = now + slot.timeout <= deadline;
            // The HTTP adapter's client owns the configured transport timeout.
            // Give its source-owned durable accounting the rest of the sealed
            // turn deadline instead of charging local journal latency to the
            // narrower network allowance. The factory still bounds the whole
            // call at the manifest deadline.
            let attempt_deadline = if accounting.is_some() && slot.backend.bounds_transport_independently() {
                deadline
            } else {
                deadline.min(now + slot.timeout)
            };
            let observed_accounting =
                accounting.map(crate::decision_engine::prediction::budget::ObservedAccounting::new);
            let outcome = tokio::time::timeout_at(
                attempt_deadline.into(),
                slot.backend.evaluate_with_accounting(
                    request,
                    &body,
                    attempt_deadline,
                    observed_accounting
                        .as_ref()
                        .map(|owner| owner as &dyn crate::cost::decision_attempt::DecisionAttemptAccounting),
                ),
            )
            .await
            .unwrap_or_else(|_| {
                Err(observed_accounting.as_ref().map_or(
                    DecisionError::Timeout,
                    crate::decision_engine::prediction::budget::ObservedAccounting::timeout_error,
                ))
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
        accounting: Option<&dyn crate::cost::decision_attempt::DecisionAttemptAccounting>,
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
            return Err(DecisionError::InvalidRequest(
                crate::cost::decision_attempt::error::RequestRule::ModelInvalid,
            ));
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
mod tests;
