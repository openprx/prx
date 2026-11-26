//! Tool-call screening and execution for one round of the turn engine.

use crate::hooks::context::ContextPoint;
use crate::hooks::{HookEvent, payload_error};
use crate::providers::ChatMessage;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::agent::context_budget::reserve_request_tool_surface;
use crate::agent::diagnostics::{redact_sensitive_json_keys, scrub_credentials, tool_parse_log_digest};
use crate::agent::document_ingest::{persist_tool_output_document, persist_untrusted_external_copy};
use crate::agent::tool_result_recording::{
    ExecutedToolRecords, ObservedToolResult, ToolResultRecorder, compact_tool_result_for_history_with_status,
    tool_result_inline_budget_for_history,
};
use crate::agent::tool_scheduler::{execute_tools_with_service, shared_unrecoverable_tool_error};
use crate::agent::turn_contracts::{ToolCallNotification, ToolLoopCancelled, ToolLoopOutcome};
use crate::agent::turn_history::TurnHistory;
use crate::context::api::hook_points;
use crate::context::budget::anchor::{self};
use crate::context::reduce::engine::AfterTools;
use crate::context::transcript::{RoundLog, append_hook_contributions, call_id_for};
use crate::providers::ToolCall;

use super::state::{PreparedRound, TurnInputs, TurnState};
use crate::agent::tool_call_parser::ParsedToolCall;

/// The tool calls of one round, as the provider answered them.
pub(super) struct ToolCalls {
    pub(super) tool_calls: Vec<ParsedToolCall>,
    pub(super) assistant_history_content: String,
    pub(super) native_tool_calls: Vec<ToolCall>,
}

