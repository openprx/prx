//! The decision plane's read and test service (SP-CDP-A 5.13, CDP-PLAN
//! 1.14; CDP-11), shared by `prx causal` and the Gateway (CDP-15).
//!
//! Every answer is a `Serialize` DTO that is redacted by construction: no
//! DTO has a credential field, provider labels are shown through the
//! factory's userinfo redaction, and free text from errors passes through
//! `redact_endpoint_userinfo`.
//!
//! - `status` and `backends` read the factory of the current configuration,
//!   which the caller's [`DecisionPlaneBuilder`] builds (the causal domain
//!   never names a transport or a client, AG-01); a loopback backend is
//!   probed, a remote one never is.
//! - `test` asks the configured backend the neutral fixture (one question of
//!   each primitive, no user state). A loopback backend runs; a remote one
//!   needs `--live` with confirmation (CDP-14). It never touches the store or
//!   the configuration.
//! - `inspect` reads one decision in the local operator's scope (the local
//!   CLI is the operator, D-019 Q-5).
//! - `evaluate` writes a versioned report and changes nothing else.
//!
//! The store is opened read-write only while the plane is enabled, which is
//! when the v1 feedback import runs; a disabled plane's store stays
//! read-only (D-040).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::evaluate::{EvaluateError, EvaluationOptions, EvaluationReport, evaluate, write_report};
use super::ids::DecisionId;
use super::store::{CausalDecisionStore, DecisionScope, DecisionView, Retention, StoreError, StoreMode, TimeWindow};
use super::v1_compat::{ImportSummary, import_v1_feedback};
use crate::config::Config;
use crate::decision::factory::{
    AttemptOutcome, DecisionBackendFactory, FactoryBuildError, FactoryDeps, HealthView, ServedBy,
};
use crate::decision::telemetry::{CausalEventSink, CausalLineage};
use crate::decision::{
    BackendKind, BackendLocality, DecisionKey, DecisionQuestion, DecisionRequest, GlobalQuestion, OptionId, QuestionId,
    RequestLimits, RequestParts, StructuredValue,
};

/// Builds the decision backend factory of a configuration: the caller's
/// transports, catalog and event sink (the CLI's, and the Gateway's from
/// CDP-15).
pub(crate) trait DecisionPlaneBuilder: Send + Sync {
    /// The factory of `config`'s `[causal_tree]` section with `key`.
    ///
    /// # Errors
    ///
    /// [`FactoryBuildError`] when the configured backend is not selectable
    /// or cannot be built.
    fn build(
        &self,
        config: &Config,
        key: Option<Arc<DecisionKey>>,
    ) -> Result<DecisionBackendFactory, FactoryBuildError>;

    /// The integration owns accounting and cleanup; this domain never creates
    /// runtime source authority. Test planes may keep their in-memory backend.
    fn check_health<'a>(
        &'a self,
        factory: &'a DecisionBackendFactory,
        _config: &'a Config,
        _key: Option<Arc<DecisionKey>>,
    ) -> AdminPlaneFuture<'a, bool> {
        Box::pin(async move {
            factory.check_health().await;
            Ok(true)
        })
    }

    fn decide<'a>(
        &'a self,
        factory: &'a DecisionBackendFactory,
        _config: &'a Config,
        _key: Arc<DecisionKey>,
        request: &'a DecisionRequest,
        lineage: &'a CausalLineage,
    ) -> AdminPlaneFuture<'a, AdminPrediction> {
        Box::pin(async move {
            factory
                .decide(request, lineage)
                .await
                .map(|result| {
                    AdminPrediction::from_validated_parts(
                        result.served,
                        result.served_by,
                        result
                            .attempts
                            .into_iter()
                            .map(|attempt| (attempt.identity.kind, attempt.outcome, attempt.latency)),
                    )
                })
                .map_err(|failure| AdminError::Fixture(failure.0.class().to_owned()))
        })
    }
}

/// The validated facts consumed by the administration test view.
/// Factory-local routing evidence remains owned by the prediction producer.
pub(crate) struct AdminPrediction {
    served: crate::decision::ValidatedDecision,
    served_by: ServedBy,
    sent: bool,
    attempts: Vec<AttemptView>,
}

impl AdminPrediction {
    pub(crate) fn from_validated_parts(
        served: crate::decision::ValidatedDecision,
        served_by: ServedBy,
        attempts: impl Iterator<Item = (BackendKind, AttemptOutcome, Duration)>,
    ) -> Self {
        let mut sent = false;
        let attempts = attempts
            .map(|(backend, outcome, latency)| {
                sent |= matches!(outcome, AttemptOutcome::Served | AttemptOutcome::Failed(_));
                let (outcome, error_class) = match outcome {
                    AttemptOutcome::Served => ("served", None),
                    AttemptOutcome::Cached => ("cached", None),
                    AttemptOutcome::Failed(error) => ("failed", Some(error.class())),
                    AttemptOutcome::Skipped(error) => ("skipped", Some(error.class())),
                };
                AttemptView {
                    backend: backend.as_str(),
                    outcome,
                    error_class,
                    latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
                }
            })
            .collect();
        Self {
            served,
            served_by,
            sent,
            attempts,
        }
    }
}

pub(crate) type AdminPlaneFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, AdminError>> + Send + 'a>>;

/// How long a test decision may take.
const TEST_DEADLINE: Duration = Duration::from_secs(30);

/// How long closing waits for the store's queue.
const CLOSE_DEADLINE: Duration = Duration::from_secs(5);

/// Why an admin request failed. The messages carry no secret.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AdminError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Evaluate(#[from] EvaluateError),
    #[error("the decision test request could not be built: {0}")]
    Fixture(String),
}

/// One backend of the order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct BackendView {
    pub(crate) kind: &'static str,
    pub(crate) locality: &'static str,
    /// The provider label, with any URL userinfo redacted.
    pub(crate) provider: Option<String>,
    pub(crate) model: String,
    /// `closed`, `open`, `half_open` or `open_for_generation`.
    pub(crate) circuit: &'static str,
    /// `healthy` or `unhealthy` after a probe; `unprobed` for a remote
    /// backend, which is never probed (it reports configuration readiness
    /// only).
    pub(crate) health: &'static str,
    /// The class of its last failure, if any (no detail: classes carry no
    /// data).
    pub(crate) last_failure: Option<&'static str>,
}

/// A configured backend left out of the order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ExcludedView {
    pub(crate) kind: &'static str,
    pub(crate) locality: &'static str,
    pub(crate) reason: &'static str,
}

