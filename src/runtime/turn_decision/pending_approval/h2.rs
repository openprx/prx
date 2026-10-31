//! The local terminal reviews the authenticated original sealed call.
use super::{
    Context, DecisionApprovalReceipt, DecisionId, KeyDomain, TurnDecisionContext, TurnDecisionService, TurnSourceProof,
    VerifiedPendingDecisionApproval,
};
use crate::runtime::h2_entry::{EntryAction, LocalControlAuthority, LocalOwnerReview, LocalOwnerReviewStamp};

impl TurnDecisionService {
    pub(crate) async fn review_h2_local(
        &self,
        authority: &LocalControlAuthority,
        session: &str,
        decision_id: DecisionId,
        expected_digest: &str,
        approved: bool,
    ) -> anyhow::Result<DecisionApprovalReceipt> {
        authority.profile().verify(&self.generation.effective)?;
        let pending = self
            .load_pending_decision_approval(authority.principal(), session, decision_id)
            .await?;
        let record = &pending.record;
        let original = record
            .h2_origin
            .as_ref()
            .context("pending call has no controlled source")?;
        authority.verify_source(original)?;
        let call = self.open_pending_call(&pending)?;
        anyhow::ensure!(
            call.name == "skills_manage",
            "local experiment review names another backend"
        );
        EntryAction::parse(
            call.arguments
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        )?;
        anyhow::ensure!(
            hex::encode(record.binding.call_digest) == expected_digest,
            "local review digest differs from the authenticated sealed call"
        );
        let pending_mac = self
            .state_key
            .hmac(KeyDomain::ApprovalState, &[b"pending", &serde_json::to_vec(record)?]);
        let stamp = if approved {
            Some(
                authority.attest(
                    original,
                    pending_mac,
                    &record.binding.decision_id,
                    record.binding.call_digest,
                    &call.arguments,
                    record
                        .capabilities
                        .first()
                        .context("exact pending capability missing")?
                        .2,
                    record.binding.catalog_generation,
                    record.expires_at,
                )?,
            )
        } else {
            None
        };
        self.review_pending_with_local(authority.principal(), session, decision_id, approved, stamp)
            .await
    }
    pub(super) fn verify_h2_review_requirement(
        &self,
        pending: &VerifiedPendingDecisionApproval,
        approved: bool,
        stamp: Option<&LocalOwnerReviewStamp>,
    ) -> anyhow::Result<()> {
        let Some(_) = pending.record.sealed_call else {
            anyhow::ensure!(
                !approved || pending.record.h2_origin.is_none(),
                "controlled approval requires the original authenticated sealed call"
            );
            return Ok(());
        };
        let call = self.open_pending_call(pending)?;
        let Some(action) = call
            .arguments
            .get("action")
            .and_then(serde_json::Value::as_str)
            .filter(|name| call.name == "skills_manage" && name.starts_with("lifecycle_experiment_"))
        else {
            return Ok(());
        };
        let action = EntryAction::parse(action)?;
        let source = pending
            .record
            .h2_origin
            .as_ref()
            .context("experiment pending call lacks original controlled input")?;
        anyhow::ensure!(
            source.actor == pending.record.subject.owner_id
                && source.workspace == pending.record.subject.workspace_id
                && source.session == pending.record.subject.session_key.as_deref().unwrap_or_default()
                && source.run == pending.record.run_id
                && source.event == pending.record.source_event_id,
            "controlled pending source differs from its authenticated decision"
        );
        if approved && source.kind == "gateway" && action.requires_local_review() {
            let stamp = stamp.context("original local lifecycle owner must review this exact call")?;
            let pending_mac = self.state_key.hmac(
                KeyDomain::ApprovalState,
                &[b"pending", &serde_json::to_vec(&pending.record)?],
            );
            stamp.verify_pending(
                pending_mac,
                source,
                &pending.record.binding.decision_id,
                pending.record.binding.call_digest,
            )?;
        }
        if let Some(stamp) = stamp {
            let pending_mac = self.state_key.hmac(
                KeyDomain::ApprovalState,
                &[b"pending", &serde_json::to_vec(&pending.record)?],
            );
            stamp.verify_pending(
                pending_mac,
                source,
                &pending.record.binding.decision_id,
                pending.record.binding.call_digest,
            )?;
        }
        Ok(())
    }
    pub(in crate::runtime::turn_decision) fn resume_local_owner_review(
        &self,
        context: &TurnDecisionContext,
        proof: &dyn TurnSourceProof,
    ) -> anyhow::Result<Option<LocalOwnerReview>> {
        let resume = context.resume.as_ref().context("controlled resume admission missing")?;
        let approval = &resume.prepared.approval;
        let call = &resume.prepared.command;
        let requested = call.name == "skills_manage"
            && call
                .arguments
                .get("action")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| name.starts_with("lifecycle_experiment_"));
        if !requested {
            return Ok(None);
        }
        let original = approval
            .pending
            .record
            .h2_origin
            .as_ref()
            .context("original pending input was not controlled")?;
        let origin = proof
            .h2_entry_origin()
            .cloned()
            .context("resume has no actual current controlled source")?;
        origin.ensure_live()?;
        let current = origin.record();
        anyhow::ensure!(
            current.kind == original.kind
                && current.profile == original.profile
                && current.actor == original.actor
                && current.workspace == original.workspace
                && current.target_owner == original.target_owner,
            "resume changed the original controlled delegation"
        );
        // This iteration intentionally has no source/binary/budget authority
        // reconstruction across boots. A new exact current request can perform
        // permitted historical maintenance; old pending execution stays closed.
        anyhow::ensure!(
            current.boot == original.boot,
            "old-boot pending execution requires a fresh exact current request"
        );
        match &approval.local_owner {
            Some(stamp) => Ok(Some(LocalOwnerReview::from_signed(origin, stamp.as_ref().clone())?)),
            None => {
                anyhow::ensure!(
                    current.boot == original.boot,
                    "restart requires explicit original local-owner reconfirmation"
                );
                Ok(None)
            }
        }
    }
}
