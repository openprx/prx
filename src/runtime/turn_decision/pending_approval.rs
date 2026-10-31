//! Authenticated pending approval metadata. No caller-supplied operation or
//! digest can create a record; issuance requires the service's gated context.
pub(super) mod collection;
mod h2;
mod sealed_call;
use super::{TurnDecisionContext, TurnDecisionService, TurnExecutionDirective};
use crate::acl::approval_grant::{ApprovalGrantV2, DecisionApprovalBinding, RiskLevel, Subject, WitnessKeyring};
use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::store::DecisionScope;
use crate::decision::KeyDomain;
use crate::memory::{MemoryFabric, MessageEvent, MessageEventScope};
use crate::runtime::turn_source::TurnSourceProof;
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    h2_origin: Option<crate::runtime::h2_entry::EntryOriginRecord>,
    subject: Subject,
    binding: DecisionApprovalBinding,
    source_event_id: String,
    source_mac: [u8; 32],
    run_id: String,
    decision_digests: Vec<(String, String)>,
    policy_version: String,
    capabilities: Vec<(String, [u8; 32], [u8; 32])>,
    op_ids: Vec<String>,
    effect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    risk: Option<RiskLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schema_dialect: Option<crate::tools::schema::ToolSchemaDialect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sealed_call: Option<sealed_call::EncryptedCall>,
    expires_at: DateTime<Utc>,
}

/// A durable pending or declined collection result. Only writer/readback can construct it.
pub(super) struct CollectedApprovalResult {
    event: MessageEvent,
}
impl CollectedApprovalResult {
    pub(super) const fn event(&self) -> &MessageEvent {
        &self.event
    }
}

impl PendingRecord {
    fn approval_risk(&self) -> anyhow::Result<RiskLevel> {
        Ok(self.risk.unwrap_or(match self.effect.as_str() {
            "read_only" => RiskLevel::Low,
            "local_write" => RiskLevel::Medium,
            "process_exec" | "external_send" | "credential_or_config" | "destructive" => RiskLevel::High,
            _ => anyhow::bail!("pending approval effect is unknown"),
        }))
    }
}

