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
        let bound_call = decision_call_digest("", capability, source, &argument_hash, operation, risk)?;
        let mut digest = Sha256::new();
        digest.update(b"prx.call.approval.v1");
        digest.update(bound_call);
        let call_digest = digest.finalize().into();
        Ok(Self { run_id, call_digest })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingCall {
    authority: RunCallAuthority,
    subject: Subject,
    source_event_id: String,
    source_mac: [u8; 32],
    operation: String,
    risk: RiskLevel,
    expires_at: DateTime<Utc>,
}

impl PendingCall {
    pub(crate) const fn authority(&self) -> &RunCallAuthority {
        &self.authority
    }
    pub(crate) const fn subject(&self) -> &Subject {
        &self.subject
    }
    #[cfg(test)]
    pub(crate) fn source_event_id(&self) -> &str {
        &self.source_event_id
    }
    pub(crate) fn view(&self) -> super::pending::DecisionApprovalView {
        super::pending::DecisionApprovalView {
            decision_id: String::new(),
            call_id: Some(self.authority.call_id()),
            tool: None,
            arguments: None,
            arguments_redacted: true,
            original_call_available: false,
            effect: "approval_required".into(),
            call_digest: hex::encode(self.authority.call_digest),
            pending_expires_at: self.expires_at,
            single_use: true,
        }
    }
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
        envelope: &crate::runtime::envelope::RuntimeEnvelope,
        capability: &crate::causal_tree::branch::CapabilityRef,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> anyhow::Result<PendingCall> {
        let owner = envelope
            .owner_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .context("approval owner missing")?;
        anyhow::ensure!(
            envelope.workspace_id == self.workspace && !envelope.session_key.is_empty(),
            "call approval scope is incomplete"
        );
        let source = self
            .memory
            .find_owned_message_event(
                self.workspace,
                owner,
                envelope
                    .source_message_event_id
                    .as_deref()
                    .context("approval source missing")?,
            )
            .await?
            .context("approval source is not durable")?;
        anyhow::ensure!(
            source.id > 0
                && source.role == "user"
                && source.event_type == "message.created"
                && source.workspace_id == self.workspace
                && source.owner_id.as_deref() == Some(owner)
                && source.session_key.as_deref() == Some(envelope.session_key.as_str())
                && source.run_id == envelope.run_id,
            "call approval source scope changed"
        );
        let (operation, risk) = match crate::security::approval_operation::grant_op_for_call(tool, arguments) {
            Some(crate::security::approval_operation::GrantOp::ResourceExact(op, risk)) => (op, risk),
            _ => anyhow::bail!("call approval requires an exact resource operation"),
        };
        let authority = RunCallAuthority::for_call(
            capability,
            CallDigestSource {
                workspace: &source.workspace_id,
                owner,
                session: &envelope.session_key,
                run: source.run_id.as_deref(),
                event: Some(&source.event_id),
                channel: source.channel.as_deref(),
                sender: source.sender.as_deref(),
                generation: source.config_generation_id,
                revision: source.config_source_revision.as_deref(),
            },
            arguments,
            &operation,
            risk,
        )?;
        if self
            .memory
            .find_message_event_by_idempotency_key(self.workspace, &authority.pending_key())
            .await?
            .is_some()
        {
            return self.inspect(owner, &envelope.session_key, &authority).await;
        }
        let pending = PendingCall {
            authority,
            subject: Subject {
                workspace_id: self.workspace.into(),
                owner_id: owner.into(),
                session_key: source.session_key.clone(),
                agent_id: source
                    .agent_id
                    .clone()
                    .filter(|agent| !agent.is_empty())
                    .unwrap_or_else(|| "prx_agent".into()),
                principal_id: owner.into(),
            },
            source_event_id: source.event_id.clone(),
            source_mac: self.state_key.hmac(
                KeyDomain::ApprovalState,
                &[b"call.source", &serde_json::to_vec(&source)?],
            ),
            operation,
            risk,
            expires_at: Utc::now() + chrono::Duration::minutes(4),
        };
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
        Ok(signed.record)
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

    pub(crate) async fn inspect(
        &self,
        owner: &str,
        session: &str,
        authority: &RunCallAuthority,
    ) -> anyhow::Result<PendingCall> {
        Ok(self.load(owner, session, authority).await?.0.record)
    }

    pub(crate) async fn review(
        &self,
        owner: &str,
        session: &str,
        authority: &RunCallAuthority,
        approved: bool,
    ) -> anyhow::Result<DateTime<Utc>> {
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
        Ok(stored
            .record
            .grant
            .as_ref()
            .map_or(pending.record.expires_at, |grant| grant.expires_at))
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

    pub(crate) async fn claim_if_approved(
        &self,
        owner: &str,
        session: &str,
        authority: &RunCallAuthority,
    ) -> anyhow::Result<Option<ApprovalGrantV2>> {
        let Some(issued) = self
            .memory
            .find_message_event_by_idempotency_key(self.workspace, &authority.issued_key(""))
            .await?
        else {
            return Ok(None);
        };
        let signed: Signed<CallReview> = self.decode(&issued, b"call.review")?;
        self.claim(owner, session, authority).await?;
        Ok(Some(signed.record.grant.context("call approval grant missing")?))
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
            let signed: Signed<PendingCall> = match self.decode(event, b"call.pending") {
                Ok(signed) => signed,
                Err(error) => {
                    tracing::warn!(event_id = %event.event_id, error = %error, "skipping corrupt call approval pending event");
                    continue;
                }
            };
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
                let review: Signed<CallReview> = match self.decode(&review, b"call.review") {
                    Ok(review) => review,
                    Err(error) => {
                        tracing::warn!(event_id = %review.event_id, error = %error, "skipping corrupt call approval review event");
                        continue;
                    }
                };
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