/// Screens the round's tool calls before any of them runs: the refusal of a
/// streaming ingress without a registry, the terminal `stay_silent` decision
/// and the admission of discovered tools. Returns the outcome when the turn
/// ends here.
pub(super) async fn screen_calls(
    turn: &TurnInputs<'_>,
    round: &mut PreparedRound,
    tool_calls: &mut [ParsedToolCall],
) -> Result<Option<ToolLoopOutcome>> {
    let TurnInputs {
        expose_stay_silent,
        service,
        context,
        proof,
        ref tools_registry,
        ref runtime_adapter,
        ref cancellation_token,
        ref inherited_exclusions,
        ref safety_surface,
        ..
    } = *turn;
    // A streaming ingress cannot execute a structured provider tool call
    // unless it supplied a registry. Fail at the ownership boundary rather
    // than manufacturing repeated unknown-tool results until the iteration
    // cap is exhausted. This preserves the Redux driver's permanent,
    // immediate no-registry rejection contract.
    if runtime_adapter.stream_provider && tools_registry.is_empty() {
        return Err(anyhow::anyhow!(
            "provider returned tool_calls but no tools_registry is configured"
        ));
    }

    // Smart group-reply: a `stay_silent` call is a *terminal* decision for the
    // turn. Detect it BEFORE executing any tool and BEFORE pushing the
    // assistant/tool turn into history, so a silent turn leaves zero trace in
    // the conversation history and produces no channel output. Only honored
    // when the caller advertised the tool (`expose_stay_silent`), which is
    // restricted to smart group turns; this keeps DMs / non-smart turns from
    // ever short-circuiting to Silent even if a model hallucinates the name.
    if expose_stay_silent && round.compiled_tool_context.allows(crate::tools::STAY_SILENT_TOOL_NAME) {
        if let Some(silent_call) = tool_calls
            .iter()
            .find(|call| call.name == crate::tools::STAY_SILENT_TOOL_NAME)
        {
            let reason = crate::tools::stay_silent::extract_reason(&silent_call.arguments);
            let (reason_len, reason_sha256) = tool_parse_log_digest(&reason);
            tracing::info!(reason_len, reason_sha256, "tool loop: model chose to stay silent");
            if cancellation_token.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Err(ToolLoopCancelled.into());
            }
            service.record_agent_silence(proof, context, &round.compiled_tool_context)?;
            return Ok(Some(ToolLoopOutcome::Silent { reason }));
        }
    }

    // A model may call a previously returned hit by public name or exact
    // capability ID. Admit that specific offer before widening this batch's
    // dispatch snapshot; the provider snapshot above remains immutable.
    let mut discovery_dispatch_specs: Option<Vec<crate::tools::ToolSpec>> = None;
    for call in &mut *tool_calls {
        if round.compiled_tool_context.allows(&call.name) {
            continue;
        }
        let Some(proposal) = service.prepare_discovered_tool(proof, context, &call.name)? else {
            continue;
        };
        if inherited_exclusions
            .iter()
            .any(|excluded| excluded == &proposal.spec.name || excluded == &proposal.root)
        {
            continue;
        }
        let Some(proposal) = service.admit_discovered_tool(proof, context, &call.name)? else {
            continue;
        };
        if proposal.admission.is_blocked() {
            continue;
        }
        if !safety_surface.contains(&proposal.spec.name) {
            continue;
        }
        call.name.clone_from(&proposal.spec.name);
        let specs = discovery_dispatch_specs.get_or_insert_with(|| round.compiled_tool_context.tool_specs().to_vec());
        if !specs.iter().any(|spec| spec.name == proposal.spec.name) {
            specs.push(proposal.spec);
        }
    }
    if let Some(specs) = discovery_dispatch_specs {
        let (execution, bound_context) = service.bind_agent_execution_with_runtime(
            proof,
            context,
            &round.execution_adapter.tool_execution_context,
            &specs,
            round.execution_adapter.tool_execution_service.clone(),
        )?;
        round.execution_adapter.allowed_tool_names = Some(std::sync::Arc::new(
            specs.iter().map(|spec| spec.name.clone()).collect(),
        ));
        round.execution_adapter.tool_execution_service = Some(execution);
        round.execution_adapter.tool_execution_context = bound_context;
        round.execution_adapter.tool_execution_context.call_budget = Some(
            crate::agent::send_admission::call_token_budget(service, context, proof)?,
        );
        round.execution_adapter.tool_execution_context.dispatch_origin = crate::tools::DispatchOrigin::ModelTurn;
        round.execution_adapter.tool_execution_context.turn_surface =
            Some(Arc::new(safety_surface.for_publication(&specs)));
    }

    Ok(None)
}

