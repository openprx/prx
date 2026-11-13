//! Execution-owned verification of one admitted tool call.
//!
//! Integrity only: the decision plane does not mint this receipt. Full
//! autonomy is unaffected.

use super::CapabilityCatalog;
use super::{CallDigestSource, ToolExecutionError, admitted_call_digest};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::capability::fingerprint::tool_spec_fingerprint;
use crate::tools::execution::{ToolDescriptor, ToolExecutionCommand, ToolExecutionService};
use std::sync::Arc;

/// Catalog and source scope retained by execution admission.
#[derive(Clone, Debug)]
pub(crate) struct ExecutionAdmission {
    pub(crate) catalog: Arc<CapabilityCatalog>,
    pub(crate) scope: RuntimeEnvelope,
}

impl PartialEq for ExecutionAdmission {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.catalog, &other.catalog) && self.scope == other.scope
    }
}
impl Eq for ExecutionAdmission {}

/// Opaque receipt that the execution service minted from the admitted
/// descriptor. Never decoded from caller JSON.
#[derive(Clone)]
pub(crate) struct VerifiedExecutionCall {
    command: ToolExecutionCommand,
    capability: CapabilityRef,
    scope: RuntimeEnvelope,
    call_digest: [u8; 32],
    catalog_generation: [u8; 32],
}

impl VerifiedExecutionCall {
    pub(crate) const fn command(&self) -> &ToolExecutionCommand {
        &self.command
    }
    pub(crate) const fn capability(&self) -> &CapabilityRef {
        &self.capability
    }
    pub(crate) const fn scope(&self) -> &RuntimeEnvelope {
        &self.scope
    }
    pub(crate) const fn call_digest(&self) -> [u8; 32] {
        self.call_digest
    }
    pub(crate) const fn catalog_generation(&self) -> [u8; 32] {
        self.catalog_generation
    }
}

impl std::fmt::Debug for VerifiedExecutionCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedExecutionCall")
            .field("capability", &self.capability)
            .finish_non_exhaustive()
    }
}

impl ToolExecutionService {
    /// Same scope, generation, schema and backend checks as
    /// `ExecutionBinding::verify_admitted_descriptor`, from the service's
    /// admitted catalog snapshot versus the live descriptor.
    pub(crate) fn verify_admitted_call(
        &self,
        envelope: &RuntimeEnvelope,
        admission: &ExecutionAdmission,
        descriptor: &ToolDescriptor,
        command: &ToolExecutionCommand,
    ) -> Result<VerifiedExecutionCall, ToolExecutionError> {
        let refuse = || ToolExecutionError::StaleCapability;
        if command.name != descriptor.public_name {
            return Err(refuse());
        }
        if envelope.workspace_id.is_empty()
            || envelope.session_key.is_empty()
            || envelope.resolved_owner_id().is_empty()
            || envelope.run_id.as_ref().is_none_or(|run| run.is_empty())
            || envelope
                .source_message_event_id
                .as_ref()
                .is_none_or(|event| event.is_empty())
        {
            return Err(refuse());
        }
        let admitted = self.admitted_descriptor(&descriptor.public_name).ok_or_else(refuse)?;
        if admitted.backend_name != descriptor.backend_name {
            return Err(refuse());
        }
        let admitted_fingerprint =
            tool_spec_fingerprint(&admitted.public_name, &admitted.description, &admitted.parameters);
        let live_fingerprint =
            tool_spec_fingerprint(&descriptor.public_name, &descriptor.description, &descriptor.parameters);
        if admitted_fingerprint != live_fingerprint {
            return Err(refuse());
        }
        if envelope != &admission.scope {
            return Err(refuse());
        }
        admission.catalog.check_parity(self).map_err(|_| refuse())?;
        let entry = admission
            .catalog
            .entry_for_public_name(&descriptor.public_name)
            .ok_or_else(refuse)?;
        if entry.root != descriptor.backend_name || entry.fingerprint != live_fingerprint {
            return Err(refuse());
        }
        let capability = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let owner = envelope.resolved_owner_id();
        let (operation, risk) =
            match crate::security::approval_operation::grant_op_for_call(&descriptor.public_name, &command.arguments) {
                Some(crate::security::approval_operation::GrantOp::Command(operation)) => {
                    (operation, crate::acl::approval_grant::RiskLevel::High)
                }
                Some(crate::security::approval_operation::GrantOp::ResourceExact(operation, risk)) => (operation, risk),
                _ => (
                    descriptor.public_name.clone(),
                    crate::acl::approval_grant::RiskLevel::High,
                ),
            };
        let digest = admitted_call_digest(
            "",
            &capability,
            CallDigestSource {
                workspace: &envelope.workspace_id,
                owner: &owner,
                session: &envelope.session_key,
                run: envelope.run_id.as_deref(),
                event: envelope.source_message_event_id.as_deref(),
                channel: envelope.channel.as_deref(),
                sender: envelope.sender.as_deref(),
                generation: envelope.config_generation_id,
                revision: envelope.config_source_revision.as_deref(),
            },
            &command.arguments,
            &operation,
            risk,
        )
        .map_err(|_| refuse())?;
        Ok(VerifiedExecutionCall {
            command: command.clone(),
            capability,
            scope: envelope.clone(),
            call_digest: digest,
            catalog_generation: admission.catalog.generation().0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::approval_grant::RiskLevel;
    use crate::tools::capability::contract::{CapabilityId, CapabilityTarget, CatalogGeneration, SchemaFingerprint};

    #[test]
    fn h2_call_digest_matches_previous_decision_call_digest_formula() {
        let capability = CapabilityRef {
            id: CapabilityId::new(CapabilityTarget::NativeTool, "native", "skills_manage")
                .expect("test: fixture capability id"),
            fingerprint: SchemaFingerprint([0; 32]),
            generation: CatalogGeneration([0; 32]),
        };
        let digest = admitted_call_digest(
            "",
            &capability,
            CallDigestSource {
                workspace: "ws",
                owner: "owner",
                session: "session",
                run: Some("run"),
                event: Some("event"),
                channel: Some("terminal"),
                sender: Some("local-user"),
                generation: None,
                revision: None,
            },
            &serde_json::json!({"action": "lifecycle_experiment_catalog"}),
            "skills_manage",
            RiskLevel::High,
        )
        .expect("test: fixture digest");
        assert_eq!(
            hex::encode(digest),
            "db5274901a343d56949875f0600ca3ba25acd926ae3307e05d7a7f98c6cc5cc1"
        );
    }
}