/// A complete authenticated persisted review, still not execution authority.
/// Final claim and current-call validation remain mandatory.
pub(super) struct VerifiedDecisionApproval {
    pending: VerifiedPendingDecisionApproval,
    local_owner: Option<crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    grants: Vec<ApprovalGrantV2>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedPending {
    record: PendingRecord,
    mac: [u8; 32],
}

/// Constructed only after MAC, durable source, decision and current policy
/// validation. It is metadata for human review, never execution authority.
pub(crate) struct VerifiedPendingDecisionApproval {
    record: PendingRecord,
    source: MessageEvent,
}

#[cfg(test)]
impl VerifiedPendingDecisionApproval {
    /// The exact operations the pending call asks the owner to approve.
    pub(crate) fn op_ids(&self) -> &[String] {
        &self.record.op_ids
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local_owner: Option<crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    pending_mac: [u8; 32],
    approved: bool,
    grants: Vec<ApprovalGrantV2>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedReview {
    record: ReviewRecord,
    mac: [u8; 32],
}

/// Deliberately excludes signed grants, source text, and private scope fields.
#[derive(Serialize)]
pub(crate) struct DecisionApprovalReceipt {
    decision_id: String,
    approved: bool,
    grant_count: usize,
    expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub(crate) struct DecisionRevocationReceipt {
    decision_id: String,
    revoked_grants: usize,
}

enum ApprovalRead {
    ForExecution,
    ForRevocation,
}

/// Owner-authorized review projection. It contains neither reusable grants
/// nor raw operation strings (which can themselves contain command secrets).
#[derive(Serialize)]
pub(crate) struct DecisionApprovalView {
    decision_id: String,
    tool: Option<String>,
    arguments: Option<serde_json::Value>,
    arguments_redacted: bool,
    original_call_available: bool,
    effect: String,
    call_digest: String,
    pending_expires_at: DateTime<Utc>,
    single_use: bool,
}

impl TurnDecisionService {
    async fn verify_pending_console_file_operation(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<()> {
        if pending.source.source != crate::memory::MessageEventSource::Console || pending.record.sealed_call.is_none() {
            return Ok(());
        }
        let call = self.open_pending_call(pending)?;
        if !matches!(call.name.as_str(), "file_write" | "file_edit") {
            return Ok(());
        }
        let path = call
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .context("pending Console file path missing")?;
        let filesystem = self
            .pending_console_filesystem(&pending.source)
            .await?
            .context("pending Console filesystem unavailable")?;
        let (operation, risk) = self.scoped_file_operation(Some(&filesystem), &call.name, path).await?;
        anyhow::ensure!(
            pending.record.op_ids.as_slice() == [operation] && pending.record.approval_risk()? == risk,
            "pending Console file namespace or operation changed"
        );
        Ok(())
    }

    pub(crate) async fn inspect_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<DecisionApprovalView> {
        let pending = self.load_pending_decision_approval(owner, session, decision_id).await?;
        self.verify_pending_console_file_operation(&pending).await?;
        let call = pending
            .record
            .sealed_call
            .as_ref()
            .map(|_| self.open_pending_call(&pending))
            .transpose()?;
        let arguments = call
            .as_ref()
            .map(|call| crate::security::redaction::redact_json_structure(&call.arguments))
            .transpose()?;
        let arguments_redacted = call
            .as_ref()
            .is_some_and(|call| arguments.as_ref() != Some(&call.arguments));
        self.check_live_parity()?;
        anyhow::ensure!(Utc::now() < pending.record.expires_at, "pending approval expired");
        Ok(DecisionApprovalView {
            decision_id: pending.record.binding.decision_id,
            tool: call.as_ref().map(|call| call.name.clone()),
            arguments,
            arguments_redacted,
            original_call_available: call.is_some(),
            effect: pending.record.effect,
            call_digest: hex::encode(pending.record.binding.call_digest),
            pending_expires_at: pending.record.expires_at,
            single_use: true,
        })
    }

    pub(super) async fn record_pending_decision_approval(
        &self,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
    ) -> anyhow::Result<()> {
        self.resolve_agent_model(proof)?;
        self.check_live_parity()?;
        let TurnExecutionDirective::RequestApproval { approval_scope } = &context.directive else {
            anyhow::bail!("decision has no pending approval scope");
        };
        anyhow::ensure!(
            context.source_actor_matches(Some(proof.policy_channel()), Some(proof.policy_sender()))
                && approval_scope.decision_id == context.decision_id()
                && approval_scope.owner == proof.owner_id()
                && approval_scope.workspace == proof.workspace_id()
                && approval_scope.session.as_deref() == Some(proof.session_key())
                && approval_scope.catalog_generation == *self.catalog.generation()
                && approval_scope.single_use
                && !approval_scope.op_ids.is_empty()
                && !approval_scope.capability_refs.is_empty()
                && approval_scope.expires_at > Utc::now(),
            "pending approval differs from its issued decision scope"
        );
        let fresh = crate::causal_tree::directive::directive_from_selection(
            &context.engine,
            &crate::causal_tree::directive::DirectiveContext {
                current_generation: *self.catalog.generation(),
                approval_expires_at: approval_scope.expires_at,
                discovery_limit: 1,
            },
        )?;
        anyhow::ensure!(
            matches!(fresh, TurnExecutionDirective::RequestApproval { approval_scope: fresh } if fresh == *approval_scope),
            "pending approval does not match the gated selection"
        );
        self.catalog.resolve(&approval_scope.capability_refs)?;
        let link = self.settlement_ref(Arc::clone(context))?;
        link.validate_durable_chain().await?;
        let source_id = proof.source_event_id().context("pending approval source missing")?;
        let source = self
            .memory
            .find_owned_message_event(proof.workspace_id(), proof.owner_id(), source_id)
            .await?
            .context("pending approval source is not durable")?;
        anyhow::ensure!(
            source.role == "user"
                && source.event_type == "message.created"
                && source.session_key.as_deref() == Some(proof.session_key())
                && source.run_id.as_deref() == Some(proof.run_id())
                && crate::runtime::turn_source::subject_task_id(source.subject.as_ref()) == proof.task_id()
                && context.source_actor_matches(source.channel.as_deref(), source.sender.as_deref())
                && crate::runtime::turn_source::recorded_input_matches(proof.surface(), &source, proof.routing_input()),
            "pending approval source differs from admitted input"
        );
        let store_workspace = self.store.workspace_id().context("approval decision store missing")?;
        let decision_scope = DecisionScope::Owner {
            workspace_id: Arc::clone(&store_workspace),
            owner_id: Arc::from(proof.owner_id()),
        };
        let mut decision_digests = Vec::with_capacity(context.hop_ids().len());
        for id in context.hop_ids() {
            let row = self
                .store
                .reader()
                .replay_record(&decision_scope, id)?
                .context("pending approval decision is not durable")?;
            decision_digests.push((id.to_string(), row.digest));
        }
        let principal = crate::memory::principal::OwnerPrincipal::new(
            proof.workspace_id(),
            proof.policy_channel(),
            proof.policy_sender(),
            proof.session_key(),
            vec![crate::memory::principal::Role::Anonymous],
        );
        let record = PendingRecord {
            h2_origin: proof.h2_entry_origin().map(|origin| origin.record().clone()),
            subject: Subject {
                agent_id: format!("prx:agent:{}", proof.policy_channel()),
                principal_id: principal.principal_id,
                owner_id: proof.owner_id().to_owned(),
                workspace_id: proof.workspace_id().to_owned(),
                session_key: Some(proof.session_key().to_owned()),
            },
            binding: DecisionApprovalBinding {
                decision_id: context.decision_id().to_string(),
                catalog_generation: approval_scope.catalog_generation.0,
                call_digest: approval_scope
                    .call_digest
                    .context("pending approval has no proven call digest")?,
            },
            source_event_id: source.event_id.clone(),
            source_mac: self
                .state_key
                .hmac(KeyDomain::ApprovalState, &[b"source", &serde_json::to_vec(&source)?]),
            run_id: proof.run_id().to_owned(),
            decision_digests,
            policy_version: self.policy_version.to_string(),
            capabilities: approval_scope
                .capability_refs
                .iter()
                .map(|reference| {
                    (
                        reference.id.as_str().to_owned(),
                        reference.fingerprint.0,
                        reference.generation.0,
                    )
                })
                .collect(),
            op_ids: approval_scope.op_ids.iter().map(|op| op.as_str().to_owned()).collect(),
            effect: approval_scope.effect.as_str().to_owned(),
            risk: None,
            schema_dialect: None,
            sealed_call: None,
            expires_at: proof.h2_entry_origin().map_or(approval_scope.expires_at, |origin| {
                approval_scope.expires_at.min(origin.record().authority_expires_at)
            }),
        };
        let expires_at = record.expires_at;
        self.persist_pending_record(record, &source).await?;
        if let Some(owner) = context.recursive_allowance.get() {
            owner.retain_pending(expires_at);
        }
        Ok(())
    }

    async fn persist_pending_record(&self, record: PendingRecord, source: &MessageEvent) -> anyhow::Result<()> {
        self.persist_pending_record_receipt(record, source).await.map(|_| ())
    }

    async fn persist_collection_pending_record(
        &self,
        context: Arc<TurnDecisionContext>,
        record: PendingRecord,
        source: &MessageEvent,
    ) -> anyhow::Result<()> {
        let proof = context
            .approval_collection_proof()
            .context("collection proof missing")?;
        anyhow::ensure!(
            context.service_id == self.service_id
                && context
                    .approval_collection_started
                    .load(std::sync::atomic::Ordering::Acquire)
                && record.binding.decision_id == context.decision_id().to_string()
                && record.capabilities.as_slice()
                    == [(
                        proof.capability().id.as_str().to_owned(),
                        proof.capability().fingerprint.0,
                        proof.capability().generation.0
                    )]
                && record.subject.workspace_id == source.workspace_id
                && source.owner_id.as_deref() == Some(record.subject.owner_id.as_str())
                && record.subject.session_key == source.session_key
                && source.run_id.as_deref() == Some(record.run_id.as_str())
                && record.source_event_id == source.event_id
                && record.sealed_call.is_some()
                && !record.op_ids.is_empty(),
            "collection pending commitment mismatch"
        );
        let expires_at = record.expires_at;
        let event = self.persist_pending_record_receipt(record, source).await?;
        if let Some(owner) = context.recursive_allowance.get() {
            owner.retain_pending(expires_at);
        }
        context
            .collected_approval_result
            .set(Box::new(CollectedApprovalResult { event }))
            .map_err(|_| anyhow::anyhow!("collection pending receipt already installed"))
    }

    async fn persist_pending_record_receipt(
        &self,
        record: PendingRecord,
        source: &MessageEvent,
    ) -> anyhow::Result<MessageEvent> {
        let record_owner = record.subject.owner_id.clone();
        let record_decision = record.binding.decision_id.clone();
        let signed = SignedPending {
            mac: self
                .state_key
                .hmac(KeyDomain::ApprovalState, &[b"pending", &serde_json::to_vec(&record)?]),
            record,
        };
        let expected = serde_json::to_value(&signed)?;
        let fabric = MemoryFabric::new(self.memory.clone(), &source.workspace_id);
        let event = fabric
            .record_runtime_event_idempotent(
                scope_from_source(source),
                "approval.decision.pending",
                "decision awaiting human approval",
                Some(serde_json::to_string(&signed)?),
                format!("decision-approval:{}:pending", record_decision),
            )
            .await?;
        let durable = self
            .memory
            .find_owned_message_event(&source.workspace_id, &record_owner, &event.event_id)
            .await?
            .context("pending approval receipt is not durable")?;
        anyhow::ensure!(
            durable.id > 0
                && durable.event_type == "approval.decision.pending"
                && durable.session_key == source.session_key
                && durable.run_id == source.run_id
                && serde_json::to_value(&event)? == serde_json::to_value(&durable)?
                && serde_json::from_str::<serde_json::Value>(
                    durable
                        .raw_payload_json
                        .as_deref()
                        .context("pending approval payload missing")?
                )? == expected,
            "pending approval receipt conflicts with issued scope"
        );
        Ok(durable)
    }

    /// Only the authenticated owner can resolve this pending record. Review is
    /// one durable transition for the complete operation set; retries restore
    /// the exact signed grant batch instead of issuing new grants.
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
    async fn review_pending_with_local(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        approved: bool,
        local_owner: Option<crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    ) -> anyhow::Result<DecisionApprovalReceipt> {
        let pending = self.load_pending_decision_approval(owner, session, decision_id).await?;
        if approved {
            self.verify_pending_console_file_operation(&pending).await?;
        }
        let record = &pending.record;
        self.verify_h2_review_requirement(&pending, approved, local_owner.as_ref())?;
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
            local_owner,
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
        let key = format!("decision-approval:{decision_id}:review");
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
            let expected = super::approval::DecisionGrantScope {
                subject: &record.subject,
                binding: &record.binding,
                op_id,
                risk,
            };
            self.store_decision_grant(scope.clone(), grant, &expected, keyring)
                .await?;
        }
        self.check_live_parity()?;
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
            self.recursive_allowances.retire_pending(
                &record.subject.workspace_id,
                &record.subject.owner_id,
                record.subject.session_key.as_deref().unwrap_or_default(),
                &record.run_id,
            );
        }
        Ok(DecisionApprovalReceipt {
            decision_id: record.binding.decision_id.clone(),
            approved,
            grant_count: stored.record.grants.len(),
            expires_at,
        })
    }

    /// Read-only preparation: authenticate original pending provenance, the
    /// winning human review, and every separately issued grant. No use is
    /// consumed here; a later atomic resolution must still win before dispatch.
    pub(super) async fn load_reviewed_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<VerifiedDecisionApproval> {
        self.read_reviewed_decision_approval(owner, session, decision_id, ApprovalRead::ForExecution)
            .await
    }

    /// Read-only ingress preflight for a previously reviewed operation. This
    /// returns no grant or execution authority; dispatch must independently
    /// revalidate and win the durable atomic one-use resolution.
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

    /// Revocation reads authenticated metadata even when a resolution already
    /// exists. The durable writer still requires that resolution to be the
    /// matching revocation; it never turns an existing consumption into one.
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
                &super::approval::DecisionGrantScope {
                    subject: &record.subject,
                    binding: &record.binding,
                    op_id: operation,
                    risk: record.approval_risk()?,
                },
                keyring,
            )
            .await?;
        }
        self.recursive_allowances.retire_pending(
            &record.subject.workspace_id,
            &record.subject.owner_id,
            record.subject.session_key.as_deref().unwrap_or_default(),
            &record.run_id,
        );
        Ok(DecisionRevocationReceipt {
            decision_id: decision_id.to_string(),
            revoked_grants: approval.grants.len(),
        })
    }

