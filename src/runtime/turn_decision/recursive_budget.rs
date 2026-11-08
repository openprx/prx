//! Decision admission adapts its accounting to the shared child allowance.
use super::{AgentDecisionBudgetError, TurnDecisionContext, TurnDecisionService, TurnSourceProof};
use crate::runtime::child_source::recursive_allowance::RecursiveAllowance;
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicU64, atomic::Ordering};

/// Live spend inputs of one provider send or called tool. `live` re-reads the
/// ledger and the incoming funding on every call, so a later boundary sees a
/// charge or a funding change made after the snapshot.
pub(crate) struct SpendCeiling {
    cost: crate::config::schema::CostConfig,
    workspace_dir: PathBuf,
    security_cents: u32,
    funding: Option<Arc<AtomicU64>>,
    manifest_micro: Option<u64>,
}

impl SpendCeiling {
    /// Minimum of the read-only daily, monthly and security remaining, the
    /// incoming child funding and the sealed child manifest cents x 10_000.
    pub(crate) fn live(&self) -> anyhow::Result<u64> {
        let remaining = crate::cost::tracker::remaining_micro_usd_read_only_with_security_ceiling(
            &self.cost,
            &self.workspace_dir,
            self.security_cents,
        )?;
        let remaining = self
            .funding
            .as_ref()
            .map_or(remaining, |money| remaining.min(money.load(Ordering::Acquire)));
        Ok(self
            .manifest_micro
            .map_or(remaining, |manifest| remaining.min(manifest)))
    }
}

impl TurnDecisionService {
    pub(crate) fn recursive_money(&self, proof: &dyn TurnSourceProof) -> Option<Arc<AtomicU64>> {
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

    /// A child turn without incoming funding refuses: the workspace remaining
    /// alone never stands in for a child allowance.
    pub(crate) fn spend_ceiling(&self, proof: &dyn TurnSourceProof) -> Result<SpendCeiling, AgentDecisionBudgetError> {
        let funding = self.recursive_money(proof);
        let child = proof.child_scope();
        if child.is_some() && funding.is_none() {
            return Err(AgentDecisionBudgetError::ChildFunding);
        }
        let manifest_micro =
            child.map(|scope| u64::from(scope.gate_inputs().0.authority.max_cost_cents).saturating_mul(10_000));
        let config = &self.generation.effective;
        Ok(SpendCeiling {
            cost: config.cost.clone(),
            workspace_dir: config.workspace_dir.clone(),
            security_cents: self.security.max_cost_per_day_cents,
            funding,
            manifest_micro,
        })
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
