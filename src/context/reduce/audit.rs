//! The rows a compaction leaves in the log: the context-switch handoff row, the
//! degradation record and the compaction run audit.

use super::provenance::{COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS, ProvenanceGap};
use super::summary::CompactionAuditSource;
use crate::agent::document_ingest::DocumentIngestRuntime;
use anyhow::Result;
use uuid::Uuid;

/// Event type recorded when a `switch` rollover could not stay lossless and the
/// turn continued on a token-aware trim instead of failing.
pub(crate) const CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE: &str = "context.compaction.degraded";

/// The recorded provenance status as a static label: a known status, or
/// `"unavailable"` for anything else.
pub(crate) fn provenance_status_label(status: &str) -> &'static str {
    match status {
        "exact" => "exact",
        COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS => COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS,
        "read_error" => "read_error",
        other => ProvenanceGap::ALL
            .into_iter()
            .map(ProvenanceGap::status)
            .find(|gap_status| *gap_status == other)
            .unwrap_or("unavailable"),
    }
}

/// Why the switch filter refused `source`: its provenance status when that is
/// not exact, else the bound that failed.
pub(crate) fn switch_refusal_reason(source: &CompactionAuditSource) -> &'static str {
    let status = provenance_status_label(&source.source_event_provenance_status);
    if !matches!(status, "exact" | COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS) {
        return status;
    }
    if source.source_event_ids.len() != source.message_count.saturating_sub(source.unpersisted_companion_count) {
        return "bound:count_mismatch";
    }
    if source.source_event_ids.len() > crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS {
        return "bound:too_many_events";
    }
    "bound:row_span"
}

pub(crate) async fn persist_context_switch(
    audit: &DocumentIngestRuntime,
    trigger: &'static str,
    source: &CompactionAuditSource,
    handoff_event_id: &str,
    handoff_message: &str,
    note_json: &str,
) -> Result<()> {
    let Some(range) = source.source_event_range.as_ref() else {
        return Ok(());
    };
    audit
        .memory
        .append_message_event(crate::memory::MessageEventInput {
            agent_partition: audit.agent_partition.clone(),
            event_id: Some(handoff_event_id.to_string()),
            idempotency_key: Some(format!("context-switch:{handoff_event_id}")),
            workspace_id: audit.workspace_id.clone(),
            owner_id: audit.owner_id.clone(),
            source: "context_switch".into(),
            channel: audit.channel.clone(),
            session_key: audit.session_key.clone(),
            parent_session_key: audit.legacy_session_key.clone(),
            run_id: None,
            parent_run_id: None,
            agent_id: audit.agent_id.clone(),
            persona_id: audit.persona_id.clone(),
            sender: Some("context_switch".to_string()),
            recipient: audit.sender.clone(),
            role: "assistant".to_string(),
            event_type: "context.switch.created".to_string(),
            subject: audit
                .session_key
                .as_ref()
                .map(|key| crate::memory::MessageEventSubject::Conversation(key.clone())),
            goal_id: None,
            causation_event_id: Some(range.last_event_id.clone()),
            correlation_id: audit.source_message_event_id.clone(),
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: audit.config_generation_id,
            config_source_revision: audit.config_source_revision.clone(),
            content: handoff_message.to_string(),
            raw_payload_json: Some(
                serde_json::json!({
                    "trigger": trigger,
                    "handoff_note": serde_json::from_str::<serde_json::Value>(note_json).unwrap_or_default()
                })
                .to_string(),
            ),
            visibility: audit.visibility.clone(),
        })
        .await?;
    Ok(())
}

/// Record that a `switch` rollover could not stay lossless and the turn
/// continued on a token-aware trim.
///
/// A hard switch that cannot resolve exact provenance used to abort the whole
/// turn with a non-retryable error, which made every session containing tool
/// calls permanently unusable once it crossed the hard limit. Degrading is
/// lossy, so it is recorded durably next to the compaction audit trail instead
/// of only being logged.
pub(crate) async fn persist_context_switch_degradation(
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
    reason: &'static str,
    before_tokens: usize,
    after_tokens: usize,
    hard_limit: usize,
) {
    let Some(audit) = audit else {
        return;
    };
    let detail = serde_json::json!({
        "trigger": trigger,
        "reason": reason,
        "before_used_tokens": before_tokens,
        "after_used_tokens": after_tokens,
        "hard_limit_tokens": hard_limit,
        "fallback": "token_aware_trim",
    });
    // One event per turn. The key is derived from the turn rather than from the
    // event id it is attached to: a fresh `Uuid` in both places made every write
    // a guaranteed insert and left the idempotency key unable to reject
    // anything, including a retry of this very call from a second runtime built
    // for the same turn.
    if audit.degradation_recorded.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::debug!(
            trigger,
            reason,
            "context switch degradation already recorded for this turn"
        );
        return;
    }
    let degraded_event_id = Uuid::new_v4().to_string();
    let idempotency_key = format!("context-compaction-degraded:{}", audit.turn_key);
    if let Err(error) = audit
        .memory
        .append_message_event(crate::memory::MessageEventInput {
            agent_partition: audit.agent_partition.clone(),
            event_id: Some(degraded_event_id.clone()),
            idempotency_key: Some(idempotency_key),
            workspace_id: audit.workspace_id.clone(),
            owner_id: audit.owner_id.clone(),
            source: "context_switch".into(),
            channel: audit.channel.clone(),
            session_key: audit.session_key.clone(),
            parent_session_key: audit.legacy_session_key.clone(),
            run_id: None,
            parent_run_id: None,
            agent_id: audit.agent_id.clone(),
            persona_id: audit.persona_id.clone(),
            sender: Some("context_switch".to_string()),
            recipient: audit.sender.clone(),
            role: "event".to_string(),
            event_type: CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE.to_string(),
            subject: audit
                .session_key
                .as_ref()
                .map(|key| crate::memory::MessageEventSubject::Conversation(key.clone())),
            goal_id: None,
            causation_event_id: None,
            correlation_id: audit.source_message_event_id.clone(),
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: audit.config_generation_id,
            config_source_revision: audit.config_source_revision.clone(),
            content: format!("context switch degraded to lossy trim ({reason})"),
            raw_payload_json: Some(detail.to_string()),
            visibility: audit.visibility.clone(),
        })
        .await
    {
        // Deliberately *not* claimed: the turn is marked as recorded only once
        // a write has actually landed, so a backend that was briefly
        // unavailable costs a retry on the next iteration instead of losing the
        // event for the rest of the turn. Duplicate writes are still impossible
        // — the backend is unique on `(workspace_id, idempotency_key)` and the
        // key is derived from the turn.
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::debug!(
            error_len,
            error_sha256,
            "failed to append context switch degradation event"
        );
    } else {
        audit
            .degradation_recorded
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
