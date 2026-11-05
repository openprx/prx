//! Authenticated durable intent to resume a reviewed decision call.
use super::ApprovalLedger;
use super::keys::{ApprovalAuthority, DecisionAuthority};
use super::pending::scope_from_source;
use crate::acl::approval_grant::{ApprovalGrantV2, DecisionApprovalBinding};
use crate::causal_tree::ids::DecisionId;
use crate::decision::KeyDomain;
use crate::memory::{MemoryFabric, MessageEvent};
use anyhow::Context;
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct ResumeIntent {
    binding: DecisionApprovalBinding,
    source_event_id: String,
    source_mac: [u8; 32],
    run_id: String,
    policy_version: String,
    reviewed_grants_mac: [u8; 32],
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct SignedResumeIntent {
    record: ResumeIntent,
    mac: [u8; 32],
}
/// Source-proof routing metadata, captured by the runtime admission owner.
/// This metadata alone is never execution authority.
pub(crate) struct ResumeRequestScope<'a> {
    pub(crate) decision_id: DecisionId,
    pub(crate) workspace: &'a str,
    pub(crate) owner: &'a str,
    pub(crate) source_id: &'a str,
    pub(crate) run_id: &'a str,
}

impl ApprovalLedger<'_> {
    pub(crate) async fn record_decision_resume(
        &self,
        request: ResumeRequestScope<'_>,
        source: &MessageEvent,
        binding: &DecisionApprovalBinding,
        grants: &[ApprovalGrantV2],
    ) -> anyhow::Result<MessageEvent> {
        let source_id = request.source_id;
        let record = ResumeIntent {
            binding: binding.clone(),
            source_event_id: source.event_id.clone(),
            source_mac: self.state_key.hmac(
                KeyDomain::ApprovalState,
                &[b"resume-source", &serde_json::to_vec(source)?],
            ),
            run_id: request.run_id.to_owned(),
            policy_version: self.policy_version.to_owned(),
            reviewed_grants_mac: self.state_key.hmac(
                KeyDomain::ApprovalState,
                &[b"resume-grants", &serde_json::to_vec(grants)?],
            ),
        };
        let signed = SignedResumeIntent {
            mac: self
                .state_key
                .hmac(KeyDomain::ApprovalState, &[b"resume", &serde_json::to_vec(&record)?]),
            record,
        };
        let key = DecisionAuthority(request.decision_id).resume_key(source_id);
        let expected = serde_json::to_value(&signed)?;
        let receipt = MemoryFabric::new(self.memory.clone(), request.workspace)
            .record_runtime_event_idempotent(
                scope_from_source(source),
                "approval.decision.resume_requested",
                "decision resume request admitted",
                Some(serde_json::to_string(&signed)?),
                &key,
            )
            .await?;
        let durable = self
            .memory
            .find_owned_message_event(request.workspace, request.owner, &receipt.event_id)
            .await?
            .context("resume admission receipt is not durable")?;
        anyhow::ensure!(
            durable.id > 0
                && durable.event_type == "approval.decision.resume_requested"
                && durable.idempotency_key.as_deref() == Some(key.as_str())
                && durable.session_key == source.session_key
                && durable.run_id == source.run_id
                && durable.causation_event_id.as_deref() == Some(source_id)
                && serde_json::to_value(&durable)? == serde_json::to_value(&receipt)?
                && serde_json::from_str::<serde_json::Value>(
                    durable
                        .raw_payload_json
                        .as_deref()
                        .context("resume receipt payload missing")?
                )? == expected,
            "resume admission receipt conflicts with source"
        );
        self.port.check_live_parity()?;
        Ok(durable)
    }
}
