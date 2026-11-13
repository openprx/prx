//! The turn's physical-send accounting scope, created at opening.
use super::{TurnBudgets, TurnDecisionService, TurnSourceProof, validate_agent_turn_origin};
use crate::decision::KeyDomain;
use crate::runtime::decision_accounting::AccountingScope;
use std::{sync::Arc, time::Instant};

/// The turn's one accounting scope, created at opening (R9). The scope is
/// bound to the source run exactly as the retired prediction hop bound it.
/// `None` when the turn cannot be charged: an enabled plane without durable
/// recording, an unverifiable origin, or no cleanup owner.
pub(super) fn opening_scope(
    service: &TurnDecisionService,
    proof: &dyn TurnSourceProof,
    budgets: TurnBudgets,
    deadline: Instant,
) -> Option<Arc<AccountingScope>> {
    if service.generation.effective.causal_tree.enabled && (!service.durable_key || !service.recordable) {
        return None;
    }
    validate_agent_turn_origin(&service.generation, service.surface, proof).ok()?;
    let cleanup = crate::runtime::turn_cleanup::current_tracker()?;
    let key = &service.state_key;
    Some(Arc::new(
        AccountingScope::new(
            service.generation.effective.clone(),
            key.clone(),
            cleanup,
            accounting_scope_id(key, proof),
            budgets,
            deadline,
        )
        .with_money(service.recursive_money(proof)),
    ))
}

fn accounting_scope_id(key: &crate::decision::DecisionKey, proof: &dyn TurnSourceProof) -> [u8; 32] {
    key.hmac(
        KeyDomain::TerminalClaim,
        &[
            b"turn-decision-accounting-v1",
            proof.workspace_id().as_bytes(),
            proof.owner_id().as_bytes(),
            proof.session_key().as_bytes(),
            proof.run_id().as_bytes(),
            proof.source_event_id().unwrap_or("").as_bytes(),
            proof.config_source_revision().as_bytes(),
        ],
    )
}