/// Runs the round's tool calls: start notifications, execution through the
/// scheduler, the approval exit, result recording and the mid-turn trim.
/// Returns the outcome when the turn ends on an approval, `None` to continue.
pub(super) async fn run_tools(
    turn: &TurnInputs<'_>,
    state: &mut TurnState,
    turn_history: &mut TurnHistory,
    history: &mut Vec<ChatMessage>,
    round: &PreparedRound,
    calls: ToolCalls,
) -> Result<Option<ToolLoopOutcome>> {
    let ToolCalls {
        tool_calls,
        assistant_history_content,
        native_tool_calls,
    } = calls;
    let TurnInputs {
        chat_mode,
        compaction_config,
        provider_name,
        model,
        observer,
        hooks,
        ref document_ingest,
        ref event_fabric,
        ref request_event_scope,
        ref cancellation_token,
        ref on_tool_call,
        ref usage_anchors,
        ref read_only_schedule,
        ..
    } = *turn;
    // Execute tool calls and build results. `individual_results` tracks per-call output so
    // native-mode history can emit one role=tool message per tool call with the correct ID.
    //
    // Conservative scheduler:
    // - run read-only tools in small bounded batches;
    // - keep all stateful tools strictly serial.
    let mut tool_results = String::new();
    for (index, call) in tool_calls.iter().enumerate() {
        hooks
            .emit(
                HookEvent::ToolCallStart,
                serde_json::json!({
                    "tool": call.name,
                    "arguments": redact_sensitive_json_keys(&call.arguments),
                }),
            )
            .await;
        if let Some(tx) = on_tool_call {
            let args_str = scrub_credentials(&call.arguments.to_string());
            let args_summary = truncate_with_ellipsis(&args_str, 80);
            let id = native_tool_calls
                .get(index)
                .map(|native| native.id.clone())
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("{}#{index}", call.name));
            let _ = tx
                .send(ToolCallNotification::Started {
                    id,
                    name: call.name.clone(),
                    args_summary,
                })
                .await;
        }
    }

    let tools_started_at = Instant::now();
    // K-H3: decided once per round. When a tool-result context hook may replace
    // what the calls return, the recorder holds a placeholder for each call from
    // the moment it returns until the hooks answered below, so no interruption
    // in between can persist the raw output. Without such an action the
    // recorder keeps the raw output exactly as before.
    let hold_tool_results = hooks.has_context_actions(ContextPoint::ToolResult);
    let recorder = ToolResultRecorder::with_hold(tool_calls.len(), hold_tool_results);
    // The assistant message that *requested* these calls goes into history
    // before any of them runs. Written after the batch, as it used to be, an
    // interruption partway through the batch left the calls that had already
    // run — and already had side effects — with nothing in history to attach
    // to, so a replay repeated every one of them.
    history.push(ChatMessage::assistant(assistant_history_content));
    let tool_event_scope = request_event_scope
        .clone()
        .unwrap_or_else(|| round.execution_adapter.tool_execution_context.envelope.message_scope());
    let round_log = RoundLog::new(event_fabric.as_ref(), &tool_event_scope, state.iteration);
    if let Some(log) = round_log.as_ref() {
        log.append_calls(&tool_calls, &native_tool_calls).await;
    }
    let executed = ExecutedToolRecords::new(history, &recorder, &tool_calls, &native_tool_calls);

    let individual_results = match execute_tools_with_service(
        &tool_calls,
        &native_tool_calls,
        &round.execution_adapter,
        read_only_schedule.clone(),
        cancellation_token.as_ref(),
        chat_mode,
        observer,
        &recorder,
    )
    .await
    {
        Ok(results) => results,
        Err(error) => {
            if let Some(pending) = error.downcast_ref::<crate::agent::turn_contracts::ToolLoopAwaitingApproval>() {
                // Dropping the recorder guard retains completed calls and
                // explicit cancellation placeholders for unexecuted siblings.
                drop(executed);
                return Ok(Some(ToolLoopOutcome::AwaitingApproval {
                    reply: pending.reply.clone(),
                }));
            }
            return Err(error);
        }
    };
    let tools_elapsed_ms = tools_started_at.elapsed().as_millis() as u64;

    // Only a batch that actually produced a result counts as real tool use.
    // Dispatching is not enough: a batch whose every call was denied by the
    // permission layer or the idempotency ledger did no work, so a later
    // `[Used tools: ...]` line is still provably fabricated and still worth
    // the one corrective retry.
    if individual_results.iter().any(|result| result.executed) {
        state.executed_any_tool_call = true;
    }

    // K-H3: the tool-result context hooks may replace the text the model reads
    // back or add a note. The notify event and the failure counters keep the raw
    // output; every model-reachable copy below uses the effective text. With no
    // such action configured nothing is built and every text stays as it was.
    let (effective_texts, contributions) = if hold_tool_results {
        // Every call of the batch returned, so every slot already holds the
        // placeholder recorded by the scheduler; it stays there while the hooks
        // run under the await below.
        let raw_calls: Vec<(&str, String, bool, &str)> = tool_calls
            .iter()
            .zip(individual_results.iter())
            .enumerate()
            .map(|(index, (call, outcome))| {
                (
                    call.name.as_str(),
                    call_id_for(&call.name, index, &native_tool_calls),
                    outcome.succeeded(),
                    outcome.content.as_str(),
                )
            })
            .collect();
        let (texts, contributions) = hook_points::tool_result_texts(hooks, &raw_calls).await;
        for (index, outcome) in individual_results.iter().enumerate() {
            let effective = texts
                .get(index)
                .and_then(Option::as_ref)
                .map_or(outcome.content.as_str(), String::as_str);
            recorder.record(index, effective);
        }
        (texts, contributions)
    } else {
        (Vec::new(), Vec::new())
    };
    if let Some(fabric) = event_fabric.as_ref() {
        append_hook_contributions(fabric, &tool_event_scope, &contributions).await;
    }

    // FIX-P2-02: persistence (untrusted external copy + oversized tool_output
    // document) happens exactly once per tool result, in this loop, for every
    // mode. The native-history branch below reuses the document status captured
    // here instead of persisting a second time.
    let mut document_statuses: Vec<&'static str> = Vec::with_capacity(individual_results.len());
    let mut repeated_unrecoverable: Option<String> = None;
    // Everything below that plans against the window plans against the
    // reserved one, the same window the preflight used: this turn's schemas
    // and the prompt sections compiled from them occupy it before a single
    // tool result does. Built once per tool round and shared by the inline
    // caps and the mid-turn trim.
    let reserved_config = compaction_config.map(|config| reserve_request_tool_surface(config, round.tool_surface));
    for (index, (call, outcome)) in tool_calls.iter().zip(individual_results.iter()).enumerate() {
        let success = outcome.succeeded();
        let result = &outcome.content;
        let replaced = effective_texts
            .get(index)
            .and_then(Option::as_ref)
            .map(|text| ObservedToolResult {
                content: text.clone(),
                ..outcome.clone()
            });
        let observed = replaced.as_ref().unwrap_or(outcome);
        hooks
            .emit(
                HookEvent::ToolCall,
                serde_json::json!({
                    "tool": call.name,
                    "success": success,
                    "output": result,
                }),
            )
            .await;
        if let Some(tx) = on_tool_call {
            let id = native_tool_calls
                .get(index)
                .map(|native| native.id.clone())
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("{}#{index}", call.name));
            let _ = tx
                .send(ToolCallNotification::Finished {
                    id,
                    name: call.name.clone(),
                    success,
                    duration_ms: tools_elapsed_ms,
                })
                .await;
        }
        if !success {
            hooks.emit(HookEvent::Error, payload_error("tool", result)).await;
            if shared_unrecoverable_tool_error(result) {
                let signature = format!("{}:{}", call.name, result.to_ascii_lowercase());
                let count = state.unrecoverable_failures.entry(signature).or_insert(0);
                *count = count.saturating_add(1);
                if *count >= 2 {
                    repeated_unrecoverable = Some(call.name.clone());
                }
            }
        }

        // Track consecutive failures per tool for repeated-failure hints.
        let repeated_hint = if success {
            state.consecutive_failures.remove(&call.name);
            String::new()
        } else {
            let count = state.consecutive_failures.entry(call.name.clone()).or_insert(0);
            *count += 1;
            if *count >= 2 {
                format!(
                    "\n[System: tool '{}' has failed {} times consecutively. Consider a different approach or tool.]",
                    call.name, count
                )
            } else {
                String::new()
            }
        };

        // FIX-P2-02: keep a verbatim, read-only, untrusted copy of successful
        // external content fetches (web_fetch / http_request / file_read).
        if success {
            persist_untrusted_external_copy(document_ingest.as_ref(), &call.name, &observed.content).await;
        }

        // P0-8: Keep history bounded and persist oversized outputs in the
        // document layer when a runtime document sink is available. The
        // storage threshold is the same budget the history cut uses, so a
        // result is stored exactly when it is cut.
        let max_inline_chars =
            tool_result_inline_budget_for_history(&call.name, executed.history(), reserved_config.as_ref());
        let document_status = persist_tool_output_document(
            document_ingest.as_ref(),
            &call.name,
            &observed.content,
            max_inline_chars,
        )
        .await
        .unwrap_or("pending_document_backend");
        // FIX-P2-02: record the status so the native-history branch can reuse
        // it without persisting the same document a second time.
        document_statuses.push(document_status);
        let truncated_result = compact_tool_result_for_history_with_status(
            &call.name,
            &observed.content,
            max_inline_chars,
            document_status,
        );
        if let Some(log) = round_log.as_ref() {
            let call_id = call_id_for(&call.name, index, &native_tool_calls);
            log.append_result(&call_id, &call.name, observed, &truncated_result, document_status)
                .await;
        }
        let _ = writeln!(
            tool_results,
            "<tool_result name=\"{}\">\n{}{}\n</tool_result>",
            call.name, truncated_result, repeated_hint
        );
    }

    // Tool results follow the assistant message already in history.
    // Native mode: use JSON-structured messages so convert_messages() can
    // reconstruct proper OpenAI-format tool_calls and tool result messages.
    // Prompt mode: use XML-based text format as before.
    let mut result_messages: Vec<ChatMessage> = Vec::with_capacity(native_tool_calls.len().max(1));
    if native_tool_calls.is_empty() {
        // Prompt/text mode marker. Shares the prefix constant with the CTE
        // snapshot parser (`build_causal_state_from_chat`) so a wording change
        // stays consistent across producer and parser.
        result_messages.push(ChatMessage::user(format!(
            "{}\n{tool_results}",
            crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
        )));
    } else {
        for (index, (native_call, result)) in native_tool_calls.iter().zip(individual_results.iter()).enumerate() {
            // FIX-P2-02: persistence (untrusted external copy + oversized
            // tool_output document) already ran exactly once in the loop above,
            // which iterates the same `individual_results` in order. Reuse the
            // captured document status here instead of persisting a second time
            // (the previous duplicate write doubled I/O and storage for native
            // mode). Fall back defensively if the status vector is shorter than
            // expected (positional mismatch should not happen in practice).
            let document_status = document_statuses
                .get(index)
                .copied()
                .unwrap_or("pending_document_backend");
            let max_inline_chars =
                tool_result_inline_budget_for_history(&native_call.name, executed.history(), reserved_config.as_ref());
            let effective = effective_texts
                .get(index)
                .and_then(Option::as_ref)
                .unwrap_or(&result.content);
            let truncated_result = compact_tool_result_for_history_with_status(
                &native_call.name,
                effective,
                max_inline_chars,
                document_status,
            );
            let tool_msg = serde_json::json!({
                "tool_call_id": native_call.id,
                "content": truncated_result,
            });
            result_messages.push(ChatMessage::tool(tool_msg.to_string()));
        }
    }
    executed.commit(result_messages);

    // P1-4: Token-aware mid-turn trim (primary) + count-based safety net (secondary).
    //
    // Against the same reserved window the preflight uses: the schemas and
    // the appended prompt sections of this turn occupy it too, so trimming
    // to the unreserved limit here would hand the next iteration's
    // preflight history it has to compact when a trim would have done.
    //
    // Under the same request floor the preflight has. This trim removes the
    // oldest non-system message until the history fits, and the turn's own
    // request is the oldest thing in it: on a saturated window it goes
    // first, *before* the next iteration's preflight can capture it, after
    // which that preflight has nothing to put back and the turn runs on
    // without the question it was asked. Captured inside the branch, so a
    // round that needs no trim still clones nothing.
    if let Some(reserved_config) = reserved_config.as_ref() {
        let planning = usage_anchors
            .probe(history, None, provider_name, model)
            .planning(round.tool_surface);
        let mut after_tools = AfterTools {
            history: &mut *history,
            turn_history,
            reserved_config,
        };
        anchor::scoped(
            planning,
            Box::pin(crate::context::api::round::after_tools(&mut after_tools)),
        )
        .await?;
    }
    if let Some(tool_name) = repeated_unrecoverable {
        anyhow::bail!(
            "stopped after a repeated unrecoverable tool failure ('{tool_name}' is blocked by policy/permissions); not retrying further"
        );
    }
    Ok(None)
}
