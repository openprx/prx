//! Durable storage primitives for decision-scoped approvals. These do not
//! authorize execution: pending-state verification and the pre-commit gate
//! must still bind the claimed grant to the actual call.
use super::TurnDecisionService;
use crate::acl::approval_grant::{ApprovalGrantV2, DecisionApprovalBinding, RiskLevel, Subject, WitnessKeyring};
use crate::memory::{MemoryFabric, MessageEvent, MessageEventScope};
use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(super) struct DecisionGrantScope<'a> {
    pub subject: &'a Subject,
    pub binding: &'a DecisionApprovalBinding,
    pub op_id: &'a str,
    pub risk: RiskLevel,
}

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
pub(super) struct ResumeClaimScope {
    pub(super) decision_id: String,
    pub(super) source_event_id: String,
    pub(super) run_id: String,
}

pub(super) fn decision_resolution_key(binding: &DecisionApprovalBinding, op_id: &str) -> String {
    let operation = hex::encode(Sha256::digest(op_id.as_bytes()));
    format!("decision-approval:{}:{operation}:resolution", binding.decision_id)
}

impl TurnDecisionService {
    async fn verify_approval_event_scope(
        &self,
        scope: &MessageEventScope,
        expected: &DecisionGrantScope<'_>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.recordable && self.durable_key && self.generation.effective.causal_tree.enabled,
            "decision approval storage requires durable enabled service"
        );
        anyhow::ensure!(
            expected.subject.workspace_id == self.generation.effective.workspace_dir.to_string_lossy()
                && scope.owner_id.as_deref() == Some(expected.subject.owner_id.as_str())
                && scope.session_key == expected.subject.session_key
                && expected.binding.catalog_generation == self.catalog.generation().0,
            "decision approval storage scope differs from pinned authority"
        );
        let source_id = scope
            .causation_event_id
            .as_deref()
            .context("approval source event missing")?;
        let source = self
            .memory
            .find_owned_message_event(&expected.subject.workspace_id, &expected.subject.owner_id, source_id)
            .await?
            .context("approval source is not durable")?;
        anyhow::ensure!(
            source.role == "user"
                && source.event_type == "message.created"
                && source.source == scope.source
                && source.channel == scope.channel
                && source.sender == scope.sender
                && source.recipient == scope.recipient
                && source.session_key == scope.session_key
                && source.run_id == scope.run_id
                && source.visibility == scope.visibility
                && source.config_generation_id == scope.config_generation_id
                && source.config_source_revision == scope.config_source_revision,
            "decision approval event scope differs from original input"
        );
        self.check_live_parity()?;
        Ok(())
    }

    /// Persist the exact signed grant; a retry cannot replace its subject,
    /// decision, call, expiry or operation under the same grant id.
    pub(super) async fn store_decision_grant(
        &self,
        scope: MessageEventScope,
        grant: &ApprovalGrantV2,
        expected: &DecisionGrantScope<'_>,
        keyring: &WitnessKeyring,
    ) -> anyhow::Result<()> {
        grant.verify_for_decision(
            keyring,
            expected.subject,
            expected.binding,
            expected.op_id,
            expected.risk,
            Utc::now(),
        )?;
        self.verify_approval_event_scope(&scope, expected).await?;
        let fabric = MemoryFabric::new(self.memory.clone(), expected.subject.workspace_id.clone());
        let idempotency_key = format!("decision-approval:{}:issued", grant.grant_id);
        let event = fabric
            .record_runtime_event_idempotent(
                scope.clone(),
                "approval.decision.issued",
                "decision approval recorded",
                Some(serde_json::to_string(grant)?),
                &idempotency_key,
            )
            .await?;
        let durable = self.approval_receipt(&event, &scope, &idempotency_key).await?;
        let stored: ApprovalGrantV2 = serde_json::from_str(
            durable
                .raw_payload_json
                .as_deref()
                .context("stored decision approval payload missing")?,
        )?;
        anyhow::ensure!(stored == *grant, "stored decision approval conflicts with signed grant");
        Ok(())
    }

    /// The claim has a fresh nonce for each attempt. Atomic event idempotency
    /// chooses one winner, including across processes. An uncertain write is
    /// never retried with authority: a subsequent attempt has a new nonce and
    /// fails closed if the earlier claim reached storage.
    pub(super) async fn claim_decision_grant(
        &self,
        scope: MessageEventScope,
        grant: &ApprovalGrantV2,
        expected: &DecisionGrantScope<'_>,
        keyring: &WitnessKeyring,
        execution: ResumeClaimScope,
    ) -> anyhow::Result<MessageEvent> {
        self.resolve_decision_grant(scope, grant, expected, keyring, Some(execution))
            .await
    }

    /// Revocation competes with consumption for the same durable resolution.
    /// An already claimed execution cannot be retroactively reported revoked.
    /// A repeated matching revocation is idempotent and grants no authority.
    pub(super) async fn revoke_decision_grant(
        &self,
        scope: MessageEventScope,
        grant: &ApprovalGrantV2,
        expected: &DecisionGrantScope<'_>,
        keyring: &WitnessKeyring,
    ) -> anyhow::Result<MessageEvent> {
        self.resolve_decision_grant(scope, grant, expected, keyring, None).await
    }

    async fn resolve_decision_grant(
        &self,
        scope: MessageEventScope,
        grant: &ApprovalGrantV2,
        expected: &DecisionGrantScope<'_>,
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
        self.check_live_parity()?;
        Ok(durable)
    }

    async fn approval_receipt(
        &self,
        event: &MessageEvent,
        scope: &MessageEventScope,
        idempotency_key: &str,
    ) -> anyhow::Result<MessageEvent> {
        let owner = scope.owner_id.as_deref().context("approval receipt owner missing")?;
        let workspace = self.generation.effective.workspace_dir.to_string_lossy();
        let durable = self
            .memory
            .find_owned_message_event(&workspace, owner, &event.event_id)
            .await?
            .context("decision approval receipt is not durable")?;
        anyhow::ensure!(
            durable.id > 0
                && durable.idempotency_key.as_deref() == Some(idempotency_key)
                && durable.owner_id == scope.owner_id
                && durable.session_key == scope.session_key
                && durable.run_id == scope.run_id
                && durable.source == scope.source
                && durable.causation_event_id == scope.causation_event_id
                && serde_json::to_value(&durable)? == serde_json::to_value(event)?,
            "decision approval receipt differs from requested scope"
        );
        Ok(durable)
    }
}
