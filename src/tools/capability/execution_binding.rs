//! Pinned capability identity carried from selection to backend dispatch.

mod admitted_call;
pub(crate) use admitted_call::{ExecutionAdmission, VerifiedExecutionCall};
mod dispatch;
pub(crate) use dispatch::{scope_dispatch_binding, verify_current_dispatch};

use super::catalog::{CapabilityCatalog, CapabilityEntry};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::execution::{ToolDescriptor, ToolExecutionCommand, ToolExecutionService};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ToolExecutionError {
    #[error("capability scope, generation, schema or backend binding is stale")]
    StaleCapability,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutionBinding {
    decision_id: crate::security::decision_id::DecisionId,
    capability: CapabilityRef,
    catalog: Arc<CapabilityCatalog>,
    scope: RuntimeEnvelope,
    selected: bool,
}

impl PartialEq for ExecutionBinding {
    fn eq(&self, other: &Self) -> bool {
        self.decision_id == other.decision_id
            && self.capability == other.capability
            && Arc::ptr_eq(&self.catalog, &other.catalog)
            && self.scope == other.scope
            && self.selected == other.selected
    }
}
impl Eq for ExecutionBinding {}

impl ExecutionBinding {
    pub(crate) fn admission(&self) -> ExecutionAdmission {
        ExecutionAdmission {
            catalog: Arc::clone(&self.catalog),
            scope: self.scope.clone(),
        }
    }

    pub(crate) const fn is_selected(&self) -> bool {
        self.selected
    }
    pub(crate) fn capability_id(&self) -> &str {
        self.capability.id.as_str()
    }

    pub(crate) fn issued(
        decision_id: crate::security::decision_id::DecisionId,
        entry: &CapabilityEntry,
        catalog: Arc<CapabilityCatalog>,
        scope: RuntimeEnvelope,
        selected: bool,
    ) -> Self {
        Self {
            decision_id,
            capability: CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            },
            catalog,
            scope,
            selected,
        }
    }

    /// Scope, generation, schema and backend identity of the admitted
    /// capability against the live descriptor. Child membership and child
    /// catalog parity stay with 02c `admit_child` at dispatch; this check
    /// remains on the gateway-issued admitted binding (MCP compat) that 02c never sees.
    pub(crate) fn verify_admitted_descriptor(
        &self,
        service: &ToolExecutionService,
        scope: &RuntimeEnvelope,
        descriptor: &ToolDescriptor,
    ) -> Result<(), ToolExecutionError> {
        let refuse = || ToolExecutionError::StaleCapability;
        if scope != &self.scope {
            return Err(refuse());
        }
        self.catalog.check_parity(service).map_err(|_| refuse())?;
        let entry = self
            .catalog
            .entry_for_public_name(&descriptor.public_name)
            .ok_or_else(refuse)?;
        if entry.id != self.capability.id
            || entry.generation != self.capability.generation
            || entry.root != descriptor.backend_name
            || entry.fingerprint != self.capability.fingerprint
            || super::fingerprint::tool_spec_fingerprint(
                &descriptor.public_name,
                &descriptor.description,
                &descriptor.parameters,
            ) != self.capability.fingerprint
        {
            return Err(refuse());
        }
        self.catalog
            .resolve(std::slice::from_ref(&self.capability))
            .map_err(|_| refuse())?;
        Ok(())
    }
}

/// The decision owner supplies preflight metadata separately from the final
/// durable claim. Only the latter can authorize a backend's side-effect gate.
#[async_trait::async_trait]
pub(crate) trait DecisionResumeAdapter: Send + Sync {
    fn verify(
        &self,
        command: &ToolExecutionCommand,
        context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<crate::security::policy::ApprovalGrant>;
    async fn claim(
        &self,
        command: &ToolExecutionCommand,
        context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<Box<dyn crate::security::decision_approval::DecisionGrantAuthority>>;
}

#[derive(Clone)]
pub(crate) struct DecisionResumePort(Arc<dyn DecisionResumeAdapter>);
impl DecisionResumePort {
    pub(crate) fn new(adapter: Arc<dyn DecisionResumeAdapter>) -> Self {
        Self(adapter)
    }
    pub(crate) fn verify(
        &self,
        command: &ToolExecutionCommand,
        context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<crate::security::policy::ApprovalGrant> {
        self.0.verify(command, context)
    }
    pub(crate) async fn claim(
        &self,
        command: &ToolExecutionCommand,
        context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<Box<dyn crate::security::decision_approval::DecisionGrantAuthority>> {
        self.0.claim(command, context).await
    }
}
impl std::fmt::Debug for DecisionResumePort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DecisionResumePort")
    }
}
impl PartialEq for DecisionResumePort {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for DecisionResumePort {}

/// Digest inputs shared with authenticated resume preparation. This computes
/// identity only and cannot manufacture an execution binding or approval.
pub(crate) struct CallDigestSource<'a> {
    pub(crate) workspace: &'a str,
    pub(crate) owner: &'a str,
    pub(crate) session: &'a str,
    pub(crate) run: Option<&'a str>,
    pub(crate) event: Option<&'a str>,
    pub(crate) channel: Option<&'a str>,
    pub(crate) sender: Option<&'a str>,
    pub(crate) generation: Option<u64>,
    pub(crate) revision: Option<&'a str>,
}

pub(crate) fn admitted_call_digest(
    decision_id: &str,
    capability: &CapabilityRef,
    source: CallDigestSource<'_>,
    arguments: &serde_json::Value,
    operation: &str,
    risk: crate::acl::approval_grant::RiskLevel,
) -> anyhow::Result<[u8; 32]> {
    let material = serde_json::to_vec(&(
        "prx.decision.call.v1",
        decision_id,
        capability.id.as_str(),
        capability.fingerprint.0,
        capability.generation.0,
        source.workspace,
        source.owner,
        source.session,
        source.run,
        source.event,
        source.channel,
        source.sender,
        source.generation,
        source.revision,
        (arguments, operation, risk),
    ))?;
    Ok(Sha256::digest(material).into())
}