/// The store's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StoreView {
    pub(crate) path: String,
    /// `read_write` or `read_only`.
    pub(crate) mode: &'static str,
    pub(crate) schema_version: i64,
    pub(crate) decisions: i64,
    pub(crate) legacy_decisions: i64,
    pub(crate) outcomes: i64,
    /// What the v1 feedback import did on this open.
    pub(crate) v1_import: Option<ImportView>,
    /// Why the store could not be read.
    pub(crate) error: Option<String>,
}

/// A v1 import summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct ImportView {
    pub(crate) files: u64,
    pub(crate) unchanged: u64,
    pub(crate) imported: u64,
    pub(crate) malformed: u64,
    pub(crate) dropped: u64,
}

impl From<ImportSummary> for ImportView {
    fn from(summary: ImportSummary) -> Self {
        Self {
            files: summary.files,
            unchanged: summary.unchanged,
            imported: summary.imported,
            malformed: summary.malformed,
            dropped: summary.dropped,
        }
    }
}

/// Legacy `[causal_tree]` keys found in the configuration file: names only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct LegacyKeysView {
    pub(crate) moved: Vec<&'static str>,
    pub(crate) dropped: Vec<&'static str>,
    /// Why the file could not be checked.
    pub(crate) error: Option<String>,
}

/// Numeric policy fields allowed in the management status DTO. Calibration
/// profiles and endpoint-bearing configuration are deliberately excluded.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct PolicyView {
    pub(crate) max_candidates: u8,
    pub(crate) max_decision_hops: u8,
    pub(crate) extra_latency_budget_ms: u64,
    pub(crate) commit_threshold: f32,
    pub(crate) minimum_choice_confidence: f64,
    pub(crate) minimum_top_margin: f64,
    pub(crate) extra_token_ratio_limit: f32,
}

impl From<&super::config::CausalPolicy> for PolicyView {
    fn from(policy: &super::config::CausalPolicy) -> Self {
        Self {
            max_candidates: policy.max_candidates,
            max_decision_hops: policy.max_decision_hops,
            extra_latency_budget_ms: policy.extra_latency_budget_ms,
            commit_threshold: policy.commit_threshold,
            minimum_choice_confidence: policy.minimum_choice_confidence,
            minimum_top_margin: policy.minimum_top_margin,
            extra_token_ratio_limit: policy.extra_token_ratio_limit,
        }
    }
}

/// `prx causal status`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct StatusView {
    pub(crate) enabled: bool,
    pub(crate) remote_enabled: bool,
    pub(crate) backend_timeout_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latency_budget_warning: Option<String>,
    pub(crate) policy: PolicyView,
    pub(crate) key_available: bool,
    /// The order the factory tries; the builtin always ends it.
    pub(crate) order: Vec<BackendView>,
    pub(crate) excluded: Vec<ExcludedView>,
    /// Why the configured backend could not be built.
    pub(crate) factory_error: Option<String>,
    pub(crate) store: StoreView,
    pub(crate) legacy_keys: LegacyKeysView,
}

/// One position of the computed order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PlannedView {
    pub(crate) kind: &'static str,
    pub(crate) locality: &'static str,
    /// Why it is left out, if it is.
    pub(crate) excluded: Option<&'static str>,
}

/// `prx causal backends`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct BackendsView {
    pub(crate) enabled: bool,
    pub(crate) order: Vec<PlannedView>,
}

/// Which backend a test asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestTarget {
    /// The configured backend.
    Configured,
    /// The builtin only.
    Builtin,
}

/// How a test may reach the backend (PMM-D-7: local unless explicitly
/// live).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestMode {
    /// Loopback and builtin backends only: a remote backend is not asked
    /// (`requires_live`).
    Local,
    /// The operator confirmed one live call: a remote backend is asked the
    /// neutral fixture once (it is never retried, and it may be billed).
    Live,
}

/// A test request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TestRequest {
    pub(crate) target: TestTarget,
    pub(crate) mode: TestMode,
}

/// One attempt of a test decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AttemptView {
    pub(crate) backend: &'static str,
    /// `served`, `cached`, `failed` or `skipped`.
    pub(crate) outcome: &'static str,
    pub(crate) error_class: Option<&'static str>,
    pub(crate) latency_ms: u64,
}

/// `prx causal test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TestView {
    /// `passed` (the asked backend answered), `fell_back` (it failed and
    /// the builtin answered), `not_tested` (it was excluded and never
    /// asked), `requires_live`, `disabled` or `key_unavailable`.
    pub(crate) outcome: &'static str,
    /// Whether any request left PRX.
    pub(crate) sent: bool,
    /// `primary`, `fallback`, `cache` or `builtin`.
    pub(crate) served_by: Option<&'static str>,
    pub(crate) attempts: Vec<AttemptView>,
    /// The primitives the served answer covered.
    pub(crate) answered: Vec<&'static str>,
    pub(crate) note: Option<String>,
}

/// One candidate of an inspected decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CandidateView {
    pub(crate) id: String,
    pub(crate) action: String,
    pub(crate) target: String,
}

/// One outcome of an inspected decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OutcomeSummary {
    pub(crate) settlement_id: String,
    pub(crate) classification: String,
    pub(crate) completeness: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ChildOutcomeSummary {
    pub(crate) child_decision_id: String,
    pub(crate) settlement_id: String,
    pub(crate) classification: String,
    pub(crate) completeness: String,
}

impl From<super::store::ChildOutcomeView> for ChildOutcomeSummary {
    fn from(value: super::store::ChildOutcomeView) -> Self {
        Self {
            child_decision_id: value.child_decision_id,
            settlement_id: value.settlement_id,
            classification: value.classification,
            completeness: value.completeness,
        }
    }
}

/// Only canonical action, target and Noul question labels may leave the store as
/// distributions. Legacy, absent or malformed maps remain unavailable;
/// values are never filled in or renormalized by the presentation layer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct DistributionView {
    pub(crate) next_action: Option<std::collections::BTreeMap<String, f64>>,
    pub(crate) capability_target: Option<std::collections::BTreeMap<String, f64>>,
    pub(crate) features: Option<std::collections::BTreeMap<String, f64>>,
}

