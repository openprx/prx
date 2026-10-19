use crate::agent::execution_policy::{is_write_tool, preview_tool_arguments};
use crate::agent::tool_call_parser::{ParsedToolCall, unparseable_native_arguments};
use crate::agent::tool_result_recording::ToolResultRecorder;
use crate::agent::turn_contracts::{
    ChatMode, ToolLoopCancelled, ToolLoopEvent, ToolLoopRuntimeAdapter, emit_tool_loop_event,
};
use crate::observability::{Observer, ObserverEvent};
use crate::providers::ToolCall;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber as _;
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

/// Timeout for acquiring a tool barrier lock. A timed-out mutation is refused:
/// running it without the shared-resource barrier would defeat the scheduler's
/// serialization guarantee.
const TOOL_BARRIER_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "tool '{tool_name}' could not acquire shared-resource barrier '{barrier_key}' within {timeout_secs}s; tool was not executed"
)]
struct ToolBarrierTimeout {
    tool_name: String,
    barrier_key: &'static str,
    timeout_secs: u64,
}

async fn acquire_tool_barrier(
    key: &'static str,
    tool_name: &str,
) -> Result<tokio::sync::OwnedMutexGuard<()>, ToolBarrierTimeout> {
    let barrier = {
        let mut barriers = TOOL_BARRIERS.lock();
        barriers
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };

    tokio::time::timeout(
        std::time::Duration::from_secs(TOOL_BARRIER_TIMEOUT_SECS),
        barrier.lock_owned(),
    )
    .await
    .map_err(|_| {
        tracing::warn!(
            tool = %tool_name,
            barrier_key = key,
            "Tool barrier lock timed out after {TOOL_BARRIER_TIMEOUT_SECS}s; refusing tool execution",
        );
        ToolBarrierTimeout {
            tool_name: tool_name.to_string(),
            barrier_key: key,
            timeout_secs: TOOL_BARRIER_TIMEOUT_SECS,
        }
    })
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
        let result = service
            .unexposed_policy_denial(&call.name, &adapter.tool_execution_context)
            .map_or_else(
                || {
                    format!(
                        "Error: tool '{}' was not exposed for this turn and was not executed",
                        call.name
                    )
                },
                |denial| format!("Error: {denial}"),
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

    let _barrier = match tool_barrier_key(&call.name) {
        Some(key) => match acquire_tool_barrier(key, &call.name).await {
            Ok(guard) => Some(guard),
            Err(error) => {
                let result = format!("Error: {error}");
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
        },
        None => None,
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
        Self(Some(tokio::spawn(work.with_current_subscriber())))
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
