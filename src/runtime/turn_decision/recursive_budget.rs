//! Decision admission adapts its accounting to the shared child allowance.
use super::{TurnDecisionContext, TurnDecisionService, TurnSourceProof};
use crate::runtime::child_source::recursive_allowance::RecursiveAllowance;
use std::sync::{Arc, atomic::AtomicU64};

impl TurnDecisionService {
    pub(super) fn recursive_money(&self, proof: &dyn TurnSourceProof) -> Option<Arc<AtomicU64>> {
        if let Some(incoming) = &self.parent_grant {
            return incoming
                .recursive_child_allowance()
                .and_then(|owner| owner.funded_money());
        }
        if let Some(money) = &self.ordinary_money {
            return Some(Arc::clone(money));
        }
        self.recursive_allowances
            .lookup(proof)
            .and_then(|owner| owner.funded_money())
    }

    pub(super) fn issue_recursive_allowance(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> Option<Arc<RecursiveAllowance>> {
        if let Some(incoming) = &self.parent_grant {
            return incoming.recursive_child_allowance();
        }
        Some(Arc::clone(context.recursive_allowance.get_or_init(|| {
            let money = context.accounting.as_ref().map_or_else(
                || {
                    self.recursive_money(proof).unwrap_or_else(|| {
                        Arc::new(AtomicU64::new(context.engine.state.trusted.parts().budgets.micro_usd))
                    })
                },
                |scope| scope.money_owner(),
            );
            self.recursive_allowances.issue(proof, money)
        })))
    }
}

impl TurnDecisionContext {
    pub(crate) fn retire_recursive_allowance(&self) {
        if let Some(owner) = self.recursive_allowance.get() {
            owner.retire();
        }
    }
}