impl From<&super::store::RecordedDistributions> for DistributionView {
    fn from(source: &super::store::RecordedDistributions) -> Self {
        fn allowed(
            values: &std::collections::BTreeMap<String, f64>,
            labels: impl Iterator<Item = &'static str>,
            legacy: bool,
        ) -> Option<std::collections::BTreeMap<String, f64>> {
            let labels: std::collections::BTreeSet<_> = labels.collect();
            (!legacy
                && !values.is_empty()
                && values.iter().all(|(label, value)| {
                    labels.contains(label.as_str()) && value.is_finite() && (0.0..=1.0).contains(value)
                }))
            .then(|| values.clone())
        }
        fn feature_label(label: &str) -> bool {
            if [GlobalQuestion::EvidenceSufficient, GlobalQuestion::NeedsFreshState]
                .into_iter()
                .any(|question| question.as_str() == label)
            {
                return true;
            }
            let mut segments = label.split('.');
            let (Some("candidate"), Some(key), Some(question), None) =
                (segments.next(), segments.next(), segments.next(), segments.next())
            else {
                return false;
            };
            if !matches!(question, "applicable" | "success") {
                return false;
            }
            super::branch::CausalActionKind::ALL.into_iter().any(|action| {
                super::branch::CapabilityTarget::ALL.into_iter().any(|target| {
                    let base = format!("{}-{}", action.as_str(), target.as_str());
                    key == base
                        || key
                            .strip_prefix(&base)
                            .and_then(|tail| tail.strip_prefix('-'))
                            .is_some_and(|suffix| {
                                suffix.len() == 8
                                    && suffix
                                        .bytes()
                                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                            })
                })
            })
        }
        Self {
            features: (!source.legacy_v1
                && !source.features.is_empty()
                && source
                    .features
                    .iter()
                    .all(|(label, value)| feature_label(label) && value.is_finite() && (0.0..=1.0).contains(value)))
            .then(|| source.features.clone()),
            next_action: allowed(
                &source.next_action,
                super::branch::CausalActionKind::ALL
                    .into_iter()
                    .map(super::branch::CausalActionKind::as_str),
                source.legacy_v1,
            ),
            capability_target: allowed(
                &source.capability_target,
                super::branch::CapabilityTarget::ALL
                    .into_iter()
                    .map(super::branch::CapabilityTarget::as_str),
                source.legacy_v1,
            ),
        }
    }
}

/// Content-free counts for each recorded gate pass. Operation digests,
/// candidate identifiers and arbitrary diagnostic strings are not exposed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct GateSummary {
    pub(crate) passed: u64,
    pub(crate) approval_only: u64,
    pub(crate) approval_deferred: u64,
    pub(crate) blocked: std::collections::BTreeMap<u8, u64>,
}

fn inspect_gate_summary(raw: &serde_json::Value) -> Option<std::collections::BTreeMap<&'static str, GateSummary>> {
    let mut summaries = std::collections::BTreeMap::new();
    for phase in ["pre_prediction", "post_rehearsal", "pre_commit"] {
        let entries = raw.get(phase)?.as_object()?;
        let mut summary = GateSummary::default();
        for outcome in entries.values() {
            match outcome.get("status")?.as_str()? {
                "passed" => summary.passed += 1,
                "approval_only" => summary.approval_only += 1,
                "approval_deferred" => summary.approval_deferred += 1,
                "blocked" => {
                    let gate = u8::try_from(outcome.get("gate")?.as_u64()?).ok()?;
                    if !(1..=9).contains(&gate) {
                        return None;
                    }
                    *summary.blocked.entry(gate).or_default() += 1;
                }
                _ => return None,
            }
        }
        summaries.insert(phase, summary);
    }
    Some(summaries)
}

/// An existing opt-in projection with current display redaction reapplied.
/// Never include its contents in Debug diagnostics.
#[derive(Clone, PartialEq, Serialize)]
pub(crate) struct RecordedProjectionView {
    pub(crate) state: serde_json::Value,
    pub(crate) display_redacted: bool,
}

impl std::fmt::Debug for RecordedProjectionView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordedProjectionView")
            .field("display_redacted", &self.display_redacted)
            .finish_non_exhaustive()
    }
}

impl From<serde_json::Value> for RecordedProjectionView {
    fn from(original: serde_json::Value) -> Self {
        let mut state = original.clone();
        crate::security::redaction::redact_json_value(&mut state);
        Self {
            display_redacted: state != original,
            state,
        }
    }
}

/// `prx causal inspect`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct InspectView {
    pub(crate) decision_id: String,
    pub(crate) created_at_ms: i64,
    pub(crate) legacy_v1: bool,
    pub(crate) has_owner: bool,
    pub(crate) run_id: Option<String>,
    pub(crate) task_id: Option<String>,
    pub(crate) channel: Option<String>,
    pub(crate) enabled_snapshot: bool,
    pub(crate) policy_version: String,
    pub(crate) backend_kind: String,
    pub(crate) backend_locality: String,
    pub(crate) configured_model: String,
    pub(crate) directive_kind: String,
    pub(crate) selected: Option<CandidateView>,
    pub(crate) candidates: Vec<CandidateView>,
    pub(crate) distributions: DistributionView,
    pub(crate) recorded_projection: Option<RecordedProjectionView>,
    /// Present only on a detailed, owner-scoped inspection when recorded.
    pub(crate) gate_summary: Option<std::collections::BTreeMap<&'static str, GateSummary>>,
    pub(crate) fallback_order: Vec<String>,
    pub(crate) abstention_reason: Option<String>,
    pub(crate) propensity: f64,
    pub(crate) explored: bool,
    pub(crate) error_class: Option<String>,
    pub(crate) cache_hit: bool,
    pub(crate) latency_ms: i64,
    /// `none`, `incomplete` or `joined`.
    pub(crate) outcome_join: &'static str,
    pub(crate) outcomes: Vec<OutcomeSummary>,
    pub(crate) child_outcomes: Vec<ChildOutcomeSummary>,
    pub(crate) child_outcomes_truncated: bool,
}

impl From<DecisionView> for InspectView {
    fn from(view: DecisionView) -> Self {
        let candidate = |candidate: &super::store::RecordedCandidate| CandidateView {
            id: candidate.id.clone(),
            action: candidate.action.clone(),
            target: candidate.target.clone(),
        };
        let outcome_join = if view.outcomes.is_empty() {
            "none"
        } else if view.outcomes.iter().any(|outcome| outcome.completeness == "complete") {
            "joined"
        } else {
            "incomplete"
        };
        Self {
            decision_id: view.decision_id.to_string(),
            created_at_ms: view.created_at_ms,
            legacy_v1: view.legacy_v1,
            has_owner: view.has_owner,
            selected: view.selected().map(candidate),
            candidates: view.candidates.iter().map(candidate).collect(),
            distributions: DistributionView::from(&view.distributions),
            recorded_projection: None,
            gate_summary: None,
            run_id: view.run_id,
            task_id: view.task_id,
            channel: view.channel,
            enabled_snapshot: view.enabled_snapshot,
            policy_version: view.policy_version,
            backend_kind: view.backend_kind,
            backend_locality: view.backend_locality,
            configured_model: view.configured_model,
            directive_kind: view.directive_kind,
            fallback_order: view.fallback_order,
            abstention_reason: view.abstention_reason,
            propensity: view.propensity,
            explored: view.explored,
            error_class: view.error_class,
            cache_hit: view.cache_hit,
            latency_ms: view.latency_ms,
            outcome_join,
            child_outcomes: Vec::new(),
            child_outcomes_truncated: false,
            outcomes: view
                .outcomes
                .into_iter()
                .map(|outcome| OutcomeSummary {
                    settlement_id: outcome.settlement_id,
                    classification: outcome.classification,
                    completeness: outcome.completeness,
                })
                .collect(),
        }
    }
}

