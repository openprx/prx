//! Durable decision approval pending lifecycle.
use super::ApprovalLedger;
use super::keys::{ApprovalAuthority, DecisionAuthority};
use crate::acl::approval_grant::{ApprovalGrantV2, DecisionApprovalBinding, RiskLevel, Subject};
use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::store::DecisionScope;
use crate::decision::KeyDomain;
use crate::memory::{MemoryFabric, MessageEvent, MessageEventScope};
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EncryptedCall {
    pub(crate) version: u8,
    pub(crate) nonce: [u8; 12],
    pub(crate) ciphertext: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) h2_origin: Option<Box<crate::runtime::h2_entry::EntryOriginRecord>>,
    pub(crate) subject: Subject,
    pub(crate) binding: DecisionApprovalBinding,
    pub(crate) source_event_id: String,
    pub(crate) source_mac: [u8; 32],
    pub(crate) run_id: String,
    pub(crate) decision_digests: Vec<(String, String)>,
    pub(crate) policy_version: String,
    pub(crate) capabilities: Vec<(String, [u8; 32], [u8; 32])>,
    pub(crate) op_ids: Vec<String>,
    pub(crate) effect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) risk: Option<RiskLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) schema_dialect: Option<crate::tools::schema::ToolSchemaDialect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sealed_call: Option<EncryptedCall>,
    pub(crate) expires_at: DateTime<Utc>,
}

