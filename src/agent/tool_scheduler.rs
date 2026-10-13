#[cfg(test)]
use crate::agent::diagnostics::scrub_credentials;
#[cfg(test)]
use crate::agent::execution_policy::runtime_approval_grant_for_call;
use crate::agent::execution_policy::{is_write_tool, preview_tool_arguments};
#[cfg(test)]
use crate::agent::tool_call_parser::find_tool;
use crate::agent::tool_call_parser::{ParsedToolCall, unparseable_native_arguments};
use crate::agent::tool_result_recording::ToolResultRecorder;
#[cfg(test)]
use crate::agent::turn_contracts::ApprovalDecision;
#[cfg(test)]
use crate::agent::turn_contracts::{ApprovalResolver, ScopeContext};
use crate::agent::turn_contracts::{
    ChatMode, ToolLoopCancelled, ToolLoopEvent, ToolLoopRuntimeAdapter, emit_tool_loop_event,
};
#[cfg(test)]
use crate::approval::ApprovalManager;
#[cfg(test)]
use crate::approval::{ApprovalRequest, ApprovalResponse};
use crate::observability::{Observer, ObserverEvent};
use crate::providers::ToolCall;
#[cfg(test)]
use crate::security::policy::{RUNTIME_APPROVAL_GRANT_ARG, RUNTIME_APPROVAL_GRANTED_ARG};
#[cfg(test)]
use crate::tools::Tool;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
#[cfg(test)]
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

static TOOL_BARRIERS: LazyLock<parking_lot::Mutex<HashMap<&'static str, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

fn tool_barrier_key(name: &str) -> Option<&'static str> {
    match name {
        // Workspace mutations / command execution.
        "file_write" | "shell" | "git_operations" => Some("workspace_write"),
        // Shared runtime configuration / scheduler state.
        "config_reload" | "cron" | "proxy_config" => Some("runtime_config"),
        // Shared long-term memory writes.
        "memory_store" | "memory_forget" | "memory_reindex" => Some("memory_write"),
        // Background session lifecycle operations.
        "sessions_spawn" | "sessions_send" | "delegate" | "subagents" => Some("session_lifecycle"),
        _ => None,
    }
}

/// Timeout for acquiring a tool barrier lock. If the lock holder panics or
/// hangs, we proceed without serialisation rather than deadlocking forever.
const TOOL_BARRIER_TIMEOUT_SECS: u64 = 60;

async fn acquire_tool_barrier(key: &'static str, tool_name: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
    let barrier = {
        let mut barriers = TOOL_BARRIERS.lock();
        barriers
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };

    (tokio::time::timeout(
        std::time::Duration::from_secs(TOOL_BARRIER_TIMEOUT_SECS),
        barrier.lock_owned(),
    )
    .await)
        .map_or_else(
            |_| {
                tracing::warn!(
                    tool = %tool_name,
                    barrier_key = key,
                    "Tool barrier lock timed out after {TOOL_BARRIER_TIMEOUT_SECS}s, proceeding without barrier",
                );
                None
            },
            |guard| Some(guard),
        )
}

