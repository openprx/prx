//! Bounded, authenticated references for replies that cannot fit a claim.
use crate::memory::ingress_recovery::{IngressEventLookup, IngressRecoveryScope};
use crate::memory::recovery_authority::{MAX_SERIALIZED_RECOVERY_FACTS, RecoveryChain, RecoveryMacDomain, RecoveryTag};
use crate::memory::{Memory, MessageEvent, MessageEventScope};
use crate::runtime::ingress_recovery::SettledTerminalFacts;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplyReference {
    pub(super) reply_sha256: String,
    pub(super) reply_bytes: u64,
    pub(super) result_hash: String,
}

impl ReplyReference {
    pub(super) fn new(reply: &str, model: &str) -> anyhow::Result<Self> {
        Ok(Self {
            reply_sha256: hex::encode(Sha256::digest(reply.as_bytes())),
            reply_bytes: u64::try_from(reply.len()).context("reply byte length cannot be represented")?,
            result_hash: result_hash(reply, model),
        })
    }

    fn parse(value: &Value) -> anyhow::Result<Self> {
        let reference: Self = serde_json::from_value(value.clone()).context("invalid reply reference")?;
        anyhow::ensure!(reference.reply_bytes > 0, "empty unavailable reply reference");
        for digest in [&reference.reply_sha256, &reference.result_hash] {
            anyhow::ensure!(
                digest.len() == 64 && hex::decode(digest)?.len() == 32,
                "invalid reply digest"
            );
        }
        Ok(reference)
    }
}

/// The gateway's existing response/model result-hash representation.
pub(crate) fn result_hash(reply: &str, model: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(reply.as_bytes());
    hasher.update([0]);
    hasher.update(model.as_bytes());
    hex::encode(hasher.finalize())
}

/// Replace copies of the reply, preserving every other semantic field.
pub(super) fn compact_reply(value: &mut Value, reply: &str) {
    match value {
        Value::String(text) if text == reply => *value = serde_json::json!({"reply_reference": true}),
        Value::Array(values) => values.iter_mut().for_each(|value| compact_reply(value, reply)),
        Value::Object(values) => values.values_mut().for_each(|value| compact_reply(value, reply)),
        _ => {}
    }
}

/// An authenticated settled reply exists, but its text cannot be replayed.
#[derive(Clone, thiserror::Error)]
#[error("terminal reply is unavailable for replay")]
pub(crate) struct ReplayUnavailable {
    pub(crate) terminal_id: String,
    pub(crate) result_hash: String,
}

impl std::fmt::Debug for ReplayUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayUnavailable").finish_non_exhaustive()
    }
}

fn payload(event: &MessageEvent) -> anyhow::Result<Value> {
    serde_json::from_str(event.raw_payload_json.as_deref().context("terminal payload missing")?)
        .context("invalid terminal payload")
}

/// A newly committed receipt is already authenticated by its terminal owner.
pub(crate) fn from_receipt(receipt: &super::TurnTerminalReceipt) -> anyhow::Result<Option<ReplayUnavailable>> {
    let value = payload(&receipt.terminal_event)?;
    value
        .get("replay_unavailable")
        .map(|value| {
            let reference = ReplyReference::parse(value)?;
            Ok(ReplayUnavailable {
                terminal_id: receipt.terminal_id.clone(),
                result_hash: reference.result_hash,
            })
        })
        .transpose()
}

fn verify_scope(event: &MessageEvent, scope: &Value) -> anyhow::Result<()> {
    let event = serde_json::to_value(event)?;
    for (field, expected) in scope.as_object().context("claim scope missing")? {
        anyhow::ensure!(
            event.get(field) == Some(expected),
            "reply reference scope differs from its claim"
        );
    }
    Ok(())
}

