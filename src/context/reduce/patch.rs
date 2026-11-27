//! Building and applying the compaction patch: the demand that triggers a
//! compaction, the summary or handoff it writes, and the replacement it applies.

use super::audit::{persist_context_switch, switch_refusal_reason};
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
use super::summary::{build_compaction_audit_source, compaction_patch_guard_for};
use crate::agent::context_budget::planning_history_tokens;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_contracts::CompactionPatch;
use crate::providers::ChatMessage;
use crate::providers::Provider;
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

/// Why a compaction built no patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotNeededReason {
    /// The history is within the limit, or compaction is off.
    BelowLimit,
    /// Nothing before the live turn can be folded enough to fit the limit.
    NothingFoldable,
    /// Exact transcript provenance could not be resolved.
    ProvenanceUnavailable,
    /// No durable transcript scope to write the handoff to.
    NoAudit,
    /// The handoff note is too large or does not validate.
    Oversized,
}

impl NotNeededReason {
    /// The degraded reason a caller reports when a switch is not built and the
    /// turn continues on its trim.
    pub(crate) const fn degraded_reason(self) -> &'static str {
        match self {
            Self::BelowLimit => "below_limit",
            Self::NothingFoldable => "nothing_foldable",
            Self::ProvenanceUnavailable | Self::NoAudit | Self::Oversized => "handoff_unavailable",
        }
    }
}

/// What the builder decided for one compaction demand.
pub(crate) enum CompactionBuild {
    /// No patch, with the reason. A switch that is not built degrades to the
    /// caller's trim, labelled by [`NotNeededReason::degraded_reason`].
    NotNeeded(NotNeededReason),
    /// A patch that replaces the cold window.
    Patch(CompactionPatch),
    /// An automatic `switch` this admitted turn already spent: one succeeded
    /// earlier in the turn. Nothing is built, and nothing is
    /// reported as a degradation; the turn fits its request instead.
    Suppressed,
}

/// Tokens a switch leaves room for in the handoff note it writes.
const HANDOFF_NOTE_ALLOWANCE_TOKENS: usize = 600;

/// The fewest leading weights whose sum reaches `needed`, or `None` when all of
/// them together fall short. `Some(0)` when nothing is needed.
fn leading_count_covering(weights: &[usize], needed: usize) -> Option<usize> {
    let mut covered = 0usize;
    for (index, weight) in weights.iter().enumerate() {
        if covered >= needed {
            return Some(index);
        }
        covered = covered.saturating_add(*weight);
    }
    (covered >= needed).then_some(weights.len())
}