#[cfg(test)]
async fn execute_one_tool(
    call_name: &str,
    mut call_arguments: serde_json::Value,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    scope_ctx: Option<&ScopeContext<'_>>,
    runtime_approval_granted: bool,
    chat_mode: ChatMode,
) -> Result<String> {
    // The non-service tool path; the service path beats in
    // `ToolExecutionService::execute` / `finish`.
    crate::agent::idle::beat(crate::agent::idle::ProgressKind::ToolStart);
    if chat_mode.intercepts_writes() && is_write_tool(call_name) {
        let preview = preview_tool_arguments(&call_arguments);
        observer.record_event(&ObserverEvent::ToolCall {
            tool: call_name.to_string(),
            duration: std::time::Duration::ZERO,
            success: true,
        });
        return Ok(format!("[plan mode] would call {call_name} with {preview}"));
    }
    let Some(tool) = find_tool(tools_registry, call_name) else {
        let available_names: Vec<&str> = tools_registry.iter().map(|t| t.name()).collect();
        let suggestion = crate::tools::error_hints::suggest_tool_name(call_name, &available_names);
        let hint = suggestion.map(|s| format!(" Did you mean '{s}'?")).unwrap_or_default();
        return Ok(format!(
            "Error: unknown tool '{call_name}'.{hint}\nAvailable tools: {}",
            available_names.join(", ")
        ));
    };

    let root = call_arguments
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("tool arguments must be a JSON object"))?;
    // Never trust model/user supplied runtime markers — approval grant, scope
    // or principal. The runtime is the only thing allowed to write them, and it
    // re-injects the authoritative values below. `strip_runtime_only_args` is
    // the single source of truth for which keys those are.
    crate::tools::execution::strip_runtime_only_args(root);

    // Validate only model-supplied arguments. Trusted runtime-only scope and
    // approval fields are injected afterward and intentionally do not belong in
    // provider-facing schemas.
    let schema = tool.parameters_schema();
    let validation_issues = crate::tools::schema::validate_tool_arguments(&schema, &call_arguments);
    if !validation_issues.is_empty() {
        return Ok(crate::tools::schema::format_argument_validation_error(
            call_name,
            &validation_issues,
        ));
    }

    let runtime_approval_grant = runtime_approval_granted
        .then(|| runtime_approval_grant_for_call(call_name, &call_arguments, scope_ctx))
        .flatten();
    let root = call_arguments
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("validated tool arguments must remain a JSON object"))?;
    root.insert(
        RUNTIME_APPROVAL_GRANTED_ARG.to_string(),
        serde_json::Value::Bool(runtime_approval_granted),
    );
    if let Some(grant) = runtime_approval_grant {
        root.insert(RUNTIME_APPROVAL_GRANT_ARG.to_string(), serde_json::to_value(grant)?);
    }
    if let Some(ctx) = scope_ctx {
        let mut trusted_scope = serde_json::json!({
            "sender": ctx.sender,
            "channel": ctx.channel,
            "chat_type": ctx.chat_type,
            "chat_id": ctx.chat_id,
        });
        if let Some(scope) = trusted_scope.as_object_mut() {
            if let Some(owner_id) = ctx.owner_id {
                scope.insert("owner_id".to_string(), serde_json::Value::String(owner_id.to_string()));
            }
            if let Some(topic_id) = ctx.topic_id {
                scope.insert("topic_id".to_string(), serde_json::Value::String(topic_id.to_string()));
            }
            if let Some(task_id) = ctx.task_id {
                scope.insert("task_id".to_string(), serde_json::Value::String(task_id.to_string()));
            }
            if let Some(source_message_event_id) = ctx.source_message_event_id {
                scope.insert(
                    "source_message_event_id".to_string(),
                    serde_json::Value::String(source_message_event_id.to_string()),
                );
            }
            if let Some(config_generation_id) = ctx.config_generation_id {
                scope.insert(
                    "config_generation_id".to_string(),
                    serde_json::Value::Number(config_generation_id.into()),
                );
            }
            if let Some(config_source_revision) = ctx.config_source_revision {
                scope.insert(
                    "config_source_revision".to_string(),
                    serde_json::Value::String(config_source_revision.to_string()),
                );
            }
        }
        if let Ok(spawn_ctx) = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.try_with(Clone::clone) {
            if let Some(scope) = trusted_scope.as_object_mut() {
                scope.insert("task_id".to_string(), serde_json::Value::String(spawn_ctx.run_id));
                scope.insert(
                    "session_scope_key".to_string(),
                    serde_json::Value::String(spawn_ctx.session_scope_key),
                );
                if let Some(owner_id) = spawn_ctx.owner_id {
                    scope.insert("owner_id".to_string(), serde_json::Value::String(owner_id));
                }
                if let Some(topic_id) = spawn_ctx.topic_id {
                    scope.insert("topic_id".to_string(), serde_json::Value::String(topic_id));
                }
                if let Some(source_message_event_id) = spawn_ctx.source_message_event_id {
                    scope.insert(
                        "source_message_event_id".to_string(),
                        serde_json::Value::String(source_message_event_id),
                    );
                }
            }
        }
        root.insert("_zc_scope".to_string(), trusted_scope);
        root.insert("_zc_scope_trusted".to_string(), serde_json::Value::Bool(true));
    } else {
        // No trusted scope: the strip above already dropped any user-provided
        // scope payload, so only the explicit "untrusted" marker is needed.
        root.insert("_zc_scope_trusted".to_string(), serde_json::Value::Bool(false));
    }

    let _barrier_guard = if let Some(key) = tool_barrier_key(call_name) {
        acquire_tool_barrier(key, call_name).await
    } else {
        None
    };

    observer.record_event(&ObserverEvent::ToolCallStart {
        tool: call_name.to_string(),
    });
    let start = Instant::now();

    let tool_future = tool.execute_with_cancellation(call_arguments, cancellation_token.cloned());
    let tool_result = if let Some(token) = cancellation_token {
        tokio::select! {
            () = token.cancelled() => return Err(ToolLoopCancelled.into()),
            result = tool_future => result,
        }
    } else {
        tool_future.await
    };

    match tool_result {
        Ok(r) => {
            if crate::tools::traits::is_tool_cancelled_result(&r) {
                return Err(ToolLoopCancelled.into());
            }
            observer.record_event(&ObserverEvent::ToolCall {
                tool: call_name.to_string(),
                duration: start.elapsed(),
                success: r.success,
            });
            if r.success {
                Ok(scrub_credentials(&r.output))
            } else {
                let error_text = r.error.unwrap_or_else(|| r.output);
                let hint = crate::tools::error_hints::recovery_hint(call_name, &error_text);
                if hint.is_empty() {
                    Ok(format!("Error: {error_text}"))
                } else {
                    Ok(format!("Error: {error_text}\n{hint}"))
                }
            }
        }
        Err(e) => {
            observer.record_event(&ObserverEvent::ToolCall {
                tool: call_name.to_string(),
                duration: start.elapsed(),
                success: false,
            });
            let error_str = e.to_string();
            let hint = crate::tools::error_hints::recovery_hint(call_name, &error_str);
            if hint.is_empty() {
                Ok(format!("Error executing {call_name}: {error_str}"))
            } else {
                Ok(format!("Error executing {call_name}: {error_str}\n{hint}"))
            }
        }
    }
}

