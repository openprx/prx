//! Child-scope carriage of one turn: which verified child scope a publication
//! was issued under, and how an authenticated source's child authority is
//! carried into the execution context of a dispatch.

use std::sync::Arc;

use super::IssuedChildGrant;
use super::ordinary_process::OrdinaryProcessAuthority;
use super::ordinary_task::OrdinaryTaskAuthority;
use crate::runtime::turn_decision::OrdinaryChildPublication;
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::runtime_key::DecisionKey;
use crate::tools::ToolExecutionContext;
use crate::tools::capability::catalog::CapabilityCatalog;
use crate::tools::capability::child::{ChildDispatchPin, VerifiedChildScope};

/// The verified child scope a publication was issued under: an incoming
/// grant, an ordinary parent publication, an ordinary task or an ordinary
/// process, in that order.
pub(crate) fn issued_child_scope<'a>(
    parent_grant: Option<&'a IssuedChildGrant>,
    ordinary_parent: Option<&'a OrdinaryChildPublication>,
    ordinary_task: Option<&'a OrdinaryTaskAuthority>,
    ordinary_process: Option<&'a OrdinaryProcessAuthority>,
) -> Option<&'a Arc<VerifiedChildScope>> {
    parent_grant
        .map(IssuedChildGrant::scope)
        .or_else(|| ordinary_parent.map(OrdinaryChildPublication::scope))
        .or_else(|| ordinary_task.map(|task| task.publication().scope()))
        .or_else(|| ordinary_process.map(|process| &process.snapshots().scope))
}

/// Incoming child authority comes only from the authenticated source and is
/// overwritten unconditionally, so a caller cannot carry or strip it.
pub(crate) fn carry_child_authority(
    execution_context: &mut ToolExecutionContext,
    proof: &dyn TurnSourceProof,
    catalog: &Arc<CapabilityCatalog>,
    key: &Arc<DecisionKey>,
) {
    let child_origin = proof.child_scope().is_some()
        || proof.ordinary_task_authority_id().is_some()
        || proof.ordinary_process_authority_id().is_some();
    execution_context.child_scope = proof.child_scope().cloned();
    execution_context.child_pin = child_origin.then(|| {
        Arc::new(ChildDispatchPin {
            catalog: Arc::clone(catalog),
            key: Arc::clone(key),
            owner: proof.owner_id().to_owned(),
            workspace: proof.workspace_id().to_owned(),
        })
    });
}
