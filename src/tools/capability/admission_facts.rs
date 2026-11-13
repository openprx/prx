//! Execution-owned admission facts of one turn.
//!
//! The hard-gate inputs other than state and catalog, captured once when the
//! turn opens: actor-scoped policy facts (G4), model facts and the inherited
//! child scope. Approval evidence is not a fact; an authenticated resume turn
//! carries it separately in `resume_evidence::VerifiedResumeEvidenceV1`.
//! Owned, read-only and cheap to clone; no interior mutability.

use std::sync::Arc;

use crate::security::SecurityPolicy;
use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest};

#[derive(Clone)]
pub(crate) struct AdmissionFactsV1 {
    pub(crate) policy: Arc<SecurityPolicy>,
    pub(crate) owner_id: Arc<str>,
    /// The exact actor the execution service uses for policy.
    pub(crate) sender: Arc<str>,
    pub(crate) policy_channel: Arc<str>,
    pub(crate) chat_type: Arc<str>,
    pub(crate) model_ref: Arc<str>,
    pub(crate) model_can_chat: bool,
    pub(crate) model_can_call_tools: bool,
    /// The restriction inherited by the current worker, never an outgoing one.
    pub(crate) child_scope: Option<(Arc<ChildCapabilityManifest>, Arc<CapabilityConstraints>)>,
}

impl AdmissionFactsV1 {
    /// The inherited child scope in the borrowed shape the gates read.
    pub(crate) fn child_scope(&self) -> Option<(&ChildCapabilityManifest, &CapabilityConstraints)> {
        self.child_scope
            .as_ref()
            .map(|(manifest, constraints)| (manifest.as_ref(), constraints.as_ref()))
    }
}
