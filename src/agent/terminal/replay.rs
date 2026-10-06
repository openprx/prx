//! Durable terminal authentication. A verified replay repairs recorded facts;
//! it never reconstructs a live execution context or grants tool authority.

mod operator;
mod router_history;

use super::{DecisionSettlementRef, TurnTerminalCommit, TurnTerminalStatus, terminal_semantic_scope};
use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::store::{DecisionScope, DecisionStoreReader, DecisionView};
use crate::decision::{DecisionKey, KeyDomain};
use crate::memory::{Memory, MessageEvent, MessageEventScope};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

const SIGNATURE_FIELD: &str = "terminal_replay_mac_v1";
const BINDING_FIELD: &str = "terminal_replay_v1";
const MAC_DOMAIN: &[u8] = b"terminal-replay-v1";
const AUXILIARY_DOMAIN: &[u8] = b"terminal-replay-served-auxiliary-v1";
const SOURCE_DOMAIN: &[u8] = b"terminal-replay-source-v1";
const CLAIM_DOMAIN: &[u8] = b"terminal-replay-claim-v1";
const ASSISTANT_DOMAIN: &[u8] = b"terminal-replay-assistant-v1";
const COLLECTION_DOMAIN: &[u8] = b"terminal-replay-collected-pending-v1";
const RESUME_DOMAIN: &[u8] = b"terminal-replay-resume-claim-v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayHop {
    decision_id: String,
    digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventBinding {
    event_id: String,
    mac: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServedAuxiliaryReplay {
    event: EventBinding,
    persisted_reply_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayParent {
    decision_id: String,
    digest: String,
    source: EventBinding,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayBinding {
    version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collected_pending: Option<EventBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    router_read: Option<router_history::RouterReadReplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    served_auxiliary: Option<ServedAuxiliaryReplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<ReplayParent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operator: Option<operator::ReplayOperator>,
    workspace_id: String,
    store_workspace_id: String,
    owner_id: String,
    session_key: String,
    run_id: String,
    terminal_id: String,
    terminal_scope: Value,
    source: EventBinding,
    hops: Vec<ReplayHop>,
    claim: Option<EventBinding>,
    assistant: Option<EventBinding>,
    fixed_close: bool,
    #[serde(default)]
    run_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume_claim: Option<EventBinding>,
}

/// Can only be produced from the service-issued settlement reference before
/// terminal side effects. It retains no key or raw inbound content.
pub(super) struct PreparedReplay(ReplayBinding);

fn event_mac(key: &DecisionKey, domain: &[u8], event: &MessageEvent) -> anyhow::Result<String> {
    Ok(hex::encode(key.hmac(
        KeyDomain::TerminalClaim,
        &[domain, &serde_json::to_vec(event)?],
    )))
}

fn verify_event_mac(key: &DecisionKey, domain: &[u8], event: &MessageEvent, mac: &str) -> anyhow::Result<()> {
    let tag = hex::decode(mac).context("invalid terminal event binding")?;
    anyhow::ensure!(
        key.verify_hmac(KeyDomain::TerminalClaim, &[domain, &serde_json::to_vec(event)?], &tag),
        "terminal replay event authentication failed"
    );
    Ok(())
}

fn event_binding(key: &DecisionKey, domain: &[u8], event: &MessageEvent) -> anyhow::Result<EventBinding> {
    anyhow::ensure!(
        event.id > 0 && !event.event_id.is_empty(),
        "terminal replay requires a durable event"
    );
    Ok(EventBinding {
        event_id: event.event_id.clone(),
        mac: event_mac(key, domain, event)?,
    })
}

fn persisted_scope(scope: &MessageEventScope) -> Value {
    let mut scope = scope.clone();
    for label in [&mut scope.sender, &mut scope.recipient] {
        if let Some(value) = label.as_mut() {
            if crate::providers::byo_dialect_of(value).is_some() {
                *value = crate::providers::redact_endpoint_userinfo(value).into_owned();
            }
        }
    }
    terminal_semantic_scope(&scope)
}

fn event_scope(event: &MessageEvent) -> Value {
    serde_json::json!({
        "source": event.source, "owner_id": event.owner_id, "channel": event.channel,
        "session_key": event.session_key, "parent_session_key": event.parent_session_key,
        "run_id": event.run_id, "parent_run_id": event.parent_run_id,
        "agent_id": event.agent_id, "persona_id": event.persona_id,
        "sender": event.sender, "recipient": event.recipient, "subject": event.subject,
        "goal_id": event.goal_id, "causation_event_id": event.causation_event_id,
        "correlation_id": event.correlation_id, "attempt_id": event.attempt_id,
        "lease_epoch": event.lease_epoch, "config_generation_id": event.config_generation_id,
        "config_source_revision": event.config_source_revision, "visibility": event.visibility,
    })
}

impl PreparedReplay {
    pub(super) async fn capture(link: &DecisionSettlementRef, commit: &TurnTerminalCommit) -> anyhow::Result<Self> {
        link.context
            .validate_silent_terminal(commit.status, commit.provider_outcome.as_ref())?;
        let recorded = link
            .recorded
            .as_ref()
            .context("disabled decision has no durable replay")?;
        let root = link.context.audit_hops().next().context("decision has no source hop")?;
        let trusted = root.state.trusted.parts();
        let owner = trusted.owner_id.as_deref().context("decision source has no owner")?;
        let session = trusted
            .session_key
            .as_deref()
            .context("decision source has no session")?;
        let run = trusted.run_id.as_deref().context("decision source has no run")?;
        let source_id = trusted
            .source_event_id
            .as_deref()
            .context("decision source has no recorded event")?;
        let source = link
            .memory
            .find_owned_message_event(&trusted.workspace_id, owner, source_id)
            .await?
            .context("decision original source event is missing")?;
        anyhow::ensure!(
            source.id > 0
                && source.event_id == source_id
                && source.workspace_id == trusted.workspace_id.as_ref()
                && source.owner_id.as_deref() == Some(owner)
                && source.session_key.as_deref() == Some(session)
                && source.run_id.as_deref() == Some(run)
                && crate::runtime::turn_source::subject_task_id(source.subject.as_ref()) == trusted.task_id.as_deref()
                && link
                    .context
                    .source_actor_matches(source.channel.as_deref(), source.sender.as_deref())
                && source.role == "user"
                && source.event_type == "message.created"
                && crate::runtime::turn_source::recorded_input_matches(
                    trusted.surface,
                    &source,
                    root.state.routing_input.as_str()
                ),
            "decision original source does not match its recorded turn"
        );
        let scope = DecisionScope::Owner {
            workspace_id: Arc::clone(&recorded.store_workspace_id),
            owner_id: Arc::from(owner),
        };
        let expected_records = crate::causal_tree::outcome::DecisionRecordBuilder::build_enabled_chain(
            &link.context,
            &recorded.key,
            true,
            link.record_redacted_projection,
        )?;
        let mut router_read = link
            .context
            .router_replay_facts(&recorded.key)?
            .map(router_history::RouterReadReplay::from_issued)
            .transpose()?;
        if let Some(router) = &router_read {
            router.validate_source(&source)?;
        }
        let mut hops = Vec::with_capacity(link.context.hop_ids().len());
        for (position, hop) in link.context.audit_hops().enumerate() {
            let row = recorded
                .reader
                .replay_record(&scope, &hop.decision_id)?
                .context("decision replay row is missing")?;
            anyhow::ensure!(
                row.decision.enabled_snapshot
                    && !row.decision.legacy_v1
                    && row.decision.has_owner
                    && row.decision.decision_id == hop.decision_id
                    && row.decision.prior_decision_id
                        == position
                            .checked_sub(1)
                            .and_then(|n| link.context.hop_ids().get(n))
                            .copied()
                    && row.decision.session_key.as_deref() == Some(session)
                    && row.decision.run_id.as_deref() == Some(run)
                    && row.source_event_id.as_deref() == Some(source_id)
                    && row.catalog_generation == hop.state.trusted.parts().catalog_generation.to_hex()
                    && row.decision.policy_version == hop.state.trusted.parts().policy_version.as_ref()
                    && row.state_fingerprint == hop.state.material_fingerprint(&recorded.key),
                "persisted decision replay binding differs from the issued context"
            );
            let expected = expected_records
                .get(position)
                .context("issued decision hop is missing")?;
            anyhow::ensure!(
                row.digest == expected.replay_digest(&recorded.store_workspace_id)?,
                "persisted decision facts differ from the service-issued record"
            );
            if let Some(router) = &mut router_read {
                router.capture_row(&row)?;
            }
            hops.push(ReplayHop {
                decision_id: hop.decision_id.to_string(),
                digest: row.digest,
            });
        }
        anyhow::ensure!(!hops.is_empty() && hops.len() <= 5, "invalid decision replay chain");
        // Completion can legitimately outlive the decision-stage deadline.
        // Inspect the issued directive without issuing a fresh execution lease.
        let fixed_close = match link.context.directive() {
            directive @ (crate::causal_tree::directive::TurnExecutionDirective::Clarify { .. }
            | crate::causal_tree::directive::TurnExecutionDirective::RequestApproval { .. }
            | crate::causal_tree::directive::TurnExecutionDirective::Refuse { .. }) => {
                let reply = crate::causal_tree::directive::render_terminal_directive(directive)?;
                commit.status == TurnTerminalStatus::Completed
                    && commit.provider_outcome.is_none()
                    && commit
                        .history
                        .as_ref()
                        .is_some_and(|history| history.assistant_content == reply)
            }
            _ => false,
        };
        if let Some(router) = &router_read {
            router.validate_complete()?;
            router.validate_serving(
                &recorded.key,
                commit.provider_outcome.as_ref(),
                commit.status,
                fixed_close,
            )?;
        }
        let resume_claim = if let Some(expected) = link.context.claimed_resume_event() {
            let stored = link
                .memory
                .find_owned_message_event(&trusted.workspace_id, owner, &expected.event_id)
                .await?
                .context("resume consumption receipt is no longer durable")?;
            anyhow::ensure!(
                serde_json::to_value(&stored)? == serde_json::to_value(expected)?,
                "resume consumption receipt changed before terminal commit"
            );
            Some(event_binding(&recorded.key, RESUME_DOMAIN, &stored)?)
        } else {
            None
        };
        let parent = if let Some(grant) = &link.parent_grant {
            let parent_id = grant.scope().gate_inputs().0.parent_decision_id;
            let (store_workspace, expected_digest) = grant.parent_record_binding();
            let row = recorded
                .reader
                .replay_record(&scope, &parent_id)?
                .context("parent decision replay row is missing")?;
            let proof = grant.parent();
            anyhow::ensure!(
                store_workspace == recorded.store_workspace_id.as_ref()
                    && row.digest == expected_digest
                    && row.decision.directive_kind == "delegate"
                    && row.decision.run_id.as_deref() == Some(proof.run_id())
                    && row.decision.session_key.as_deref() == Some(proof.session_key())
                    && row.source_event_id.as_deref() == proof.source_event_id()
                    && proof.owner_id() == owner
                    && proof.workspace_id() == trusted.workspace_id.as_ref(),
                "parent decision differs from issued child authority"
            );
            let parent_source = link
                .memory
                .find_owned_message_event(
                    &trusted.workspace_id,
                    owner,
                    proof.source_event_id().context("parent source identity missing")?,
                )
                .await?
                .context("parent source event missing")?;
            anyhow::ensure!(
                parent_source.id > 0
                    && Some(parent_source.event_id.as_str()) == proof.source_event_id()
                    && parent_source.workspace_id == trusted.workspace_id.as_ref()
                    && parent_source.owner_id.as_deref() == Some(owner)
                    && parent_source.role == "user"
                    && parent_source.event_type == "message.created"
                    && source.parent_run_id == parent_source.run_id
                    && source.parent_session_key == parent_source.session_key
                    && source.causation_event_id.as_deref() == Some(parent_source.event_id.as_str())
                    && source.run_id != parent_source.run_id
                    && parent_source.run_id.as_deref() == Some(proof.run_id())
                    && parent_source.session_key.as_deref() == Some(proof.session_key())
                    && crate::runtime::turn_source::recorded_input_matches(
                        proof.surface(),
                        &parent_source,
                        proof.routing_input()
                    ),
                "parent source differs from issued child authority"
            );
            Some(ReplayParent {
                decision_id: parent_id.to_string(),
                digest: row.digest,
                source: event_binding(&recorded.key, SOURCE_DOMAIN, &parent_source)?,
            })
        } else {
            None
        };
        let operator = operator::prepare(link, &source, trusted.task_id.as_deref()).await?;
        let served_auxiliary = if let Some(served) = link.context.served_auxiliary() {
            let event = served
                .event()
                .context("recorded auxiliary reply has no durable event")?;
            let stored = link
                .memory
                .find_owned_message_event(&trusted.workspace_id, owner, &event.event_id)
                .await?
                .context("served auxiliary event is missing")?;
            anyhow::ensure!(
                serde_json::to_value(&stored)? == serde_json::to_value(event)?
                    && event_scope(event) == persisted_scope(&commit.scope)
                    && event.id > source.id,
                "served auxiliary event differs from its issued turn"
            );
            let reply = &commit
                .history
                .as_ref()
                .context("served auxiliary reply missing")?
                .assistant_content;
            served.validate_reply(reply)?;
            super::served_auxiliary::validate_event(event, reply, &commit.terminal_id)?;
            let persisted = super::redact_persisted_json(serde_json::json!({ "history": commit.history }));
            let persisted_reply = persisted
                .get("history")
                .and_then(|history| history.get("assistant_content"))
                .and_then(Value::as_str)
                .context("persisted auxiliary reply missing")?;
            Some(ServedAuxiliaryReplay {
                event: event_binding(&recorded.key, AUXILIARY_DOMAIN, event)?,
                persisted_reply_sha256: {
                    use sha2::Digest;
                    hex::encode(sha2::Sha256::digest(persisted_reply.as_bytes()))
                },
            })
        } else {
            None
        };

        let collected_pending = link
            .context
            .collected_pending_event()
            .map(|event| event_binding(&recorded.key, COLLECTION_DOMAIN, event))
            .transpose()?;
        let collection = link.context.approval_collection_proof().is_some();
        anyhow::ensure!(
            collected_pending.is_none() || collection,
            "pending collection on ordinary terminal"
        );
        anyhow::ensure!(
            !collection || commit.status != TurnTerminalStatus::Completed || collected_pending.is_some(),
            "completed collection lacks durable pending receipt"
        );
        Ok(Self(ReplayBinding {
            collected_pending,
            version: 1,
            router_read,
            served_auxiliary,
            parent,
            operator,
            workspace_id: trusted.workspace_id.to_string(),
            store_workspace_id: recorded.store_workspace_id.to_string(),
            owner_id: owner.to_owned(),
            session_key: session.to_owned(),
            run_id: run.to_owned(),
            terminal_id: commit.terminal_id.clone(),
            terminal_scope: persisted_scope(&commit.scope),
            source: event_binding(&recorded.key, SOURCE_DOMAIN, &source)?,
            hops,
            claim: None,
            assistant: None,
            fixed_close,
            resume_claim,
            run_capabilities: match link.context.directive() {
                crate::causal_tree::directive::TurnExecutionDirective::Execute { allowed_capability_ids } => {
                    let mut ids: Vec<_> = allowed_capability_ids
                        .iter()
                        .map(|id| id.as_str().to_owned())
                        .chain(
                            link.context
                                .discovered_execution_refs()
                                .iter()
                                .map(|reference| reference.id.as_str().to_owned()),
                        )
                        .collect();
                    ids.sort_unstable();
                    ids.dedup();
                    ids
                }
                crate::causal_tree::directive::TurnExecutionDirective::Delegate { .. } => link
                    .context
                    .execution_refs()
                    .iter()
                    .map(|reference| reference.id.as_str().to_owned())
                    .collect(),
                _ => Vec::new(),
            },
        }))
    }

    pub(super) fn bind_claim(&mut self, key: &DecisionKey, claim: &MessageEvent) -> anyhow::Result<()> {
        self.0.claim = Some(event_binding(key, CLAIM_DOMAIN, claim)?);
        Ok(())
    }

    pub(super) fn seal(
        mut self,
        key: &DecisionKey,
        payload: &mut Value,
        assistant: Option<&MessageEvent>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.0.claim.is_some(), "terminal replay claim is missing");
        self.0.assistant = assistant
            .map(|event| event_binding(key, ASSISTANT_DOMAIN, event))
            .transpose()?;
        let object = payload.as_object_mut().context("terminal payload is not an object")?;
        anyhow::ensure!(
            !object.contains_key(BINDING_FIELD) && !object.contains_key(SIGNATURE_FIELD),
            "terminal replay binding already exists"
        );
        object.insert(BINDING_FIELD.to_owned(), serde_json::to_value(&self.0)?);
        let mac = hex::encode(key.hmac(KeyDomain::TerminalClaim, &[MAC_DOMAIN, &serde_json::to_vec(payload)?]));
        payload
            .as_object_mut()
            .context("terminal payload is not an object")?
            .insert(SIGNATURE_FIELD.to_owned(), Value::String(mac));
        Ok(())
    }
}

/// Authentication of the old source and durable terminal does not mint a new
/// generation's source proof. Consumers may use this only to repair telemetry.
pub(crate) struct VerifiedTerminalReplay {
    collected_pending: bool,
    parent_digest: Option<String>,
    decision_digest: String,
    parent_decision_id: Option<DecisionId>,
    assistant: Option<MessageEvent>,
    decision: DecisionView,
    source: MessageEvent,
    fixed_close: bool,
    run_capabilities: Vec<String>,
    resume_claimed: bool,
}

impl VerifiedTerminalReplay {
    pub(crate) const fn collected_pending(&self) -> bool {
        self.collected_pending
    }
    pub(crate) fn parent_digest(&self) -> Option<&str> {
        self.parent_digest.as_deref()
    }
    pub(crate) fn decision_digest(&self) -> &str {
        &self.decision_digest
    }

    pub(crate) const fn parent_decision_id(&self) -> Option<DecisionId> {
        self.parent_decision_id
    }

    pub(crate) const fn resume_claimed(&self) -> bool {
        self.resume_claimed
    }
    pub(crate) const fn assistant(&self) -> Option<&MessageEvent> {
        self.assistant.as_ref()
    }
    pub(crate) fn run_capabilities(&self) -> &[String] {
        &self.run_capabilities
    }
    pub(crate) const fn decision(&self) -> &DecisionView {
        &self.decision
    }
    pub(crate) const fn source(&self) -> &MessageEvent {
        &self.source
    }
    pub(crate) const fn fixed_close(&self) -> bool {
        self.fixed_close
    }
}

pub(crate) async fn verify_terminal_replay(
    memory: &dyn Memory,
    reader: &DecisionStoreReader,
    key: &DecisionKey,
    store_workspace_id: &str,
    workspace_id: &str,
    owner_id: &str,
    terminal_id: &str,
    event: &MessageEvent,
) -> anyhow::Result<VerifiedTerminalReplay> {
    verify_terminal_replay_inner(
        memory,
        reader,
        key,
        store_workspace_id,
        workspace_id,
        owner_id,
        terminal_id,
        event,
        None,
    )
    .await
}

pub(crate) async fn verify_terminal_replay_until(
    memory: &dyn Memory,
    reader: &DecisionStoreReader,
    key: &DecisionKey,
    store_workspace_id: &str,
    workspace_id: &str,
    owner_id: &str,
    terminal_id: &str,
    event: &MessageEvent,
    deadline: std::time::Instant,
    owner: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
) -> anyhow::Result<VerifiedTerminalReplay> {
    verify_terminal_replay_inner(
        memory,
        reader,
        key,
        store_workspace_id,
        workspace_id,
        owner_id,
        terminal_id,
        event,
        Some((deadline, owner)),
    )
    .await
}

async fn verify_terminal_replay_inner(
    memory: &dyn Memory,
    reader: &DecisionStoreReader,
    key: &DecisionKey,
    store_workspace_id: &str,
    workspace_id: &str,
    owner_id: &str,
    terminal_id: &str,
    event: &MessageEvent,
    bounded: Option<(std::time::Instant, &Arc<crate::runtime::turn_cleanup::CleanupTracker>)>,
) -> anyhow::Result<VerifiedTerminalReplay> {
    anyhow::ensure!(
        event.id > 0
            && !event.event_id.is_empty()
            && event.workspace_id == workspace_id
            && event.owner_id.as_deref() == Some(owner_id)
            && event.role == "event"
            && event.event_type == "turn.finalized"
            && crate::runtime::turn_source::ingress_event_surface(
                &event.source,
                event.channel.as_deref(),
                &event.visibility
            )
            .is_some()
            && event.idempotency_key.as_deref() == Some(format!("turn:{terminal_id}:final").as_str()),
        "terminal replay scope does not match its owner"
    );
    let mut payload: Value = serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("terminal replay payload is missing")?,
    )?;
    let signature = payload
        .as_object_mut()
        .context("terminal replay payload is not an object")?
        .remove(SIGNATURE_FIELD)
        .context("unsigned terminal cannot be replayed")?;
    let tag = hex::decode(signature.as_str().context("invalid terminal replay signature")?)
        .context("invalid terminal replay signature")?;
    anyhow::ensure!(
        key.verify_hmac(
            KeyDomain::TerminalClaim,
            &[MAC_DOMAIN, &serde_json::to_vec(&payload)?],
            &tag
        ),
        "terminal replay authentication failed"
    );
    let binding: ReplayBinding = serde_json::from_value(
        payload
            .get(BINDING_FIELD)
            .context("terminal replay binding is missing")?
            .clone(),
    )?;
    anyhow::ensure!(
        binding.version == 1
            && binding.workspace_id == workspace_id
            && binding.store_workspace_id == store_workspace_id
            && binding.owner_id == owner_id
            && binding.terminal_id == terminal_id
            && event.session_key.as_deref() == Some(binding.session_key.as_str())
            && event.run_id.as_deref() == Some(binding.run_id.as_str())
            && event_scope(event) == binding.terminal_scope
            && !binding.hops.is_empty()
            && binding.hops.len() <= 5
            && payload.get("terminal_id").and_then(Value::as_str) == Some(terminal_id),
        "terminal replay binding does not match the committed marker"
    );
    let source = read_replay_event(
        memory,
        workspace_id,
        owner_id,
        event,
        crate::memory::ingress_recovery::IngressEventLookup::OwnedEventId(binding.source.event_id.clone()),
        bounded,
    )
    .await?
    .context("original terminal source event is missing")?;
    verify_event_mac(key, SOURCE_DOMAIN, &source, &binding.source.mac)?;
    anyhow::ensure!(
        binding.served_auxiliary.is_some() == payload.get("served_auxiliary").is_some(),
        "served auxiliary projection has no authenticated event binding"
    );
    if let Some(auxiliary) = &binding.served_auxiliary {
        let auxiliary_event = read_replay_event(
            memory,
            workspace_id,
            owner_id,
            event,
            crate::memory::ingress_recovery::IngressEventLookup::OwnedEventId(auxiliary.event.event_id.clone()),
            bounded,
        )
        .await?
        .context("served auxiliary event is missing")?;
        verify_event_mac(key, AUXILIARY_DOMAIN, &auxiliary_event, &auxiliary.event.mac)?;
        anyhow::ensure!(
            auxiliary_event.id > source.id
                && auxiliary_event.id < event.id
                && auxiliary_event.workspace_id == workspace_id
                && event_scope(&auxiliary_event) == binding.terminal_scope
                && payload.get("status").and_then(Value::as_str) == Some("completed"),
            "served auxiliary replay has another scope or terminal status"
        );
        let reply = payload
            .get("history")
            .and_then(|history| history.get("assistant_content"))
            .and_then(Value::as_str)
            .context("served auxiliary reply missing")?;
        use sha2::Digest;
        anyhow::ensure!(
            hex::encode(sha2::Sha256::digest(reply.as_bytes())) == auxiliary.persisted_reply_sha256,
            "persisted auxiliary reply differs from signed projection"
        );
        super::served_auxiliary::validate_event_identity(&auxiliary_event, terminal_id)?;
        let expected_projection = super::served_auxiliary::projection_from_event(&auxiliary_event)?;
        anyhow::ensure!(
            payload.get("served_auxiliary") == Some(&serde_json::to_value(expected_projection)?),
            "served auxiliary outcome differs from the authenticated event"
        );
    }

    anyhow::ensure!(
        source.id > 0
            && source.event_id == binding.source.event_id
            && source.role == "user"
            && source.event_type == "message.created"
            && source.workspace_id == workspace_id
            && source.owner_id.as_deref() == Some(owner_id)
            && source.session_key == event.session_key
            && source.run_id == event.run_id
            && source.source == event.source
            && source.channel == event.channel
            && source.visibility == event.visibility,
        "terminal replay source lineage changed"
    );
    let parent_decision_id = if let Some(parent) = &binding.parent {
        let id = DecisionId::parse(&parent.decision_id)?;
        let scope = DecisionScope::Owner {
            workspace_id: Arc::from(store_workspace_id),
            owner_id: Arc::from(owner_id),
        };
        let row = read_replay_decision(reader, &scope, &id, bounded)
            .await?
            .context("parent replay decision missing")?;
        let parent_source = read_replay_event(
            memory,
            workspace_id,
            owner_id,
            event,
            crate::memory::ingress_recovery::IngressEventLookup::OwnedEventId(parent.source.event_id.clone()),
            bounded,
        )
        .await?
        .context("parent replay source missing")?;
        verify_event_mac(key, SOURCE_DOMAIN, &parent_source, &parent.source.mac)?;
        anyhow::ensure!(
            row.digest == parent.digest
                && row.decision.directive_kind == "delegate"
                && row.decision.enabled_snapshot
                && !row.decision.legacy_v1
                && row.source_event_id.as_deref() == Some(parent_source.event_id.as_str())
                && row.decision.run_id == parent_source.run_id
                && row.decision.session_key == parent_source.session_key
                && parent_source.id > 0
                && parent_source.role == "user"
                && parent_source.event_type == "message.created"
                && parent_source.workspace_id == workspace_id
                && parent_source.owner_id.as_deref() == Some(owner_id)
                && source.parent_run_id == parent_source.run_id
                && source.parent_session_key == parent_source.session_key
                && source.causation_event_id.as_deref() == Some(parent_source.event_id.as_str())
                && source.run_id != parent_source.run_id
                && !binding.hops.iter().any(|hop| hop.decision_id == parent.decision_id),
            "child terminal parent lineage changed"
        );
        Some(id)
    } else {
        None
    };
    if let Some(operator) = &binding.operator {
        anyhow::ensure!(
            parent_decision_id.is_none(),
            "operator replay has mixed parent authority"
        );
        operator::verify(operator, memory, key, workspace_id, owner_id, &source).await?;
    }
    anyhow::ensure!(
        (parent_decision_id.is_some() || binding.operator.is_some())
            == matches!(
                source.source,
                crate::memory::MessageEventSource::Delegate
                    | crate::memory::MessageEventSource::SessionsSpawn
                    | crate::memory::MessageEventSource::SessionWorker
            ),
        "child terminal parent binding is missing or unexpected"
    );
    let claim_binding = binding
        .claim
        .as_ref()
        .context("terminal replay claim binding is missing")?;
    let claim = read_replay_event(
        memory,
        workspace_id,
        owner_id,
        event,
        crate::memory::ingress_recovery::IngressEventLookup::OwnedIdempotencyKey(format!(
            "turn:{terminal_id}:decision-claim"
        )),
        bounded,
    )
    .await?
    .context("terminal replay claim is missing")?;
    verify_event_mac(key, CLAIM_DOMAIN, &claim, &claim_binding.mac)?;
    anyhow::ensure!(
        claim.event_id == claim_binding.event_id
            && claim.event_type == "turn.decision.claimed"
            && event_scope(&claim) == binding.terminal_scope,
        "terminal replay claim changed"
    );
    let assistant = if let Some(assistant_binding) = &binding.assistant {
        let assistant = read_replay_event(
            memory,
            workspace_id,
            owner_id,
            event,
            crate::memory::ingress_recovery::IngressEventLookup::IdempotencyKey(format!(
                "turn:{terminal_id}:assistant"
            )),
            bounded,
        )
        .await?
        .context("terminal replay assistant projection is missing")?;
        verify_event_mac(key, ASSISTANT_DOMAIN, &assistant, &assistant_binding.mac)?;
        anyhow::ensure!(
            assistant.event_id == assistant_binding.event_id
                && assistant.role == "assistant"
                && payload.get("assistant_event_id").and_then(Value::as_str) == Some(assistant.event_id.as_str()),
            "terminal replay assistant projection changed"
        );
        Some(assistant)
    } else {
        anyhow::ensure!(
            payload.get("assistant_event_id").is_none_or(Value::is_null),
            "unsigned assistant projection"
        );
        None
    };
    let scope = DecisionScope::Owner {
        workspace_id: Arc::from(store_workspace_id),
        owner_id: Arc::from(owner_id),
    };
    if let Some(router) = &binding.router_read {
        router.validate_complete()?;
        router.validate_source(&source)?;
    }
    let mut router_rows = 0u8;
    let mut previous = None;
    let mut final_decision = None;
    for hop in &binding.hops {
        let id =
            DecisionId::parse(&hop.decision_id).map_err(|_| anyhow::anyhow!("invalid terminal replay decision id"))?;
        let row = read_replay_decision(reader, &scope, &id, bounded)
            .await?
            .context("terminal replay decision row is missing")?;
        anyhow::ensure!(
            row.digest == hop.digest
                && row.decision.enabled_snapshot
                && !row.decision.legacy_v1
                && row.decision.prior_decision_id == previous
                && row.decision.session_key == event.session_key
                && row.decision.run_id == event.run_id
                && row.source_event_id.as_deref() == Some(binding.source.event_id.as_str()),
            "terminal replay decision chain changed"
        );
        if let Some(router) = &binding.router_read {
            let matched = router.verify_row(&row)?;
            anyhow::ensure!(router_rows & matched == 0, "Router replay repeats a decision row");
            router_rows |= matched;
        }
        previous = Some(id);
        final_decision = Some(row.decision);
    }
    if let Some(router) = &binding.router_read {
        anyhow::ensure!(
            router_rows == 3,
            "Router replay is missing its selected read or dependent decision"
        );
        let outcome: Option<crate::llm::route_decision::ProviderExecutionOutcome> =
            serde_json::from_value(payload.get("provider_outcome").cloned().unwrap_or(Value::Null))?;
        let status: TurnTerminalStatus =
            serde_json::from_value(payload.get("status").context("Router terminal status missing")?.clone())?;
        router.validate_serving(key, outcome.as_ref(), status, binding.fixed_close)?;
    }
    // New exact-call evidence is optional, but never silently ignored when present.
    // Its strict decoder validates version, size and physically consistent facts.
    if let Some(facts) = payload
        .get("execution_trace")
        .and_then(|trace| trace.get("exact_call_observations"))
    {
        let _: crate::causal_tree::state::exact_calls::ExactCallObservations = serde_json::from_value(facts.clone())?;
    }
    let terminal_status = TurnTerminalStatus::deserialize(payload.get("status").context("terminal status missing")?)?;
    anyhow::ensure!(
        payload
            .get("execution_trace")
            .and_then(|trace| trace.get("authorized_silent"))
            .and_then(Value::as_bool)
            != Some(true)
            || terminal_status != TurnTerminalStatus::Completed,
        "authorized silence cannot be relabeled as a completed answer"
    );
    if terminal_status == TurnTerminalStatus::Silent {
        let trace: crate::tools::capability::execution_trace::ExecutionTraceSnapshot = serde_json::from_value(
            payload
                .get("execution_trace")
                .context("silent execution trace missing")?
                .clone(),
        )?;
        let outcome: crate::llm::route_decision::ProviderExecutionOutcome = serde_json::from_value(
            payload
                .get("provider_outcome")
                .context("silent provider outcome missing")?
                .clone(),
        )?;
        anyhow::ensure!(
            trace.confirms_silent(&outcome),
            "silent terminal lacks confirmed runtime execution facts"
        );
    }
    let decision = final_decision.context("terminal replay has no decision")?;
    anyhow::ensure!(
        payload.get("decision_id").and_then(Value::as_str) == Some(decision.decision_id.to_string().as_str()),
        "terminal replay final decision changed"
    );
    let collection = decision.directive_kind == "collect_approval_arguments";
    anyhow::ensure!(
        binding.collected_pending.is_none() || collection,
        "collection receipt on ordinary decision"
    );
    anyhow::ensure!(
        !collection || terminal_status != TurnTerminalStatus::Completed || binding.collected_pending.is_some(),
        "completed collection replay lacks pending receipt"
    );
    if let Some(pending) = &binding.collected_pending {
        let receipt = read_replay_event(
            memory,
            workspace_id,
            owner_id,
            event,
            crate::memory::ingress_recovery::IngressEventLookup::OwnedEventId(pending.event_id.clone()),
            bounded,
        )
        .await?
        .context("collection pending replay event missing")?;
        verify_event_mac(key, COLLECTION_DOMAIN, &receipt, &pending.mac)?;
        anyhow::ensure!(
            receipt.id > 0
                && receipt.role == "event"
                && receipt.event_type == "approval.decision.pending"
                && receipt.workspace_id == workspace_id
                && receipt.owner_id.as_deref() == Some(owner_id)
                && receipt.session_key.as_deref() == Some(binding.session_key.as_str())
                && receipt.run_id.as_deref() == Some(binding.run_id.as_str())
                && receipt.idempotency_key.as_deref()
                    == Some(format!("decision-approval:{}:pending", decision.decision_id).as_str()),
            "collection pending replay scope mismatch"
        );
    }
    if let Some(resume) = &binding.resume_claim {
        let claim = read_replay_event(
            memory,
            workspace_id,
            owner_id,
            event,
            crate::memory::ingress_recovery::IngressEventLookup::OwnedEventId(resume.event_id.clone()),
            bounded,
        )
        .await?
        .context("resume consumption receipt is missing")?;
        verify_event_mac(key, RESUME_DOMAIN, &claim, &resume.mac)?;
        let payload: Value = serde_json::from_str(
            claim
                .raw_payload_json
                .as_deref()
                .context("resume consumption payload missing")?,
        )?;
        anyhow::ensure!(
            claim.id > 0
                && claim.event_id == resume.event_id
                && claim.event_type == "approval.decision.resolved"
                && claim.workspace_id == workspace_id
                && claim.owner_id.as_deref() == Some(owner_id)
                && claim.session_key.as_deref() == Some(binding.session_key.as_str())
                && payload.get("disposition").and_then(Value::as_str) == Some("consumed")
                && payload
                    .get("execution")
                    .and_then(|execution| execution.get("decision_id"))
                    .and_then(Value::as_str)
                    == Some(decision.decision_id.to_string().as_str())
                && payload
                    .get("execution")
                    .and_then(|execution| execution.get("source_event_id"))
                    .and_then(Value::as_str)
                    == Some(binding.source.event_id.as_str())
                && payload
                    .get("execution")
                    .and_then(|execution| execution.get("run_id"))
                    .and_then(Value::as_str)
                    == Some(binding.run_id.as_str()),
            "resume consumption receipt differs from terminal execution"
        );
    }
    Ok(VerifiedTerminalReplay {
        collected_pending: binding.collected_pending.is_some(),
        parent_digest: binding.parent.map(|parent| parent.digest),
        decision_digest: binding
            .hops
            .last()
            .context("replay decision chain empty")?
            .digest
            .clone(),
        parent_decision_id,
        assistant,
        decision,
        source,
        fixed_close: binding.fixed_close,
        run_capabilities: binding.run_capabilities,
        resume_claimed: binding.resume_claim.is_some(),
    })
}

async fn read_replay_event(
    memory: &dyn Memory,
    workspace: &str,
    owner_id: &str,
    terminal: &MessageEvent,
    lookup: crate::memory::ingress_recovery::IngressEventLookup,
    bounded: Option<(std::time::Instant, &Arc<crate::runtime::turn_cleanup::CleanupTracker>)>,
) -> anyhow::Result<Option<MessageEvent>> {
    if let Some((deadline, _)) = bounded {
        let scope = crate::memory::ingress_recovery::IngressRecoveryScope {
            workspace_id: workspace.to_owned(),
            owner_id: owner_id.to_owned(),
            source: terminal.source.clone(),
        };
        memory.read_ingress_event(&scope, &lookup, deadline).await
    } else {
        match lookup {
            crate::memory::ingress_recovery::IngressEventLookup::EventId(id)
            | crate::memory::ingress_recovery::IngressEventLookup::OwnedEventId(id) => {
                memory.find_owned_message_event(workspace, owner_id, &id).await
            }
            crate::memory::ingress_recovery::IngressEventLookup::IdempotencyKey(key) => {
                memory.find_message_event_by_idempotency_key(workspace, &key).await
            }
            crate::memory::ingress_recovery::IngressEventLookup::OwnedIdempotencyKey(key) => {
                memory
                    .find_owned_message_event_by_idempotency_key(workspace, owner_id, &key)
                    .await
            }
        }
    }
}
async fn read_replay_decision(
    reader: &DecisionStoreReader,
    scope: &DecisionScope,
    id: &DecisionId,
    bounded: Option<(std::time::Instant, &Arc<crate::runtime::turn_cleanup::CleanupTracker>)>,
) -> anyhow::Result<Option<crate::causal_tree::store::DecisionReplayRecord>> {
    if let Some((deadline, owner)) = bounded {
        let reader = reader.clone();
        let scope = scope.clone();
        let id = id.clone();
        crate::runtime::ingress_recovery::owned_store_read(owner, move || {
            Ok(reader.replay_record_until(&scope, &id, deadline)?)
        })
        .await
    } else {
        Ok(reader.replay_record(scope, id)?)
    }
}
