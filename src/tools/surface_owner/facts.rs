//! Execution-owned publication facts of one turn.
//!
//! The publication inputs other than the trusted state and the catalog,
//! captured once when the turn opens: actor-scoped policy facts, model facts
//! and the inherited child scope. Approval evidence is not a fact.
//! Owned, read-only and cheap to clone; no interior mutability.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::security::SecurityPolicy;
use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest};
use crate::tools::capability::contract::CapabilityId;

#[derive(Clone)]
pub(crate) struct PublicationFacts {
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
    /// Whether a child turn's own executor carries the live discovery
    /// control. With it, the child reaches its deferred manifest members
    /// through `capability_search` (R28); without it, they are published.
    pub(crate) child_discovery: bool,
    /// For a Full child turn, the pinned catalog entries the child's own
    /// executor serves with descriptor parity. Full has no manifest
    /// membership limit (R51), but a child sees only what its executor runs,
    /// so an entry outside the manifest is published only from this set.
    /// `None` below Full and for non-child turns, where the manifest (or no
    /// child scope) bounds publication.
    pub(crate) child_served: Option<Arc<BTreeSet<CapabilityId>>>,
}

impl PublicationFacts {
    /// The inherited child scope in borrowed shape.
    pub(crate) fn child_scope(&self) -> Option<(&ChildCapabilityManifest, &CapabilityConstraints)> {
        self.child_scope
            .as_ref()
            .map(|(manifest, constraints)| (manifest.as_ref(), constraints.as_ref()))
    }
}
