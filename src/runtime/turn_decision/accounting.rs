//! Bounded admission resources and deferred physical-send accounting.
use super::{TurnBudgets, TurnDecisionService, TurnSourceProof, validate_agent_turn_origin};
use crate::decision::budget::{self, AttemptBinding, DecisionAttemptAccounting, DecisionAttemptReceipt};
use crate::decision::{DecisionError, KeyDomain};
use crate::runtime::causal_events::PersistenceGuard;
use crate::runtime::decision_accounting::AccountingScope;
use async_trait::async_trait;
use std::{
    sync::{Arc, OnceLock},
    time::Instant,
};

pub(super) struct TurnAccounting<'a> {
    service: &'a TurnDecisionService,
    proof: &'a dyn TurnSourceProof,
    budgets: TurnBudgets,
    deadline: Instant,
    scope: OnceLock<Option<Arc<AccountingScope>>>,
}

/// The telemetry guard belongs to admission, independently of the physical
/// accounting scope that an issued context can retain after admission ends.
pub(super) fn admission_resources<'a>(
    service: &'a TurnDecisionService,
    proof: &'a dyn TurnSourceProof,
    budgets: TurnBudgets,
    deadline: Instant,
) -> (TurnAccounting<'a>, Option<PersistenceGuard>) {
    (
        TurnAccounting::new(service, proof, budgets, deadline),
        service
            .durable_events
            .as_ref()
            .map(|events| events.defer_ordinary_persistence()),
    )
}

impl<'a> TurnAccounting<'a> {
    pub(super) fn ensure_scope(&self) -> Result<Arc<AccountingScope>, DecisionError> {
        if self.service.generation.effective.causal_tree.enabled
            && (!self.service.durable_key || !self.service.recordable)
        {
            return Err(DecisionError::BudgetExceeded);
        }
        validate_agent_turn_origin(&self.service.generation, self.service.surface, self.proof)
            .map_err(|_| DecisionError::BudgetExceeded)?;
        let scope = self.scope.get_or_init(|| {
            let cleanup = crate::runtime::turn_cleanup::current_tracker()?;
            let key = &self.service.state_key;
            let scope_id = key.hmac(
                KeyDomain::TerminalClaim,
                &[
                    b"turn-decision-accounting-v1",
                    self.proof.workspace_id().as_bytes(),
                    self.proof.owner_id().as_bytes(),
                    self.proof.session_key().as_bytes(),
                    self.proof.run_id().as_bytes(),
                    self.proof.source_event_id().unwrap_or("").as_bytes(),
                    self.proof.config_source_revision().as_bytes(),
                ],
            );
            Some(Arc::new(AccountingScope::new(
                self.service.generation.effective.clone(),
                key.clone(),
                cleanup,
                scope_id,
                self.budgets,
                self.deadline,
            )))
        });
        scope.as_ref().map(Arc::clone).ok_or(DecisionError::BudgetExceeded)
    }

    pub(super) fn retained_scope(&self) -> Option<Arc<AccountingScope>> {
        self.scope.get().and_then(|scope| scope.as_ref().map(Arc::clone))
    }

    pub(super) fn remaining_budgets(&self) -> TurnBudgets {
        self.scope.get().and_then(Option::as_ref).map_or_else(
            || TurnBudgets {
                latency_ms: self.budgets.latency_ms.min(
                    u64::try_from(self.deadline.saturating_duration_since(Instant::now()).as_millis())
                        .unwrap_or(u64::MAX),
                ),
                ..self.budgets
            },
            |scope| scope.remaining_budgets(),
        )
    }

    pub(super) const fn new(
        service: &'a TurnDecisionService,
        proof: &'a dyn TurnSourceProof,
        budgets: TurnBudgets,
        deadline: Instant,
    ) -> Self {
        Self {
            service,
            proof,
            budgets,
            deadline,
            scope: OnceLock::new(),
        }
    }
}

impl budget::sealed::AccountingOwner for TurnAccounting<'_> {}

#[async_trait]
impl DecisionAttemptAccounting for TurnAccounting<'_> {
    fn remaining_budget(&self) -> (u64, u64) {
        let budget = self.remaining_budgets();
        (budget.tokens, budget.micro_usd)
    }

    async fn begin(&self, attempt: AttemptBinding<'_>) -> Result<Box<dyn DecisionAttemptReceipt>, DecisionError> {
        self.ensure_scope()?.begin(attempt).await
    }
}

#[cfg(test)]
mod tests;
