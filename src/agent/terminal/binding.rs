//! Execution-owned terminal binding of one model turn (R29).
//!
//! [`TerminalBinding`] carries the non-decision half of the former settlement
//! link: the pinned turn runtime context, the source channel captured at
//! opening, the operator parent, the workspace and
//! cost configuration of the pinned generation, and the turn's memory. Terminal
//! scope checks compare against the trusted parts pinned in
//! [`TurnRuntimeContext`] (R22), never against a decision hop.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::schema::CostConfig;
use crate::memory::{Memory, MemoryFabric, MessageEventScope};
use crate::runtime::turn_context::TurnRuntimeContext;
use crate::runtime::turn_terminal::settlement::SettlementIssueToken;

use super::TurnTerminalStatus;

#[derive(Clone)]
pub struct TerminalBinding {
    runtime: Arc<TurnRuntimeContext>,
    source_channel: Arc<str>,
    pub(super) operator_parent: Option<Arc<crate::runtime::child_source::parent::ParentSource>>,
    pub(super) workspace_dir: PathBuf,
    cost_config: CostConfig,
    pub(super) memory: Arc<dyn Memory>,
}

impl std::fmt::Debug for TerminalBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalBinding")
            .field("decision_id", &self.runtime.decision_id)
            .finish_non_exhaustive()
    }
}

impl TerminalBinding {
    /// Issued only by the runtime's settlement owner for a turn it opened.
    pub(crate) fn issued(
        runtime: Arc<TurnRuntimeContext>,
        source_channel: Arc<str>,
        workspace_dir: PathBuf,
        cost_config: CostConfig,
        memory: Arc<dyn Memory>,
        _token: SettlementIssueToken,
    ) -> Self {
        Self {
            runtime,
            source_channel,
            operator_parent: None,
            workspace_dir,
            cost_config,
            memory,
        }
    }

    pub(crate) fn with_operator_parent(
        mut self,
        parent: Option<Arc<crate::runtime::child_source::parent::ParentSource>>,
    ) -> Self {
        self.operator_parent = parent;
        self
    }

    /// The pinned runtime context of the bound turn.
    pub(crate) const fn runtime(&self) -> &Arc<TurnRuntimeContext> {
        &self.runtime
    }

    /// A terminal guard may reuse only its own binding, never another turn's
    /// binding with matching scope strings.
    pub(crate) fn same_turn(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.runtime, &other.runtime)
    }

    pub(super) fn validate_terminal_scope(
        &self,
        fabric: &MemoryFabric,
        scope: &MessageEventScope,
        history_scope: Option<&MessageEventScope>,
        workspace_dir: &Path,
        cost_config: &CostConfig,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.memory, &fabric.memory()),
            "decision terminal memory does not match the pinned service"
        );
        // The pinned trusted scope captured at opening (R22), not a decision hop.
        let trusted = &self.runtime.trusted;
        anyhow::ensure!(
            workspace_dir == self.workspace_dir && cost_config == &self.cost_config,
            "decision settlement workspace or cost configuration does not match the pinned turn"
        );
        anyhow::ensure!(
            trusted.workspace_id.as_ref() == fabric.workspace_id(),
            "decision terminal workspace does not match the pinned turn"
        );
        // Both writes retain the recorded ingress identity and visibility. Sender
        // and recipient are presentation labels and may differ for history.
        for write_scope in [scope, history_scope.unwrap_or(scope)] {
            anyhow::ensure!(
                crate::runtime::turn_source::ingress_event_surface(
                    &write_scope.source,
                    write_scope.channel.as_deref(),
                    &write_scope.visibility
                ) == Some(trusted.surface)
                    && write_scope.channel.as_deref() == Some(self.source_channel.as_ref())
                    && write_scope.owner_id.as_deref() == trusted.owner_id.as_deref()
                    && write_scope.session_key.as_deref() == trusted.session_key.as_deref()
                    && write_scope.run_id.as_deref() == trusted.run_id.as_deref()
                    && crate::runtime::turn_source::subject_task_id(write_scope.subject.as_ref())
                        == trusted.task_id.as_deref(),
                "decision terminal or history scope does not match the pinned turn"
            );
        }
        Ok(())
    }
}

/// A silent terminal needs an actual execution trace; any other status is
/// checked against the trace when one exists.
pub(crate) fn validate_silent_terminal(
    trace: Option<&crate::tools::capability::execution_trace::ExecutionTrace>,
    status: TurnTerminalStatus,
    outcome: Option<&crate::llm::route_decision::ProviderExecutionOutcome>,
) -> anyhow::Result<()> {
    if let Some(trace) = trace {
        trace.validate_terminal_status(status, outcome)
    } else {
        anyhow::ensure!(
            status != TurnTerminalStatus::Silent,
            "silent terminal has no actual execution trace"
        );
        Ok(())
    }
}