/// Conservative scheduler policy:
/// - only known read-only tools can run concurrently;
/// - all stateful tools execute strictly serially;
/// - read-only batches use a bounded concurrency window.
///
/// Tool execution is never time-bounded. A long-running tool is normal agent
/// business, so both the parallel read-only lane and the serial lane run tools
/// without any deadline. Runaway work is surfaced and stopped through the
/// runtime registry (`prx tasks list|kill`) and the cancellation token, never
/// by silently killing a call that is still making progress.
#[derive(Debug, Clone)]
pub(super) struct ReadOnlyToolScheduleConfig {
    pub(super) concurrency_window: usize,
    pub(super) priority_enabled: bool,
    pub(super) low_priority_tool_names: std::collections::HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSchedulingClass {
    ReadOnly,
    Stateful,
}

fn classify_tool_call(name: &str) -> ToolSchedulingClass {
    if is_read_only_tool_name(name) {
        ToolSchedulingClass::ReadOnly
    } else {
        ToolSchedulingClass::Stateful
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolPriority {
    High,
    Low,
}

fn classify_tool_priority(name: &str, schedule: &ReadOnlyToolScheduleConfig) -> ToolPriority {
    if schedule.priority_enabled && schedule.low_priority_tool_names.contains(name) {
        ToolPriority::Low
    } else {
        ToolPriority::High
    }
}

/// Read-only classification for the parallel scheduler.
///
/// Permission-model Phase 1: this delegates to the single source of truth
/// [`crate::security::policy::is_read_only_tool`] so the scheduler's read-only
/// fast path and [`SecurityPolicy::decide`] can never disagree about which
/// tools have no side effects.
fn is_read_only_tool_name(name: &str) -> bool {
    crate::security::policy::is_read_only_tool(name)
}

/// Permission-model Phase 1: resolve the unified [`ToolDecision`] for a call.
///
/// When no scope context is present (callers that do not gate, e.g. internal
/// sub-agent paths) the call is allowed. Otherwise the single
/// [`SecurityPolicy::decide`] entry point governs the outcome.
#[cfg(test)]
fn decide_tool_call(
    call: &ParsedToolCall,
    scope_ctx: Option<&ScopeContext<'_>>,
) -> crate::tools::ToolExecutionDecision {
    scope_ctx.map_or(crate::tools::ToolExecutionDecision::Allow, |ctx| {
        crate::tools::decide_tool_execution(ctx.policy, &call.name, ctx.sender, ctx.channel, ctx.chat_type)
    })
}

/// Whether a tool call would require interactive approval (`decide` → `Ask`).
///
/// Used by the parallel scheduler to keep approval-gated calls off the
/// unattended read-only fast path. When no scope context is present the call is
/// not gated (matches [`decide_tool_call`] returning `Allow`).
#[cfg(test)]
fn requires_approval(call: &ParsedToolCall, scope_ctx: Option<&ScopeContext<'_>>) -> bool {
    decide_tool_call(call, scope_ctx) == crate::tools::ToolExecutionDecision::Ask
}

/// Denial message returned to the model when [`decide_tool_call`] yields `Deny`.
#[cfg(test)]
fn scope_denial_message(call: &ParsedToolCall) -> String {
    format!(
        "Error: Tool '{}' is not permitted under the current autonomy policy / user context.",
        call.name
    )
}

#[cfg(test)]
async fn execute_tool_call_serial(
    call: &ParsedToolCall,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    approval: Option<&ApprovalManager>,
    channel_name: &str,
    cancellation_token: Option<&CancellationToken>,
    scope_ctx: Option<&ScopeContext<'_>>,
    chat_mode: ChatMode,
    approval_resolver: Option<&Arc<dyn ApprovalResolver>>,
) -> Result<String> {
    // Permission-model Phase 1: single unified decision point.
    use crate::tools::ToolExecutionDecision;
    let decision = decide_tool_call(call, scope_ctx);
    if decision == ToolExecutionDecision::Deny {
        return Ok(scope_denial_message(call));
    }

    // `Ask` routes through the approval flow (manager + grant). `Allow` falls
    // through to direct execution below.
    if decision == ToolExecutionDecision::Ask {
        // P0 fail-closed: `decide` demands approval but no `ApprovalManager` is
        // wired to satisfy it (non-CLI channel / session-worker / gateway paths
        // that pass `approval = None` while carrying a scope under supervised
        // autonomy). Executing here would silently bypass the approval gate, so
        // deny instead of falling through to direct execution.
        let Some(mgr) = approval else {
            return Ok(format!(
                "Denied: tool '{}' requires approval but no approval resolver is available in this context.",
                call.name
            ));
        };

        // P2-a: preserve the session "Always" allowlist. A tool previously
        // approved with "Always" within this session executes immediately with a
        // runtime grant, without re-prompting.
        if mgr.is_session_allowlisted(&call.name) {
            return execute_one_tool(
                &call.name,
                call.arguments.clone(),
                tools_registry,
                observer,
                cancellation_token,
                scope_ctx,
                true,
                chat_mode,
            )
            .await;
        }

        {
            let request = ApprovalRequest {
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
            };

            // Phase-0 approval decision point. When an `ApprovalResolver` is
            // supplied (phase 1+), await it for the allow/deny/grant decision.
            // When `None` (every current caller) the historical synchronous
            // path runs unchanged: interactive CLI prompt on the `cli` channel,
            // implicit deny everywhere else.
            let (approval_decision, runtime_grant) = if let Some(resolver) = approval_resolver {
                // An `ApprovalResolver` may *suspend* (the chat `/bg` NeedsInput
                // path awaits an operator decision, up to a 300s timeout). Race
                // it against the cancellation token so a steer / send / kill that
                // cancels the run does not stay pinned on a suspended approval:
                // on cancel we drop the resolver future (its RAII guard clears
                // the pending waiter + restores run status) and treat the call as
                // denied (No, no grant) so the loop unwinds cleanly without a
                // zombie `AwaitingInput` run.
                let resolved = match cancellation_token {
                    Some(token) => {
                        tokio::select! {
                            biased;
                            () = token.cancelled() => None,
                            decision = resolver.resolve(&request, channel_name) => Some(decision),
                        }
                    }
                    None => Some(resolver.resolve(&request, channel_name).await),
                };
                match resolved {
                    Some(ApprovalDecision::Grant) => (ApprovalResponse::Yes, true),
                    Some(ApprovalDecision::Deny) | None => (ApprovalResponse::No, false),
                }
            } else if channel_name == "cli" {
                (mgr.prompt_cli(&request), true)
            } else {
                (ApprovalResponse::No, true)
            };

            mgr.record_decision(&call.name, &call.arguments, approval_decision, channel_name);

            if approval_decision == ApprovalResponse::No {
                return Ok("Denied by user.".to_string());
            }

            return execute_one_tool(
                &call.name,
                call.arguments.clone(),
                tools_registry,
                observer,
                cancellation_token,
                scope_ctx,
                runtime_grant,
                chat_mode,
            )
            .await;
        }
    }

    execute_one_tool(
        &call.name,
        call.arguments.clone(),
        tools_registry,
        observer,
        cancellation_token,
        scope_ctx,
        false,
        chat_mode,
    )
    .await
}

#[cfg(test)]
async fn execute_read_only_batch(
    calls: &[ParsedToolCall],
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    schedule: ReadOnlyToolScheduleConfig,
    cancellation_token: Option<&CancellationToken>,
    scope_ctx: Option<&ScopeContext<'_>>,
    chat_mode: ChatMode,
) -> Result<Vec<String>> {
    use futures::stream::{self, StreamExt};

    let batch_calls: Vec<ParsedToolCall> = calls.to_vec();

    let mut indexed_results: Vec<(usize, String)> = stream::iter(batch_calls.into_iter().enumerate())
        .map(|(idx, call)| async move {
            // Read-only batch: only a scope/autonomy `Deny` blocks. Read-only
            // tools never yield `Ask`, so `Allow`/`Ask` both proceed here.
            if decide_tool_call(&call, scope_ctx) == crate::tools::ToolExecutionDecision::Deny {
                return Ok((idx, scope_denial_message(&call)));
            }

            // No deadline: the batch lane runs tools exactly as long as the
            // serial lane does. Cancellation is the only way out.
            let execute_future = execute_one_tool(
                &call.name,
                call.arguments.clone(),
                tools_registry,
                observer,
                cancellation_token,
                scope_ctx,
                false,
                chat_mode,
            );

            let result = if let Some(token) = cancellation_token {
                tokio::select! {
                    () = token.cancelled() => return Err(ToolLoopCancelled.into()),
                    executed = execute_future => executed?,
                }
            } else {
                execute_future.await?
            };

            Ok((idx, result))
        })
        .buffer_unordered(schedule.concurrency_window.max(1))
        .collect::<Vec<Result<(usize, String)>>>()
        .await
        .into_iter()
        .collect::<Result<Vec<(usize, String)>>>()?;

    indexed_results.sort_by_key(|(idx, _)| *idx);
    Ok(indexed_results
        .into_iter()
        .map(|(_, result)| result)
        .collect::<Vec<String>>())
}

// SAFETY: All indexing in this function is guarded:
// - ordered_indices[cursor] is under `while cursor < ordered_indices.len()`
// - tool_calls[index] where index comes from 0..tool_calls.len()
// - results_by_original[index] where index is a valid tool_calls index
#[allow(clippy::indexing_slicing)]
#[cfg(test)]
async fn execute_tools_with_policy(
    tool_calls: &[ParsedToolCall],
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    approval: Option<&ApprovalManager>,
    channel_name: &str,
    schedule: ReadOnlyToolScheduleConfig,
    cancellation_token: Option<&CancellationToken>,
    scope_ctx: Option<&ScopeContext<'_>>,
    chat_mode: ChatMode,
    approval_resolver: Option<&Arc<dyn ApprovalResolver>>,
) -> Result<Vec<String>> {
    let mut ordered_indices = Vec::with_capacity(tool_calls.len());
    if schedule.priority_enabled {
        ordered_indices.extend(tool_calls.iter().enumerate().filter_map(|(idx, call)| {
            (classify_tool_priority(&call.name, &schedule) == ToolPriority::High).then_some(idx)
        }));
        ordered_indices.extend(tool_calls.iter().enumerate().filter_map(|(idx, call)| {
            (classify_tool_priority(&call.name, &schedule) == ToolPriority::Low).then_some(idx)
        }));
    } else {
        ordered_indices.extend(0..tool_calls.len());
    }

    let mut results_by_original = vec![String::new(); tool_calls.len()];
    let mut cursor = 0;

    while cursor < ordered_indices.len() {
        if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolLoopCancelled.into());
        }

        let index = ordered_indices[cursor];
        let call = &tool_calls[index];
        let approval_required = requires_approval(call, scope_ctx);

        if classify_tool_call(&call.name) == ToolSchedulingClass::ReadOnly && !approval_required {
            let batch_start = cursor;
            let mut batch_end = cursor + 1;
            while batch_end < ordered_indices.len() {
                let next_index = ordered_indices[batch_end];
                let next_call = &tool_calls[next_index];
                let next_approval = requires_approval(next_call, scope_ctx);
                if classify_tool_call(&next_call.name) != ToolSchedulingClass::ReadOnly || next_approval {
                    break;
                }
                batch_end += 1;
            }

            let batch_calls: Vec<ParsedToolCall> = ordered_indices[batch_start..batch_end]
                .iter()
                .map(|idx| tool_calls[*idx].clone())
                .collect();
            let batch_results = execute_read_only_batch(
                &batch_calls,
                tools_registry,
                observer,
                schedule.clone(),
                cancellation_token,
                scope_ctx,
                chat_mode,
            )
            .await?;

            observer.record_event(&ObserverEvent::ToolBatch {
                batch_size: batch_results.len(),
                concurrency_window: schedule.concurrency_window,
            });

            for (offset, result) in batch_results.into_iter().enumerate() {
                let original_index = ordered_indices[batch_start + offset];
                results_by_original[original_index] = result;
            }
            cursor = batch_end;
            continue;
        }

        let result = execute_tool_call_serial(
            call,
            tools_registry,
            observer,
            approval.clone(),
            channel_name,
            cancellation_token,
            scope_ctx,
            chat_mode,
            approval_resolver,
        )
        .await?;
        results_by_original[index] = result;
        cursor += 1;
    }

    Ok(results_by_original)
}

/// Execute one tool call.
///
/// There is deliberately no deadline parameter. A tool runs until it returns,
/// is cancelled, or the turn's stall detector (`crate::agent::idle`) ends the
/// whole turn; reintroducing a per-call clock here would put back exactly the
/// ceiling the unbounded-runtime work removed.
async fn execute_one_with_service(
    index: usize,
    call: &ParsedToolCall,
    native_tool_calls: &[ToolCall],
    adapter: &ToolLoopRuntimeAdapter,
    cancellation: Option<&CancellationToken>,
    chat_mode: ChatMode,
    recorder: &ToolResultRecorder,
) -> Result<String> {
    let result = match execute_one_unrecorded(index, call, native_tool_calls, adapter, cancellation, chat_mode).await {
        Ok(result) => result,
        Err(error) => {
            if let Some(pending) = error.downcast_ref::<super::turn_contracts::ToolLoopAwaitingApproval>() {
                recorder.record(index, &pending.reply);
            }
            return Err(error);
        }
    };
    // Recorded before returning, so a caller dropped between here and the end
    // of the batch still leaves evidence that this call ran.
    recorder.record(index, &result);
    Ok(result)
}

async fn execute_one_unrecorded(
    index: usize,
    call: &ParsedToolCall,
    native_tool_calls: &[ToolCall],
    adapter: &ToolLoopRuntimeAdapter,
    cancellation: Option<&CancellationToken>,
    chat_mode: ChatMode,
) -> Result<String> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(ToolLoopCancelled.into());
    }
    let tool_call_id = native_tool_calls
        .get(index)
        .map(|native| native.id.clone())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| Uuid::now_v7().to_string());

    if let Some(result) = native_tool_calls.get(index).and_then(unparseable_native_arguments) {
        tracing::warn!(tool = %call.name, "native tool call refused: arguments are not a complete JSON object");
        emit_tool_loop_event(
            adapter,
            ToolLoopEvent::ToolFinished {
                tool_call_id,
                name: call.name.clone(),
                success: false,
                duration_ms: 0,
                result: result.clone(),
            },
        )
        .await?;
        return Ok(result);
    }

    let command = crate::tools::ToolExecutionCommand::new(&call.name, call.arguments.clone())
        .with_operation_id(tool_call_id.clone())
        .with_idempotency_key(tool_call_id.clone());
    let Some(service) = adapter.tool_execution_service.as_ref() else {
        let result = "Error: ToolExecutionService is unavailable; tool rejected for safety".to_string();
        emit_tool_loop_event(
            adapter,
            ToolLoopEvent::ToolFinished {
                tool_call_id,
                name: call.name.clone(),
                success: false,
                duration_ms: 0,
                result: result.clone(),
            },
        )
        .await?;
        return Ok(result);
    };

    if adapter
        .allowed_tool_names
        .as_ref()
        .is_some_and(|allowed| !allowed.contains(&call.name))
        && service.supports_public_name(&call.name)
    {
        if let Some(trace) = adapter.tool_execution_context.execution_trace.as_ref() {
            trace
                .begin(&tool_call_id, None, false)
                .finish(crate::tools::ToolExecutionStatus::Denied);
        }
        let result = format!(
            "Error: tool '{}' was not exposed for this turn and was not executed",
            call.name
        );
        emit_tool_loop_event(
            adapter,
            ToolLoopEvent::ToolFinished {
                tool_call_id,
                name: call.name.clone(),
                success: false,
                duration_ms: 0,
                result: result.clone(),
            },
        )
        .await?;
        return Ok(result);
    }

    if chat_mode.intercepts_writes() && is_write_tool(&call.name) {
        let preview = preview_tool_arguments(&call.arguments);
        emit_tool_loop_event(
            adapter,
            ToolLoopEvent::ToolStarted {
                tool_call_id: tool_call_id.clone(),
                name: call.name.clone(),
                args: call.arguments.to_string(),
            },
        )
        .await?;
        let result = format!("[plan mode] would call {} with {preview}", call.name);
        emit_tool_loop_event(
            adapter,
            ToolLoopEvent::ToolFinished {
                tool_call_id,
                name: call.name.clone(),
                success: true,
                duration_ms: 0,
                result: result.clone(),
            },
        )
        .await?;
        return Ok(result);
    }

    let _barrier = if let Some(key) = tool_barrier_key(&call.name) {
        acquire_tool_barrier(key, &call.name).await
    } else {
        None
    };
    let execution_context = adapter.tool_execution_context.clone();
    let cancellation = cancellation.cloned();
    let outcome = if matches!(call.name.as_str(), "delegate" | "sessions_spawn") {
        spawn_delegation_tool(service.clone(), command, execution_context, cancellation)
            .join()
            .await?
    } else {
        service.execute(command, execution_context, cancellation).await
    };
    if outcome.status == crate::tools::ToolExecutionStatus::Cancelled {
        return Err(ToolLoopCancelled.into());
    }
    let awaiting_approval = outcome.status == crate::tools::ToolExecutionStatus::AwaitingApproval;
    let success = outcome.succeeded();
    let result = outcome.model_content;
    emit_tool_loop_event(
        adapter,
        ToolLoopEvent::ToolFinished {
            tool_call_id,
            name: call.name.clone(),
            success,
            duration_ms: outcome.duration_ms,
            result: result.clone(),
        },
    )
    .await?;
    if awaiting_approval {
        return Err(super::turn_contracts::ToolLoopAwaitingApproval { reply: result }.into());
    }
    Ok(result)
}

