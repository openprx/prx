//! Durable decision approval review lifecycle.
use super::keys::{ApprovalAuthority, DecisionAuthority};
use super::pending::{
    DecisionApprovalReceipt, DecisionRevocationReceipt, VerifiedDecisionApproval, VerifiedPendingDecisionApproval,
    scope_from_source,
};
use super::{ApprovalLedger, GrantScope};
use crate::acl::approval_grant::{ApprovalGrantV2, WitnessKeyring};
use crate::causal_tree::ids::DecisionId;
use crate::decision::KeyDomain;
use crate::memory::{MemoryFabric, MessageEvent};
use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) local_owner: Option<Box<crate::runtime::h2_entry::LocalOwnerReviewStamp>>,
    pub(crate) pending_mac: [u8; 32],
    pub(crate) approved: bool,
    pub(crate) grants: Vec<ApprovalGrantV2>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SignedReview {
    pub(crate) record: ReviewRecord,
    pub(crate) mac: [u8; 32],
}

pub(crate) enum ApprovalRead {
    ForExecution,
    ForRevocation,
}

impl ApprovalLedger<'_> {
    pub(crate) async fn pending_review_denies(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<bool> {
        let record = &pending.record;
        let review_key = DecisionAuthority(DecisionId::parse(&record.binding.decision_id)?).review_key();
        let Some(event) = self
            .memory
            .find_message_event_by_idempotency_key(&record.subject.workspace_id, &review_key)
            .await?
        else {
            return Ok(false);
        };
        let session = record.subject.session_key.as_deref().unwrap_or("");
        let lineage_matches = event.id > 0
            && event.workspace_id == record.subject.workspace_id
            && event.owner_id.as_deref() == Some(record.subject.owner_id.as_str())
            && event.session_key.as_deref() == Some(session)
            && event.run_id == pending.source.run_id
            && event.causation_event_id.as_deref() == Some(record.source_event_id.as_str())
            && event.idempotency_key.as_deref() == Some(review_key.as_str())
            && event.event_type == "approval.decision.reviewed";
        if !lineage_matches {
            tracing::debug!("pending review lineage did not match; inbox keeps the item");
            return Ok(false);
        }
        let Some(payload) = event.raw_payload_json.as_deref() else {
            tracing::debug!("pending review payload missing; inbox keeps the item");
            return Ok(false);
        };
        let review: SignedReview = match serde_json::from_str(payload) {
            Ok(review) => review,
            Err(error) => {
                tracing::debug!(%error, "pending review payload is not a signed review");
                return Ok(false);
            }
        };
        let pending_mac = self
            .state_key
            .hmac(KeyDomain::ApprovalState, &[b"pending", &serde_json::to_vec(record)?]);
        let authentic = self.state_key.verify_hmac(
            KeyDomain::ApprovalState,
            &[b"review", &serde_json::to_vec(&review.record)?],
            &review.mac,
        ) && review.record.pending_mac == pending_mac;
        if !authentic {
            tracing::debug!("pending review mac did not authenticate; inbox keeps the item");
            return Ok(false);
        }
        Ok(!review.record.approved)
    }

    pub(crate) async fn review_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        approved: bool,
    ) -> anyhow::Result<DecisionApprovalReceipt> {
        self.review_pending_with_local(owner, session, decision_id, approved, None)
            .await
    }

    pub(crate) async fn review_pending_with_local(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        approved: bool,
        local_owner: Option<crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    ) -> anyhow::Result<DecisionApprovalReceipt> {
        let pending = self.load_pending_decision_approval(owner, session, decision_id).await?;
        if approved {
            self.port.verify_pending_console_file_operation(&pending).await?;
        }
        let record = &pending.record;
        self.port
            .verify_h2_review_requirement(&pending, approved, local_owner.as_ref())?;
        let risk = record.approval_risk()?;
        let keyring = WitnessKeyring::global()?;
        let pending_mac = self
            .state_key
            .hmac(KeyDomain::ApprovalState, &[b"pending", &serde_json::to_vec(record)?]);
        let mut grants = Vec::new();
        if approved {
            for op_id in &record.op_ids {
                grants.push(ApprovalGrantV2::issue_decision_one_shot(
                    keyring,
                    record.subject.clone(),
                    op_id,
                    risk,
                    record.binding.clone(),
                    record.expires_at,
                )?);
            }
        }
        let review = ReviewRecord {
            local_owner: local_owner.map(Box::new),
            pending_mac,
            approved,
            grants,
        };
        let proposed = SignedReview {
            mac: self
                .state_key
                .hmac(KeyDomain::ApprovalState, &[b"review", &serde_json::to_vec(&review)?]),
            record: review,
        };
        let key = DecisionAuthority(decision_id).review_key();
        let scope = scope_from_source(&pending.source);
        let fabric = MemoryFabric::new(self.memory.clone(), record.subject.workspace_id.clone());
        let event = fabric
            .record_runtime_event_idempotent(
                scope.clone(),
                "approval.decision.reviewed",
                "human decision approval review recorded",
                Some(serde_json::to_string(&proposed)?),
                &key,
            )
            .await?;
        let durable = self
            .memory
            .find_owned_message_event(&record.subject.workspace_id, owner, &event.event_id)
            .await?
            .context("decision review receipt is not durable")?;
        anyhow::ensure!(
            durable.id > 0
                && durable.idempotency_key.as_deref() == Some(key.as_str())
                && durable.event_type == "approval.decision.reviewed"
                && durable.owner_id.as_deref() == Some(owner)
                && durable.session_key.as_deref() == Some(session)
                && durable.run_id == pending.source.run_id
                && durable.causation_event_id.as_deref() == Some(record.source_event_id.as_str())
                && serde_json::to_value(&event)? == serde_json::to_value(&durable)?,
            "decision review receipt differs from pending source"
        );
        let stored: SignedReview = serde_json::from_str(
            durable
                .raw_payload_json
                .as_deref()
                .context("decision review payload missing")?,
        )?;
        anyhow::ensure!(
            self.state_key.verify_hmac(
                KeyDomain::ApprovalState,
                &[b"review", &serde_json::to_vec(&stored.record)?],
                &stored.mac
            ) && stored.record.pending_mac == pending_mac,
            "decision review does not authenticate this pending scope"
        );
        anyhow::ensure!(
            stored.record.approved == approved && stored.record.local_owner == proposed.record.local_owner,
            "pending decision already has a different review"
        );
        anyhow::ensure!(
            stored.record.grants.len() == if approved { record.op_ids.len() } else { 0 },
            "decision review grant set is incomplete"
        );
        for (grant, op_id) in stored.record.grants.iter().zip(&record.op_ids) {
            anyhow::ensure!(
                grant.expires_at <= record.expires_at,
                "decision grant outlives pending approval"
            );
            let expected = GrantScope {
                subject: &record.subject,
                binding: &record.binding,
                op_id,
                risk,
            };
            self.store_decision_grant(scope.clone(), grant, &expected, keyring)
                .await?;
        }
        self.port.check_live_parity()?;
        // The batch is usable only while every operation grant remains valid.
        // Pending metadata can outlive the shorter signed grant lifetime.
        let expires_at = stored
            .record
            .grants
            .iter()
            .map(|grant| grant.expires_at)
            .min()
            .unwrap_or(record.expires_at)
            .min(record.expires_at);
        anyhow::ensure!(expires_at > Utc::now(), "decision approval expired during review");
        if approved {
            // Read through the same complete-batch verifier used by resume
            // preparation. An issued subset can never produce a ready receipt.
            let verified = self
                .load_reviewed_decision_approval(owner, session, decision_id)
                .await?;
            anyhow::ensure!(
                verified.grants.len() == stored.record.grants.len()
                    && verified.pending.record.binding == record.binding,
                "decision approval batch changed during review"
            );
        }
        if !approved {
            self.port.retire_pending(record);
        }
        Ok(DecisionApprovalReceipt {
            decision_id: record.binding.decision_id.clone(),
            approved,
            grant_count: stored.record.grants.len(),
            expires_at,
        })
    }

    pub(crate) async fn load_reviewed_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<VerifiedDecisionApproval> {
        self.read_reviewed_decision_approval(owner, session, decision_id, ApprovalRead::ForExecution)
            .await
    }

    pub(crate) async fn validate_pending_decision_resume(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<()> {
        self.load_reviewed_decision_approval(owner, session, decision_id)
            .await
            .map(|_| ())
    }

    pub(crate) async fn revoke_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<DecisionRevocationReceipt> {
        let approval = self
            .read_reviewed_decision_approval(owner, session, decision_id, ApprovalRead::ForRevocation)
            .await?;
        let record = &approval.pending.record;
        let scope = scope_from_source(&approval.pending.source);
        let keyring = WitnessKeyring::global()?;
        for (grant, operation) in approval.grants.iter().zip(&record.op_ids) {
            self.revoke_decision_grant(
                scope.clone(),
                grant,
                &GrantScope {
                    subject: &record.subject,
                    binding: &record.binding,
                    op_id: operation,
                    risk: record.approval_risk()?,
                },
                keyring,
            )
            .await?;
        }
        self.port.retire_pending(record);
        Ok(DecisionRevocationReceipt {
            decision_id: decision_id.to_string(),
            revoked_grants: approval.grants.len(),
        })
    }

    pub(crate) async fn read_reviewed_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        purpose: ApprovalRead,
    ) -> anyhow::Result<VerifiedDecisionApproval> {
        let pending = self.load_pending_decision_approval(owner, session, decision_id).await?;
        let record = &pending.record;
        let risk = record.approval_risk()?;
        let keyring = WitnessKeyring::global()?;
        let review_key = DecisionAuthority(decision_id).review_key();
        let review_event = self
            .memory
            .find_message_event_by_idempotency_key(&record.subject.workspace_id, &review_key)
            .await?
            .context("decision review missing")?;
        let verify_lineage = |event: &MessageEvent, key: &str, kind: &str| -> anyhow::Result<()> {
            anyhow::ensure!(
                event.id > 0
                    && event.workspace_id == record.subject.workspace_id
                    && event.owner_id.as_deref() == Some(owner)
                    && event.session_key.as_deref() == Some(session)
                    && event.run_id == pending.source.run_id
                    && event.source == pending.source.source
                    && event.channel == pending.source.channel
                    && event.sender == pending.source.sender
                    && event.config_generation_id == pending.source.config_generation_id
                    && event.config_source_revision == pending.source.config_source_revision
                    && event.causation_event_id.as_deref() == Some(record.source_event_id.as_str())
                    && event.idempotency_key.as_deref() == Some(key)
                    && event.event_type == kind,
                "decision review grant lineage changed"
            );
            Ok(())
        };
        verify_lineage(&review_event, &review_key, "approval.decision.reviewed")?;
        let review: SignedReview = serde_json::from_str(
            review_event
                .raw_payload_json
                .as_deref()
                .context("decision review payload missing")?,
        )?;
        let pending_mac = self
            .state_key
            .hmac(KeyDomain::ApprovalState, &[b"pending", &serde_json::to_vec(record)?]);
        anyhow::ensure!(
            self.state_key.verify_hmac(
                KeyDomain::ApprovalState,
                &[b"review", &serde_json::to_vec(&review.record)?],
                &review.mac
            ) && review.record.pending_mac == pending_mac
                && review.record.approved
                && !record.op_ids.is_empty()
                && review.record.grants.len() == record.op_ids.len(),
            "decision review is denied, incomplete or unauthenticated"
        );
        for (grant, op_id) in review.record.grants.iter().zip(&record.op_ids) {
            grant.verify_for_decision(keyring, &record.subject, &record.binding, op_id, risk, Utc::now())?;
            anyhow::ensure!(
                grant.expires_at <= record.expires_at,
                "decision grant outlives pending scope"
            );
            let issued_key = DecisionAuthority(decision_id).issued_key(&grant.grant_id);
            let issued = self
                .memory
                .find_message_event_by_idempotency_key(&record.subject.workspace_id, &issued_key)
                .await?
                .context("decision review grant was not issued")?;
            verify_lineage(&issued, &issued_key, "approval.decision.issued")?;
            let stored: ApprovalGrantV2 = serde_json::from_str(
                issued
                    .raw_payload_json
                    .as_deref()
                    .context("issued approval payload missing")?,
            )?;
            anyhow::ensure!(stored == *grant, "issued approval differs from reviewed batch");
            // Any existing resolution closes this operation, including a
            // malformed/conflicting row. Absence is only a preflight fact;
            // final dispatch must still win the atomic resolution write.
            let resolution_key = DecisionAuthority(decision_id).resolution_key(op_id);
            if matches!(purpose, ApprovalRead::ForExecution) {
                anyhow::ensure!(
                    self.memory
                        .find_message_event_by_idempotency_key(&record.subject.workspace_id, &resolution_key,)
                        .await?
                        .is_none(),
                    "decision approval is already resolved"
                );
            }
        }
        self.port
            .verify_h2_review_requirement(&pending, true, review.record.local_owner.as_deref())?;
        self.port.check_live_parity()?;
        let now = Utc::now();
        anyhow::ensure!(
            record.expires_at > now && review.record.grants.iter().all(|grant| grant.is_time_valid(now)),
            "decision approval expired during preparation"
        );
        Ok(VerifiedDecisionApproval {
            pending,
            grants: review.record.grants,
            local_owner: review.record.local_owner,
        })
    }
}
