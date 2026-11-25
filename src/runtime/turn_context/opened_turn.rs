//! The per-turn handle of one opened turn.
//!
//! Only the turn opening constructs it. It wraps the execution-owned
//! [`TurnRuntimeContext`] together with the facts the opening bound to the
//! source: the owner filesystem, the policy actor, the service identity, the
//! main compaction and the turn deadline.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use super::TurnRuntimeContext;
use crate::security::decision_id::DecisionId;
use crate::tools::capability::contract::{CapabilityRef, CatalogGeneration};

pub(crate) struct OpenedTurn {
    pub(in crate::runtime) owner_filesystem: Option<Arc<crate::tools::owner_filesystem::OwnerFilesystem>>,
    /// The execution-owned runtime context of this turn (R60).
    pub(in crate::runtime) runtime: Arc<TurnRuntimeContext>,
    pub(in crate::runtime) source_actor: (Arc<str>, Arc<str>),
    pub(in crate::runtime) service_id: uuid::Uuid,
    pub(in crate::runtime) main_compaction: crate::config::AgentCompactionConfig,
    /// Budgeted tokens the published tool definitions occupy in a request,
    /// measured at opening, so a budget check made before the first request
    /// has been measured reserves them too.
    pub(in crate::runtime) tool_surface_tokens: usize,
    /// The logical deadline of this turn, fixed at opening from its budget.
    pub(in crate::runtime) deadline: Instant,
    /// Set once the turn has logged that the live registry drifted from its
    /// pinned catalog, so the line appears once per turn.
    pub(in crate::runtime) registry_drift_noted: std::sync::atomic::AtomicBool,
}

impl OpenedTurn {
    /// Record that the registry drift was logged; returns whether it already was.
    pub(in crate::runtime) fn note_registry_drift(&self) -> bool {
        self.registry_drift_noted
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    pub(crate) fn source_channel(&self) -> &str {
        &self.source_actor.0
    }

    pub(crate) fn source_actor_matches(&self, channel: Option<&str>, sender: Option<&str>) -> bool {
        channel == Some(self.source_actor.0.as_ref()) && sender == Some(self.source_actor.1.as_ref())
    }

    /// Delegates to the R25 owner on the turn runtime context.
    pub(crate) fn terminal_execution_trace(&self) -> crate::tools::capability::execution_trace::ExecutionTraceSnapshot {
        self.runtime.terminal_execution_trace()
    }

    pub(crate) const fn main_compaction(&self) -> &crate::config::AgentCompactionConfig {
        &self.main_compaction
    }

    /// Tokens the tool definitions of this turn are estimated to occupy.
    pub(crate) const fn tool_surface_tokens(&self) -> usize {
        self.tool_surface_tokens
    }

    pub(crate) fn decision_id(&self) -> DecisionId {
        self.runtime.decision_id
    }

    pub(crate) fn catalog_generation(&self) -> CatalogGeneration {
        self.runtime.catalog_generation
    }

    pub(crate) fn execution_refs(&self) -> &BTreeSet<CapabilityRef> {
        &self.runtime.published
    }

    pub(crate) fn is_run(&self) -> bool {
        crate::runtime::decision_accounting::is_run(self)
    }

    /// The execution-owned runtime context of this turn.
    pub(crate) const fn runtime(&self) -> &Arc<TurnRuntimeContext> {
        &self.runtime
    }

    /// The logical deadline of this turn.
    pub(crate) const fn deadline(&self) -> Instant {
        self.deadline
    }
}
