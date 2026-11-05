//! Persisted approval adapter; construction does not install it into dispatch.
use super::run_call::ApprovalLedgerPort;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::execution::{ApprovalStrategy, ToolApprovalDecision, ToolApprovalRequest};

pub(crate) struct LedgerApprovalStrategy<'a> {
    pub(crate) port: ApprovalLedgerPort<'a>,
    pub(crate) envelope: RuntimeEnvelope,
    pub(crate) catalog: &'a crate::tools::capability::catalog::CapabilityCatalog,
}

#[async_trait::async_trait]
impl ApprovalStrategy for LedgerApprovalStrategy<'_> {
    async fn resolve(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        let result = async {
            anyhow::ensure!(request.context.envelope == self.envelope, "approval envelope changed");
            let entry = self
                .catalog
                .entry_for_public_name(&request.descriptor.public_name)
                .ok_or_else(|| anyhow::anyhow!("approval capability unavailable"))?;
            let capability = crate::causal_tree::branch::CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            };
            let pending = self
                .port
                .record_pending(
                    &self.envelope,
                    &capability,
                    &request.descriptor.backend_name,
                    &request.command.arguments,
                )
                .await?;
            // Issuance/claim is deliberately separate from dispatch (card 02a).
            let grant = self
                .port
                .claim_if_approved(
                    self.envelope.owner_id.as_deref().unwrap_or(""),
                    &self.envelope.session_key,
                    pending.authority(),
                )
                .await?;
            Ok::<_, anyhow::Error>((
                pending.authority().call_id(),
                grant.map(serde_json::to_value).transpose()?,
            ))
        }
        .await;
        match result {
            Ok((_, Some(grant))) => ToolApprovalDecision::Approved {
                runtime_approval_granted: true,
                runtime_grant: Some(grant),
            },
            Ok((id, None)) => ToolApprovalDecision::Cancelled {
                reason: format!("awaiting approval: {id}"),
            },
            Err(error) => ToolApprovalDecision::Denied {
                reason: error.to_string(),
            },
        }
    }
}
