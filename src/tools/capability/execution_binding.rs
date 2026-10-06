//! Pinned capability identity carried from selection to backend dispatch.

mod dispatch;
pub(crate) use dispatch::{scope_dispatch_binding, verify_current_dispatch};

use super::catalog::{CapabilityCatalog, CapabilityEntry};
use crate::causal_tree::branch::CapabilityRef;
use crate::runtime::envelope::RuntimeEnvelope;
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
    child_scope: Option<Arc<super::child::VerifiedChildScope>>,
    decision_id: crate::causal_tree::ids::DecisionId,
    capability: CapabilityRef,
    catalog: Arc<CapabilityCatalog>,
    scope: RuntimeEnvelope,
    selected: bool,
    requires_approval: bool,
    discovery_control: bool,
}

impl PartialEq for ExecutionBinding {
    fn eq(&self, other: &Self) -> bool {
        self.decision_id == other.decision_id
            && self.capability == other.capability
            && Arc::ptr_eq(&self.catalog, &other.catalog)
            && self.scope == other.scope
            && self.selected == other.selected
            && self.requires_approval == other.requires_approval
            && self.discovery_control == other.discovery_control
            && match (&self.child_scope, &other.child_scope) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }
}
impl Eq for ExecutionBinding {}

impl ExecutionBinding {
    pub(crate) const fn requires_approval(&self) -> bool {
        self.requires_approval
    }

    pub(crate) const fn with_required_approval(mut self, required: bool) -> Self {
        self.requires_approval = required;
        self
    }

    pub(crate) const fn is_selected(&self) -> bool {
        self.selected
    }
    pub(crate) fn capability_id(&self) -> &str {
        self.capability.id.as_str()
    }

