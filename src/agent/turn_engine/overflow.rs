//! Overflow recovery: a provider call rejected for its context size is
//! answered by compacting the canonical history and retrying the request.

use anyhow::Result;

use crate::agent::compaction::{
    COMPACTION_KEEP_RECENT_MESSAGES, CompactionBuild, build_configurable_compaction_patch_with_source_history,
    persist_context_switch_degradation,
};
use crate::agent::context_budget::{
    PRE_TURN_FLUSH_THRESHOLD, estimate_history_tokens, plan_context_budget, trim_history_to_context_budget,
    trim_history_to_context_budget_preserving_compaction_replacement_with_floor,
};
use crate::agent::context_preflight::{
    RestoredRequestPosition, capture_current_turn_request, restore_current_turn_request,
};
use crate::agent::history::apply_aggressive_trim;
use crate::agent::turn_history::TurnHistory;
use crate::context::reduce::engine::report_durable_compaction;
use crate::providers::ChatMessage;

use super::rederive_provider_view;
use super::state::{TurnInputs, TurnState};

/// Recover from one provider overflow: compact the canonical history, trim
/// this retry's projection, and fail the turn when no progress is possible.
pub(super) async fn recover_from_overflow(
    turn: &TurnInputs<'_>,
    state: &mut TurnState,
    turn_history: &mut TurnHistory,
    history: &mut Vec<ChatMessage>,
    error: &anyhow::Error,
) -> Result<()> {
    let TurnInputs {
        compaction_config,
        provider,
        model,
        channel_name,
        hooks,
        ref document_ingest,
        ref runtime_adapter,
        ref usage_anchors,
        ..
    } = *turn;
    if state.overflow_recovery_passes >= crate::agent::turn_contracts::MAX_OVERFLOW_RECOVERY_PASSES {
        return Err(crate::agent::turn_contracts::ContextCapacityError::RecoveryPassLimit {
            limit: crate::agent::turn_contracts::MAX_OVERFLOW_RECOVERY_PASSES,
            provider_context: crate::providers::sanitize_api_error(&format!("{error:#}")),
        }
        .into());
    }
    state.overflow_recovery_passes += 1;
    usage_anchors.invalidate();
    let messages_before_compaction = history.len();
    let tokens_before_compaction = estimate_history_tokens(history);
    tracing::warn!("Context overflow detected; running compaction and retrying LLM call");
    // The handoff or summary is built from and applied to the canonical
    // history and reported to the session authority; every trim below is
    // this retry's projection of the provider view only, so the session
    // keeps the turns a later handoff can still recover.
    let mut durable_applied = false;
    if let Some(config) = compaction_config {
        let mut before_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
        let mut degraded_reason = None;
        let canonical = turn_history.canonical_mut(history);
        let canonical_request = capture_current_turn_request(canonical.messages());
        // Budgeted on the overflowed provider view, built from
        // the canonical history.
        let compaction = match build_configurable_compaction_patch_with_source_history(
            history,
            canonical,
            provider,
            model,
            config,
            document_ingest.as_ref(),
            "overflow_retry",
        )
        .await
        {
            Ok(CompactionBuild::Patch(patch)) => {
                crate::agent::idle::beat(crate::agent::idle::ProgressKind::Compaction);
                let mut compacted = canonical.clone();
                match compacted.apply_patch_with_refresh_aside(&patch) {
                    Ok(()) => {
                        // The whole resulting history, request included, is what
                        // the session adopts: restoring the request only in the
                        // view would leave the saved history without it. A
                        // refresh aside moves into the projection when the
                        // compaction is finished.
                        compacted.restore_request_at_tail(canonical_request, "overflow_retry");
                        Some((std::mem::replace(canonical, compacted), patch.replacement.len()))
                    }
                    Err(error) => {
                        warn_overflow_patch_refused(&error);
                        degraded_reason = Some("handoff_unavailable");
                        None
                    }
                }
            }
            Ok(CompactionBuild::NotNeeded(not_built)) => {
                degraded_reason = Some(not_built.degraded_reason());
                None
            }
            // An automatic switch this turn already spent: the trim below is the
            // remediation and is not a new degradation.
            Ok(CompactionBuild::Suppressed) => None,
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::warn!(error_len, error_sha256, "Overflow retry compaction failed");
                degraded_reason = Some("handoff_failed");
                None
            }
        };
        if let Some((before, replacement_floor)) = compaction {
            durable_applied = true;
            report_durable_compaction(
                &runtime_adapter,
                Some(before),
                turn_history.finish_durable_compaction(history),
                config,
            )
            .await?;
            rederive_provider_view(turn_history, history, hooks, channel_name).await;
            if runtime_adapter.stream_provider {
                let switch_mode = matches!(config.mode, crate::config::AgentCompactionMode::Switch);
                let budget = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                before_trim = budget;
                if switch_mode && budget.over_hard_limit {
                    degraded_reason = Some("over_hard_limit_after_handoff");
                }
                if !switch_mode || budget.over_hard_limit {
                    let floor = replacement_floor.saturating_add(turn_history.replacement_offset());
                    turn_history.reshape(history, |view| {
                        let request = capture_current_turn_request(view);
                        trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
                            view, config, floor,
                        );
                        restore_current_turn_request(view, request, "overflow_retry", RestoredRequestPosition::Tail);
                    });
                }
            }
        } else {
            let stream_provider = runtime_adapter.stream_provider;
            let handoff_failed = degraded_reason == Some("handoff_failed");
            turn_history.reshape(history, |view| {
                let request = capture_current_turn_request(view);
                if handoff_failed {
                    apply_aggressive_trim(view, config.keep_recent_messages);
                } else {
                    trim_overflow_without_handoff(
                        view,
                        config,
                        stream_provider,
                        messages_before_compaction,
                        tokens_before_compaction,
                    );
                }
                restore_current_turn_request(
                    view,
                    request,
                    "overflow_retry",
                    RestoredRequestPosition::AboveEverythingLeftStanding,
                );
            });
        }
        if matches!(config.mode, crate::config::AgentCompactionMode::Switch)
            && let Some(reason) = degraded_reason
        {
            let after_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
            persist_context_switch_degradation(
                document_ingest.as_ref(),
                "overflow_retry",
                reason,
                before_trim.used_tokens,
                after_trim.used_tokens,
                after_trim.available_input_tokens,
            )
            .await;
        }
    } else {
        turn_history.reshape(history, |view| {
            let request = capture_current_turn_request(view);
            apply_aggressive_trim(view, COMPACTION_KEEP_RECENT_MESSAGES);
            restore_current_turn_request(
                view,
                request,
                "overflow_retry",
                RestoredRequestPosition::AboveEverythingLeftStanding,
            );
        });
    }
    // A view re-derived from the compacted canonical history can be
    // larger than the view that overflowed, which held this turn's
    // earlier trims: this retry then trims its projection as well.
    if durable_applied
        && let Some(config) = compaction_config
        && history.len() >= messages_before_compaction
        && estimate_history_tokens(history) >= tokens_before_compaction
    {
        let stream_provider = runtime_adapter.stream_provider;
        turn_history.reshape(history, |view| {
            let request = capture_current_turn_request(view);
            trim_overflow_without_handoff(
                view,
                config,
                stream_provider,
                messages_before_compaction,
                tokens_before_compaction,
            );
            restore_current_turn_request(view, request, "overflow_retry", RestoredRequestPosition::Tail);
        });
    }
    let made_progress =
        history.len() < messages_before_compaction || estimate_history_tokens(history) < tokens_before_compaction;
    if !made_progress {
        return Err(crate::agent::turn_contracts::ContextCapacityError::NoProgress {
            provider_context: crate::providers::sanitize_api_error(&format!("{error:#}")),
        }
        .into());
    }

    Ok(())
}

