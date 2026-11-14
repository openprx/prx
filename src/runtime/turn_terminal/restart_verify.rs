//! Restart verification of a repository terminal (Part-2 D§4.4, OV-9).
//!
//! The settled receipt (E5 over E4) authenticates the settled facts and the
//! final row. The facts carry the E2 tag of the persisted claim semantic; the
//! semantic must match the authenticated final payload field by field. Any
//! failure refuses with [`TerminalNotReplayable`]: no re-execution and no new
//! settlement.
use crate::memory::MessageEvent;
use crate::memory::recovery_authority::{
    MAX_SERIALIZED_RECOVERY_FACTS, RecoveryMacDomain, RecoveryPricing, RecoveryRepository, RecoveryTag,
};
use crate::runtime::ingress_recovery::SettledTerminalFacts;
use serde_json::Value;

/// A settled terminal whose evidence does not verify. Never re-executed.
#[derive(Debug, thiserror::Error)]
#[error("terminal is not replayable: {0}")]
pub(crate) struct TerminalNotReplayable(pub(crate) &'static str);

/// Claim semantic fields that must be JSON `null` on a repository terminal.
const NULL_SEMANTIC_FIELDS: [&str; 7] = [
    "decision_id",
    "execution_trace",
    "decision_chain",
    "catalog_generation",
    "model_catalog_revision",
    "model_catalog_content_sha256",
    "policy_version",
];

/// The complete key set of a repository claim payload.
const CLAIM_FIELDS: [&str; 4] = ["terminal_id", "semantic_mac_v3", "recovery_key_id", "semantic"];

/// Semantic fields that must equal the final payload exactly.
const MIRRORED_FIELDS: [&str; 6] = [
    "status",
    "history",
    "history_scope",
    "provider_outcome",
    "telemetry",
    "delivery_intent",
];

fn refuse(reason: &'static str) -> TerminalNotReplayable {
    TerminalNotReplayable(reason)
}

fn payload(event: &MessageEvent, missing: &'static str) -> Result<Value, TerminalNotReplayable> {
    let raw = event.raw_payload_json.as_deref().ok_or(refuse(missing))?;
    serde_json::from_str(raw).map_err(|_| refuse(missing))
}

/// Steps 3–6: the final payload, the claim identity and the semantic ↔
/// payload comparison. Returns the E2 preimage
/// (`serde_json::to_vec(semantic) ‖ key_id`) for the step-5 MAC check.
pub(crate) fn verify_semantics(
    facts: &SettledTerminalFacts,
    workspace_id: &str,
    claim: &MessageEvent,
    terminal: &MessageEvent,
) -> Result<Vec<u8>, TerminalNotReplayable> {
    // Step 3: the final payload of the row bound by E4.
    if terminal.event_id != facts.final_event_id {
        return Err(refuse("final event differs from the settled facts"));
    }
    let final_payload = payload(terminal, "final payload missing or malformed")?;
    if final_payload.get("terminal_id").and_then(Value::as_str) != Some(facts.terminal_id.as_str()) {
        return Err(refuse("final terminal id differs"));
    }
    let final_status = final_payload.get("status").and_then(Value::as_str);
    if final_status != Some(facts.status.as_str()) {
        return Err(refuse("final status differs from the settled facts"));
    }
    if final_payload.get("assistant_event_id").and_then(Value::as_str) != facts.assistant_event_id.as_deref() {
        return Err(refuse("final assistant event differs"));
    }
    let has_history = final_payload.get("history").is_some_and(|history| !history.is_null());
    if facts.assistant_event_id.is_some() != (final_status == Some("completed") && has_history) {
        return Err(refuse("assistant event presence contradicts the final status"));
    }

    // Step 4: the claim identity.
    if claim.event_id != facts.claim_event_id
        || claim.role != "event"
        || claim.event_type != "turn.decision.claimed"
        || claim.idempotency_key.as_deref() != Some(format!("turn:{}:decision-claim", facts.terminal_id).as_str())
    {
        return Err(refuse("claim identity differs"));
    }
    let claim_payload = payload(claim, "claim payload missing or malformed")?;
    let claim_object = claim_payload
        .as_object()
        .ok_or(refuse("claim payload is not an object"))?;
    if claim_object.get("recovery_key_id").and_then(Value::as_str) != Some(facts.key_id.as_str())
        || claim_object.get("semantic_mac_v3").and_then(Value::as_str) != Some(facts.semantic_mac_v3.as_str())
    {
        return Err(refuse("claim key or tag differs from the settled facts"));
    }
    // The repository claim schema is closed: any other key (a legacy
    // decision id or MAC included) refuses the terminal.
    if claim_object.len() != CLAIM_FIELDS.len() || CLAIM_FIELDS.iter().any(|field| !claim_object.contains_key(*field)) {
        return Err(refuse("claim carries fields outside its schema"));
    }
    let semantic = claim_object.get("semantic").ok_or(refuse("claim semantic missing"))?;

    // Step 6: semantic ↔ payload.
    let semantic_object = semantic.as_object().ok_or(refuse("claim semantic is not an object"))?;
    if semantic_object.get("version").and_then(Value::as_u64) != Some(2)
        || semantic_object.get("terminal_id").and_then(Value::as_str) != Some(facts.terminal_id.as_str())
        || semantic_object.get("workspace_id").and_then(Value::as_str) != Some(workspace_id)
    {
        return Err(refuse("claim semantic identity differs"));
    }
    if NULL_SEMANTIC_FIELDS
        .iter()
        .any(|field| semantic_object.get(*field) != Some(&Value::Null))
    {
        return Err(refuse("claim semantic carries decision fields"));
    }
    if MIRRORED_FIELDS
        .iter()
        .any(|field| semantic_object.get(*field) != final_payload.get(*field))
    {
        return Err(refuse("claim semantic differs from the final payload"));
    }
    for field in ["prepared_parameters", "replay_unavailable"] {
        if semantic_object.get(field) != final_payload.get(field) {
            return Err(refuse("claim semantic and final payload disagree on an optional field"));
        }
    }
    let scope = semantic_object
        .get("scope")
        .ok_or(refuse("claim semantic scope missing"))?;
    crate::agent::terminal::oversized_reply::verify_scope(terminal, scope)
        .map_err(|_| refuse("final event scope differs from the claim"))?;

    // Step 5 preimage.
    let mut preimage = serde_json::to_vec(semantic).map_err(|_| refuse("claim semantic cannot be encoded"))?;
    preimage.extend_from_slice(facts.key_id.as_bytes());
    if preimage.len() > MAX_SERIALIZED_RECOVERY_FACTS {
        return Err(refuse("oversized recovery MAC input"));
    }
    Ok(preimage)
}

/// Step 5 (E2 over the persisted semantic) and step 7 (usage event and the
/// authenticated outbox row of a metered terminal).
pub(crate) fn verify_claim_and_cost(
    repository: &dyn RecoveryRepository,
    chain: &crate::memory::recovery_authority::RecoveryChain,
    facts: &SettledTerminalFacts,
    preimage: &[u8],
) -> Result<(), TerminalNotReplayable> {
    let tag_bytes = hex::decode(&facts.semantic_mac_v3).map_err(|_| refuse("claim tag is not hex"))?;
    let tag = RecoveryTag {
        key_id: facts.key_id.clone(),
        tag: tag_bytes
            .as_slice()
            .try_into()
            .map_err(|_| refuse("claim tag length"))?,
    };
    repository
        .verify_recovery_mac(&tag, RecoveryMacDomain::ClaimV3, preimage)
        .map_err(|_| refuse("claim semantic does not authenticate"))?;
    if let Some(id) = &facts.usage_event_id {
        repository
            .recovery_event_evidence(chain, id)
            .map_err(|_| refuse("usage event missing from the chain"))?;
    }
    if facts.pricing == RecoveryPricing::Metered.as_str() {
        let row = repository
            .recovery_cost_row(&chain.workspace_id, &facts.terminal_id)
            .map_err(|_| refuse("cost outbox row does not authenticate"))?
            .ok_or(refuse("metered terminal has no cost outbox row"))?;
        let outbox_final = serde_json::from_slice::<Value>(&row.facts)
            .ok()
            .and_then(|value| value.get("final_event_id").and_then(Value::as_str).map(str::to_owned));
        if row.pricing.as_str() != facts.pricing || outbox_final.as_deref() != Some(facts.final_event_id.as_str()) {
            return Err(refuse("cost outbox row differs from the settled facts"));
        }
    }
    Ok(())
}
