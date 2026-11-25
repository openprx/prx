//! Building and applying the compaction patch: the demand that triggers a
//! compaction, the summary or handoff it writes, and the replacement it applies.

use super::audit::{persist_compaction_audit, persist_context_switch, switch_refusal_reason};
use super::handoff_note::{
    MAX_HANDOFF_MESSAGE_BYTES, context_handoff_lineage, format_context_handoff_message, validated_context_handoff_note,
};
use super::handoff_summary::summarize_cold_window;
use super::provenance::{
    COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS, attach_compaction_event_provenance, stored_attachments,
};
use super::roundtrips::{
    RoundCompanions, compaction_boundary_without_orphan_tool_results, folded_tool_roundtrips,
    format_folded_tool_roundtrips_notice, switch_cold_window_end,
};
#[cfg(test)]
use super::summary::apply_compaction_patch_exact;
use super::summary::{
    COMPACTION_MAX_SUMMARY_CHARS, MEMORY_FLUSH_MAX_CHARS, bounded_compaction_projection, build_compaction_audit_source,
    build_compaction_transcript, compaction_patch_guard_for, compaction_summary_fidelity_status,
};
use crate::agent::context_budget::planning_history_tokens;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::response_guard::strip_assistant_fabricated_tool_markers;
use crate::agent::turn_contracts::CompactionPatch;
use crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER;
use crate::providers::ChatMessage;
use crate::providers::Provider;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use uuid::Uuid;

pub(crate) fn compaction_trigger_limit(config: &crate::config::AgentCompactionConfig) -> Option<usize> {
    if config.mode == crate::config::AgentCompactionMode::Off {
        return None;
    }
    let max_tokens = config.max_context_tokens;
    let reserve = config.reserve_tokens;
    if max_tokens <= reserve {
        return Some(0);
    }
    Some(max_tokens - reserve)
}

/// Trigger name of an explicit `/compact` request from a chat front end.
pub(crate) const MANUAL_COMPACT_TRIGGER: &str = "chat_manual_compact";

/// Why a compaction runs, which decides whether the configured hard input
/// limit gates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactionDemand {
    /// The history crossed the configured hard input limit (preflight).
    Threshold,
    /// The provider reported a context overflow. Authoritative even when the
    /// cl100k proxy says the request fits, so tokenizer differences can be
    /// repaired.
    ProviderOverflow,
    /// The user asked for `/compact`: every compactable message beyond
    /// `keep_recent_messages` is compacted, whatever the budget says.
    Manual,
}

impl CompactionDemand {
    fn from_trigger(trigger: &'static str) -> Self {
        if trigger == MANUAL_COMPACT_TRIGGER {
            Self::Manual
        } else if trigger.contains("overflow") {
            Self::ProviderOverflow
        } else {
            Self::Threshold
        }
    }

    const fn is_forced(self) -> bool {
        matches!(self, Self::ProviderOverflow | Self::Manual)
    }
}

/// What the builder decided for one compaction demand.
pub(crate) enum CompactionBuild {
    /// No patch: nothing to compact for this demand, or no exact handoff could
    /// be produced (the caller degrades as it always did).
    NotNeeded,
    /// A patch that replaces the cold window.
    Patch(CompactionPatch),
    /// An automatic `switch` this admitted turn already spent: one succeeded
    /// earlier in the turn. Nothing is built, and nothing is
    /// reported as a degradation; the turn fits its request instead.
    Suppressed,
}