/// Delegation backends contain another complete agent loop. Start their fixed
/// execution pipeline in a separate task so the backend gets a fresh worker
/// stack instead of extending the caller's already-deep turn stack. The
/// service-owned pipeline still installs the child grant and call receipt; the
/// explicit scopes below carry the parent turn's cleanup, lineage and liveness.
fn spawn_delegation_tool(
    service: Arc<crate::tools::ToolExecutionService>,
    command: crate::tools::ToolExecutionCommand,
    context: crate::tools::ToolExecutionContext,
    cancellation: Option<CancellationToken>,
) -> DelegationExecutionTask {
    let cleanup = crate::runtime::turn_cleanup::current_tracker();
    let parent_work = crate::runtime::registry::current_work_id();
    let progress = crate::agent::idle::current_beat();
    let spawn_context = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
        .try_with(Clone::clone)
        .ok();
    let work: std::pin::Pin<Box<dyn std::future::Future<Output = crate::tools::ToolExecutionOutcome> + Send>> =
        Box::pin(async move {
            let execute = async move { service.execute(command, context, cancellation).await };
            let execute = async move {
                match spawn_context {
                    Some(spawn_context) => {
                        crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                            .scope(spawn_context, execute)
                            .await
                    }
                    None => execute.await,
                }
            };
            let execute = async move {
                match cleanup {
                    Some(cleanup) => cleanup.scope(execute).await,
                    None => execute.await,
                }
            };
            let execute = async move {
                match parent_work {
                    Some(parent_work) => crate::runtime::registry::scope_current(parent_work, execute).await,
                    None => execute.await,
                }
            };
            crate::agent::idle::scope_beat(progress, execute).await
        });
    DelegationExecutionTask::spawn(work)
}

