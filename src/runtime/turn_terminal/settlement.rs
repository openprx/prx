//! Terminal settlement authority: the token that alone authorizes a terminal
//! binding, and the binding issuer of an admitted publication.
use std::sync::Arc;

use crate::agent::terminal::TerminalBinding;
use crate::runtime::turn_decision::{SettlementRefUnavailable, TurnDecisionContext, TurnDecisionService};

/// Only this module can authorize construction of a terminal binding.
pub(crate) struct SettlementIssueToken(());

/// The execution-owned terminal binding of a turn this service opened: the
/// pinned runtime context, the source channel, the workspace and cost
/// configuration of the pinned generation, the turn memory, and the incoming
/// child grant or operator parent of the publication.
pub(crate) fn terminal_binding(
    service: &TurnDecisionService,
    context: &TurnDecisionContext,
) -> Result<TerminalBinding, SettlementRefUnavailable> {
    if context.service_id != service.service_id {
        return Err(SettlementRefUnavailable);
    }
    Ok(TerminalBinding::issued(
        Arc::clone(context.runtime()),
        Arc::from(context.source_channel()),
        service.generation.effective.workspace_dir.clone(),
        service.generation.effective.cost.clone(),
        Arc::clone(&service.memory),
        SettlementIssueToken(()),
    )
    .with_operator_parent(
        service
            .operator_task
            .as_ref()
            .map(|task| Arc::new(task.parent_source()))
            .or_else(|| {
                service
                    .operator_process
                    .as_ref()
                    .map(|process| Arc::new(process.snapshots().parent.clone()))
            }),
    ))
}