/// `excess` tokens of the planning total, expressed in provider weight units.
/// The weights are budgeted tokens of the provider view while the excess is
/// measured on the planning total, so the excess is scaled by their ratio.
fn excess_in_weight_units(excess: usize, planning_total: usize, weights: &[usize]) -> usize {
    if planning_total == 0 {
        return excess;
    }
    let weight_total = weights.iter().fold(0u128, |sum, weight| {
        sum.saturating_add(u128::try_from(*weight).unwrap_or(u128::MAX))
    });
    let scaled = u128::try_from(excess).unwrap_or(u128::MAX).saturating_mul(weight_total)
        / u128::try_from(planning_total).unwrap_or(u128::MAX);
    usize::try_from(scaled).unwrap_or(usize::MAX)
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
        CompactionBuild::NotNeeded(_) => Ok(None),
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
        return Ok(CompactionBuild::NotNeeded(NotNeededReason::BelowLimit));
    };
    if !CompactionDemand::from_trigger(trigger).is_forced() && planning_history_tokens(budget_history) <= limit {
        return Ok(CompactionBuild::NotNeeded(NotNeededReason::BelowLimit));
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
        return Ok(CompactionBuild::NotNeeded(NotNeededReason::NothingFoldable));
    }

    // Weighed by what the provider view sends for each message (replayed tool
    // rounds included), not by its stored text alone.
    let provider_weights = source.provider_weights();
    let non_system = source_history.get(start..).unwrap_or_default();
    let requested_keep_recent = crate::agent::budget_inputs::effective_keep_recent_weighted(
        provider_weights.get(start..).unwrap_or_default(),
        crate::agent::budget_inputs::keep_recent_cap(config, non_system.len()),
        config,
    );
    // The live request is never folded: the fold ends at the latest user query
    // (`fold_end`). An automatic switch folds at least enough of the oldest
    // messages before it to bring the request under the limit, with room for the
    // handoff note; when that region cannot cover the excess, all of it folds. A
    // manual `/compact` keeps the keep-recent count alone.
    let (fit_count, fold_end) = if automatic_switch {
        let live_start = crate::agent::history::latest_user_query_index(source_history)
            .filter(|&index| index >= start)
            .unwrap_or(start);
        let region = provider_weights.get(start..live_start).unwrap_or_default();
        let planning_total = planning_history_tokens(budget_history);
        let excess = planning_total
            .saturating_sub(limit)
            .saturating_add(HANDOFF_NOTE_ALLOWANCE_TOKENS);
        let excess = excess_in_weight_units(excess, planning_total, &provider_weights);
        (
            leading_count_covering(region, excess).unwrap_or(region.len()),
            live_start,
        )
    } else {
        (0, usize::MAX)
    };
    let requested_compact_count = non_system_count
        .saturating_sub(requested_keep_recent)
        .max(fit_count)
        .min(fold_end.saturating_sub(start));
    if requested_compact_count == 0 {
        return Ok(CompactionBuild::NotNeeded(NotNeededReason::NothingFoldable));
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
    if compact_end > fold_end {
        // The orphan walk moved forward past the live request: nothing may fold.
        return Ok(CompactionBuild::NotNeeded(NotNeededReason::NothingFoldable));
    }
    let compact_count = compact_end.saturating_sub(start);
    if compact_count == 0 {
        return Ok(CompactionBuild::NotNeeded(NotNeededReason::NothingFoldable));
    }
    let keep_recent = source_history.len().saturating_sub(compact_end);
    let compacted_source_messages = source_history.get(start..compact_end);
    let compacted_companions = RoundCompanions::Marked(companions.get(start..compact_end).unwrap_or_default());
    let mut audit_source = audit.and_then(|_| compacted_source_messages.map(build_compaction_audit_source));
    if let (Some(audit), Some(source_messages), Some(audit_source)) =
        (audit, compacted_source_messages, audit_source.as_mut())
    {
        // The kept messages that claim their own rows in a joint resolution:
        // acknowledged turns and caller-supplied turns other than the pending
        // request (never this turn's unstored reply or its tool round-trip
        // traffic).
        let acknowledged = source.provenance_kept_flags();
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
        return Ok(CompactionBuild::NotNeeded(NotNeededReason::NothingFoldable));
    };
    let timestamp = chrono::Utc::now().to_rfc3339();

    if matches!(config.mode, crate::config::AgentCompactionMode::Switch) {
        let Some(audit) = audit else {
            tracing::warn!(
                trigger,
                "context switch skipped because durable transcript scope is unavailable"
            );
            return Ok(CompactionBuild::NotNeeded(NotNeededReason::NoAudit));
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
            return Ok(CompactionBuild::NotNeeded(NotNeededReason::ProvenanceUnavailable));
        };
        let Some(range) = source.source_event_range.as_ref() else {
            return Ok(CompactionBuild::NotNeeded(NotNeededReason::ProvenanceUnavailable));
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
        // The skills this session loaded, newest first, so the note can ask the model to
        // re-read them. A session without a key, or a failed read, lists none.
        let skills = match audit.session_key.as_deref() {
            Some(session_key) => {
                crate::context::transcript::skills_in_use(
                    audit.memory.as_ref(),
                    &crate::memory::MemoryPrincipal {
                        legacy_session_key: None,
                        ..audit.memory_principal()
                    },
                    session_key,
                )
                .await
            }
            None => Vec::new(),
        };
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
            // Omitted when empty, so a handoff without loaded skills is byte-identical.
            if !skills.is_empty()
                && let Some(object) = note.as_object_mut()
            {
                object.insert("skills_in_use".to_string(), serde_json::json!(skills));
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
            return Ok(CompactionBuild::NotNeeded(NotNeededReason::Oversized));
        }
        // The stored attachments of the session's own rows (the same key the switch
        // row is written to), so the summary reads each user message's own words. A
        // failed read gives none, and the summary then reads the replayed text.
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
        // K-H3: the before-handoff context hooks of this turn add summary instructions.
        let instructions = crate::context::api::hook_points::summary_instructions(
            trigger,
            compacted_source_messages.map_or(0, <[_]>::len),
        )
        .await;
        let cold_summary = summarize_cold_window(
            provider,
            model,
            compacted_source_messages.unwrap_or_default(),
            &attachments,
            config.max_context_tokens,
            &instructions,
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
            return Ok(CompactionBuild::NotNeeded(NotNeededReason::Oversized));
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

    Ok(CompactionBuild::NotNeeded(NotNeededReason::NothingFoldable))
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
        CompactionBuild::NotNeeded(reason) => return Ok(CompactionApply::NotNeeded(reason)),
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
    NotNeeded(NotNeededReason),
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