    async fn read_reviewed_decision_approval(
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
        let review_key = format!("decision-approval:{decision_id}:review");
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
            let issued_key = format!("decision-approval:{}:issued", grant.grant_id);
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
            let resolution_key = super::approval::decision_resolution_key(&record.binding, op_id);
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
        self.verify_h2_review_requirement(&pending, true, review.record.local_owner.as_ref())?;
        self.check_live_parity()?;
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

    pub(crate) async fn load_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<VerifiedPendingDecisionApproval> {
        anyhow::ensure!(
            self.recordable && self.durable_key && self.generation.effective.causal_tree.enabled,
            "pending approval requires enabled durable service"
        );
        let workspace = self.generation.effective.workspace_dir.to_string_lossy();
        let key = format!("decision-approval:{decision_id}:pending");
        let event = self
            .memory
            .find_message_event_by_idempotency_key(&workspace, &key)
            .await?
            .context("pending decision approval not found")?;
        anyhow::ensure!(
            event.id > 0
                && event.workspace_id == workspace
                && event.owner_id.as_deref() == Some(owner)
                && event.session_key.as_deref() == Some(session)
                && event.idempotency_key.as_deref() == Some(key.as_str())
                && event.event_type == "approval.decision.pending",
            "pending decision approval belongs to another scope"
        );
        let signed: SignedPending = serde_json::from_str(
            event
                .raw_payload_json
                .as_deref()
                .context("pending approval payload missing")?,
        )?;
        anyhow::ensure!(
            self.state_key.verify_hmac(
                KeyDomain::ApprovalState,
                &[b"pending", &serde_json::to_vec(&signed.record)?],
                &signed.mac
            ),
            "pending approval authentication failed"
        );
        let record = signed.record;
        anyhow::ensure!(
            record.subject.owner_id == owner
                && record.subject.workspace_id == workspace
                && record.subject.session_key.as_deref() == Some(session)
                && record.binding.decision_id == decision_id.to_string()
                && record.binding.catalog_generation == self.catalog.generation().0
                && record.policy_version == self.policy_version.as_ref()
                && record.expires_at > Utc::now(),
            "pending approval is stale or out of scope"
        );
        anyhow::ensure!(
            event.run_id.as_deref() == Some(record.run_id.as_str())
                && event.causation_event_id.as_deref() == Some(record.source_event_id.as_str()),
            "pending approval marker lineage changed"
        );
        let source = self
            .memory
            .find_owned_message_event(&workspace, owner, &record.source_event_id)
            .await?
            .context("pending approval original input missing")?;
        anyhow::ensure!(
            self.state_key.verify_hmac(
                KeyDomain::ApprovalState,
                &[b"source", &serde_json::to_vec(&source)?],
                &record.source_mac
            ),
            "pending approval original input changed"
        );
        let store_workspace = self.store.workspace_id().context("approval decision store missing")?;
        anyhow::ensure!(
            record.decision_digests.last().map(|hop| hop.0.as_str()) == Some(record.binding.decision_id.as_str()),
            "pending approval final hop changed"
        );
        let decision_scope = DecisionScope::Owner {
            workspace_id: store_workspace,
            owner_id: Arc::from(owner),
        };
        let mut prior = None;
        for (id, digest) in &record.decision_digests {
            let id = DecisionId::parse(id)?;
            let row = self
                .store
                .reader()
                .replay_record(&decision_scope, &id)?
                .context("pending approval decision missing")?;
            anyhow::ensure!(
                row.digest == *digest
                    && row.decision.prior_decision_id == prior
                    && row.decision.session_key.as_deref() == Some(session)
                    && row.decision.run_id.as_deref() == Some(record.run_id.as_str()),
                "pending approval recorded chain changed"
            );
            prior = Some(id);
        }
        self.check_live_parity()?;
        Ok(VerifiedPendingDecisionApproval { record, source })
    }
}

fn scope_from_source(source: &MessageEvent) -> MessageEventScope {
    let mut scope = MessageEventScope::new(source.source.clone(), source.visibility.clone());
    scope.owner_id = source.owner_id.clone();
    scope.channel = source.channel.clone();
    scope.session_key = source.session_key.clone();
    scope.run_id = source.run_id.clone();
    scope.parent_session_key = source.parent_session_key.clone();
    scope.parent_run_id = source.parent_run_id.clone();
    scope.agent_id = source.agent_id.clone();
    scope.persona_id = source.persona_id.clone();
    scope.sender = source.sender.clone();
    scope.recipient = source.recipient.clone();
    scope.causation_event_id = Some(source.event_id.clone());
    scope.config_generation_id = source.config_generation_id;
    scope.config_source_revision = source.config_source_revision.clone();
    scope.attempt_id = source.attempt_id.clone();
    scope.lease_epoch = source.lease_epoch;
    scope
}

/// The execution adapter owns only a reference to the issued turn. It cannot
/// supply a new decision, operation set, actor or source event.
pub(super) struct ExecutionApprovalRecorder {
    pub(super) h2_origin: Option<Arc<crate::runtime::h2_entry::VerifiedEntryOrigin>>,
    pub(super) service: Arc<TurnDecisionService>,
    pub(super) context: Arc<TurnDecisionContext>,
}

#[async_trait::async_trait]
impl crate::tools::capability::execution_binding::DecisionApprovalRecorder for ExecutionApprovalRecorder {
    async fn file_operation(&self, tool: &str, path: &str) -> anyhow::Result<(String, RiskLevel)> {
        use std::sync::atomic::Ordering;
        anyhow::ensure!(
            matches!(tool, "file_write" | "file_edit")
                && self.context.service_id == self.service.service_id
                && !self.context.execution_closed.load(Ordering::Acquire),
            "file approval scope is unavailable"
        );
        let entry = self
            .service
            .catalog
            .entry_for_public_name(tool)
            .context("file capability missing")?;
        anyhow::ensure!(
            self.context
                .execution_refs
                .iter()
                .any(|reference| reference.id == entry.id
                    && reference.fingerprint == entry.fingerprint
                    && reference.generation == entry.generation)
                || self.context.discovery_admission(tool).is_some_and(|admission| {
                    admission.reference.id == entry.id
                        && admission.reference.fingerprint == entry.fingerprint
                        && admission.reference.generation == entry.generation
                }),
            "file capability was not selected or admitted from discovery"
        );
        self.service.check_live_parity()?;
        let operation = self
            .service
            .scoped_file_operation(self.context.owner_filesystem.as_ref(), tool, path)
            .await?;
        self.service.check_live_parity()?;
        anyhow::ensure!(
            !self.context.execution_closed.load(Ordering::Acquire),
            "file approval turn closed"
        );
        Ok(operation)
    }
    async fn record(
        &self,
        call: &crate::tools::capability::execution_binding::VerifiedDecisionCall,
    ) -> anyhow::Result<()> {
        self.service
            .record_pending_execution_call(&self.context, call, self.h2_origin.as_deref())
            .await
    }
}

impl TurnDecisionService {
    async fn record_pending_execution_call(
        &self,
        context: &Arc<TurnDecisionContext>,
        call: &crate::tools::capability::execution_binding::VerifiedDecisionCall,
        h2_origin: Option<&crate::runtime::h2_entry::VerifiedEntryOrigin>,
    ) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        let trusted = context.engine.state.trusted.parts();
        let scope = call.scope();
        let resolved_owner = scope.resolved_owner_id();
        anyhow::ensure!(
            !context.execution_closed.load(Ordering::Acquire)
                && context.execution_trace.get().is_some_and(|trace| !trace.is_sealed())
                && call.binding().decision_id == context.decision_id().to_string()
                && call.binding().catalog_generation == self.catalog.generation().0
                && (context.execution_refs.contains(call.capability())
                    || context.has_discovery_admission(call.capability()))
                && trusted.workspace_id.as_ref() == scope.workspace_id
                && trusted.owner_id.as_deref() == Some(resolved_owner.as_str())
                && trusted.session_key.as_deref() == Some(scope.session_key.as_str())
                && trusted.run_id.as_deref() == scope.run_id.as_deref()
                && trusted.task_id.as_deref() == scope.task_id.as_deref()
                && trusted.source_event_id.as_deref() == scope.source_message_event_id.as_deref()
                && context.source_actor_matches(scope.channel.as_deref(), scope.sender.as_deref()),
            "approval call differs from its active selected decision"
        );
        self.check_live_parity()?;
        self.settlement_ref(Arc::clone(context))?
            .validate_durable_chain()
            .await?;
        let entries = self.catalog.resolve(std::slice::from_ref(call.capability()))?;
        let entry = entries.first().context("approval capability missing")?;
        anyhow::ensure!(!call.operation().is_empty(), "approval operation scope missing");
        let source_id = scope
            .source_message_event_id
            .as_deref()
            .context("approval source missing")?;
        let owner = resolved_owner.as_str();
        let source = self
            .memory
            .find_owned_message_event(&scope.workspace_id, owner, source_id)
            .await?
            .context("approval source is not durable")?;
        anyhow::ensure!(
            source.role == "user"
                && source.event_type == "message.created"
                && source.session_key.as_deref() == Some(scope.session_key.as_str())
                && source.run_id == scope.run_id
                && crate::runtime::turn_source::subject_task_id(source.subject.as_ref()) == scope.task_id.as_deref()
                && source.config_generation_id == scope.config_generation_id
                && source.config_source_revision == scope.config_source_revision
                && context.source_actor_matches(source.channel.as_deref(), source.sender.as_deref())
                && crate::runtime::turn_source::recorded_input_matches(
                    trusted.surface,
                    &source,
                    context.engine.state.routing_input.as_str()
                ),
            "approval call source differs from admitted input"
        );
        let key = format!("decision-approval:{}:pending", context.decision_id());
        if self
            .memory
            .find_message_event_by_idempotency_key(&scope.workspace_id, &key)
            .await?
            .is_some()
        {
            let previous = self
                .load_pending_decision_approval(owner, &scope.session_key, context.decision_id())
                .await?;
            anyhow::ensure!(
                &previous.record.binding == call.binding(),
                "decision already awaits a different call"
            );
            return Ok(());
        }
        let decision_scope = DecisionScope::Owner {
            workspace_id: self.store.workspace_id().context("approval store missing")?,
            owner_id: Arc::from(owner),
        };
        let mut decision_digests = Vec::with_capacity(context.hop_ids().len());
        for id in context.hop_ids() {
            let row = self
                .store
                .reader()
                .replay_record(&decision_scope, id)?
                .context("approval decision is not durable")?;
            decision_digests.push((id.to_string(), row.digest));
        }
        let channel = scope.channel.as_deref().context("approval channel missing")?;
        let sender = scope.sender.as_deref().context("approval sender missing")?;
        let principal = crate::memory::principal::OwnerPrincipal::new(
            &scope.workspace_id,
            channel,
            sender,
            &scope.session_key,
            vec![crate::memory::principal::Role::Anonymous],
        );
        let mut record = PendingRecord {
            h2_origin: h2_origin.map(|origin| origin.record().clone()),
            subject: Subject {
                agent_id: format!("prx:agent:{channel}"),
                principal_id: principal.principal_id,
                owner_id: owner.to_owned(),
                workspace_id: scope.workspace_id.clone(),
                session_key: Some(scope.session_key.clone()),
            },
            binding: call.binding().clone(),
            source_event_id: source.event_id.clone(),
            source_mac: self
                .state_key
                .hmac(KeyDomain::ApprovalState, &[b"source", &serde_json::to_vec(&source)?]),
            run_id: scope.run_id.clone().context("approval run missing")?,
            decision_digests,
            policy_version: self.policy_version.to_string(),
            capabilities: vec![(
                call.capability().id.as_str().to_owned(),
                call.capability().fingerprint.0,
                call.capability().generation.0,
            )],
            op_ids: vec![call.operation().to_owned()],
            effect: entry.effect.as_str().to_owned(),
            risk: Some(call.risk()),
            schema_dialect: Some(call.schema_dialect()),
            sealed_call: None,
            expires_at: h2_origin.map_or_else(
                || Utc::now() + chrono::Duration::minutes(5),
                |origin| {
                    origin
                        .record()
                        .authority_expires_at
                        .min(Utc::now() + chrono::Duration::minutes(5))
                },
            ),
        };
        record.sealed_call = Some(self.seal_pending_call(&record, call.command())?);
        let expires_at = record.expires_at;
        self.persist_pending_record(record, &source).await?;
        if let Some(owner) = context.recursive_allowance.get() {
            owner.retain_pending(expires_at);
        }
        Ok(())
    }
}

