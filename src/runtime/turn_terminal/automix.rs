//! A settled main response can fund one premium continuation, not reopen tools.
use super::TurnTerminalGuard;
use crate::llm::route_decision::{AttemptStatus, ExecutionStatus, ProviderExecutionOutcome};
use crate::runtime::turn_decision::{TurnDecisionContext, TurnDecisionService};
use crate::runtime::turn_source::TurnSourceProof;
use std::sync::Arc;

/// No public constructor, Clone, serde or transferable execution authority.
pub(crate) struct SettledAutomixContinuation {
    service: Arc<TurnDecisionService>,
    context: Arc<TurnDecisionContext>,
    envelope: crate::runtime::envelope::RuntimeEnvelope,
    cancellation: tokio_util::sync::CancellationToken,
}

impl SettledAutomixContinuation {
    pub(crate) fn verify(
        &self,
        service: &TurnDecisionService,
        source: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.cancellation.is_cancelled()
                && std::ptr::eq(self.service.as_ref(), service)
                && std::ptr::eq(self.context.as_ref(), context)
                && self.envelope.workspace_id == source.workspace_id()
                && self.envelope.resolved_owner_id() == source.owner_id()
                && self.envelope.session_key == source.session_key()
                && self.envelope.run_id.as_deref() == Some(source.run_id())
                && self.envelope.source_message_event_id.as_deref() == source.source_event_id()
                && self.envelope.config_generation_id == Some(source.config_generation_id())
                && self.envelope.config_source_revision.as_deref() == Some(source.config_source_revision()),
            "Automix continuation differs from settled main source"
        );
        Ok(())
    }
}

impl TurnTerminalGuard {
    pub(super) fn claim_automix_continuation(&self, outcome: &ProviderExecutionOutcome) -> anyhow::Result<bool> {
        use crate::causal_tree::branch::CausalActionKind;
        if !matches!(
            self.context.terminal_outcome_action(),
            Some(CausalActionKind::Answer | CausalActionKind::UseTool)
        ) || !matches!(
            self.context.disposition(),
            Err(crate::causal_tree::directive::DirectiveError::NotAdmitted)
        ) || !matches!(
            outcome.status,
            ExecutionStatus::Success | ExecutionStatus::FallbackSuccess
        ) {
            return Ok(false);
        }
        let Some((retained, route)) = &self.provider_trace else {
            return Ok(false);
        };
        let (actual, unknown) = retained.snapshot();
        let Some(actual) = actual.filter(|_| !unknown) else {
            return Ok(false);
        };
        let expected = crate::agent::terminal::provider_outcome_from_trace(route, outcome.started_at, actual);
        anyhow::ensure!(
            expected.decision_id == outcome.decision_id
                && expected.attempts == outcome.attempts
                && expected.final_provider == outcome.final_provider
                && expected.final_model == outcome.final_model
                && expected.status == outcome.status
                && expected.tokens_used == outcome.tokens_used,
            "Automix main outcome differs from retained provider trace"
        );
        if !outcome
            .attempts
            .iter()
            .rev()
            .find(|attempt| attempt.status != AttemptStatus::Skipped)
            .is_some_and(|attempt| {
                attempt.status == AttemptStatus::Success
                    && attempt.provider == outcome.final_provider
                    && attempt.model == outcome.final_model
            })
        {
            return Ok(false);
        }
        let execution = self.context.terminal_execution_trace();
        if execution.authorized_silent
            || execution.awaiting_approval_attempts != 0
            || execution.pending_attempts != 0
            || execution.provider_pending != 0
            || execution.incomplete
            || execution.provider_attempts_incomplete
        {
            return Ok(false);
        }
        let mut state = self.state.lock();
        if state.finished || state.pending.is_some() || state.automix_claimed || !self.auxiliary_calls.is_empty() {
            return Ok(false);
        }
        state.automix_claimed = true;
        Ok(true)
    }

    /// Called only after the owned main usage journal returned a real confirmed
    /// settlement. The one-way claim remains set on error, timeout or Drop.
    pub(super) fn settled_automix_continuation(
        &self,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> SettledAutomixContinuation {
        SettledAutomixContinuation {
            service: Arc::clone(&self.service),
            context: Arc::clone(&self.context),
            envelope: self.envelope.clone(),
            cancellation,
        }
    }
}