/// Authenticate the compact terminal, its claim, and its assistant reference.
/// The repository enforces the unchanged evidence-entry budget before reads.
async fn verify_terminal(
    memory: Arc<dyn Memory>,
    terminal: MessageEvent,
    input_id: String,
) -> anyhow::Result<Option<ReplayUnavailable>> {
    let terminal_payload = payload(&terminal)?;
    let Some(reference) = terminal_payload.get("replay_unavailable").cloned() else {
        return Ok(None);
    };
    let parsed = ReplyReference::parse(&reference)?;
    let owner = terminal
        .owner_id
        .as_deref()
        .context("terminal owner missing")?
        .to_owned();
    let run = terminal.run_id.as_deref().context("terminal run missing")?.to_owned();
    let chain = RecoveryChain {
        workspace_id: terminal.workspace_id.clone(),
        owner_id: owner.clone(),
        source: terminal.source.as_str().to_owned(),
        run_id: run.clone(),
    };
    let checked_memory = Arc::clone(&memory);
    let checked_chain = chain.clone();
    let checked_input = input_id.clone();
    let facts = crate::runtime::blocking::spawn_blocking(move || {
        let repository = checked_memory
            .recovery_repository()
            .context("recovery repository unavailable")?;
        let receipt = repository
            .read_recovery_receipt(&checked_chain)?
            .context("settled receipt missing")?;
        anyhow::ensure!(receipt.state == "settled", "reply terminal is not settled");
        anyhow::ensure!(
            receipt.input_event_id()?.as_deref() == Some(checked_input.as_str()),
            "reply receipt belongs to another input"
        );
        let facts = SettledTerminalFacts::parse(&receipt.settled_facts()?)?;
        anyhow::ensure!(facts.key_id == receipt.key_id, "reply receipt key differs");
        repository.recovery_event_evidence(&checked_chain, &facts.claim_event_id)?;
        if let Some(id) = &facts.assistant_event_id {
            repository.recovery_event_evidence(&checked_chain, id)?;
        }
        Ok::<_, anyhow::Error>(facts)
    })
    .await??;
    anyhow::ensure!(
        terminal.role == "event"
            && terminal.event_type == "turn.finalized"
            && facts.terminal_id == run
            && facts.final_event_id == terminal.event_id
            && facts.status == "completed",
        "reply terminal differs from settled receipt"
    );
    let scope = IngressRecoveryScope {
        workspace_id: chain.workspace_id.clone(),
        owner_id: owner.clone(),
        source: terminal.source.clone(),
    };
    let claim = read_event(
        memory.as_ref(),
        &scope,
        IngressEventLookup::OwnedEventId(facts.claim_event_id.clone()),
    )
    .await?
    .context("reply claim missing")?;
    let claim_payload = payload(&claim)?;
    let semantic = claim_payload
        .get("semantic")
        .context("bounded reply claim semantics missing")?
        .clone();
    let claim_reference = semantic
        .get("replay_unavailable")
        .context("reply claim reference missing")?
        .clone();
    ReplyReference::parse(&claim_reference)?;
    anyhow::ensure!(
        claim.event_type == "turn.decision.claimed"
            && claim.role == "event"
            && claim_payload.get("recovery_key_id").and_then(Value::as_str) == Some(facts.key_id.as_str())
            && semantic.get("terminal_id").and_then(Value::as_str) == Some(run.as_str())
            && semantic.get("source_event_id").and_then(Value::as_str) == Some(input_id.as_str())
            && claim.idempotency_key.as_deref() == Some(format!("turn:{run}:decision-claim").as_str())
            && semantic.get("workspace_id").and_then(Value::as_str) == Some(chain.workspace_id.as_str()),
        "reply claim identity differs"
    );
    let bound_scope = semantic.get("scope").context("reply claim scope missing")?;
    verify_scope(&claim, bound_scope)?;
    verify_scope(&terminal, bound_scope)?;
    for field in [
        "status",
        "history",
        "history_scope",
        "provider_outcome",
        "telemetry",
        "delivery_intent",
    ] {
        anyhow::ensure!(
            semantic.get(field) == terminal_payload.get(field),
            "reply terminal semantics differ"
        );
    }
    let assistant_id = facts
        .assistant_event_id
        .as_deref()
        .context("reply assistant reference missing")?;
    anyhow::ensure!(
        terminal_payload.get("assistant_event_id").and_then(Value::as_str) == Some(assistant_id),
        "reply assistant identity differs"
    );
    let assistant = read_event(
        memory.as_ref(),
        &scope,
        IngressEventLookup::OwnedEventId(assistant_id.to_owned()),
    )
    .await?
    .context("reply assistant missing")?;
    anyhow::ensure!(
        assistant.role == "assistant"
            && assistant.event_type == "message.created"
            && assistant.content == "reply_unavailable",
        "reply projection is not a bounded reference"
    );
    verify_scope(
        &assistant,
        semantic
            .get("history_scope")
            .filter(|value| !value.is_null())
            .unwrap_or(bound_scope),
    )?;
    let assistant_reference = payload(&assistant)?
        .get("replay_unavailable")
        .context("reply projection reference missing")?
        .clone();
    ReplyReference::parse(&assistant_reference)?;
    let claim_mac = claim_payload
        .get("semantic_mac_v3")
        .and_then(Value::as_str)
        .context("reply claim MAC missing")?
        .to_owned();
    let key_id = facts.key_id;
    crate::runtime::blocking::spawn_blocking(move || {
        let repository = memory
            .recovery_repository()
            .context("recovery repository unavailable")?;
        for mac in [&claim_mac, &facts.semantic_mac_v3] {
            let tag = RecoveryTag {
                key_id: key_id.clone(),
                tag: hex::decode(mac)?
                    .as_slice()
                    .try_into()
                    .context("invalid reply claim MAC length")?,
            };
            // Check every copy through the existing ClaimV3 verifier, including
            // digest and byte-length fields; never compare tags/digests with ==.
            for reference in [claim_reference.clone(), reference.clone(), assistant_reference.clone()] {
                let mut candidate = semantic.clone();
                candidate
                    .as_object_mut()
                    .context("claim is not an object")?
                    .insert("replay_unavailable".into(), reference);
                let mut bytes = serde_json::to_vec(&candidate)?;
                bytes.extend_from_slice(key_id.as_bytes());
                anyhow::ensure!(
                    bytes.len() <= MAX_SERIALIZED_RECOVERY_FACTS,
                    "oversized recovery MAC input"
                );
                repository.verify_recovery_mac(&tag, RecoveryMacDomain::ClaimV3, &bytes)?;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(Some(ReplayUnavailable {
        terminal_id: run,
        result_hash: parsed.result_hash,
    }))
}

/// Read-only replay for the same authenticated owner, stable key and request.
/// This creates no source proof, admission ticket, or permission to execute.
pub(crate) async fn for_input(
    memory: Arc<dyn Memory>,
    workspace: &str,
    scope: &MessageEventScope,
    key: &str,
    input_text: &str,
) -> anyhow::Result<Option<ReplayUnavailable>> {
    let owner = scope.owner_id.as_deref().context("gateway replay owner missing")?;
    let recovery_scope = IngressRecoveryScope {
        workspace_id: workspace.to_owned(),
        owner_id: owner.to_owned(),
        source: scope.source.clone(),
    };
    let Some(input) = read_event(
        memory.as_ref(),
        &recovery_scope,
        IngressEventLookup::OwnedIdempotencyKey(key.to_owned()),
    )
    .await?
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        input.role == "user"
            && input.event_type == "message.created"
            && input.source == scope.source
            && input.channel == scope.channel
            && input.session_key == scope.session_key
            && input.sender == scope.sender
            && input.recipient == scope.recipient
            && crate::runtime::turn_source::recorded_input_matches(
                crate::causal_tree::state::TurnSurfaceKind::Server,
                &input,
                input_text
            ),
        "stable gateway input differs from replay request"
    );
    let run = input.run_id.as_deref().context("gateway replay input run missing")?;
    let Some(terminal) = read_event(
        memory.as_ref(),
        &recovery_scope,
        IngressEventLookup::OwnedIdempotencyKey(format!("turn:{run}:final")),
    )
    .await?
    else {
        return Ok(None);
    };
    verify_terminal(memory, terminal, input.event_id).await
}

async fn read_event(
    memory: &dyn Memory,
    scope: &IngressRecoveryScope,
    lookup: IngressEventLookup,
) -> anyhow::Result<Option<MessageEvent>> {
    memory
        .read_ingress_event(
            scope,
            &lookup,
            std::time::Instant::now() + std::time::Duration::from_secs(15),
        )
        .await
}