/// `prx causal evaluate`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EvaluateView {
    /// Where the report was written; `None` on a disabled plane.
    pub(crate) report_path: Option<String>,
    pub(crate) report: EvaluationReport,
}

/// The admin service over one configuration.
pub(crate) struct DecisionAdmin {
    config: Config,
    plane: Arc<dyn DecisionPlaneBuilder>,
    events: Arc<dyn CausalEventSink>,
    store: parking_lot::Mutex<Option<Arc<CausalDecisionStore>>>,
}

impl std::fmt::Debug for DecisionAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionAdmin").finish_non_exhaustive()
    }
}

/// Free text for output: URL userinfo redacted.
/// The builtin, which always ends the order.
fn builtin_view() -> BackendView {
    BackendView {
        kind: BackendKind::Builtin.as_str(),
        locality: BackendLocality::Builtin.as_str(),
        provider: None,
        model: crate::decision::builtin::weights::MODEL_VERSION.to_string(),
        circuit: "closed",
        health: "healthy",
        last_failure: None,
    }
}

fn shown(text: &str) -> String {
    crate::providers::redact_endpoint_userinfo(text).into_owned()
}

impl DecisionAdmin {
    /// The service for `config`: `plane` builds its factories, and the
    /// store reports to `events`.
    pub(crate) fn new(config: Config, plane: Arc<dyn DecisionPlaneBuilder>, events: Arc<dyn CausalEventSink>) -> Self {
        Self {
            config,
            plane,
            events,
            store: parking_lot::Mutex::new(None),
        }
    }

    fn key(&self) -> Option<Arc<DecisionKey>> {
        FactoryDeps::load_key(&self.config)
    }

    fn factory(&self, key: Option<Arc<DecisionKey>>) -> Result<DecisionBackendFactory, FactoryBuildError> {
        self.plane.build(&self.config, key)
    }

    /// The workspace's store: read-write while the plane is enabled, when
    /// the first open also imports v1 feedback; read-only otherwise.
    async fn store(&self) -> Result<(Arc<CausalDecisionStore>, Option<ImportSummary>), AdminError> {
        let opened = self.store.lock().clone();
        if let Some(store) = opened {
            return Ok((store, None));
        }
        let section = &self.config.causal_tree;
        let mode = if section.enabled {
            StoreMode::ReadWrite
        } else {
            StoreMode::ReadOnly
        };
        let store = super::store_preparation::open_store(
            self.config.workspace_dir.clone(),
            Retention::days(
                section.store.retention_days,
                self.config.self_system.fitness.retention_days,
            ),
            Arc::clone(&self.events),
            Some(mode),
            |_| {},
        )
        .await?;
        let key = super::store_preparation::load_key(self.config.clone()).await;
        let import = match (store.mode(), key) {
            (StoreMode::ReadWrite, Some(key)) => {
                Some(import_v1_feedback(&store, &self.config.workspace_dir, &key).await?)
            }
            _ => None,
        };
        *self.store.lock() = Some(Arc::clone(&store));
        Ok((store, import))
    }

    /// The scope of `owner` in `store`, or the local operator's; keyed by
    /// the store's stable workspace id (D-091 F-13). A store that does not
    /// exist yet has no id, and its scope matches nothing.
    fn scope(store: &CausalDecisionStore, owner: Option<&str>) -> DecisionScope {
        let workspace_id = store.workspace_id().unwrap_or_else(|| Arc::from(""));
        match owner {
            Some(owner) => DecisionScope::Owner {
                workspace_id,
                owner_id: Arc::from(owner),
            },
            None => DecisionScope::LocalOperator { workspace_id },
        }
    }

    /// The plane's status: the order with health and breakers, exclusions,
    /// the store and the legacy keys of the configuration file.
    pub(crate) async fn status(&self) -> StatusView {
        let key = self.key();
        let key_available = key.is_some();
        let (order, excluded, factory_error) = match self.factory(key.clone()) {
            Ok(factory) => {
                let health = self.plane.check_health(&factory, &self.config, key).await;
                let health_confirmed = matches!(&health, Ok(true));
                let health_error = health.err();
                let status = factory.status();
                let mut order: Vec<BackendView> = status
                    .order
                    .iter()
                    .map(|slot| BackendView {
                        kind: slot.identity.kind.as_str(),
                        locality: slot.locality.as_str(),
                        provider: slot.identity.provider.as_ref().map(|provider| shown(provider.as_str())),
                        model: slot.identity.configured_model.to_string(),
                        circuit: slot.breaker.as_str(),
                        health: match (slot.locality, slot.healthy) {
                            (BackendLocality::Remote, _) => "unprobed",
                            (BackendLocality::Loopback, _) if !health_confirmed => "unprobed",
                            (_, true) => "healthy",
                            (_, false) => "unhealthy",
                        },
                        last_failure: slot.last_failure,
                    })
                    .collect();
                order.push(builtin_view());
                let excluded = status
                    .excluded
                    .iter()
                    .map(|(kind, locality, exclusion)| ExcludedView {
                        kind: kind.as_str(),
                        locality: locality.as_str(),
                        reason: exclusion.as_str(),
                    })
                    .collect();
                (order, excluded, health_error.map(|error| shown(&error.to_string())))
            }
            // A refused backend leaves the builtin, as at startup (D-094
            // C-1).
            Err(error) => (vec![builtin_view()], Vec::new(), Some(shown(&error.to_string()))),
        };
        StatusView {
            enabled: self.config.causal_tree.enabled,
            remote_enabled: self.config.causal_tree.backend.remote_enabled,
            backend_timeout_ms: self.config.causal_tree.backend.timeout_ms,
            latency_budget_warning: self.config.decision_latency_budget_warning(),
            policy: PolicyView::from(&self.config.causal_tree.policy),
            key_available,
            order,
            excluded,
            factory_error,
            store: self.store_view().await,
            legacy_keys: self.legacy_keys(),
        }
    }