#[cfg(test)]
pub(crate) async fn build_configurable_compaction_patch(
    history: &[ChatMessage],
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<Option<CompactionPatch>> {
    let source = crate::agent::turn_history::CanonicalHistory::from_test_messages(history.to_vec());
    match build_configurable_compaction_patch_with_source_history(
        history, &source, provider, model, config, audit, trigger,
    )
    .await?
    {
        CompactionBuild::Patch(patch) => Ok(Some(patch)),
        CompactionBuild::NotNeeded => Ok(None),
        CompactionBuild::Suppressed => anyhow::bail!("context switch suppressed: the turn spent its switch budget"),
    }
}

pub(crate) async fn build_configurable_compaction_patch_with_source_history(
    budget_history: &[ChatMessage],
    source: &crate::agent::turn_history::CanonicalHistory,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<CompactionBuild> {
    let Some(limit) = compaction_trigger_limit(config) else {
        return Ok(CompactionBuild::NotNeeded);
    };
    if !CompactionDemand::from_trigger(trigger).is_forced() && planning_history_tokens(budget_history) <= limit {
        return Ok(CompactionBuild::NotNeeded);
    }
    // One successful automatic switch per admitted turn.
    // Manual `/compact` is outside the budget and always builds.
    let automatic_switch = trigger != MANUAL_COMPACT_TRIGGER;
    if automatic_switch
        && matches!(config.mode, crate::config::AgentCompactionMode::Switch)
        && audit.is_some_and(DocumentIngestRuntime::automatic_switch_spent)
    {
        tracing::debug!(
            trigger,
            "context switch suppressed: this turn has already spent its switch budget"
        );
        return Ok(CompactionBuild::Suppressed);
    }

    let source_history = source.messages();
    // This turn's tool round traffic, by provenance (an acknowledged turn is
    // never folded, whatever its text).
    let companions = source.tool_round_companions();
    let has_system = source_history.first().is_some_and(|m| m.role == "system");
    let start = if has_system { 1 } else { 0 };
    let non_system_count = source_history.len().saturating_sub(start);
    if non_system_count <= 1 {
        return Ok(CompactionBuild::NotNeeded);
    }

    let requested_keep_recent = crate::agent::budget_inputs::effective_keep_recent_messages(
        source_history.get(start..).unwrap_or_default(),
        config,
    );
    let requested_compact_count = non_system_count.saturating_sub(requested_keep_recent);
    if requested_compact_count == 0 {
        return Ok(CompactionBuild::NotNeeded);
    }
    // Never cut inside a tool round-trip: an orphan `role="tool"` result whose
    // requesting assistant message was compacted away is rejected by every
    // native provider.
    let requested_end = start.saturating_add(requested_compact_count);
    let requested_end = if matches!(config.mode, crate::config::AgentCompactionMode::Switch) {
        switch_cold_window_end(&companions, start, requested_end)
    } else {
        requested_end
    };
    let compact_end = compaction_boundary_without_orphan_tool_results(&companions, start, requested_end);
    let compact_count = compact_end.saturating_sub(start);
    if compact_count == 0 {
        return Ok(CompactionBuild::NotNeeded);
    }
    let keep_recent = source_history.len().saturating_sub(compact_end);
    let compacted_source_messages = source_history.get(start..compact_end);
    let compacted_companions = RoundCompanions::Marked(companions.get(start..compact_end).unwrap_or_default());
    let mut audit_source = audit.and_then(|_| compacted_source_messages.map(build_compaction_audit_source));
    if let (Some(audit), Some(source_messages), Some(audit_source)) =
        (audit, compacted_source_messages, audit_source.as_mut())
    {
        // The kept messages that claim their own rows in a joint resolution:
        // acknowledged turns only (never the pending request, this turn's
        // unstored reply or its tool round-trip traffic).
        let acknowledged = source.acknowledged_flags();
        let kept_acknowledged = source_history
            .iter()
            .zip(&acknowledged)
            .skip(compact_end)
            .filter(|(message, acknowledged)| **acknowledged && matches!(message.role.as_str(), "user" | "assistant"))
            .map(|(message, _)| message)
            .collect::<Vec<_>>();
        attach_compaction_event_provenance(
            audit,
            source_messages,
            compacted_companions,
            &kept_acknowledged,
            audit_source,
        )
        .await;
    }
    let Some(guard) = compaction_patch_guard_for(source_history, start, compact_end) else {
        return Ok(CompactionBuild::NotNeeded);
    };
    let timestamp = chrono::Utc::now().to_rfc3339();

    if matches!(config.mode, crate::config::AgentCompactionMode::Switch) {
        let Some(audit) = audit else {
            tracing::warn!(
                trigger,
                "context switch skipped because durable transcript scope is unavailable"
            );
            return Ok(CompactionBuild::NotNeeded);
        };
        let Some(source) = audit_source.as_ref().filter(|source| {
            matches!(
                source.source_event_provenance_status.as_str(),
                "exact" | COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS
            ) && source.source_event_ids.len()
                == source.message_count.saturating_sub(source.unpersisted_companion_count)
                && source.source_event_ids.len() <= crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS
                && source.source_event_range.is_some()
                && source.source_event_range.as_ref().is_some_and(|range| {
                    range.last_row_id.saturating_sub(range.first_row_id)
                        <= crate::tools::transcript_history_lookup::MAX_ROW_SPAN
                })
        }) else {
            let status: &'static str = audit_source.as_ref().map_or("unavailable", switch_refusal_reason);
            tracing::warn!(
                trigger,
                provenance_status = %status,
                "context switch skipped because exact transcript provenance is unavailable"
            );
            return Ok(CompactionBuild::NotNeeded);
        };
        let Some(range) = source.source_event_range.as_ref() else {
            return Ok(CompactionBuild::NotNeeded);
        };
        let (from_generation, parent_handoff_event_id) = context_handoff_lineage(source_history);
        let to_generation = from_generation.saturating_add(1);
        let handoff_event_id = Uuid::new_v4().to_string();
        // The handoff replaces the cold window with event-id pointers, and tool
        // round-trips have no events of their own. Without this line the note
        // reads as an exact transfer while the tool output — usually the
        // substance of an agentic window — is gone with nothing naming it.
        let folded = folded_tool_roundtrips(compacted_source_messages.unwrap_or_default(), compacted_companions);
        let folded_notice = format_folded_tool_roundtrips_notice(&folded);
        let build_note = |summary_status: &'static str, summary: Option<&str>| {
            let mut note = serde_json::json!({
                "schema_version": 1,
                "kind": "context_handoff",
                "strategy": "hard_switch",
                "handoff_event_id": handoff_event_id,
                "parent_handoff_event_id": parent_handoff_event_id,
                "from_generation": from_generation,
                "to_generation": to_generation,
                "created_at": timestamp,
                "trigger": trigger,
                "source_message_count": source.message_count,
                "source_token_estimate": source.token_estimate,
                "unpersisted_companion_count": source.unpersisted_companion_count,
                "folded_tool_roundtrips": {
                    "count": folded.count,
                    "tools": folded.tools,
                },
                "folded_tool_roundtrips_notice": folded_notice,
                "provenance_status": source.source_event_provenance_status,
                "source_event_range": range,
                "retained_recent_message_count": keep_recent,
                "lookup": {
                    "tool": crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME,
                    "arguments": {
                        "event_ids": source.source_event_ids,
                        "first_row_id": range.first_row_id,
                        "last_row_id": range.last_row_id
                    }
                },
                "summary_status": summary_status,
            });
            if let (Some(object), Some(text)) = (note.as_object_mut(), summary) {
                object.insert("summary".to_string(), serde_json::Value::String(text.to_string()));
            }
            note
        };
        // The smallest note this handoff can be has no summary; `omitted_size` is
        // the longest status such a note carries. When that does not validate, the
        // handoff is refused before the model is asked anything.
        let no_summary_message =
            format_context_handoff_message(&serde_json::to_string(&build_note("omitted_size", None))?);
        if validated_context_handoff_note(&no_summary_message).is_none() {
            tracing::warn!(
                trigger,
                handoff_bytes = no_summary_message.len(),
                limit = MAX_HANDOFF_MESSAGE_BYTES,
                referenced_events = source.source_event_ids.len(),
                "context switch skipped because the handoff note is oversized or does not validate"
            );
            return Ok(CompactionBuild::NotNeeded);
        }
        // The stored attachments of the session's own rows (the same key the switch
        // row is written to), so the summary reads each user message's own words. A
        // failed read gives none, and the summary then reads the composed text.
        let attachments = stored_attachments(
            audit.memory.as_ref(),
            &crate::memory::MemoryPrincipal {
                legacy_session_key: None,
                ..audit.memory_principal()
            },
        )
        .await;
        // One model summary of the cold window, asked of this turn's own model. It
        // never fails the turn: a failed or empty call writes the note without one.
        let cold_summary = summarize_cold_window(
            provider,
            model,
            compacted_source_messages.unwrap_or_default(),
            &attachments,
            config.max_context_tokens,
            &[],
            trigger,
        )
        .await;
        let mut note_json = serde_json::to_string(&build_note(cold_summary.status(), cold_summary.text()))?;
        let mut handoff_message = format_context_handoff_message(&note_json);
        let mut final_status = cold_summary.status();
        // A summary that pushes the note past its bound is dropped; the handoff is kept.
        if cold_summary.text().is_some() && validated_context_handoff_note(&handoff_message).is_none() {
            final_status = "omitted_size";
            note_json = serde_json::to_string(&build_note(final_status, None))?;
            handoff_message = format_context_handoff_message(&note_json);
        }
        cold_summary.log(trigger, final_status);
        // Every applied handoff is one a later load can validate, within its
        // size bound; anything else is refused before it is written.
        if validated_context_handoff_note(&handoff_message).is_none() {
            tracing::warn!(
                trigger,
                handoff_bytes = handoff_message.len(),
                limit = MAX_HANDOFF_MESSAGE_BYTES,
                referenced_events = source.source_event_ids.len(),
                "context switch skipped because the handoff note is oversized or does not validate"
            );
            return Ok(CompactionBuild::NotNeeded);
        }
        persist_context_switch(audit, trigger, source, &handoff_event_id, &handoff_message, &note_json).await?;
        if automatic_switch {
            audit.mark_automatic_switch_spent();
        }
        return Ok(CompactionBuild::Patch(CompactionPatch {
            replacement: vec![ChatMessage::assistant(handoff_message)],
            append_after: Vec::new(),
            guard,
        }));
    }

    let source_projection = source_history
        .get(start..compact_end)
        .map(bounded_compaction_projection)
        .unwrap_or_default();
    if source_projection.is_empty() {
        return Ok(CompactionBuild::NotNeeded);
    }

    // FU-02c-AUX-SENDS: compaction summarizer is exempt from admit_provider_send.
    // (a) tools is None (`chat_with_system`). (b) input is capped by
    // COMPACTION_MAX_SOURCE_CHARS and stored output by COMPACTION_MAX_SUMMARY_CHARS
    // / MEMORY_FLUSH_MAX_CHARS. (c) `compaction_aux_send_exemption_caps_input_and_output`
    // pins both literals. (d) the follow-up stays open for spend settlement.
    let mut replacement = Vec::new();
    if config.memory_flush {
        let flush_prompt = format!(
            "Write a concise memory flush message (max 6 bullets) capturing durable facts, decisions, and unresolved tasks from this history:\n\n{}",
            build_compaction_transcript(&source_projection)
        );
        let flush_note = provider
            .chat_with_system(
                Some("You write memory flush notes before context compaction."),
                &flush_prompt,
                model,
                0.1,
            )
            .await
            .unwrap_or_else(|_| "Memory flush fallback: key context retained.".to_string());
        replacement.push(ChatMessage::assistant(format!(
            "[Memory flush at {timestamp}: {}]",
            truncate_with_ellipsis(&flush_note, MEMORY_FLUSH_MAX_CHARS)
        )));
    }

    let summary = match config.mode {
        crate::config::AgentCompactionMode::Safeguard => {
            let transcript = build_compaction_transcript(&source_projection);
            let structured_prompt = format!(
                "Summarize this conversation into a structured context summary. \
                 You MUST include ALL of these sections (use ## headers):\n\
                 ## Decisions\n\
                 ## Open TODOs\n\
                 ## Constraints/Rules\n\
                 ## Pending user asks\n\
                 ## Exact identifiers\n\
                 Preserve all UUIDs, hashes, file paths, URLs, port numbers, and \
                 IP addresses EXACTLY as written. Never paraphrase identifiers.\n\n\
                 ## Progress\n\
                 ### Done\n\
                 ### In Progress\n\
                 ### Blocked\n\n\
                 ## Recent turns preserved verbatim\n\
                 Include the last 2-3 user/assistant exchanges word-for-word.\n\n\
                 ## Critical Context\n\
                 Key technical details that would be lost if forgotten.\n\n\
                 Conversation to summarize:\n{}",
                transcript
            );
            let raw = provider
                .chat_with_system(
                    Some("You are a context compaction summarizer."),
                    &structured_prompt,
                    model,
                    0.2,
                )
                .await
                .unwrap_or_else(|_| "Summary unavailable; compacted conservatively.".to_string());
            // Validate: require at least 3 of the expected ## headers.
            let required_headers = [
                "## Decisions",
                "## Open TODOs",
                "## Constraints",
                "## Pending",
                "## Exact",
                "## Progress",
                "## Critical Context",
            ];
            let found_count = required_headers.iter().filter(|h| raw.contains(*h)).count();
            let validated = if found_count < 3 {
                tracing::warn!(
                    found_count,
                    "Compaction summary missing required sections; retrying with explicit prompt"
                );
                let retry_prompt = format!(
                    "The previous summary was missing required sections. \
                     You MUST produce a summary with these exact ## headers: \
                     ## Decisions, ## Open TODOs, ## Constraints/Rules, ## Pending user asks, \
                     ## Exact identifiers, ## Progress, ## Critical Context. \
                     Do not skip any. Conversation:\n{}",
                    transcript
                );
                provider
                    .chat_with_system(
                        Some("You are a context compaction summarizer. Always include all required ## sections."),
                        &retry_prompt,
                        model,
                        0.1,
                    )
                    .await
                    .unwrap_or(raw)
            } else {
                raw
            };
            truncate_with_ellipsis(&validated, COMPACTION_MAX_SUMMARY_CHARS)
        }
        crate::config::AgentCompactionMode::Aggressive => {
            let mut summary = format!(
                "Aggressive compaction removed {} older messages; retained {} recent messages.",
                compact_count, keep_recent
            );
            if let Some(last_user) = source_projection.iter().rev().find(|m| m.role == "user") {
                summary.push_str(" Last preserved user intent: ");
                summary.push_str(&truncate_with_ellipsis(&last_user.content, 240));
            }
            truncate_with_ellipsis(&summary, COMPACTION_MAX_SUMMARY_CHARS)
        }
        crate::config::AgentCompactionMode::Switch | crate::config::AgentCompactionMode::Off => {
            return Ok(CompactionBuild::NotNeeded);
        }
    };
    // The summarizer reads the very turns that may still carry a fabricated
    // marker and is free to copy one into its output — the "recent turns
    // preserved verbatim" section asks it to. A summary is replayed on every
    // later turn and is persisted as compaction audit, so clean it in both
    // grains before it is measured, stored or inserted.
    let summary = strip_assistant_fabricated_tool_markers(&summary);
    let summary = crate::agent::tool_evidence::strip_model_written_tool_evidence(&summary).into_owned();
    let original_source = source_history.get(start..compact_end).unwrap_or(&[]);
    let fidelity_status = compaction_summary_fidelity_status(&summary, config.mode, original_source);
    persist_compaction_audit(
        audit,
        trigger,
        &config.mode,
        audit_source.as_ref(),
        &summary,
        fidelity_status,
    )
    .await;

    replacement.insert(
        0,
        ChatMessage::assistant(format!("[Context compacted at {timestamp}. Summary: {summary}]")),
    );

    // P1-1: Inject post-compaction context refresh so the model knows critical
    // rules may have been summarized and should re-read workspace instructions.
    let append_after = vec![post_compaction_refresh_aside()];

    Ok(CompactionBuild::Patch(CompactionPatch {
        replacement,
        append_after,
        guard,
    }))
}

#[cfg(test)]
pub(crate) async fn apply_configurable_compaction_with_replacement_len(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<Option<usize>> {
    let Some(patch) = build_configurable_compaction_patch(history, provider, model, config, audit, trigger).await?
    else {
        return Ok(None);
    };
    // Compacting history is real work done on the turn's behalf, so it counts
    // as progress even though it produces no user-visible output.
    crate::agent::idle::beat(crate::agent::idle::ProgressKind::Compaction);
    let replacement_len = patch.replacement.len();
    apply_compaction_patch_exact(history, &patch)?;
    Ok(Some(replacement_len))
}

/// [`apply_configurable_compaction_with_replacement_len`] over a `source`
/// history whose provider projection is `budget_history`: whether to compact
/// and how much is measured on the projection (what the provider receives,
/// enrichment included), the patch is built from and applied to `source`.
pub(crate) async fn apply_configurable_compaction_over_view(
    source: &mut crate::agent::turn_history::CanonicalHistory,
    budget_history: &[ChatMessage],
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<CompactionApply> {
    let patch = match build_configurable_compaction_patch_with_source_history(
        budget_history,
        source,
        provider,
        model,
        config,
        audit,
        trigger,
    )
    .await?
    {
        CompactionBuild::Patch(patch) => patch,
        CompactionBuild::NotNeeded => return Ok(CompactionApply::NotNeeded),
        CompactionBuild::Suppressed => return Ok(CompactionApply::Suppressed),
    };
    crate::agent::idle::beat(crate::agent::idle::ProgressKind::Compaction);
    let replacement_len = patch.replacement.len();
    source.apply_patch_with_refresh_aside(&patch)?;
    Ok(CompactionApply::Applied(replacement_len))
}

/// What [`apply_configurable_compaction_over_view`] did to the canonical history.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CompactionApply {
    /// Nothing applied; see [`CompactionBuild::NotNeeded`].
    NotNeeded,
    /// Applied; the replacement's length in the canonical history.
    Applied(usize),
    /// An automatic switch the turn already spent; nothing was applied.
    Suppressed,
}

#[cfg(test)]
pub(crate) async fn apply_configurable_compaction(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<bool> {
    Ok(
        apply_configurable_compaction_with_replacement_len(history, provider, model, config, audit, trigger)
            .await?
            .is_some(),
    )
}

/// The `user` aside summary compaction appends so the model knows critical
/// rules may have been summarized.
pub(crate) fn post_compaction_refresh_aside() -> ChatMessage {
    ChatMessage::user(format!(
        "{POST_COMPACTION_REFRESH_MARKER}\n\
         Session was just compacted. Critical rules may have been summarized.\n\
         Re-read workspace instructions if available."
    ))
}