/// Source-authenticated, normalized original call, ready for a new constrained
/// resume decision. This is deliberately not an executor permit.
pub(super) struct PreparedDecisionResume {
    approval: VerifiedDecisionApproval,
    command: crate::tools::ToolExecutionCommand,
}

impl TurnDecisionService {
    pub(super) async fn prepare_decision_resume(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        mut command: crate::tools::ToolExecutionCommand,
    ) -> anyhow::Result<PreparedDecisionResume> {
        use crate::security::approval_operation::{GrantOp, grant_op_for_call};
        use crate::tools::capability::execution_binding::{CallDigestSource, decision_call_digest};
        let approval = self
            .load_reviewed_decision_approval(owner, session, decision_id)
            .await?;
        let record = &approval.pending.record;
        let source = &approval.pending.source;

        let entry = self
            .catalog
            .entry_for_public_name(&command.name)
            .context("resume tool unavailable")?;
        anyhow::ensure!(
            record.capabilities.len() == 1
                && record
                    .capabilities
                    .first()
                    .is_some_and(|(id, schema, generation)| id == entry.id.as_str()
                        && *schema == entry.fingerprint.0
                        && *generation == entry.generation.0),
            "resume capability differs from pending call"
        );
        let descriptor = self
            .execution
            .current_descriptor(&command.name)
            .context("resume descriptor unavailable")?;
        anyhow::ensure!(
            crate::tools::capability::fingerprint::tool_spec_fingerprint(
                &descriptor.public_name,
                &descriptor.description,
                &descriptor.parameters
            ) == entry.fingerprint,
            "resume tool schema changed"
        );
        let dialect = record
            .schema_dialect
            .context("pending approval lacks actual-call schema provenance")?;
        crate::tools::execution::enforce_command_contract(&mut command.arguments, &descriptor, dialect)
            .map_err(anyhow::Error::msg)?;
        if record.sealed_call.is_some() {
            let stored = self.open_pending_call(&approval.pending)?;
            anyhow::ensure!(
                stored.name == command.name && stored.arguments == command.arguments,
                "resume differs from the persisted original call"
            );
        }
        let (operation, risk) = match grant_op_for_call(&command.name, &command.arguments) {
            Some(GrantOp::Command(operation)) => (operation, RiskLevel::High),
            Some(GrantOp::ResourceExact(operation, risk)) => (operation, risk),
            Some(GrantOp::ResourceGlob(_, _)) => match command.name.as_str() {
                "message_send" => {
                    self.execution
                        .resolve_approval_operation(&command.name, &command.arguments)
                        .await?
                }
                "memory_store" => (
                    crate::security::approval_operation::memory_write_operation(None),
                    RiskLevel::Medium,
                ),
                "memory_forget" => {
                    let key = command
                        .arguments
                        .get("key")
                        .and_then(serde_json::Value::as_str)
                        .context("resume key missing")?;
                    (
                        crate::security::approval_operation::memory_delete_operation(None, key),
                        RiskLevel::High,
                    )
                }
                "file_write" | "file_edit" => {
                    let path = command
                        .arguments
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .context("resume path missing")?;
                    let filesystem = self.pending_console_filesystem(source).await?;
                    self.scoped_file_operation(filesystem.as_ref(), &command.name, path)
                        .await?
                }
                _ => anyhow::bail!("resume operation is not resolved exactly"),
            },
            None => match entry.op_ids.as_slice() {
                [operation] => (operation.as_str().to_owned(), RiskLevel::High),
                _ => anyhow::bail!("resume operation is unavailable"),
            },
        };
        anyhow::ensure!(
            record.op_ids.as_slice() == [operation.as_str()] && risk == record.approval_risk()?,
            "resume operation or risk changed"
        );
        let capability = crate::causal_tree::branch::CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let digest = decision_call_digest(
            &record.binding.decision_id,
            &capability,
            CallDigestSource {
                workspace: &record.subject.workspace_id,
                owner,
                session,
                run: source.run_id.as_deref(),
                event: Some(&source.event_id),
                channel: source.channel.as_deref(),
                sender: source.sender.as_deref(),
                generation: source.config_generation_id,
                revision: source.config_source_revision.as_deref(),
            },
            &command.arguments,
            &operation,
            risk,
        )?;
        anyhow::ensure!(
            digest == record.binding.call_digest,
            "resume arguments differ from approved call"
        );
        self.check_live_parity()?;
        let now = Utc::now();
        anyhow::ensure!(
            record.expires_at > now && approval.grants.iter().all(|grant| grant.is_time_valid(now)),
            "resume approval expired during preparation"
        );
        // The request supplies only the effect, never retry/ledger identity.
        command.operation_id = format!("decision-resume:{decision_id}");
        command.idempotency_key = Some(format!("decision-resume:{decision_id}"));
        Ok(PreparedDecisionResume { approval, command })
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DecisionResumeRequest {
    Stored(StoredDecisionResumeRequest),
    Explicit(ExplicitDecisionResumeRequest),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDecisionResumeRequest {
    decision_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplicitDecisionResumeRequest {
    decision_id: String,
    tool: String,
    arguments: serde_json::Value,
}

#[derive(Serialize)]
struct ResumeIntent {
    binding: DecisionApprovalBinding,
    source_event_id: String,
    source_mac: [u8; 32],
    run_id: String,
    policy_version: String,
    reviewed_grants_mac: [u8; 32],
}

#[derive(Serialize)]
struct SignedResumeIntent {
    record: ResumeIntent,
    mac: [u8; 32],
}

/// Fresh admitted request plus the authenticated original call. No execution
/// permission is exposed: re-decision and final durable claim still follow.
pub(super) struct AdmittedDecisionResume {
    prepared: PreparedDecisionResume,
    source: MessageEvent,
    receipt: MessageEvent,
    claimed: std::sync::OnceLock<MessageEvent>,
}

/// A successful durable claim, retained privately until the dedicated backend
/// adapter can install its exact-call scope. It cannot be cloned or serialized.
pub(super) struct ClaimedDecisionResume {
    command: crate::tools::ToolExecutionCommand,
    grant: ApprovalGrantV2,
    execution: super::approval::ResumeClaimScope,
    operation: String,
    service: Arc<TurnDecisionService>,
    context: Arc<TurnDecisionContext>,
}

pub(super) struct ExecutionResumeAdapter {
    service: Arc<TurnDecisionService>,
    context: Arc<TurnDecisionContext>,
    proof: crate::runtime::turn_source::OwnedTurnSourceProof,
    envelope: crate::runtime::envelope::RuntimeEnvelope,
}

impl ExecutionResumeAdapter {
    pub(super) fn new(
        service: Arc<TurnDecisionService>,
        context: Arc<TurnDecisionContext>,
        proof: &dyn TurnSourceProof,
        envelope: crate::runtime::envelope::RuntimeEnvelope,
    ) -> Self {
        Self {
            service,
            context,
            proof: crate::runtime::turn_source::OwnedTurnSourceProof::capture(proof),
            envelope,
        }
    }
}

#[async_trait::async_trait]
impl crate::tools::capability::execution_binding::DecisionResumeAdapter for ExecutionResumeAdapter {
    fn verify(
        &self,
        command: &crate::tools::ToolExecutionCommand,
        execution_context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<crate::security::policy::ApprovalGrant> {
        use std::sync::atomic::Ordering;
        let resume = self.context.resume.as_ref().context("resume admission missing")?;
        let (prepared, dialect) = resume.command()?;
        anyhow::ensure!(
            *command == *prepared
                && execution_context.envelope == self.envelope
                && execution_context.schema_dialect == dialect
                && !self.context.execution_closed.load(Ordering::Acquire)
                && self
                    .context
                    .execution_trace
                    .get()
                    .is_some_and(|trace| !trace.is_sealed()),
            "resume call or execution scope changed"
        );
        let selected = self.service.agent_execution_specs(&self.proof, &self.context)?;
        anyhow::ensure!(
            matches!(self.context.directive, TurnExecutionDirective::Execute { .. })
                && selected.specs.len() == 1
                && selected.specs.first().is_some_and(|spec| spec.name == command.name),
            "resume call was not selected"
        );
        let [grant] = resume.prepared.approval.grants.as_slice() else {
            anyhow::bail!("resume requires one exact grant");
        };
        // This wrapper is only injection metadata. The downstream verifier
        // still requires the independently successful durable claim scope.
        Ok(crate::security::policy::ApprovalGrant::from_verified_v2(
            command.name.clone(),
            "PRXOperator",
            grant.clone(),
        ))
    }

    async fn claim(
        &self,
        command: &crate::tools::ToolExecutionCommand,
        execution_context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<Box<dyn crate::security::decision_approval::DecisionGrantAuthority>> {
        self.verify(command, execution_context)?;
        let claimed = self
            .service
            .claim_decision_resume(&self.proof, &self.context, command)
            .await?;
        Ok(Box::new(claimed))
    }
}

impl crate::security::decision_approval::DecisionGrantAuthority for ClaimedDecisionResume {
    fn verify(
        &self,
        grant: &ApprovalGrantV2,
        tool: &str,
        operation: &str,
        risk: RiskLevel,
        caller_principal: Option<&str>,
    ) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let verify = || -> anyhow::Result<()> {
            anyhow::ensure!(
                *grant == self.grant
                    && tool == self.command.name
                    && operation == self.operation
                    && caller_principal == Some(self.grant.subject.principal_id.as_str())
                    && self.context.service_id == self.service.service_id
                    && self.execution.decision_id == self.context.decision_id().to_string()
                    && !self.context.execution_closed.load(Ordering::Acquire)
                    && self
                        .context
                        .execution_trace
                        .get()
                        .is_some_and(|trace| !trace.is_sealed()),
                "decision grant differs from active claimed call"
            );
            self.service.check_live_parity()?;
            let binding = self
                .grant
                .decision_binding
                .as_ref()
                .context("claimed binding missing")?;
            anyhow::ensure!(
                binding.catalog_generation == self.service.catalog.generation().0,
                "claimed decision catalog changed"
            );
            grant.verify_for_decision(
                WitnessKeyring::global()?,
                &self.grant.subject,
                binding,
                operation,
                risk,
                Utc::now(),
            )?;
            Ok(())
        };
        verify().map_err(|error| error.to_string())
    }
}

impl TurnDecisionService {
    /// Final claim for the prepared call, never for model-generated replacement
    /// arguments. The caller must place this after admission/idempotency checks
    /// and immediately before the scoped backend dispatch. A later failure burns
    /// the grant; it does not restore approval for an uncertain execution.
    pub(super) async fn claim_decision_resume(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        command: &crate::tools::ToolExecutionCommand,
    ) -> anyhow::Result<ClaimedDecisionResume> {
        use std::sync::atomic::Ordering;
        let resume = context.resume.as_ref().context("resume admission missing")?;
        let ensure_active = || -> anyhow::Result<()> {
            anyhow::ensure!(
                !context.execution_closed.load(Ordering::Acquire)
                    && context.execution_trace.get().is_some_and(|trace| !trace.is_sealed()),
                "resume execution is closed"
            );
            let selected = self.agent_execution_specs(proof, context)?;
            anyhow::ensure!(
                matches!(context.directive, TurnExecutionDirective::Execute { .. })
                    && selected.specs.len() == 1
                    && selected.specs.first().is_some_and(|spec| spec.name == command.name)
                    && *command == resume.prepared.command,
                "resume dispatch differs from the prepared selected call"
            );
            Ok(())
        };
        ensure_active()?;
        // Reload the original pending/review/issued records and resolve the
        // operation again. This also checks that no resolution already exists.
        let prepared = self
            .prepare_decision_resume(
                proof.owner_id(),
                proof.session_key(),
                DecisionId::parse(&resume.prepared.approval.pending.record.binding.decision_id)?,
                command.clone(),
            )
            .await?;
        anyhow::ensure!(
            prepared.command == *command
                && prepared.approval.grants == resume.prepared.approval.grants
                && serde_json::to_value(&prepared.approval.pending.record)?
                    == serde_json::to_value(&resume.prepared.approval.pending.record)?,
            "resume approval changed after admission"
        );
        // Admission was authenticated earlier, but do not rely on an in-memory
        // source or intent if its durable record is now missing or different.
        for expected in [&resume.source, &resume.receipt] {
            let actual = self
                .memory
                .find_owned_message_event(proof.workspace_id(), proof.owner_id(), &expected.event_id)
                .await?
                .context("resume provenance is no longer durable")?;
            anyhow::ensure!(
                serde_json::to_value(&actual)? == serde_json::to_value(expected)?,
                "resume durable provenance changed"
            );
        }
        self.settlement_ref(Arc::clone(context))?
            .validate_durable_chain()
            .await?;
        ensure_active()?;
        let pending = &prepared.approval.pending;
        let [grant] = prepared.approval.grants.as_slice() else {
            anyhow::bail!("resume dispatch requires one exact grant");
        };
        let [operation] = pending.record.op_ids.as_slice() else {
            anyhow::bail!("resume dispatch requires one exact operation");
        };
        let execution = super::approval::ResumeClaimScope {
            decision_id: context.decision_id().to_string(),
            source_event_id: resume.source.event_id.clone(),
            run_id: proof.run_id().to_owned(),
        };
        anyhow::ensure!(
            execution.decision_id != pending.record.binding.decision_id,
            "resume must consume under a fresh decision"
        );
        let receipt = self
            .claim_decision_grant(
                scope_from_source(&pending.source),
                grant,
                &super::approval::DecisionGrantScope {
                    subject: &pending.record.subject,
                    binding: &pending.record.binding,
                    op_id: operation,
                    risk: pending.record.approval_risk()?,
                },
                WitnessKeyring::global()?,
                execution.clone(),
            )
            .await?;
        resume
            .claimed
            .set(receipt)
            .map_err(|_| anyhow::anyhow!("resume claim already attached"))?;
        // The shared atomic resolution wins against both concurrent resume and
        // revoke. Never roll it back when parity/expiry/closure fails here.
        ensure_active()?;
        Ok(ClaimedDecisionResume {
            command: prepared.command,
            grant: grant.clone(),
            execution,
            operation: operation.clone(),
            service: Arc::clone(self),
            context: Arc::clone(context),
        })
    }
}

impl TurnDecisionService {
    pub(super) async fn admit_decision_resume(
        &self,
        proof: &dyn TurnSourceProof,
    ) -> anyhow::Result<AdmittedDecisionResume> {
        self.resolve_agent_model(proof)?;
        self.check_live_parity()?;
        // Parse the source-owned routing input rather than a second command
        // argument that could silently select a different approved operation.
        let request: DecisionResumeRequest =
            serde_json::from_str(proof.routing_input()).context("resume requires an explicit source-bound request")?;
        let (decision_id, command) = match request {
            DecisionResumeRequest::Stored(request) => {
                let id = DecisionId::parse(&request.decision_id)?;
                let pending = self
                    .load_pending_decision_approval(proof.owner_id(), proof.session_key(), id)
                    .await?;
                (id, self.open_pending_call(&pending)?)
            }
            DecisionResumeRequest::Explicit(request) => (
                DecisionId::parse(&request.decision_id)?,
                crate::tools::ToolExecutionCommand::new(request.tool, request.arguments),
            ),
        };
        let prepared = self
            .prepare_decision_resume(proof.owner_id(), proof.session_key(), decision_id, command)
            .await?;
        let original = &prepared.approval.pending;
        let source_id = proof.source_event_id().context("resume source missing")?;
        anyhow::ensure!(
            source_id != original.record.source_event_id
                && proof.run_id() != original.record.run_id
                && proof.workspace_id() == original.record.subject.workspace_id
                && original.source.channel.as_deref() == Some(proof.policy_channel())
                && original.source.sender.as_deref() == Some(proof.policy_sender()),
            "resume requires a fresh request from the original actor scope"
        );
        let source = self
            .memory
            .find_owned_message_event(proof.workspace_id(), proof.owner_id(), source_id)
            .await?
            .context("resume input is not durable")?;
        anyhow::ensure!(
            source.id > 0
                && source.event_type == "message.created"
                && source.role == "user"
                && source.session_key.as_deref() == Some(proof.session_key())
                && source.run_id.as_deref() == Some(proof.run_id())
                && crate::runtime::turn_source::subject_task_id(source.subject.as_ref()) == proof.task_id()
                && source.channel.as_deref() == Some(proof.policy_channel())
                && source.sender.as_deref() == Some(proof.policy_sender())
                && source.config_generation_id == Some(proof.config_generation_id())
                && source.config_source_revision.as_deref() == Some(proof.config_source_revision())
                && crate::runtime::turn_source::recorded_input_matches(proof.surface(), &source, proof.routing_input()),
            "resume input differs from admitted source"
        );
        let record = ResumeIntent {
            binding: original.record.binding.clone(),
            source_event_id: source.event_id.clone(),
            source_mac: self.state_key.hmac(
                KeyDomain::ApprovalState,
                &[b"resume-source", &serde_json::to_vec(&source)?],
            ),
            run_id: proof.run_id().to_owned(),
            policy_version: self.policy_version.to_string(),
            reviewed_grants_mac: self.state_key.hmac(
                KeyDomain::ApprovalState,
                &[b"resume-grants", &serde_json::to_vec(&prepared.approval.grants)?],
            ),
        };
        let signed = SignedResumeIntent {
            mac: self
                .state_key
                .hmac(KeyDomain::ApprovalState, &[b"resume", &serde_json::to_vec(&record)?]),
            record,
        };
        let key = format!("decision-approval:{decision_id}:resume:{source_id}");
        let expected = serde_json::to_value(&signed)?;
        let receipt = MemoryFabric::new(self.memory.clone(), proof.workspace_id())
            .record_runtime_event_idempotent(
                scope_from_source(&source),
                "approval.decision.resume_requested",
                "decision resume request admitted",
                Some(serde_json::to_string(&signed)?),
                &key,
            )
            .await?;
        let durable = self
            .memory
            .find_owned_message_event(proof.workspace_id(), proof.owner_id(), &receipt.event_id)
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
        self.check_live_parity()?;
        let now = Utc::now();
        anyhow::ensure!(
            original.record.expires_at > now && prepared.approval.grants.iter().all(|grant| grant.is_time_valid(now)),
            "resume approval expired during admission"
        );
        Ok(AdmittedDecisionResume {
            prepared,
            source,
            receipt: durable,
            claimed: std::sync::OnceLock::new(),
        })
    }
}

pub(super) struct ResumeGateEvidence {
    /// Authenticated original source content, still untrusted decision data.
    pub(super) original_request: Arc<str>,
    pub(super) candidate_reference: crate::causal_tree::branch::CapabilityRef,
    pub(super) calls: std::collections::BTreeMap<crate::causal_tree::branch::CandidateId, [u8; 32]>,
    pub(super) approvals:
        std::collections::BTreeMap<[u8; 32], std::collections::BTreeSet<crate::causal_tree::branch::CapabilityRef>>,
    pub(super) grants: Vec<Arc<str>>,
    pub(super) causation: Arc<str>,
}

impl AdmittedDecisionResume {
    pub(super) fn original_run(&self) -> &str {
        &self.prepared.approval.pending.record.run_id
    }

    pub(super) fn claimed_event(&self) -> Option<&MessageEvent> {
        self.claimed.get()
    }
    pub(super) fn command(
        &self,
    ) -> anyhow::Result<(
        &crate::tools::ToolExecutionCommand,
        crate::tools::schema::ToolSchemaDialect,
    )> {
        Ok((
            &self.prepared.command,
            self.prepared
                .approval
                .pending
                .record
                .schema_dialect
                .context("resume dialect missing")?,
        ))
    }

    pub(super) fn gate_evidence(
        &self,
        service: &TurnDecisionService,
        proof: &dyn TurnSourceProof,
    ) -> anyhow::Result<ResumeGateEvidence> {
        use crate::causal_tree::branch::{CandidateId, CapabilityRef, CausalActionKind, Ns8};
        use std::collections::{BTreeMap, BTreeSet};
        let pending = &self.prepared.approval.pending.record;
        anyhow::ensure!(
            self.source.event_id == proof.source_event_id().context("resume source missing")?
                && self.source.run_id.as_deref() == Some(proof.run_id())
                && crate::runtime::turn_source::subject_task_id(self.source.subject.as_ref()) == proof.task_id()
                && pending.subject.owner_id == proof.owner_id()
                && pending.subject.workspace_id == proof.workspace_id()
                && pending.subject.session_key.as_deref() == Some(proof.session_key())
                && pending.policy_version == service.policy_version.as_ref()
                && pending.binding.catalog_generation == service.catalog.generation().0
                && pending.expires_at > Utc::now(),
            "resume gate scope changed or expired"
        );
        service.resolve_agent_model(proof)?;
        service.check_live_parity()?;
        let entry = service
            .catalog
            .entry_for_public_name(&self.prepared.command.name)
            .context("resume capability missing")?;
        anyhow::ensure!(
            pending.capabilities.len() == 1
                && pending
                    .capabilities
                    .first()
                    .is_some_and(|(id, schema, generation)| id == entry.id.as_str()
                        && *schema == entry.fingerprint.0
                        && *generation == entry.generation.0),
            "resume gate capability changed"
        );
        let keyring = WitnessKeyring::global()?;
        anyhow::ensure!(
            self.prepared.approval.grants.len() == pending.op_ids.len() && !pending.op_ids.is_empty(),
            "resume gate grants incomplete"
        );
        for (grant, op) in self.prepared.approval.grants.iter().zip(&pending.op_ids) {
            grant.verify_for_decision(
                keyring,
                &pending.subject,
                &pending.binding,
                op,
                pending.approval_risk()?,
                Utc::now(),
            )?;
        }
        let digest = pending.binding.call_digest;
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        Ok(ResumeGateEvidence {
            original_request: Arc::from(self.prepared.approval.pending.source.content.as_str()),
            candidate_reference: reference.clone(),
            calls: BTreeMap::from([
                (CandidateId::new(CausalActionKind::UseTool, entry.family, None), digest),
                (
                    CandidateId::new(
                        CausalActionKind::UseTool,
                        entry.family,
                        Some(Ns8::of(&service.state_key, &entry.namespace)),
                    ),
                    digest,
                ),
            ]),
            approvals: BTreeMap::from([(digest, BTreeSet::from([reference]))]),
            grants: self
                .prepared
                .approval
                .grants
                .iter()
                .map(|grant| Arc::from(grant.grant_id.as_str()))
                .collect(),
            causation: Arc::from(self.receipt.event_id.as_str()),
        })
    }
}