    async fn store_view(&self) -> StoreView {
        let path = self
            .config
            .workspace_dir
            .join("self")
            .join("causal_tree")
            .display()
            .to_string();
        let failed = |error: &AdminError| StoreView {
            path: path.clone(),
            mode: "unavailable",
            schema_version: 0,
            decisions: 0,
            legacy_decisions: 0,
            outcomes: 0,
            v1_import: None,
            error: Some(shown(&error.to_string())),
        };
        let (store, import) = match self.store().await {
            Ok(opened) => opened,
            Err(error) => return failed(&error),
        };
        match store.reader().counts() {
            Ok(counts) => StoreView {
                path: store.path().display().to_string(),
                mode: match store.mode() {
                    StoreMode::ReadWrite => "read_write",
                    StoreMode::ReadOnly => "read_only",
                },
                schema_version: counts.schema_version,
                decisions: counts.decisions,
                legacy_decisions: counts.legacy_decisions,
                outcomes: counts.outcomes,
                v1_import: import.map(ImportView::from),
                error: None,
            },
            Err(error) => failed(&AdminError::Store(error)),
        }
    }

    /// The legacy `[causal_tree]` keys of the configuration file, by name.
    fn legacy_keys(&self) -> LegacyKeysView {
        let path = &self.config.config_path;
        if path.as_os_str().is_empty() || !path.is_file() {
            return LegacyKeysView::default();
        }
        let migration = crate::config::files::with_consistent_config_snapshot(path, || {
            let mut root = crate::config::files::read_merged_toml_with_gate_once(path)?;
            let locate = |key: &[&str]| crate::config::deprecations::file_holding(path, key);
            Ok(super::config::migrate_legacy(&mut root, &locate))
        });
        match migration {
            Ok(Ok(report)) => LegacyKeysView {
                moved: report.moved,
                dropped: report.dropped,
                error: None,
            },
            Ok(Err(error)) => LegacyKeysView {
                error: Some(error.to_string()),
                ..LegacyKeysView::default()
            },
            // Parser diagnostics can contain configuration values. Status is
            // read-only and reports no raw TOML or secret-bearing error chain.
            Err(_) => LegacyKeysView {
                error: Some("could not read the merged configuration tree".to_string()),
                ..LegacyKeysView::default()
            },
        }
    }

    /// The computed order with its exclusion reasons.
    /// The provider a live test of the configured backend would reach, URL
    /// userinfo redacted; `None` when the configured backend is not remote
    /// (a live test then sends nothing off this machine).
    pub(crate) fn live_target(&self) -> Option<String> {
        crate::decision::factory::configured_selection(&self.config.causal_tree)
            .filter(|selection| selection.locality == BackendLocality::Remote)
            .map(|selection| shown(&selection.provider))
    }

    pub(crate) fn backends(&self) -> BackendsView {
        let health = HealthView {
            key_unavailable: self.config.causal_tree.enabled && self.key().is_none(),
            ..HealthView::default()
        };
        BackendsView {
            enabled: self.config.causal_tree.enabled,
            order: DecisionBackendFactory::backend_order(&self.config.causal_tree, &health)
                .into_iter()
                .map(|slot| PlannedView {
                    kind: slot.kind.as_str(),
                    locality: slot.locality.as_str(),
                    excluded: slot.exclusion.map(|exclusion| exclusion.as_str()),
                })
                .collect(),
        }
    }

    /// Ask the neutral fixture. Never writes the store or the configuration.
    ///
    /// # Errors
    ///
    /// [`AdminError::Fixture`] when the fixture cannot be sealed.
    pub(crate) async fn test(&self, request: TestRequest) -> Result<TestView, AdminError> {
        let refused = |outcome, note| TestView {
            outcome,
            sent: false,
            served_by: None,
            attempts: Vec::new(),
            answered: Vec::new(),
            note: Some(String::from(note)),
        };
        if !self.config.causal_tree.enabled {
            return Ok(refused(
                "disabled",
                "the decision plane is disabled: the builtin serves every decision and nothing is sent",
            ));
        }
        let Some(key) = self.key() else {
            return Ok(refused(
                "key_unavailable",
                "the workspace decision key is unavailable: only the builtin runs",
            ));
        };
        let factory = match request.target {
            TestTarget::Builtin => Ok(DecisionBackendFactory::builtin_only(
                &self.config,
                Arc::clone(&self.events),
            )),
            TestTarget::Configured => self.factory(Some(Arc::clone(&key))),
        }
        .map_err(|error| AdminError::Fixture(shown(&error.to_string())))?;
        let status = factory.status();
        if request.target == TestTarget::Configured && status.order.is_empty() {
            // The configured backend was excluded: it is not asked, and the
            // builtin answering is not a pass (D-091 F-8).
            let reason = status
                .excluded
                .first()
                .map_or("no decision backend is configured", |(_, _, exclusion)| {
                    exclusion.as_str()
                });
            return Ok(TestView {
                note: Some(format!("the configured backend was not asked: {reason}")),
                ..refused("not_tested", "")
            });
        }
        if request.mode == TestMode::Local && status.order.iter().any(|slot| slot.locality == BackendLocality::Remote) {
            return Ok(refused(
                "requires_live",
                "a remote backend is tested only with a confirmed live call (`--live --yes`); nothing was sent",
            ));
        }
        let model = self.config.causal_tree.backend.model.trim();
        let fixture = neutral_fixture(&key, (!model.is_empty()).then_some(model))?;
        // The fixture is the probe: no separate health check first.
        let lineage = CausalLineage {
            workspace_id: Arc::from(self.config.workspace_dir.to_string_lossy().as_ref()),
            ..CausalLineage::default()
        };
        let result = self
            .plane
            .decide(&factory, &self.config, key, &fixture, &lineage)
            .await?;
        let sent = result.sent;
        let served_by = match result.served_by {
            ServedBy::Primary => "primary",
            ServedBy::Fallback => "fallback",
            ServedBy::Cache => "cache",
            ServedBy::Builtin => "builtin",
        };
        let asked_builtin = request.target == TestTarget::Builtin;
        Ok(TestView {
            outcome: match (asked_builtin, result.served_by) {
                (true, _) | (false, ServedBy::Primary | ServedBy::Fallback) => "passed",
                (false, ServedBy::Builtin | ServedBy::Cache) => "fell_back",
            },
            sent,
            served_by: Some(served_by),
            attempts: result.attempts,
            answered: FIXTURE
                .iter()
                .filter(|(question, _)| result.served.answer(&QuestionId::global(*question)).is_some())
                .map(|(_, primitive)| *primitive)
                .collect(),
            note: None,
        })
    }

    /// The decision `id` in the local operator's scope.
    ///
    /// # Errors
    ///
    /// [`AdminError::Store`] when the store cannot be read.
    pub(crate) async fn inspect(&self, id: &DecisionId) -> Result<Option<InspectView>, AdminError> {
        self.inspect_for(None, id).await
    }