/// Logs a refused overflow compaction patch with lengths and range only.
fn warn_overflow_patch_refused(error: &crate::agent::turn_contracts::CompactionPatchError) {
    match error {
        crate::agent::turn_contracts::CompactionPatchError::RangeOutOfBounds { start, end, len } => {
            let (range_start_index, range_end_index, history_len): (usize, usize, usize) = (*start, *end, *len);
            tracing::warn!(
                range_start_index,
                range_end_index,
                history_len,
                "overflow compaction patch range out of bounds; trimming without a handoff"
            );
        }
        crate::agent::turn_contracts::CompactionPatchError::GuardMismatch {
            expected_len,
            actual_len,
        } => {
            let (guard_history_len, history_len): (usize, usize) = (*expected_len, *actual_len);
            tracing::warn!(
                guard_history_len,
                history_len,
                "overflow compaction patch guard mismatch; trimming without a handoff"
            );
        }
    }
}

/// The overflow retry's trim when no handoff can be applied: budget trim for
/// streaming providers, aggressive trim when that made no progress.
fn trim_overflow_without_handoff(
    history: &mut Vec<ChatMessage>,
    config: &crate::config::AgentCompactionConfig,
    stream_provider: bool,
    messages_before_compaction: usize,
    tokens_before_compaction: usize,
) {
    if stream_provider {
        trim_history_to_context_budget(history, config);
    }
    if !stream_provider
        || history.len() == messages_before_compaction && estimate_history_tokens(history) >= tokens_before_compaction
    {
        apply_aggressive_trim(history, config.keep_recent_messages);
    }
}
