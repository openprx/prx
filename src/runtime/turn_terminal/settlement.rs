//! Terminal settlement authority: the token that alone authorizes a terminal
//! decision link, the link issuers of an admitted publication, and the
//! applied-directive admission reconstructed from authenticated terminals.
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context;

use crate::agent::terminal::{DecisionSettlementRef, TerminalBinding};
use crate::observability::causal_event::{CausalEvent, CausalEventFields, CausalEventKind, CausalLineage};
use crate::runtime::turn_decision::{SettlementRefUnavailable, TurnDecisionContext, TurnDecisionService};
use crate::tools::capability::turn_state::TurnSurfaceKind;

/// Only this module can authorize construction of a terminal decision link.
pub(crate) struct SettlementIssueToken(());

/// Disabled decisions still constrain execution, but cannot authorize a
/// decision-store write. Enabled decisions retain strict receipt checks.
pub(crate) fn terminal_settlement_ref(
    service: &TurnDecisionService,
    context: Arc<TurnDecisionContext>,
) -> Result<DecisionSettlementRef, SettlementRefUnavailable> {
    if context.service_id != service.service_id {
        return Err(SettlementRefUnavailable);
    }
    if !service.generation.effective.causal_tree.enabled {
        let trusted = context
            .audit_hops()
            .last()
            .ok_or(SettlementRefUnavailable)?
            .state
            .trusted
            .parts();
        if trusted.surface != service.surface
            || trusted.workspace_id.as_ref() != service.generation.effective.workspace_dir.to_string_lossy()
            || trusted.catalog_generation != *service.catalog.generation()
            || trusted.policy_version != service.policy_version
            || context.hop_ids().last().copied() != Some(context.decision_id())
        {
            return Err(SettlementRefUnavailable);
        }
        return Ok(DecisionSettlementRef::issued_disabled(
            context,
            SettlementIssueToken(()),
        ));
    }
    settlement_ref(service, context)
}

/// Issue a terminal link only after this service returned a recordable
/// decision. The caller cannot choose its decision id or writer scope.
pub(crate) fn settlement_ref(
    service: &TurnDecisionService,
    context: Arc<TurnDecisionContext>,
) -> Result<DecisionSettlementRef, SettlementRefUnavailable> {
    let trusted = context
        .audit_hops()
        .last()
        .ok_or(SettlementRefUnavailable)?
        .state
        .trusted
        .parts();
    if !service.generation.effective.causal_tree.enabled
        || context.service_id != service.service_id
        || !context.record_admitted.load(Ordering::Acquire)
        || !service.durable_key
        || !service.recordable
        || !matches!(
            service.surface,
            TurnSurfaceKind::AgentLoop
                | TurnSurfaceKind::Interactive
                | TurnSurfaceKind::Server
                | TurnSurfaceKind::Cron
                | TurnSurfaceKind::Xin
                | TurnSurfaceKind::Worker
                | TurnSurfaceKind::Channel
        )
        || trusted.surface != service.surface
        || trusted.workspace_id.as_ref() != service.generation.effective.workspace_dir.to_string_lossy()
        || trusted.catalog_generation != *service.catalog.generation()
        || trusted.policy_version != service.policy_version
        || context.hop_ids().last().copied() != Some(context.decision_id())
    {
        return Err(SettlementRefUnavailable);
    }
    let writer = service.store.writer().ok_or(SettlementRefUnavailable)?;
    let store_workspace_id = service.store.workspace_id().ok_or(SettlementRefUnavailable)?;
    Ok(DecisionSettlementRef::issued(
        context,
        writer,
        service.store.reader(),
        store_workspace_id,
        Arc::clone(&service.state_key),
        service.generation.effective.workspace_dir.clone(),
        SettlementIssueToken(()),
    ))
}

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

pub(in crate::runtime) struct VerifiedDirectiveApplication {
    event: CausalEvent,
    idempotency_key: String,
    original_generation: Option<(u64, Arc<str>)>,
}

impl VerifiedDirectiveApplication {
    /// Reconstruct admission from authenticated source and decision evidence.
    pub(in crate::runtime) fn from_replay(
        replay: &crate::agent::terminal::VerifiedTerminalReplay,
    ) -> anyhow::Result<Self> {
        let source = replay.source();
        let decision = replay.decision();
        let original_generation = (
            source
                .config_generation_id
                .context("original source generation is missing")?,
            Arc::from(
                source
                    .config_source_revision
                    .as_deref()
                    .context("original source revision is missing")?,
            ),
        );
        Ok(Self {
            event: CausalEvent {
                kind: CausalEventKind::DirectiveApplied,
                lineage: CausalLineage {
                    workspace_id: Arc::from(source.workspace_id.as_str()),
                    owner_id: source.owner_id.as_deref().map(Arc::from),
                    session_key: source.session_key.as_deref().map(Arc::from),
                    run_id: source.run_id.as_deref().map(Arc::from),
                    source_event_id: Some(Arc::from(source.event_id.as_str())),
                    causation_event_id: Some(Arc::from(replay.terminal_event_id())),
                    decision_id: Some(Arc::from(decision.decision_id.to_string())),
                    ..CausalLineage::default()
                },
                fields: CausalEventFields {
                    item_count: Some(1),
                    ..CausalEventFields::default()
                },
            },
            idempotency_key: format!("causal:{}:directive-applied", decision.decision_id),
            original_generation: Some(original_generation),
        })
    }

    #[cfg(test)]
    pub(in crate::runtime) const fn fixture(event: CausalEvent, idempotency_key: String) -> Self {
        Self {
            event,
            idempotency_key,
            original_generation: None,
        }
    }

    pub(in crate::runtime) fn into_parts(self) -> (CausalEvent, String, Option<(u64, Arc<str>)>) {
        (self.event, self.idempotency_key, self.original_generation)
    }
}