    /// The decision `id` in `owner`'s scope (`None`: the local operator's,
    /// the whole workspace). A decision of another owner reads as absent,
    /// exactly like an unknown id (V6.04).
    ///
    /// # Errors
    ///
    /// [`AdminError::Store`] when the store cannot be read.
    pub(crate) async fn inspect_for(
        &self,
        owner: Option<&str>,
        id: &DecisionId,
    ) -> Result<Option<InspectView>, AdminError> {
        let (store, _) = self.store().await?;
        let scope = Self::scope(&store, owner);
        let Some(decision) = store.reader().get(&scope, id)? else {
            return Ok(None);
        };
        let mut view = InspectView::from(decision);
        let (children, truncated) = store.reader().child_outcomes(&scope, id, 64)?;
        view.child_outcomes = children.into_iter().map(ChildOutcomeSummary::from).collect();
        view.child_outcomes_truncated = truncated;
        if !view.legacy_v1 {
            view.recorded_projection = store
                .reader()
                .inspection_projection(&scope, id)?
                .map(RecordedProjectionView::from);
            view.gate_summary = store
                .reader()
                .inspection_gates(&scope, id)?
                .as_ref()
                .and_then(inspect_gate_summary);
        }
        Ok(Some(view))
    }

    /// The decisions of `owner`'s scope in `window`, newest first, at most
    /// `limit` (the Gateway's list).
    ///
    /// # Errors
    ///
    /// [`AdminError::Store`] when the store cannot be read.
    pub(crate) async fn list(
        &self,
        owner: Option<&str>,
        window: TimeWindow,
        limit: u32,
    ) -> Result<Vec<InspectView>, AdminError> {
        let (store, _) = self.store().await?;
        Ok(store
            .reader()
            .list(&Self::scope(&store, owner), window, limit)?
            .into_iter()
            .map(InspectView::from)
            .collect())
    }

    pub(crate) fn saved_reports(&self, owner: &str) -> Result<super::report_catalog::SavedReports, AdminError> {
        if !self.config.causal_tree.enabled {
            return Ok(super::report_catalog::SavedReports::default());
        }
        let Some(key) = self.key() else {
            return Ok(super::report_catalog::SavedReports::default());
        };
        super::report_catalog::list_owned_reports(&self.config.workspace_dir, owner, &key)
            .map_err(|error| AdminError::Fixture(error.to_string()))
    }

    /// Evaluate `owner`'s decisions (the local operator's when `None`) over
    /// `window`. The report is written only while the plane is enabled; a
    /// disabled plane creates nothing and the report is only returned
    /// (D-091 F-5).
    ///
    /// # Errors
    ///
    /// [`AdminError`] when the store cannot be read or the report written.
    pub(crate) async fn evaluate(&self, owner: Option<&str>, window: TimeWindow) -> Result<EvaluateView, AdminError> {
        self.evaluate_with(owner, window, true).await
    }

    /// [`Self::evaluate`], writing the report file only when `write` (the
    /// Gateway's read endpoint answers with the report and writes nothing).
    ///
    /// # Errors
    ///
    /// As [`Self::evaluate`].
    pub(crate) async fn evaluate_with(
        &self,
        owner: Option<&str>,
        window: TimeWindow,
        write: bool,
    ) -> Result<EvaluateView, AdminError> {
        let (store, _) = self.store().await?;
        let report = evaluate(
            &store.reader(),
            &Self::scope(&store, owner),
            window,
            &EvaluationOptions::now(),
        )?;
        let report_path = if write && self.config.causal_tree.enabled {
            let path: PathBuf = if let Some(owner) = owner {
                let key = self.key().ok_or_else(|| {
                    AdminError::Fixture("a durable decision key is required to bind an owner report".to_owned())
                })?;
                super::report_catalog::write_owned_report(&self.config.workspace_dir, owner, &key, &report)?
            } else {
                write_report(&self.config.workspace_dir, &report)?
            };
            Some(path.display().to_string())
        } else {
            None
        };
        Ok(EvaluateView { report_path, report })
    }

    /// Drain the store's queue.
    pub(crate) async fn close(&self) {
        let store = self.store.lock().take();
        if let Some(store) = store {
            CausalDecisionStore::close(store, Instant::now() + CLOSE_DEADLINE).await;
        }
    }
}

/// The fixture's questions: one of each primitive.
const FIXTURE: [(GlobalQuestion, &str); 3] = [
    (GlobalQuestion::NextAction, "choice"),
    (GlobalQuestion::EvidenceSufficient, "noul"),
    (GlobalQuestion::SemanticRisk, "score"),
];

/// The neutral test request for the configured `model`: no user state,
/// one question per primitive.
fn neutral_fixture(key: &DecisionKey, model: Option<&str>) -> Result<DecisionRequest, AdminError> {
    let fixture = |error: crate::decision::DecisionError| AdminError::Fixture(error.class().to_string());
    let text = |value: &str| StructuredValue::try_from(serde_json::Value::from(value)).map_err(fixture);
    let mut questions = std::collections::BTreeMap::new();
    questions.insert(
        QuestionId::global(GlobalQuestion::NextAction),
        DecisionQuestion::Choice {
            instructions: text("Test fixture: which action fits a greeting?")?,
            criteria: std::collections::BTreeMap::from([
                (OptionId::code("answer"), text("Answer directly.")?),
                (OptionId::code("retrieve"), text("Retrieve first.")?),
            ]),
        },
    );
    questions.insert(
        QuestionId::global(GlobalQuestion::EvidenceSufficient),
        DecisionQuestion::Noul {
            instructions: text("Test fixture: is the evidence sufficient?")?,
            criteria: None,
        },
    );
    questions.insert(
        QuestionId::global(GlobalQuestion::SemanticRisk),
        DecisionQuestion::Score {
            instructions: text("Test fixture: how risky is a greeting?")?,
            levels: (0..5)
                .map(|level| text(&format!("level {level}")))
                .collect::<Result<Vec<_>, _>>()?,
        },
    );
    DecisionRequest::seal(
        RequestParts {
            state: StructuredValue::try_from(serde_json::json!({"test_fixture": "neutral"})).map_err(fixture)?,
            questions,
            model: model.map(str::to_string),
            state_fingerprint: "test-fixture".to_string(),
            candidate_set_hash: "test-fixture".to_string(),
            policy_version: Arc::from("test-fixture"),
            catalog_generation: "test-fixture".to_string(),
            omitted_questions: Vec::new(),
            approval_pending: false,
        },
        key,
        Instant::now() + TEST_DEADLINE,
        &RequestLimits::default(),
    )
    .map_err(fixture)
}