/// Aborts delegated execution if its parent turn is dropped or cancelled.
struct DelegationExecutionTask(Option<tokio::task::JoinHandle<crate::tools::ToolExecutionOutcome>>);

impl DelegationExecutionTask {
    fn spawn(
        work: std::pin::Pin<Box<dyn std::future::Future<Output = crate::tools::ToolExecutionOutcome> + Send>>,
    ) -> Self {
        Self(Some(tokio::spawn(work)))
    }

    async fn join(mut self) -> Result<crate::tools::ToolExecutionOutcome> {
        let Some(task) = self.0.take() else {
            anyhow::bail!("delegation execution task is missing");
        };
        task.await.map_err(anyhow::Error::new)
    }
}

impl Drop for DelegationExecutionTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

#[allow(clippy::indexing_slicing)]
pub(super) async fn execute_tools_with_service(
    tool_calls: &[ParsedToolCall],
    native_tool_calls: &[ToolCall],
    adapter: &ToolLoopRuntimeAdapter,
    schedule: ReadOnlyToolScheduleConfig,
    cancellation: Option<&CancellationToken>,
    chat_mode: ChatMode,
    observer: &dyn Observer,
    recorder: &ToolResultRecorder,
) -> Result<Vec<String>> {
    use futures::stream::{self, StreamExt};

    let mut ordered_indices = (0..tool_calls.len()).collect::<Vec<_>>();
    if schedule.priority_enabled {
        ordered_indices.sort_by_key(
            |index| match classify_tool_priority(&tool_calls[*index].name, &schedule) {
                ToolPriority::High => 0_u8,
                ToolPriority::Low => 1_u8,
            },
        );
    }
    let mut results = vec![String::new(); tool_calls.len()];
    let mut cursor = 0;
    while cursor < ordered_indices.len() {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolLoopCancelled.into());
        }
        let index = ordered_indices[cursor];
        // Read-only calls always take the parallel lane; there is no switch that
        // can force them back to serial. Neither lane can carry a deadline —
        // `execute_one_with_service` takes none — so a tool behaves identically
        // whichever lane it runs in.
        if classify_tool_call(&tool_calls[index].name) == ToolSchedulingClass::ReadOnly {
            let start = cursor;
            let mut end = cursor + 1;
            while end < ordered_indices.len()
                && classify_tool_call(&tool_calls[ordered_indices[end]].name) == ToolSchedulingClass::ReadOnly
            {
                end += 1;
            }
            let mut pending = stream::iter(ordered_indices[start..end].iter().copied())
                .map(|batch_index| async move {
                    let result = execute_one_with_service(
                        batch_index,
                        &tool_calls[batch_index],
                        native_tool_calls,
                        adapter,
                        cancellation,
                        chat_mode,
                        recorder,
                    )
                    .await?;
                    Ok::<_, anyhow::Error>((batch_index, result))
                })
                .buffer_unordered(schedule.concurrency_window.max(1));
            let mut completed = Vec::with_capacity(end - start);
            while let Some(result) = pending.next().await {
                if result.as_ref().is_err_and(|error| {
                    error
                        .downcast_ref::<super::turn_contracts::ToolLoopAwaitingApproval>()
                        .is_some()
                }) {
                    // Do not poll the stream again: that would admit another
                    // queued call. Dropping in-flight futures preserves their
                    // cancellation observations; completed results were already
                    // recorded by execute_one_with_service.
                    drop(pending);
                    return result.map(|_| Vec::new());
                }
                // Other errors retain the existing drain-before-return
                // behavior, including records from sibling calls.
                completed.push(result);
            }
            let batch = completed.into_iter().collect::<Result<Vec<_>>>()?;
            observer.record_event(&ObserverEvent::ToolBatch {
                batch_size: batch.len(),
                concurrency_window: schedule.concurrency_window,
            });
            for (batch_index, result) in batch {
                results[batch_index] = result;
            }
            cursor = end;
            continue;
        }
        results[index] = execute_one_with_service(
            index,
            &tool_calls[index],
            native_tool_calls,
            adapter,
            cancellation,
            chat_mode,
            recorder,
        )
        .await?;
        cursor += 1;
    }
    Ok(results)
}

pub(super) fn shared_unrecoverable_tool_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "permission denied",
        "not permitted",
        "not allowed",
        "blocked by policy",
        "approval denied",
        "preparation denied",
        "unknown tool",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[path = "turn_tests/tool_scheduler.rs"]
mod tests;