pub(crate) struct VerifiedDecisionApproval {
    pub(crate) pending: VerifiedPendingDecisionApproval,
    pub(crate) local_owner: Option<Box<crate::runtime::h2_entry::LocalOwnerReviewStamp>>,
    pub(crate) grants: Vec<ApprovalGrantV2>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SignedPending {
    pub(crate) record: PendingRecord,
    pub(crate) mac: [u8; 32],
}

pub(crate) struct VerifiedPendingDecisionApproval {
    pub(crate) record: PendingRecord,
    pub(crate) source: MessageEvent,
}

#[derive(Serialize)]
pub(crate) struct DecisionApprovalReceipt {
    pub(crate) decision_id: String,
    pub(crate) approved: bool,
    pub(crate) grant_count: usize,
    pub(crate) expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub(crate) struct DecisionRevocationReceipt {
    pub(crate) decision_id: String,
    pub(crate) revoked_grants: usize,
}

#[derive(Serialize)]
pub(crate) struct DecisionApprovalView {
    pub(crate) decision_id: String,
    pub(crate) tool: Option<String>,
    pub(crate) arguments: Option<serde_json::Value>,
    pub(crate) arguments_redacted: bool,
    pub(crate) original_call_available: bool,
    pub(crate) effect: String,
    pub(crate) call_digest: String,
    pub(crate) pending_expires_at: DateTime<Utc>,
    pub(crate) single_use: bool,
}

pub(crate) struct PendingApprovalListItem {
    pub(crate) session_id: String,
    pub(crate) view: DecisionApprovalView,
}

pub(crate) struct PendingApprovalPage {
    pub(crate) items: Vec<PendingApprovalListItem>,
    pub(crate) next_cursor: Option<i64>,
}

impl PendingRecord {
    pub(crate) fn approval_risk(&self) -> anyhow::Result<RiskLevel> {
        Ok(self.risk.unwrap_or(match self.effect.as_str() {
            "read_only" => RiskLevel::Low,
            "local_write" => RiskLevel::Medium,
            "process_exec" | "external_send" | "credential_or_config" | "destructive" => RiskLevel::High,
            _ => anyhow::bail!("pending approval effect is unknown"),
        }))
    }
}

#[cfg(test)]
impl VerifiedPendingDecisionApproval {
    /// The exact operations the pending call asks the owner to approve.
    pub(crate) fn op_ids(&self) -> &[String] {
        &self.record.op_ids
    }
}

fn pending_decision_id(idempotency_key: Option<&str>) -> Option<DecisionId> {
    let key = idempotency_key?;
    let rest = key.strip_prefix("decision-approval:")?.strip_suffix(":pending")?;
    DecisionId::parse(rest).ok()
}

pub(crate) fn scope_from_source(source: &MessageEvent) -> MessageEventScope {
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

impl ApprovalLedger<'_> {
    pub(crate) async fn inspect_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<DecisionApprovalView> {
        let pending = self.load_pending_decision_approval(owner, session, decision_id).await?;
        self.inspect_verified_pending(&pending).await
    }

    pub(crate) async fn inspect_verified_pending(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<DecisionApprovalView> {
        self.port.verify_pending_console_file_operation(pending).await?;
        let call = pending
            .record
            .sealed_call
            .as_ref()
            .map(|_| self.port.open_pending_call(pending))
            .transpose()?;
        let arguments = call
            .as_ref()
            .map(|call| crate::security::redaction::redact_json_structure(&call.arguments))
            .transpose()?;
        let arguments_redacted = call
            .as_ref()
            .is_some_and(|call| arguments.as_ref() != Some(&call.arguments));
        self.port.check_live_parity()?;
        anyhow::ensure!(Utc::now() < pending.record.expires_at, "pending approval expired");
        Ok(DecisionApprovalView {
            decision_id: pending.record.binding.decision_id.clone(),
            tool: call.as_ref().map(|call| call.name.clone()),
            arguments,
            arguments_redacted,
            original_call_available: call.is_some(),
            effect: pending.record.effect.clone(),
            call_digest: hex::encode(pending.record.binding.call_digest),
            pending_expires_at: pending.record.expires_at,
            single_use: true,
        })
    }

    pub(crate) async fn list_pending_decision_approvals(
        &self,
        owner: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<PendingApprovalPage> {
        const MAX_SCAN: usize = 256;
        anyhow::ensure!(self.enabled, "pending approval requires enabled durable service");
        anyhow::ensure!(
            !owner.is_empty() && after_id >= 0 && (1..=100).contains(&limit),
            "invalid approval inbox page"
        );
        let workspace = self.workspace.as_str();
        anyhow::ensure!(!workspace.is_empty(), "approval inbox workspace missing");
        let mut cursor = after_id;
        let mut scanned = 0usize;
        let mut items = Vec::new();
        let mut exhausted = false;
        while items.len() < limit && scanned < MAX_SCAN {
            let batch_limit = (limit - items.len()).clamp(1, 100);
            let batch = self
                .memory
                .list_owned_message_events_by_type(&workspace, owner, "approval.decision.pending", cursor, batch_limit)
                .await?;
            if batch.is_empty() {
                exhausted = true;
                break;
            }
            let batch_len = batch.len();
            let mut examined_batch = true;
            for event in batch {
                scanned = scanned.saturating_add(1);
                cursor = event.id;
                if let Some(item) = self.inbox_item_for_event(owner, &workspace, &event).await? {
                    items.push(item);
                }
                if items.len() == limit || scanned >= MAX_SCAN {
                    examined_batch = false;
                    break;
                }
            }
            if examined_batch && batch_len < batch_limit {
                exhausted = true;
                break;
            }
            if !examined_batch || items.len() == limit || scanned >= MAX_SCAN {
                break;
            }
        }
        let next_cursor = if exhausted { None } else { Some(cursor) };
        Ok(PendingApprovalPage { items, next_cursor })
    }

    pub(crate) async fn inbox_item_for_event(
        &self,
        owner: &str,
        workspace: &str,
        event: &MessageEvent,
    ) -> anyhow::Result<Option<PendingApprovalListItem>> {
        if event.owner_id.as_deref() != Some(owner) || event.workspace_id != workspace {
            return Ok(None);
        }
        let Some(session_id) = event.session_key.clone().filter(|session| !session.is_empty()) else {
            return Ok(None);
        };
        let Some(decision_id) = pending_decision_id(event.idempotency_key.as_deref()) else {
            return Ok(None);
        };
        let pending = match self
            .load_pending_decision_approval(owner, &session_id, decision_id)
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                tracing::debug!(%error, "pending approval omitted from inbox");
                return Ok(None);
            }
        };
        if self.pending_review_denies(&pending).await? || self.pending_operations_resolved(&pending).await? {
            return Ok(None);
        }
        match self.inspect_verified_pending(&pending).await {
            Ok(view) => Ok(Some(PendingApprovalListItem { session_id, view })),
            Err(error) => {
                tracing::debug!(%error, "pending approval omitted from inbox");
                Ok(None)
            }
        }
    }

    pub(crate) async fn pending_operations_resolved(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<bool> {
        if pending.record.op_ids.is_empty() {
            return Ok(false);
        }
        let workspace = pending.record.subject.workspace_id.as_str();
        for op_id in &pending.record.op_ids {
            let key = DecisionAuthority(DecisionId::parse(&pending.record.binding.decision_id)?).resolution_key(op_id);
            if self
                .memory
                .find_message_event_by_idempotency_key(workspace, &key)
                .await?
                .is_none()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) async fn persist_pending_record_receipt(
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
                DecisionAuthority(DecisionId::parse(&record_decision)?).pending_key(),
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

    pub(crate) async fn load_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<VerifiedPendingDecisionApproval> {
        anyhow::ensure!(self.enabled, "pending approval requires enabled durable service");
        let workspace = self.workspace.as_str();
        let key = DecisionAuthority(decision_id).pending_key();
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
                && record.binding.catalog_generation == self.catalog_generation
                && record.policy_version == self.policy_version
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
        self.port.check_live_parity()?;
        Ok(VerifiedPendingDecisionApproval { record, source })
    }
}