/// The kind of a test target named on the command line.
pub(crate) const fn test_target(kind: Option<BackendKind>) -> TestTarget {
    match kind {
        Some(BackendKind::Builtin) => TestTarget::Builtin,
        _ => TestTarget::Configured,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::decision::backend::DecisionBackend;
    use crate::decision::factory::SlotLimits;
    use crate::decision::outbound::{GuardedBody, OutboundSecrets};
    use crate::decision::telemetry::testing::RecordingSink;
    use crate::decision::validate::ModelVersionPolicy;
    use crate::decision::{BackendIdentity, DecisionError, DecisionResponse};

    /// A remote backend that counts every call it gets.
    /// A counting backend at `locality`; its health probe fails unless it
    /// is `healthy`.
    struct Remote {
        identity: BackendIdentity,
        locality: BackendLocality,
        healthy: bool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DecisionBackend for Remote {
        fn identity(&self) -> &BackendIdentity {
            &self.identity
        }

        fn locality(&self) -> BackendLocality {
            self.locality
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
            if self.healthy {
                Ok(())
            } else {
                Err(DecisionError::Unavailable)
            }
        }
    }

    /// A plane whose configured backend is the counting one.
    struct RemotePlane {
        calls: Arc<AtomicUsize>,
        locality: BackendLocality,
        provider: Option<&'static str>,
        healthy: bool,
    }

    impl DecisionPlaneBuilder for RemotePlane {
        fn build(
            &self,
            config: &Config,
            key: Option<Arc<DecisionKey>>,
        ) -> Result<DecisionBackendFactory, FactoryBuildError> {
            // Without a key (a disabled plane) only the builtin runs.
            let Some(key) = key else {
                return Ok(DecisionBackendFactory::builtin_only(
                    config,
                    Arc::new(RecordingSink::default()),
                ));
            };
            let remote = Remote {
                identity: BackendIdentity {
                    kind: BackendKind::CompatibleHttp,
                    provider: self.provider.map(crate::providers::ProviderId::of),
                    configured_model: Arc::from("decider-1"),
                },
                locality: self.locality,
                healthy: self.healthy,
                calls: Arc::clone(&self.calls),
            };
            Ok(DecisionBackendFactory::from_slots(
                vec![Box::new(remote)],
                key,
                OutboundSecrets::default(),
                SlotLimits::default(),
            ))
        }
    }

    fn admin(enabled: bool, workspace: &std::path::Path, calls: &Arc<AtomicUsize>) -> DecisionAdmin {
        admin_over(
            enabled,
            workspace,
            RemotePlane {
                calls: Arc::clone(calls),
                locality: BackendLocality::Remote,
                provider: None,
                healthy: true,
            },
        )
    }

    fn admin_over(enabled: bool, workspace: &std::path::Path, plane: RemotePlane) -> DecisionAdmin {
        let mut config = Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.causal_tree.enabled = enabled;
        DecisionAdmin::new(config, Arc::new(plane), Arc::new(RecordingSink::default()))
    }

    /// A plane whose configured backend is excluded: remote without
    /// `remote_enabled`.
    struct ExcludedPlane;

    impl DecisionPlaneBuilder for ExcludedPlane {
        fn build(
            &self,
            config: &Config,
            key: Option<Arc<DecisionKey>>,
        ) -> Result<DecisionBackendFactory, FactoryBuildError> {
            let mut config = config.clone();
            config.causal_tree.backend.kind =
                crate::causal_tree::config::BackendSelection::Fixed(BackendKind::CompatibleHttp);
            config.causal_tree.backend.endpoint = "https://decisions.example.com/v1".to_string();
            config.causal_tree.backend.model = "decider-1".to_string();
            config.causal_tree.backend.credential_ref = crate::causal_tree::config::CredentialRefSetting::None;
            let catalog = crate::providers::catalog::CatalogView::from_config(&config).with_curated([
                crate::providers::catalog::CuratedModel {
                    operations: Some(vec![crate::providers::catalog::ModelOperation::DecisionEndpoint]),
                    ..crate::providers::catalog::CuratedModel::new(
                        crate::providers::ProviderId::of("custom:https://decisions.example.com/v1"),
                        Some("decider-1"),
                    )
                },
            ]);
            DecisionBackendFactory::from_config(
                &config,
                FactoryDeps {
                    catalog: &catalog,
                    key,
                    transports: &crate::decision::compatible_http::HttpDecisionTransports,
                    config_generation: 0,
                    events: Arc::new(RecordingSink::default()),
                },
            )
        }
    }

    /// D-091 F-8: a configured backend that was excluded is `not_tested`,
    /// with the reason; the builtin answering is not a pass.
    #[tokio::test]
    async fn an_excluded_configured_backend_is_not_tested() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.causal_tree.enabled = true;
        let admin = DecisionAdmin::new(config, Arc::new(ExcludedPlane), Arc::new(RecordingSink::default()));
        let view = admin
            .test(TestRequest {
                target: TestTarget::Configured,
                mode: TestMode::Local,
            })
            .await
            .unwrap();
        assert_eq!(view.outcome, "not_tested", "{view:?}");
        assert!(!view.sent);
        assert!(
            view.note
                .as_deref()
                .is_some_and(|note| note.contains("remote_disabled")),
            "{view:?}"
        );
    }

    /// A plane whose configured backend is refused at build.
    struct RefusedPlane;

    impl DecisionPlaneBuilder for RefusedPlane {
        fn build(&self, _: &Config, _: Option<Arc<DecisionKey>>) -> Result<DecisionBackendFactory, FactoryBuildError> {
            Err(FactoryBuildError::Transport(DecisionError::UnsupportedOperation))
        }
    }

    #[tokio::test]
    async fn status_exposes_the_hosted_latency_warning_without_changing_the_ceiling() {
        let temp = tempfile::TempDir::new().expect("test: private status workspace");
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_owned();
        config.causal_tree.backend.kind = crate::causal_tree::config::BackendSelection::Fixed(BackendKind::OpenRouter);
        config.causal_tree.backend.model = "typesafe/jev-1.13".to_owned();
        let admin = DecisionAdmin::new(config, Arc::new(RefusedPlane), Arc::new(RecordingSink::default()));
        let status = admin.status().await;
        assert_eq!(status.policy.extra_latency_budget_ms, 500);
        let warning = status.latency_budget_warning.as_deref().expect("test: status warning");
        assert!(warning.contains("500") && warning.contains("3000"), "{warning}");
        let encoded = serde_json::to_value(&status).expect("test: JSON status");
        assert_eq!(encoded["latency_budget_warning"].as_str(), Some(warning));
        admin.close().await;
    }

    /// D-094 C-1: a refused backend leaves the builtin order, next to the
    /// reason, as at startup.
    #[tokio::test]
    async fn a_refused_backend_shows_the_builtin_order_and_the_reason() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.causal_tree.enabled = true;
        let admin = DecisionAdmin::new(config, Arc::new(RefusedPlane), Arc::new(RecordingSink::default()));
        let status = admin.status().await;
        assert_eq!(
            status.order.iter().map(|slot| slot.kind).collect::<Vec<_>>(),
            [BackendKind::Builtin.as_str()]
        );
        assert!(status.factory_error.is_some());
    }

    /// D-091 F-9: a remote backend is `unprobed`; a loopback one whose probe
    /// fails is `unhealthy` with the failure's class; provider labels never
    /// show URL userinfo.
    #[tokio::test]
    async fn status_shows_probe_state_and_redacted_labels() {
        let temp = tempfile::TempDir::new().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let remote = admin_over(
            true,
            temp.path(),
            RemotePlane {
                calls: Arc::clone(&calls),
                locality: BackendLocality::Remote,
                provider: Some("custom:https://user:STATUS-USERINFO-SENTINEL@decisions.example.com/v1"),
                healthy: true,
            },
        );
        let status = remote.status().await;
        assert_eq!(status.order[0].health, "unprobed");
        let rendered = serde_json::to_string(&status).unwrap();
        assert!(!rendered.contains("STATUS-USERINFO-SENTINEL"), "{rendered}");
        let loopback = admin_over(
            true,
            temp.path(),
            RemotePlane {
                calls,
                locality: BackendLocality::Loopback,
                provider: None,
                healthy: false,
            },
        );
        let status = loopback.status().await;
        assert_eq!(status.order[0].health, "unhealthy");
        assert_eq!(status.order[0].last_failure, Some("unavailable"));
    }

    /// D-091 F-5: evaluate on a disabled plane writes nothing.
    #[tokio::test]
    async fn evaluate_on_a_disabled_plane_writes_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let view = admin(false, temp.path(), &calls)
            .evaluate(
                None,
                TimeWindow {
                    from_ms: 0,
                    to_ms: i64::MAX,
                },
            )
            .await
            .unwrap();
        assert!(view.report_path.is_none());
        assert!(!temp.path().join("self").exists(), "no file or directory was created");
    }

    /// The neutral fixture asks one question of each primitive, as the
    /// labels say, about a state that holds no user text.
    #[test]
    fn the_neutral_fixture_asks_one_question_of_each_primitive() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        let fixture = neutral_fixture(&key, Some("decider-1")).unwrap();
        let questions = fixture.questions();
        assert_eq!(questions.len(), FIXTURE.len());
        for (question, label) in FIXTURE {
            let asked = questions.get(&QuestionId::global(question)).expect("test: asked");
            assert_eq!(asked.primitive().as_str(), label, "{question:?}");
        }
        let mut primitives: Vec<_> = FIXTURE.iter().map(|(_, label)| *label).collect();
        primitives.sort_unstable();
        primitives.dedup();
        assert_eq!(primitives, ["choice", "noul", "score"]);
        assert_eq!(fixture.model(), Some("decider-1"));
    }

    /// A remote backend is tested only live (CDP-14): a local test sends
    /// nothing.
    #[tokio::test]
    async fn a_remote_backend_is_not_tested_without_live() {
        let temp = tempfile::TempDir::new().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let view = admin(true, temp.path(), &calls)
            .test(TestRequest {
                target: TestTarget::Configured,
                mode: TestMode::Local,
            })
            .await
            .unwrap();
        assert_eq!(view.outcome, "requires_live");
        assert!(!view.sent);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing reached the remote backend");
    }

    /// PMM-D-7: a confirmed live test asks the remote backend exactly once
    /// and never records anything; the target it names is redacted.
    #[tokio::test]
    async fn a_live_test_asks_the_remote_backend_once() {
        let temp = tempfile::TempDir::new().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        // A remote backend is asked with its configured model.
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.causal_tree.enabled = true;
        config.causal_tree.backend.model = "decider-1".to_string();
        let admin = DecisionAdmin::new(
            config,
            Arc::new(RemotePlane {
                calls: Arc::clone(&calls),
                locality: BackendLocality::Remote,
                provider: None,
                healthy: true,
            }),
            Arc::new(RecordingSink::default()),
        );
        let view = admin
            .test(TestRequest {
                target: TestTarget::Configured,
                mode: TestMode::Live,
            })
            .await
            .unwrap();
        assert!(view.sent, "{view:?}");
        assert_ne!(view.outcome, "requires_live");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one live call");
        assert!(
            !temp.path().join("self/causal_tree/decisions.db").exists(),
            "a test writes no decision"
        );
    }

    /// A disabled plane tests nothing, creates no key and opens its store
    /// read-only.
    #[tokio::test]
    async fn a_disabled_plane_tests_nothing_and_writes_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let admin = admin(false, temp.path(), &calls);
        let view = admin
            .test(TestRequest {
                target: TestTarget::Configured,
                mode: TestMode::Local,
            })
            .await
            .unwrap();
        assert_eq!(view.outcome, "disabled");
        let status = admin.status().await;
        assert_eq!(status.store.mode, "read_only");
        assert!(!temp.path().join("self").exists(), "no key, no store");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn status_projects_the_current_store_schema_version() {
        let temp = tempfile::TempDir::new().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let admin = admin(true, temp.path(), &calls);
        let status = admin.status().await;
        admin.close().await;
        assert_eq!(status.store.mode, "read_write");
        assert!(status.store.error.is_none(), "{status:?}");
        assert_eq!(status.store.schema_version, super::super::store_schema::SCHEMA_VERSION);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "status does not evaluate a backend");
    }

    /// R-I1-phase2 R-1: closing an enabled admin checkpoints and closes the
    /// store's writer, so a CLI run leaves no pending WAL behind.
    #[tokio::test]
    async fn closing_an_enabled_admin_leaves_no_pending_wal() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("self").join("causal_tree");
        std::fs::create_dir_all(dir.join("feedback")).unwrap();
        let line = serde_json::json!({
            "timestamp": "2026-09-01T10:00:00Z",
            "session_id": "s-1",
            "request_id": "r-1",
            "goal": "close test",
            "user_intent": "simple",
            "chosen_branch_id": "b-1",
            "fallback_branch_ids": [],
            "branches": [{"branch_id": "b-1", "label": "RetrieveThenAnswer"}]
        });
        std::fs::write(dir.join("feedback").join("2026-09-01.jsonl"), format!("{line}\n")).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let admin = admin(true, temp.path(), &calls);
        let status = admin.status().await;
        assert_eq!(
            status.store.v1_import.map(|import| import.imported),
            Some(1),
            "{status:?}"
        );
        admin.close().await;
        for side in ["decisions.db-wal", "decisions.db-shm"] {
            assert!(!dir.join(side).exists(), "{side} is left after close");
        }
        let conn = rusqlite::Connection::open(dir.join("decisions.db")).unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM decisions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "the row is in the database file");
    }
}
