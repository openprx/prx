//! The request-context event of one round: the sizes and provenance of the
//! prepared request, recorded before the provider call.

use crate::providers;

use crate::agent::context_budget::{
    budgeted_tokens, estimate_history_tokens_with_source, estimate_request_tokens, token_source_label,
};

use super::state::{PreparedRound, RoundBudget, RoundSurface, TurnInputs, TurnState};

/// Records the request context event of one round when something reads it:
/// the durable event, or an enabled prompt subscriber.
pub(super) async fn record_request_context(
    turn: &TurnInputs<'_>,
    state: &TurnState,
    surface: &RoundSurface<'_>,
    budget: &RoundBudget,
    round: &PreparedRound,
) {
    let TurnInputs {
        compaction_config,
        ref event_fabric,
        ref request_event_scope,
        provider_name,
        model,
        ..
    } = *turn;
    let tool_surface_counts = &surface.tool_surface_counts;
    let tool_snapshot = round.compiled_tool_context.fingerprint();
    // How much of this request is prompt and how much is native tool
    // schema. Sizes, counts and estimates only: this is emitted into a
    // durable event, so no schema body, prompt body or tool argument may
    // appear here.
    //
    // One estimator, two inputs. The preflight measured `history`; this
    // measures `prepared_messages` after the multimodal pass, the
    // corrective asides, the middleware, the appended tool instructions
    // and the system coalescing, so the two totals are systematically
    // different numbers for the same iteration. Both are reported —
    // `budgeted_*` is what the window decision was made on, the rest is
    // what actually leaves — so a disagreement is an observable delta
    // instead of an invariant nobody can check.
    //
    // Measured only when something will read it: the durable event, or an
    // enabled subscriber on this target. Otherwise the turn would pay a
    // second full tokenizer pass over the prompt, on every iteration, for
    // a number nobody consumes.
    if event_fabric.is_some() || tracing::enabled!(target: "openprx::prompt", tracing::Level::DEBUG) {
        let budgeted_messages_tokens: Option<usize> = budget.budgeted_totals.map(|(messages, _)| messages);
        let budgeted_request_tokens: Option<usize> = budget.budgeted_totals.map(|(_, request)| request);
        let messages_bytes: usize = round.prepared_messages.messages.iter().fold(0usize, |total, message| {
            total
                .saturating_add(message.role.len())
                .saturating_add(message.content.len())
        });
        let messages_chars: usize = round.prepared_messages.messages.iter().fold(0usize, |total, message| {
            total
                .saturating_add(message.role.chars().count())
                .saturating_add(message.content.chars().count())
        });
        let messages_measurement = estimate_history_tokens_with_source(&round.prepared_messages.messages);
        let messages_tokens = budgeted_tokens(messages_measurement);
        let native_tool_tokens = budgeted_tokens(round.tool_surface.native_payload.measurement);
        let instruction_tokens = budgeted_tokens(round.tool_surface.instructions);
        let request_tokens =
            estimate_request_tokens(messages_measurement, round.tool_surface.native_payload.measurement);
        let token_source: &'static str = token_source_label(messages_measurement.source);
        tracing::debug!(
            target: "openprx::prompt",
            system_prompt_sha256 = round.system_prompt_sha256.as_deref().unwrap_or("none"),
            round.system_prompt_chars,
            tool_snapshot_sha256 = %tool_snapshot.sha256,
            tool_count = tool_snapshot.count,
            tool_snapshot_render_hits = state.tool_snapshot_render_cache.hits(),
            tool_snapshot_render_misses = state.tool_snapshot_render_cache.misses(),
            messages_bytes,
            messages_chars,
            messages_tokens,
            native_tool_bytes = round.tool_surface.native_payload.bytes,
            native_tool_tokens,
            instruction_tokens,
            budgeted_messages_tokens,
            budgeted_request_tokens,
            request_tokens,
            token_source,
            "final request system prompt compiled"
        );
        // Compilation is completed work, including a cold tokenizer or a
        // changed tool surface. Record it before the durable event yields;
        // an iteration starting is not a completed provider response.
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::RequestPrepared);

        if let Some(fabric) = event_fabric.as_ref() {
            let request_context_payload = serde_json::json!({
                "schema_version": 1,
                "iteration": state.iteration,
                // Durable: a `custom:` URL may carry `user:password@`.
                "provider": providers::redact_endpoint_userinfo(provider_name),
                "model": model,
                "messages_count": round.prepared_messages.messages.len(),
                "system_prompt": {
                    "sha256": round.system_prompt_sha256.as_deref(),
                    "chars": round.system_prompt_chars,
                },
                "tool_snapshot": &tool_snapshot,
                "tool_surface": tool_surface_counts.to_json(),
                "tool_snapshot_render": {
                    "hits": state.tool_snapshot_render_cache.hits(),
                    "misses": state.tool_snapshot_render_cache.misses(),
                },
                "budget": budget.request_budget
                    .as_ref()
                    .zip(compaction_config)
                    .map(|(planned, config)| planned.to_json(config, budgeted_request_tokens)),
                "request_estimate": {
                    "messages_bytes": messages_bytes,
                    "messages_chars": messages_chars,
                    "messages_tokens": messages_tokens,
                    "native_tool_bytes": round.tool_surface.native_payload.bytes,
                    "native_tool_tokens": native_tool_tokens,
                    "instruction_tokens": instruction_tokens,
                    "budgeted_messages_tokens": budgeted_messages_tokens,
                    "budgeted_total_tokens": budgeted_request_tokens,
                    "total_tokens": request_tokens,
                    "token_source": token_source_label(messages_measurement.source),
                },
                "provenance": crate::context::request_provenance::request_provenance(
                    &crate::context::request_provenance::RequestFacts {
                        messages: &round.prepared_messages.messages,
                        run_id: request_event_scope
                            .as_ref()
                            .and_then(|scope| scope.run_id.as_deref())
                            .or(round.execution_adapter.tool_execution_context.envelope.run_id.as_deref()),
                        system_sha256: round.system_prompt_sha256.as_deref(),
                        system_chars: round.system_prompt_chars,
                        tool_surface_sha256: &tool_snapshot.sha256,
                        tools_published: tool_snapshot.count,
                        tools_deferred: tool_surface_counts.deferred_roots(),
                    },
                ),
            });
            let scope = request_event_scope
                .clone()
                .unwrap_or_else(|| round.execution_adapter.tool_execution_context.envelope.message_scope());
            let scope = scope.with_recipient(format!("{provider_name}/{model}"));
            let content = format!(
                "system_prompt_sha256={} tool_snapshot_sha256={} iteration={iteration}",
                round.system_prompt_sha256.as_deref().unwrap_or("none"),
                tool_snapshot.sha256,
                iteration = state.iteration
            );
            if let Err(error) = fabric
                .record_runtime_event(
                    scope,
                    "llm.request.context.compiled",
                    content,
                    Some(request_context_payload.to_string()),
                )
                .await
            {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::warn!(
                    error_len,
                    error_sha256,
                    "Failed to persist compiled LLM request context"
                );
            }
        }
    } else {
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::RequestPrepared);
    }
}
