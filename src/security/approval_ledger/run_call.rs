//! Decision-free, authenticated exact-call approval over MemoryFabric.
use super::keys::{ApprovalAuthority, RunCallAuthority};
use super::pending::scope_from_source;
use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
use crate::decision::{DecisionKey, KeyDomain};
use crate::memory::{Memory, MemoryFabric, MessageEvent};
use crate::tools::capability::execution_binding::{CallDigestSource, decision_call_digest};
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

impl RunCallAuthority {
    pub(crate) fn for_call(
        capability: &crate::causal_tree::branch::CapabilityRef,
        source: CallDigestSource<'_>,
        arguments: &serde_json::Value,
        operation: &str,
        risk: RiskLevel,
    ) -> anyhow::Result<Self> {
        let run_id = source
            .run
            .filter(|run| !run.is_empty())
            .context("approval run missing")?
            .to_owned();
        let argument_hash = serde_json::json!(hex::encode(Sha256::digest(serde_json::to_vec(arguments)?)));
        let call_digest = decision_call_digest("", capability, source, &argument_hash, operation, risk)?;
        Ok(Self { run_id, call_digest })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingCall {
    pub(crate) authority: RunCallAuthority,
    pub(crate) subject: Subject,
    pub(crate) source_event_id: String,
    pub(crate) source_mac: [u8; 32],
    pub(crate) operation: String,
    pub(crate) risk: RiskLevel,
    pub(crate) expires_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallReview {
    pending_mac: [u8; 32],
    approved: bool,
    grant: Option<ApprovalGrantV2>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed<T> {
    record: T,
    mac: [u8; 32],
}

pub(crate) struct PendingCallPage {
    pub(crate) items: Vec<PendingCall>,
    pub(crate) next_cursor: Option<i64>,
}

/// Construct with the service's pinned workspace, state key and witness keyring.
/// Owner/session parameters must come from the authenticated transport envelope.
pub(crate) struct ApprovalLedgerPort<'a> {
    pub(crate) memory: &'a Arc<dyn Memory>,
    pub(crate) state_key: &'a DecisionKey,
    pub(crate) workspace: &'a str,
    pub(crate) keyring: &'a WitnessKeyring,
}

impl ApprovalLedgerPort<'_> {
    fn sign<T: Serialize>(&self, domain: &[u8], record: T) -> anyhow::Result<Signed<T>> {
        let mac = self
            .state_key
            .hmac(KeyDomain::ApprovalState, &[domain, &serde_json::to_vec(&record)?]);
        Ok(Signed { record, mac })
    }

    fn decode<T: Serialize + serde::de::DeserializeOwned>(
        &self,
        event: &MessageEvent,
        domain: &[u8],
    ) -> anyhow::Result<Signed<T>> {
        let signed: Signed<T> = serde_json::from_str(
            event
                .raw_payload_json
                .as_deref()
                .context("call approval payload missing")?,
        )?;
        anyhow::ensure!(
            self.state_key.verify_hmac(
                KeyDomain::ApprovalState,
                &[domain, &serde_json::to_vec(&signed.record)?],
                &signed.mac
            ),
            "call approval authentication failed"
        );
        Ok(signed)
    }

    async fn source(&self, pending: &PendingCall, owner: &str, session: &str) -> anyhow::Result<MessageEvent> {
        anyhow::ensure!(
            !owner.is_empty()
                && !session.is_empty()
                && pending.subject.workspace_id == self.workspace
                && pending.subject.owner_id == owner
                && pending.subject.session_key.as_deref() == Some(session),
            "pending call approval belongs to another scope"
        );
        let source = self
            .memory
            .find_owned_message_event(self.workspace, owner, &pending.source_event_id)
            .await?
            .context("approval source is not durable")?;
        anyhow::ensure!(
            source.id > 0
                && source.role == "user"
                && source.event_type == "message.created"
                && source.workspace_id == self.workspace
                && source.session_key.as_deref() == Some(session)
                && source.run_id.as_deref() == Some(pending.authority.run_id.as_str()),
            "call approval source scope changed"
        );
        Ok(source)
    }

    async fn write<T: Serialize>(
        &self,
        source: &MessageEvent,
        key: &str,
        kind: &str,
        record: &T,
    ) -> anyhow::Result<MessageEvent> {
        let event = MemoryFabric::new(self.memory.clone(), self.workspace)
            .record_runtime_event_idempotent(
                scope_from_source(source),
                kind,
                "call approval lifecycle",
                Some(serde_json::to_string(record)?),
                key,
            )
            .await?;
        let durable = self
            .memory
            .find_owned_message_event(
                self.workspace,
                source.owner_id.as_deref().context("approval owner missing")?,
                &event.event_id,
            )
            .await?
            .context("call approval receipt is not durable")?;
        self.check_event(&durable, source, key, kind)?;
        anyhow::ensure!(
            serde_json::to_value(&event)? == serde_json::to_value(&durable)?,
            "call approval receipt changed"
        );
        Ok(durable)
    }

    fn check_event(&self, event: &MessageEvent, source: &MessageEvent, key: &str, kind: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            event.workspace_id == self.workspace,
            "stored approval belongs to another scope"
        );
        anyhow::ensure!(
            event.id > 0
                && event.owner_id == source.owner_id
                && event.session_key == source.session_key
                && event.run_id == source.run_id
                && event.source == source.source
                && event.channel == source.channel
                && event.sender == source.sender
                && event.config_generation_id == source.config_generation_id
                && event.config_source_revision == source.config_source_revision
                && event.causation_event_id.as_deref() == Some(source.event_id.as_str())
                && event.idempotency_key.as_deref() == Some(key)
                && event.event_type == kind,
            "stored approval belongs to another scope"
        );
        Ok(())
    }

    pub(crate) async fn record_pending(
        &self,
        mut pending: PendingCall,
    ) -> anyhow::Result<crate::tools::ToolExecutionStatus> {
        anyhow::ensure!(
            !pending.operation.is_empty()
                && !pending.subject.agent_id.is_empty()
                && !pending.subject.principal_id.is_empty()
                && pending.expires_at > Utc::now()
                && pending.expires_at <= Utc::now() + chrono::Duration::minutes(5),
            "call approval scope is incomplete or expired"
        );
        let source = self
            .source(
                &pending,
                &pending.subject.owner_id,
                pending.subject.session_key.as_deref().unwrap_or(""),
            )
            .await?;
        pending.source_mac = self.state_key.hmac(
            KeyDomain::ApprovalState,
            &[b"call.source", &serde_json::to_vec(&source)?],
        );
        let signed = self.sign(b"call.pending", pending)?;
        let event = self
            .write(
                &source,
                &signed.record.authority.pending_key(),
                "approval.call.pending",
                &signed,
            )
            .await?;
        anyhow::ensure!(
            serde_json::from_str::<serde_json::Value>(
                event.raw_payload_json.as_deref().context("pending payload missing")?
            )? == serde_json::to_value(&signed)?,
            "pending call approval conflicts"
        );
        Ok(crate::tools::ToolExecutionStatus::AwaitingApproval)
    }

    async fn load(
        &self,
        owner: &str,
        session: &str,
        authority: &RunCallAuthority,
    ) -> anyhow::Result<(Signed<PendingCall>, MessageEvent)> {
        let key = authority.pending_key();
        let event = self
            .memory
            .find_message_event_by_idempotency_key(self.workspace, &key)
            .await?
            .context("pending call approval not found")?;
        let signed: Signed<PendingCall> = self.decode(&event, b"call.pending")?;
        anyhow::ensure!(signed.record.authority == *authority, "pending call digest changed");
        let source = self.source(&signed.record, owner, session).await?;
        anyhow::ensure!(
            self.state_key.verify_hmac(
                KeyDomain::ApprovalState,
                &[b"call.source", &serde_json::to_vec(&source)?],
                &signed.record.source_mac
            ),
            "call approval source authentication failed"
        );
        self.check_event(&event, &source, &key, "approval.call.pending")?;
        anyhow::ensure!(signed.record.expires_at > Utc::now(), "pending call approval expired");
        Ok((signed, source))
    }

    pub(crate) async fn review(
        &self,
        owner: &str,
        session: &str,
        authority: &RunCallAuthority,
        approved: bool,
    ) -> anyhow::Result<()> {
        let (pending, source) = self.load(owner, session, authority).await?;
        let grant = if approved {
            let mut grant = ApprovalGrantV2::issue_one_shot(
                self.keyring,
                pending.record.subject.clone(),
                IssuerAuthority::HumanReview,
                &pending.record.operation,
                pending.record.risk,
            )?;
            grant.expires_at = grant.expires_at.min(pending.record.expires_at);
            crate::acl::approval_grant::sign_grant(self.keyring, &mut grant)?;
            Some(grant)
        } else {
            None
        };
        let proposed = self.sign(
            b"call.review",
            CallReview {
                pending_mac: pending.mac,
                approved,
                grant,
            },
        )?;
        let event = self
            .write(&source, &authority.review_key(), "approval.call.reviewed", &proposed)
            .await?;
        let stored: Signed<CallReview> = self.decode(&event, b"call.review")?;
        anyhow::ensure!(
            stored.record.pending_mac == pending.mac && stored.record.approved == approved,
            "pending call already has a different review"
        );
        if let Some(grant) = &stored.record.grant {
            self.verify_grant(&pending.record, grant)?;
            let issued = self
                .write(&source, &authority.issued_key(""), "approval.call.issued", &stored)
                .await?;
            anyhow::ensure!(
                issued.raw_payload_json == event.raw_payload_json,
                "issued call approval changed"
            );
        }
        Ok(())
    }

    fn verify_grant(&self, pending: &PendingCall, grant: &ApprovalGrantV2) -> anyhow::Result<()> {
        grant.verify_for_operation(self.keyring, &pending.operation, pending.risk, Utc::now())?;
        anyhow::ensure!(
            grant.subject == pending.subject
                && grant.issuer.authority == IssuerAuthority::HumanReview
                && grant.max_uses == 1
                && grant.uses_consumed == 0
                && grant.capability.op_id == pending.operation
                && grant.capability.op_id_match == crate::acl::approval_grant::OpIdMatch::Exact
                && grant.expires_at <= pending.expires_at,
            "call approval grant scope changed"
        );
        Ok(())
    }

    pub(crate) async fn claim(
        &self,
        owner: &str,
        session: &str,
        authority: &RunCallAuthority,
    ) -> anyhow::Result<MessageEvent> {
        self.resolve(owner, session, authority, false).await
    }

    pub(crate) async fn revoke(
        &self,
        owner: &str,
        session: &str,
        authority: &RunCallAuthority,
    ) -> anyhow::Result<MessageEvent> {
        self.resolve(owner, session, authority, true).await
    }

    async fn resolve(
        &self,
        owner: &str,
        session: &str,
        authority: &RunCallAuthority,
        revoked: bool,
    ) -> anyhow::Result<MessageEvent> {
        let (pending, source) = self.load(owner, session, authority).await?;
        let issued_key = authority.issued_key("");
        let issued = self
            .memory
            .find_message_event_by_idempotency_key(self.workspace, &issued_key)
            .await?
            .context("call approval was not issued")?;
        self.check_event(&issued, &source, &issued_key, "approval.call.issued")?;
        let review: Signed<CallReview> = self.decode(&issued, b"call.review")?;
        anyhow::ensure!(
            review.record.pending_mac == pending.mac && review.record.approved,
            "call approval review changed"
        );
        let grant = review.record.grant.context("call approval grant missing")?;
        self.verify_grant(&pending.record, &grant)?;
        let claim = self.sign(
            b"call.resolution",
            (authority, &grant.grant_id, revoked, uuid::Uuid::new_v4().to_string()),
        )?;
        let event = self
            .write(&source, &authority.resolution_key(""), "approval.call.resolved", &claim)
            .await?;
        anyhow::ensure!(
            serde_json::from_str::<serde_json::Value>(
                event
                    .raw_payload_json
                    .as_deref()
                    .context("resolution payload missing")?
            )? == serde_json::to_value(&claim)?,
            "call approval was already consumed or revoked"
        );
        if !revoked {
            self.verify_grant(&pending.record, &grant)?;
        }
        Ok(event)
    }

    pub(crate) async fn list_pending_calls(
        &self,
        owner: &str,
        after: i64,
        limit: usize,
    ) -> anyhow::Result<PendingCallPage> {
        anyhow::ensure!(
            !owner.is_empty() && after >= 0 && (1..=100).contains(&limit),
            "invalid approval inbox page"
        );
        let mut cursor = after;
        let mut items = Vec::new();
        for _ in 0..256 {
            let batch = self
                .memory
                .list_owned_message_events_by_type(self.workspace, owner, "approval.call.pending", cursor, 1)
                .await?;
            let Some(event) = batch.first() else {
                return Ok(PendingCallPage {
                    items,
                    next_cursor: None,
                });
            };
            cursor = event.id;
            let signed: Signed<PendingCall> = self.decode(event, b"call.pending")?;
            let record = signed.record;
            if self
                .load(
                    owner,
                    record.subject.session_key.as_deref().unwrap_or(""),
                    &record.authority,
                )
                .await
                .is_err()
            {
                continue;
            }
            if self
                .memory
                .find_message_event_by_idempotency_key(self.workspace, &record.authority.resolution_key(""))
                .await?
                .is_some()
            {
                continue;
            }
            if let Some(review) = self
                .memory
                .find_message_event_by_idempotency_key(self.workspace, &record.authority.review_key())
                .await?
            {
                let review: Signed<CallReview> = self.decode(&review, b"call.review")?;
                if !review.record.approved {
                    continue;
                }
            }
            items.push(record);
            if items.len() == limit {
                break;
            }
        }
        Ok(PendingCallPage {
            items,
            next_cursor: Some(cursor),
        })
    }
}
