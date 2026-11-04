//! One durable arbitration key for claim and revocation, including restart.
use super::keys::decision_resolution_key;
use super::{ApprovalLedger, GrantScope};
use crate::acl::approval_grant::{ApprovalGrantV2, DecisionApprovalBinding, WitnessKeyring};
use crate::memory::{MemoryFabric, MessageEvent, MessageEventScope};
use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ClaimDisposition {
    Consumed,
    Revoked,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct GrantClaim {
    disposition: ClaimDisposition,
    grant_id: String,
    signed_payload_sha256: String,
    binding: DecisionApprovalBinding,
    nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution: Option<ResumeClaimScope>,
}

/// Authenticated fresh execution that consumed the original pending call.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResumeClaimScope {
    pub(crate) decision_id: String,
    pub(crate) source_event_id: String,
    pub(crate) run_id: String,
}

impl ApprovalLedger<'_> {
    /// The claim has a fresh nonce for each attempt. Atomic event idempotency
    /// chooses one winner, including across processes. An uncertain write is
    /// never retried with authority: a subsequent attempt has a new nonce and
    /// fails closed if the earlier claim reached storage.
    pub(crate) async fn claim_decision_grant(
        &self,
        scope: MessageEventScope,
        grant: &ApprovalGrantV2,
        expected: &GrantScope<'_>,
        keyring: &WitnessKeyring,
        execution: ResumeClaimScope,
    ) -> anyhow::Result<MessageEvent> {
        self.resolve_decision_grant(scope, grant, expected, keyring, Some(execution))
            .await
    }

    /// Revocation competes with consumption for the same durable resolution.
    /// An already claimed execution cannot be retroactively reported revoked.
    /// A repeated matching revocation is idempotent and grants no authority.
    pub(crate) async fn revoke_decision_grant(
        &self,
        scope: MessageEventScope,
        grant: &ApprovalGrantV2,
        expected: &GrantScope<'_>,
        keyring: &WitnessKeyring,
    ) -> anyhow::Result<MessageEvent> {
        self.resolve_decision_grant(scope, grant, expected, keyring, None).await
    }

    async fn resolve_decision_grant(
        &self,
        scope: MessageEventScope,
        grant: &ApprovalGrantV2,
        expected: &GrantScope<'_>,
        keyring: &WitnessKeyring,
        execution: Option<ResumeClaimScope>,
    ) -> anyhow::Result<MessageEvent> {
        let disposition = if execution.is_some() {
            ClaimDisposition::Consumed
        } else {
            ClaimDisposition::Revoked
        };
        grant.verify_for_decision(
            keyring,
            expected.subject,
            expected.binding,
            expected.op_id,
            expected.risk,
            Utc::now(),
        )?;
        self.verify_approval_event_scope(&scope, expected).await?;
        let issued_key = format!("decision-approval:{}:issued", grant.grant_id);
        let issued = self
            .memory
            .find_message_event_by_idempotency_key(&expected.subject.workspace_id, &issued_key)
            .await?
            .context("decision approval was not persisted")?;
        anyhow::ensure!(
            issued.owner_id.as_deref() == Some(expected.subject.owner_id.as_str())
                && issued.session_key == expected.subject.session_key
                && issued.event_type == "approval.decision.issued",
            "stored approval belongs to another scope"
        );
        let stored: ApprovalGrantV2 = serde_json::from_str(
            issued
                .raw_payload_json
                .as_deref()
                .context("persisted decision approval payload missing")?,
        )?;
        anyhow::ensure!(stored == *grant, "persisted decision approval changed");
        let claim = GrantClaim {
            disposition,
            grant_id: grant.grant_id.clone(),
            signed_payload_sha256: grant.witness_signature.signed_payload_sha256.clone(),
            binding: expected.binding.clone(),
            nonce: uuid::Uuid::new_v4().to_string(),
            execution,
        };
        // Reissuing a different grant id cannot reopen the same decision op.
        let claim_key = decision_resolution_key(expected.binding, expected.op_id);
        let fabric = MemoryFabric::new(self.memory.clone(), expected.subject.workspace_id.clone());
        let event = fabric
            .record_runtime_event_idempotent(
                scope.clone(),
                "approval.decision.resolved",
                match disposition {
                    ClaimDisposition::Consumed => "decision approval consumed",
                    ClaimDisposition::Revoked => "decision approval revoked",
                },
                Some(serde_json::to_string(&claim)?),
                &claim_key,
            )
            .await?;
        let durable = self.approval_receipt(&event, &scope, &claim_key).await?;
        let stored: GrantClaim = serde_json::from_str(
            durable
                .raw_payload_json
                .as_deref()
                .context("decision approval claim payload missing")?,
        )?;
        match disposition {
            ClaimDisposition::Consumed => {
                anyhow::ensure!(stored == claim, "decision approval was already consumed or revoked")
            }
            ClaimDisposition::Revoked => anyhow::ensure!(
                stored.disposition == ClaimDisposition::Revoked
                    && stored.grant_id == claim.grant_id
                    && stored.signed_payload_sha256 == claim.signed_payload_sha256
                    && stored.binding == claim.binding,
                "decision approval was already consumed or resolution conflicts"
            ),
        }
        // A revoke receipt remains valid if the grant expires while persisting.
        // A consumed grant must still be usable when its receipt is returned.
        if disposition == ClaimDisposition::Revoked {
            return Ok(durable);
        }
        grant.verify_for_decision(
            keyring,
            expected.subject,
            expected.binding,
            expected.op_id,
            expected.risk,
            Utc::now(),
        )?;
        self.port.check_live_parity()?;
        Ok(durable)
    }
}