    pub(crate) fn issued(
        decision_id: crate::causal_tree::ids::DecisionId,
        entry: &CapabilityEntry,
        catalog: Arc<CapabilityCatalog>,
        scope: RuntimeEnvelope,
        selected: bool,
    ) -> Self {
        Self {
            child_scope: None,
            decision_id,
            capability: CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            },
            catalog,
            scope,
            selected,
            requires_approval: false,
            discovery_control: false,
        }
    }

    pub(crate) fn with_child_scope(mut self, scope: Option<Arc<super::child::VerifiedChildScope>>) -> Self {
        self.child_scope = scope;
        self
    }

    /// The search control is not a child business capability. Its source port
    /// confines offers to the manifest; parity must still use that same child
    /// scope instead of requiring unrelated parent backends to be installed.
    pub(crate) fn with_discovery_control_scope(mut self, scope: Option<Arc<super::child::VerifiedChildScope>>) -> Self {
        self.child_scope = scope;
        self.discovery_control = true;
        self
    }

    /// Capture the actual structurally validated command at the approval
    /// boundary. This proves provenance only; it does not approve execution.
    pub(crate) async fn bind_call(
        &self,
        service: &ToolExecutionService,
        scope: &RuntimeEnvelope,
        descriptor: &ToolDescriptor,
        command: &ToolExecutionCommand,
        approval: Option<&DecisionApprovalPort>,
        schema_dialect: crate::tools::schema::ToolSchemaDialect,
    ) -> anyhow::Result<VerifiedDecisionCall> {
        self.verify(service, scope, descriptor)?;
        anyhow::ensure!(command.name == descriptor.public_name, "approval command name changed");
        let entry = self
            .catalog
            .entry_for_public_name(&descriptor.public_name)
            .ok_or(ToolExecutionError::StaleCapability)?;
        use crate::acl::approval_grant::RiskLevel;
        let (operation, risk) =
            match crate::security::approval_operation::grant_op_for_call(&descriptor.public_name, &command.arguments) {
                Some(crate::security::approval_operation::GrantOp::Command(operation)) => (operation, RiskLevel::High),
                Some(crate::security::approval_operation::GrantOp::ResourceExact(operation, risk)) => (operation, risk),
                Some(crate::security::approval_operation::GrantOp::ResourceGlob(_, _)) => {
                    // Shared execution strips _zc_principal before invocation.
                    // Bind the same default operation used by these backends;
                    // the complete owner/session still binds the signed call.
                    match descriptor.public_name.as_str() {
                        "message_send" => {
                            service
                                .resolve_approval_operation(&command.name, &command.arguments)
                                .await?
                        }
                        "memory_store" => (
                            crate::security::approval_operation::memory_write_operation(None),
                            RiskLevel::Medium,
                        ),
                        "file_write" | "file_edit" => {
                            let path = command
                                .arguments
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| anyhow::anyhow!("approval file path missing"))?;
                            approval
                                .ok_or_else(|| anyhow::anyhow!("file approval resolver missing"))?
                                .file_operation(&descriptor.public_name, path)
                                .await?
                        }
                        "memory_forget" => {
                            let key = command
                                .arguments
                                .get("key")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| anyhow::anyhow!("approval memory key missing"))?;
                            (
                                crate::security::approval_operation::memory_delete_operation(None, key),
                                RiskLevel::High,
                            )
                        }
                        _ => anyhow::bail!("decision approval requires a resolved exact operation"),
                    }
                }
                None => match entry.op_ids.as_slice() {
                    [operation] => (operation.as_str().to_owned(), RiskLevel::High),
                    _ => anyhow::bail!("decision approval operation cannot be derived exactly"),
                },
            };
        self.verify(service, scope, descriptor)?;
        anyhow::ensure!(
            !operation.is_empty()
                && (entry.op_ids.is_empty() || entry.op_ids.iter().any(|id| id.as_str() == operation)),
            "approval operation differs from pinned capability"
        );
        // Operation and idempotency IDs are transport retry identifiers, not
        // part of the requested effect. Exact arguments and pinned scope are.
        let owner = scope.resolved_owner_id();
        let digest = decision_call_digest(
            &self.decision_id.to_string(),
            &self.capability,
            CallDigestSource {
                workspace: &scope.workspace_id,
                owner: &owner,
                session: &scope.session_key,
                run: scope.run_id.as_deref(),
                event: scope.source_message_event_id.as_deref(),
                channel: scope.channel.as_deref(),
                sender: scope.sender.as_deref(),
                generation: scope.config_generation_id,
                revision: scope.config_source_revision.as_deref(),
            },
            &command.arguments,
            &operation,
            risk,
        )?;
        Ok(VerifiedDecisionCall {
            command: command.clone(),
            binding: crate::acl::approval_grant::DecisionApprovalBinding {
                decision_id: self.decision_id.to_string(),
                catalog_generation: self.capability.generation.0,
                call_digest: digest,
            },
            operation,
            risk,
            schema_dialect,
            capability: self.capability.clone(),
            scope: scope.clone(),
        })
    }

    pub(crate) fn verify(
        &self,
        service: &ToolExecutionService,
        scope: &RuntimeEnvelope,
        descriptor: &ToolDescriptor,
    ) -> Result<(), ToolExecutionError> {
        let refuse = || ToolExecutionError::StaleCapability;
        if scope != &self.scope {
            return Err(refuse());
        }
        if self.discovery_control
            && (self.selected
                || descriptor.public_name != "capability_search"
                || descriptor.backend_name != "capability_search")
        {
            return Err(refuse());
        }
        if let Some(child) = self.child_scope.as_ref() {
            if !self.discovery_control && !child.gate_inputs().0.allowed.contains(&self.capability) {
                return Err(refuse());
            }
            self.catalog.check_child_parity(service, child).map_err(|_| refuse())?;
        } else {
            self.catalog.check_parity(service).map_err(|_| refuse())?;
        }
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

/// Opaque receipt issued only by the pinned execution binding. Never decoded
/// from caller JSON and never sufficient on its own to bypass policy.
#[derive(Clone)]
pub(crate) struct VerifiedDecisionCall {
    command: ToolExecutionCommand,
    schema_dialect: crate::tools::schema::ToolSchemaDialect,
    operation: String,
    risk: crate::acl::approval_grant::RiskLevel,
    binding: crate::acl::approval_grant::DecisionApprovalBinding,
    capability: CapabilityRef,
    scope: RuntimeEnvelope,
}

/// Source-owned persistence port. It records a pending request and never
/// returns execution authority.
#[async_trait::async_trait]
pub(crate) trait DecisionApprovalRecorder: Send + Sync {
    async fn file_operation(
        &self,
        tool: &str,
        path: &str,
    ) -> anyhow::Result<(String, crate::acl::approval_grant::RiskLevel)>;
    async fn record(&self, call: &VerifiedDecisionCall) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub(crate) struct DecisionApprovalPort(Arc<dyn DecisionApprovalRecorder>);
impl DecisionApprovalPort {
    async fn file_operation(
        &self,
        tool: &str,
        path: &str,
    ) -> anyhow::Result<(String, crate::acl::approval_grant::RiskLevel)> {
        self.0.file_operation(tool, path).await
    }
    pub(crate) fn new(recorder: Arc<dyn DecisionApprovalRecorder>) -> Self {
        Self(recorder)
    }
    pub(crate) async fn record(&self, call: &VerifiedDecisionCall) -> anyhow::Result<()> {
        self.0.record(call).await
    }
}
impl std::fmt::Debug for DecisionApprovalPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DecisionApprovalPort")
    }
}
impl PartialEq for DecisionApprovalPort {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for DecisionApprovalPort {}

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

impl VerifiedDecisionCall {
    pub(crate) const fn command(&self) -> &ToolExecutionCommand {
        &self.command
    }
    pub(crate) const fn schema_dialect(&self) -> crate::tools::schema::ToolSchemaDialect {
        self.schema_dialect
    }
    pub(crate) const fn risk(&self) -> crate::acl::approval_grant::RiskLevel {
        self.risk
    }
    pub(crate) fn operation(&self) -> &str {
        &self.operation
    }
    pub(crate) const fn binding(&self) -> &crate::acl::approval_grant::DecisionApprovalBinding {
        &self.binding
    }
    pub(crate) const fn capability(&self) -> &CapabilityRef {
        &self.capability
    }
    pub(crate) const fn scope(&self) -> &RuntimeEnvelope {
        &self.scope
    }
}

impl std::fmt::Debug for VerifiedDecisionCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedDecisionCall")
            .field("binding", &self.binding)
            .field("capability", &self.capability)
            .finish_non_exhaustive()
    }
}

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

pub(crate) fn decision_call_digest(
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
