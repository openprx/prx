//! Decision-plane telemetry (CDP 18; CDP-PLAN 1.6; SP-CDP-A 5.11).
//!
//! A [`CausalEvent`] is a kind, the lineage of the decision it belongs to,
//! and bounded fields only: enums, stable class names and numbers. It never
//! carries state, question text, a request or response body, a URL or a
//! model string a backend claimed (invariant 16.10).
//!
//! - The lineage goes to logs and OpenTelemetry attributes, never to a
//!   metric label.
//! - Metric labels come from `observability::labels` (the configured-model
//!   allowlist for models, D-059) and the enums here (AG-11).
//!
//! The factory emits the prediction and backend kinds (CDP-05); the store's
//! queue emits `causal.outcome.incomplete` when it drops a record (CDP-09,
//! D-040 OQ-A8). Each later stream adds its own kinds.

use std::sync::Arc;

use super::protocol::{BackendKind, BackendLocality};
use crate::observability::{Observer, ObserverEvent};

/// What happened (the CDP 18 names this stream owns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CausalEventKind {
    /// A source-owned turn snapshot was built for one decision hop.
    SnapshotCreated,
    /// The pinned catalog produced a candidate set.
    CandidatesGenerated,
    /// The final gate pass completed for a candidate set.
    CandidateGated,
    /// Candidate scores were computed for a decision hop.
    CandidateScored,
    /// The decision row was inserted and the writer transaction committed.
    DecisionCommitted,
    /// A new terminal outcome row was inserted after its decision row.
    OutcomeRecorded,
    /// A verified fixed terminal completed and its application was persisted.
    DirectiveApplied,
    /// A non-builtin backend is asked to decide.
    PredictionStarted,
    /// It answered with a valid decision (or the cache did).
    PredictionSucceeded,
    /// It failed, or was not asked (open breaker, refused body, deadline).
    PredictionFailed,
    /// The decision moved from one backend to the next, or to the builtin.
    BackendFallback,
    /// A backend's breaker changed state.
    BackendCircuitOpened,
    /// A decision or outcome record was not stored.
    OutcomeIncomplete,
}

impl CausalEventKind {
    /// The dotted event name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SnapshotCreated => "causal.snapshot.created",
            Self::CandidatesGenerated => "causal.candidates.generated",
            Self::CandidateGated => "causal.candidate.gated",
            Self::CandidateScored => "causal.candidate.scored",
            Self::DecisionCommitted => "causal.decision.committed",
            Self::OutcomeRecorded => "causal.outcome.recorded",
            Self::DirectiveApplied => "causal.directive.applied",
            Self::PredictionStarted => "causal.prediction.started",
            Self::PredictionSucceeded => "causal.prediction.succeeded",
            Self::PredictionFailed => "causal.prediction.failed",
            Self::BackendFallback => "causal.backend.fallback",
            Self::BackendCircuitOpened => "causal.backend.circuit_opened",
            Self::OutcomeIncomplete => "causal.outcome.incomplete",
        }
    }
}

/// Where a causal event belongs: identifiers only, for logs and trace
/// attributes (never metric labels).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CausalLineage {
    pub workspace_id: Arc<str>,
    pub owner_id: Option<Arc<str>>,
    pub session_key: Option<Arc<str>>,
    pub run_id: Option<Arc<str>>,
    pub task_id: Option<Arc<str>>,
    pub source_event_id: Option<Arc<str>>,
    pub causation_event_id: Option<Arc<str>>,
    /// The decision request's id.
    pub decision_id: Option<Arc<str>>,
}

/// A breaker state as a bounded label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CircuitState {
    /// Open until a cooldown passes.
    Open,
    /// Open until the next configuration generation.
    OpenForGeneration,
    /// Closed after a probe succeeded.
    Closed,
}

impl CircuitState {
    /// The label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::OpenForGeneration => "open_for_generation",
            Self::Closed => "closed",
        }
    }
}

/// Who served a decision, as a bounded label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServedLabel {
    Primary,
    Fallback,
    Cache,
    Builtin,
}

impl ServedLabel {
    /// The label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Fallback => "fallback",
            Self::Cache => "cache",
            Self::Builtin => "builtin",
        }
    }
}

/// Why a record is incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IncompleteReason {
    /// The store's bounded queue was full and dropped the record.
    StoreDropped,
    /// The store's database was corrupt; it was moved aside and a fresh one
    /// created (CDP-09).
    StoreRecovered,
    /// A different record arrived under a recorded id and was refused.
    Conflict,
    /// A terminal settlement named a decision that is not in the store.
    UnknownDecision,
    /// The store could not write the record.
    StoreFailed,
}

impl IncompleteReason {
    /// The label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StoreDropped => "store_dropped",
            Self::StoreRecovered => "store_recovered",
            Self::Conflict => "conflict",
            Self::UnknownDecision => "unknown_decision",
            Self::StoreFailed => "store_failed",
        }
    }
}

/// The bounded fields of a causal event. Every field is an enum, a stable
/// class name or a number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CausalEventFields {
    /// The bounded number of candidates, gate outcomes, or scores for a phase.
    pub item_count: Option<u16>,
    /// The backend kind the event is about.
    pub backend: Option<BackendKind>,
    /// Where it runs.
    pub locality: Option<BackendLocality>,
    /// The configured model of the backend (a label from the allowlist is
    /// derived from it; a claimed model is never recorded).
    pub configured_model: Option<Arc<str>>,
    /// The error class (`DecisionError::class`).
    pub error_class: Option<&'static str>,
    /// The error rule, where the class has one.
    pub error_rule: Option<&'static str>,
    /// For a fallback: the backend kind the decision moved to.
    pub to_backend: Option<BackendKind>,
    /// For a breaker event: its new state.
    pub circuit: Option<CircuitState>,
    /// For a success: who served it.
    pub served: Option<ServedLabel>,
    /// For an incomplete record: why.
    pub incomplete: Option<IncompleteReason>,
    /// The attempt's latency.
    pub latency_ms: Option<u64>,
}

/// One causal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CausalEvent {
    pub kind: CausalEventKind,
    pub lineage: CausalLineage,
    pub fields: CausalEventFields,
}

/// Where causal events go.
pub trait CausalEventSink: Send + Sync {
    /// Emit `event`. Never blocks and never fails the decision.
    fn emit(&self, event: CausalEvent);
}

/// The sink that forwards causal events to the runtime observer as
/// [`ObserverEvent::Causal`]: logs and trace attributes with the lineage,
/// metrics with bounded labels.
pub struct ObserverCausalSink {
    observer: Arc<dyn Observer>,
}

impl ObserverCausalSink {
    /// A sink over `observer`.
    #[must_use]
    pub fn new(observer: Arc<dyn Observer>) -> Self {
        Self { observer }
    }
}

impl std::fmt::Debug for ObserverCausalSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObserverCausalSink")
            .field("observer", &self.observer.name())
            .finish()
    }
}

impl CausalEventSink for ObserverCausalSink {
    fn emit(&self, event: CausalEvent) {
        self.observer.record_event(&ObserverEvent::Causal(Box::new(event)));
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use parking_lot::Mutex;

    use super::{CausalEvent, CausalEventSink};

    /// A sink that keeps every event, for tests.
    #[derive(Debug, Default)]
    pub(crate) struct RecordingSink {
        pub(crate) events: Mutex<Vec<CausalEvent>>,
    }

    impl CausalEventSink for RecordingSink {
        fn emit(&self, event: CausalEvent) {
            self.events.lock().push(event);
        }
    }
}
