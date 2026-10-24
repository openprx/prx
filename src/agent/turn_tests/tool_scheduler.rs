#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;
use crate::agent::execution_policy::{
    AgentToolApprovalStrategy, AgentToolRuntimeInputs, agent_tool_execution_context, build_agent_tool_runtime_adapter,
};
use crate::agent::tool_call_parser::ParsedToolCall;
use crate::agent::tool_result_recording::ToolResultRecorder;
use crate::agent::turn_contracts::{ApprovalDecision, ApprovalResolver, ChatMode, ScopeContext};
use crate::agent::turn_tests::{DelayTool, RecordingTool, scheduler_test_adapter};
use crate::approval::ApprovalManager;
#[cfg(test)]
use crate::approval::ApprovalResponse;
use crate::observability::{NoopObserver, Observer, ObserverEvent};
use crate::tools::{Tool, ToolResult};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tempfile::TempDir;

static TOOL_SCHEDULER_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

struct ConditionalSchemaTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ConditionalSchemaTool {
    fn name(&self) -> &str {
        "conditional_schema"
    }

    fn description(&self) -> &str {
        "Tool used to verify action-specific argument preflight"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::schema::with_action_requirements(
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "write"]},
                    "content": {"type": "string"}
                },
                "required": ["action"]
            }),
            "action",
            &[crate::tools::schema::ActionRequirement {
                action: "write",
                required: &["content"],
            }],
        )
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(crate::tools::ToolResult {
            success: true,
            output: "executed".to_string(),
            error: None,
        })
    }
}

#[derive(Default)]
struct SchedulerEventObserver {
    batch_events: std::sync::Mutex<Vec<ObserverEvent>>,
}

impl Observer for SchedulerEventObserver {
    fn record_event(&self, event: &ObserverEvent) {
        if matches!(event, ObserverEvent::ToolBatch { .. }) {
            self.batch_events
                .lock()
                .expect("batch events lock should be valid")
                .push(event.clone());
        }
    }

    fn record_metric(&self, _metric: &crate::observability::traits::ObserverMetric) {}

    fn name(&self) -> &str {
        "scheduler-events"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[test]
fn classify_tool_call_marks_known_read_only_tools() {
    assert_eq!(classify_tool_call("file_read"), ToolSchedulingClass::ReadOnly);
    assert_eq!(classify_tool_call("memory_search"), ToolSchedulingClass::ReadOnly);
    assert_eq!(classify_tool_call("document_search"), ToolSchedulingClass::ReadOnly);
    assert_eq!(classify_tool_call("document_get_chunk"), ToolSchedulingClass::ReadOnly);
}

#[test]
fn classify_tool_call_marks_mutating_or_unknown_tools_as_stateful() {
    assert_eq!(classify_tool_call("shell"), ToolSchedulingClass::Stateful);
    assert_eq!(classify_tool_call("memory_reindex"), ToolSchedulingClass::Stateful);
    assert_eq!(
        classify_tool_call("totally_unknown_tool"),
        ToolSchedulingClass::Stateful
    );
}

#[test]
fn tool_barrier_groups_memory_reindex_with_memory_writes() {
    assert_eq!(tool_barrier_key("memory_store"), Some("memory_write"));
    assert_eq!(tool_barrier_key("memory_forget"), Some("memory_write"));
    assert_eq!(tool_barrier_key("memory_reindex"), Some("memory_write"));
}

#[tokio::test]
async fn production_scheduler_prioritizes_foreground_tools() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(RecordingTool {
            name: "sessions_spawn".to_string(),
            execution_order: Arc::clone(&execution_order),
        }),
        Box::new(RecordingTool {
            name: "file_read".to_string(),
            execution_order: Arc::clone(&execution_order),
        }),
    ];
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(tools_registry, &ledger_dir);

    let calls = vec![
        ParsedToolCall {
            name: "sessions_spawn".to_string(),
            arguments: serde_json::json!({}),
        },
        ParsedToolCall {
            name: "file_read".to_string(),
            arguments: serde_json::json!({}),
        },
    ];

    let results = run_through_production_scheduler(
        &calls,
        &adapter,
        ReadOnlyToolScheduleConfig {
            concurrency_window: 2,
            priority_enabled: true,
            low_priority_tool_names: std::iter::once("sessions_spawn".to_string()).collect(),
        },
        ChatMode::default(),
    )
    .await;

    assert_eq!(
        results.iter().map(|result| result.content.as_str()).collect::<Vec<_>>(),
        vec!["sessions_spawn", "file_read"]
    );
    let observed_order = execution_order
        .lock()
        .expect("execution order lock should be valid")
        .clone();
    assert_eq!(observed_order, vec!["file_read", "sessions_spawn"]);
}

/// P0 fail-closed regression: the non-CLI-channel / session-worker shape —
/// a scope context is present (so `decide` runs) under `supervised`, but no
/// `ApprovalManager` is wired (`approval = None`). A low-risk side-effecting
/// (`Act`) tool such as `delegate` resolves to `Ask`; with no approver to
/// satisfy it, the tool loop MUST deny rather than silently execute. Prior to
/// the fix it fell through to direct execution, bypassing approval entirely.
#[tokio::test]
async fn supervised_act_tool_without_approval_manager_is_denied_not_executed() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tools_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(RecordingTool {
        name: "delegate".to_string(),
        execution_order: Arc::clone(&execution_order),
    })]);

    let policy = crate::security::SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Supervised,
        ..crate::security::SecurityPolicy::default()
    };
    let scope_ctx = ScopeContext {
        policy: &policy,
        sender: "prx_user",
        channel: "telegram",
        chat_type: "private",
        chat_id: "prx_channel",
        session_key: None,
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        config_generation_id: None,
        config_source_revision: None,
    };
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let ledger: Arc<dyn crate::memory::Memory> = Arc::new(
        crate::memory::SqliteMemory::new(ledger_dir.path()).expect("test: sqlite ledger should open in a temp dir"),
    );
    let adapter = build_agent_tool_runtime_adapter(AgentToolRuntimeInputs {
        scope: Some(&scope_ctx),
        channel_name: "telegram",
        tools_registry,
        approval: None,
        approval_resolver: None,
        cancellation: None,
        ledger: Some(ledger),
        unscoped_policy: None,
    });
    let calls = vec![ParsedToolCall {
        name: "delegate".to_string(),
        arguments: serde_json::json!({}),
    }];
    let recorder = ToolResultRecorder::new(calls.len());
    let results = execute_tools_with_service(
        &calls,
        &[],
        &adapter,
        unlimited_schedule(1),
        None,
        ChatMode::default(),
        &NoopObserver,
        &recorder,
    )
    .await
    .expect("production scheduler should return a failed tool result, not an execution error");

    assert!(
        results[0]
            .content
            .contains("approval required but no approval resolver is available"),
        "expected fail-closed approval denial, got: {}",
        results[0].content
    );
    assert!(
        execution_order
            .lock()
            .expect("execution order lock should be valid")
            .is_empty(),
        "the Act tool must NOT have executed under fail-closed denial"
    );
}

#[tokio::test]
async fn default_full_policy_executes_installed_act_tool_without_approval_manager() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "delegate".to_string(),
        execution_order: Arc::clone(&execution_order),
    })];
    let policy = crate::security::SecurityPolicy::default();
    let scope_ctx = scheduler_scope(&policy, "user@example.com");

    assert_eq!(
        crate::tools::decide_tool_execution(
            scope_ctx.policy,
            "delegate",
            scope_ctx.sender,
            scope_ctx.channel,
            scope_ctx.chat_type
        ),
        crate::tools::ToolExecutionDecision::Allow
    );
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scoped_scheduler_adapter(tools_registry, &scope_ctx, None, None, &ledger_dir);
    let calls = single_call("delegate", serde_json::json!({}));
    let results = run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::default()).await;

    assert_eq!(
        results.iter().map(|result| result.content.as_str()).collect::<Vec<_>>(),
        vec!["delegate"]
    );
    assert_eq!(
        execution_order
            .lock()
            .expect("execution order lock should be valid")
            .as_slice(),
        ["delegate"]
    );
}

/// Companion to the fail-closed test: under the SAME conditions (supervised,
/// scope present, no `ApprovalManager`) a genuinely read-only tool resolves
/// to `Allow` and still executes through the production executor, so the
/// fail-closed denial does not over-block observational tools.
#[tokio::test]
async fn supervised_read_only_tool_without_approval_manager_still_executes() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "file_read".to_string(),
        execution_order: Arc::clone(&execution_order),
    })];

    let policy = crate::security::SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Supervised,
        ..crate::security::SecurityPolicy::default()
    };
    let scope_ctx = scheduler_scope(&policy, "user@example.com");
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scoped_scheduler_adapter(tools_registry, &scope_ctx, None, None, &ledger_dir);
    let calls = single_call("file_read", serde_json::json!({}));
    let results = run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::default()).await;

    assert_eq!(
        results.iter().map(|result| result.content.as_str()).collect::<Vec<_>>(),
        vec!["file_read"]
    );
    assert_eq!(
        execution_order
            .lock()
            .expect("execution order lock should be valid")
            .as_slice(),
        ["file_read"],
        "read-only tool must still execute under supervised + no approver"
    );
}

/// P2-a: a tool on the session "Always" allowlist executes immediately under
/// `supervised` without re-prompting, even when an `ApprovalManager` is wired
/// (production: `AgentToolApprovalStrategy` in `execution_policy.rs`) -- the
/// "Always" shortcut survived the move to the unified `decide` gate.
#[tokio::test]
async fn supervised_session_allowlisted_act_tool_executes_without_reprompt() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "delegate".to_string(),
        execution_order: Arc::clone(&execution_order),
    })];

    let policy = crate::security::SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Supervised,
        ..crate::security::SecurityPolicy::default()
    };
    let scope_ctx = scheduler_scope(&policy, "user@example.com");

    // Seed the session "Always" allowlist for `delegate`.
    let mgr = Arc::new(ApprovalManager::new());
    mgr.record_decision("delegate", &serde_json::json!({}), ApprovalResponse::Always, "telegram");
    assert!(mgr.is_session_allowlisted("delegate"));

    // No resolver: a non-allowlisted Act tool would deny on this channel.
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scoped_scheduler_adapter(tools_registry, &scope_ctx, Some(Arc::clone(&mgr)), None, &ledger_dir);
    let calls = single_call("delegate", serde_json::json!({}));
    let results = run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::default()).await;

    assert_eq!(
        results.iter().map(|result| result.content.as_str()).collect::<Vec<_>>(),
        vec!["delegate"]
    );
    assert_eq!(
        execution_order
            .lock()
            .expect("execution order lock should be valid")
            .as_slice(),
        ["delegate"],
        "session-allowlisted Act tool must execute without a prompt"
    );
}

fn scheduler_scope<'a>(policy: &'a crate::security::SecurityPolicy, sender: &'a str) -> ScopeContext<'a> {
    ScopeContext {
        policy,
        sender,
        channel: "telegram",
        chat_type: "private",
        chat_id: "chat-1",
        session_key: None,
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        config_generation_id: None,
        config_source_revision: None,
    }
}

/// Production adapter (real `ToolExecutionService` + the agent approval
/// strategy) for a scoped, non-CLI channel, with an optional approval manager.
fn scoped_scheduler_adapter(
    tools: Vec<Box<dyn Tool>>,
    scope: &ScopeContext<'_>,
    approval: Option<Arc<ApprovalManager>>,
    approval_resolver: Option<Arc<dyn ApprovalResolver>>,
    ledger_dir: &TempDir,
) -> ToolLoopRuntimeAdapter {
    let ledger: Arc<dyn crate::memory::Memory> = Arc::new(
        crate::memory::SqliteMemory::new(ledger_dir.path()).expect("test: sqlite ledger should open in a temp dir"),
    );
    build_agent_tool_runtime_adapter(AgentToolRuntimeInputs {
        scope: Some(scope),
        channel_name: scope.channel,
        tools_registry: Arc::new(tools),
        approval,
        approval_resolver,
        cancellation: None,
        ledger: Some(ledger),
        unscoped_policy: None,
    })
}

/// Drives `calls` through the production executor `execute_tools_with_service`.
async fn run_through_production_scheduler(
    calls: &[ParsedToolCall],
    adapter: &ToolLoopRuntimeAdapter,
    schedule: ReadOnlyToolScheduleConfig,
    chat_mode: ChatMode,
) -> Vec<crate::agent::tool_result_recording::ObservedToolResult> {
    let recorder = ToolResultRecorder::new(calls.len());
    execute_tools_with_service(calls, &[], adapter, schedule, None, chat_mode, &NoopObserver, &recorder)
        .await
        .expect("production scheduler should return tool results, not an execution error")
}

fn single_call(name: &str, arguments: serde_json::Value) -> Vec<ParsedToolCall> {
    vec![ParsedToolCall {
        name: name.to_string(),
        arguments,
    }]
}

/// Resolver double that returns a fixed decision and counts its invocations.
struct FixedApprovalResolver {
    decision: ApprovalDecision,
    calls: AtomicUsize,
}

impl FixedApprovalResolver {
    fn new(decision: ApprovalDecision) -> Arc<Self> {
        Arc::new(Self {
            decision,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ApprovalResolver for FixedApprovalResolver {
    async fn resolve(&self, _request: &crate::approval::ApprovalRequest, _channel: &str) -> ApprovalDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.decision.clone()
    }
}

/// Runs a supervised, non-allowlisted `delegate` call on the non-CLI `telegram`
/// channel through the production executor and returns (result, executions).
async fn run_supervised_act_call_with_approval(resolver: Option<Arc<dyn ApprovalResolver>>) -> (String, Vec<String>) {
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "delegate".to_string(),
        execution_order: Arc::clone(&execution_order),
    })];
    let policy = crate::security::SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Supervised,
        ..crate::security::SecurityPolicy::default()
    };
    let scope_ctx = scheduler_scope(&policy, "user@example.com");
    let mgr = Arc::new(ApprovalManager::new());
    assert!(!mgr.is_session_allowlisted("delegate"));
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scoped_scheduler_adapter(tools_registry, &scope_ctx, Some(mgr), resolver, &ledger_dir);
    let calls = single_call("delegate", serde_json::json!({}));
    let mut results =
        run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::default()).await;
    let executed = execution_order
        .lock()
        .expect("execution order lock should be valid")
        .clone();
    (results.remove(0).content, executed)
}

#[tokio::test]
async fn supervised_act_tool_with_granting_resolver_executes_once() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let resolver = FixedApprovalResolver::new(ApprovalDecision::Grant);
    let (result, executed) = run_supervised_act_call_with_approval(Some(resolver.clone())).await;

    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "the resolver must be asked once"
    );
    assert_eq!(result, "delegate", "a granted call returns the tool output: {result}");
    assert_eq!(executed, ["delegate"], "a granted Act tool must execute exactly once");
}

#[tokio::test]
async fn supervised_act_tool_with_denying_resolver_is_not_executed() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let resolver = FixedApprovalResolver::new(ApprovalDecision::Deny);
    let (result, executed) = run_supervised_act_call_with_approval(Some(resolver.clone())).await;

    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "the resolver must be asked once"
    );
    assert!(result.contains("Denied by user"), "denial text expected, got: {result}");
    assert!(executed.is_empty(), "a denied Act tool must NOT execute");
}

#[tokio::test]
async fn supervised_act_tool_with_manager_but_no_resolver_on_non_cli_channel_is_denied() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let (result, executed) = run_supervised_act_call_with_approval(None).await;

    assert!(result.contains("Denied by user"), "denial text expected, got: {result}");
    assert!(
        executed.is_empty(),
        "without a resolver a non-CLI channel must deny, not approve"
    );
}

fn slow_tool_registry(delay_ms: u64) -> Vec<Box<dyn Tool>> {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    vec![
        // `file_read` is read-only -> parallel batch lane.
        Box::new(DelayTool::new(
            "file_read",
            delay_ms,
            Arc::clone(&active),
            Arc::clone(&max_active),
        )),
        // `shell` is stateful -> serial lane.
        Box::new(DelayTool::new("shell", delay_ms, active, max_active)),
    ]
}

fn unlimited_schedule(concurrency_window: usize) -> ReadOnlyToolScheduleConfig {
    ReadOnlyToolScheduleConfig {
        concurrency_window,
        priority_enabled: false,
        low_priority_tool_names: std::collections::HashSet::new(),
    }
}

async fn run_single_tool_through_scheduler(
    tool_name: &str,
    schedule: ReadOnlyToolScheduleConfig,
    observer: &dyn Observer,
) -> String {
    // 10 minutes of simulated tool work: far past every deadline the
    // scheduler used to impose (the deleted default was 30s).
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(slow_tool_registry(600_000), &ledger_dir);
    let calls = vec![ParsedToolCall {
        name: tool_name.to_string(),
        arguments: serde_json::json!({"value": "x"}),
    }];
    let recorder = ToolResultRecorder::new(calls.len());
    let mut results = execute_tools_with_service(
        &calls,
        &[],
        &adapter,
        schedule,
        None,
        ChatMode::default(),
        observer,
        &recorder,
    )
    .await
    .expect("scheduler should complete without error");
    assert_eq!(results.len(), 1, "one call in, one result out");
    results.remove(0).content
}

/// Core evidence for the timeout/concurrency decoupling.
///
/// Before the change the two lanes disagreed: the parallel read-only batch
/// wrapped every call in `tokio::time::timeout(read_only_tool_timeout_secs)`
/// while the serial lane passed `None`. Turning parallelism off therefore
/// also turned the timeout off — the timeout rode on the concurrency switch.
///
/// Now neither lane can carry a deadline at all — `execute_one_with_service`
/// has no timeout parameter left to pass — so a tool that runs for ten
/// simulated minutes returns its real output on either lane, and the two
/// lanes produce byte-identical results.
///
/// Mutation check: re-adding a `tokio::time::timeout` around the tool
/// execution in `execute_one_with_service` turns this test red.
#[tokio::test(start_paused = true)]
async fn parallel_and_serial_lanes_agree_that_tools_have_no_timeout() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let observer = SchedulerEventObserver::default();

    // `file_read` is read-only -> parallel batch lane.
    let parallel_lane = run_single_tool_through_scheduler("file_read", unlimited_schedule(2), &observer).await;
    // `shell` is stateful -> serial lane.
    let serial_lane = run_single_tool_through_scheduler("shell", unlimited_schedule(2), &observer).await;

    assert!(
        !parallel_lane.contains("timed out"),
        "parallel lane must not impose a deadline, got: {parallel_lane}"
    );
    assert!(
        !serial_lane.contains("timed out"),
        "serial lane must not impose a deadline, got: {serial_lane}"
    );
    assert_eq!(
        parallel_lane, serial_lane,
        "the two lanes must be indistinguishable: a tool's outcome cannot depend on which lane ran it"
    );
}

/// No scheduler configuration can bring back a deadline, and none can push
/// read-only calls off the parallel lane. The schedule struct only carries
/// a window size and the priority ordering knobs; every combination of them
/// must still run both lanes deadline-free and identically.
#[tokio::test(start_paused = true)]
async fn no_schedule_config_can_reintroduce_a_timeout_or_a_serial_fallback() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    for window in [0_usize, 1, 2, 64] {
        for priority_enabled in [false, true] {
            let schedule = ReadOnlyToolScheduleConfig {
                concurrency_window: window,
                priority_enabled,
                low_priority_tool_names: ["file_read".to_string(), "shell".to_string()].into_iter().collect(),
            };
            let observer = SchedulerEventObserver::default();
            let parallel_lane = run_single_tool_through_scheduler("file_read", schedule.clone(), &observer).await;
            let serial_lane = run_single_tool_through_scheduler("shell", schedule, &observer).await;

            assert!(
                !parallel_lane.contains("timed out") && !serial_lane.contains("timed out"),
                "window={window} priority={priority_enabled} must stay deadline-free"
            );
            assert_eq!(
                parallel_lane, serial_lane,
                "window={window} priority={priority_enabled} must keep both lanes identical"
            );

            // The read-only call always takes the batch lane: a ToolBatch
            // event is proof the parallel path ran, and no configuration
            // suppresses it.
            let events = observer
                .batch_events
                .lock()
                .expect("batch events lock should be valid")
                .clone();
            assert_eq!(
                events.len(),
                1,
                "window={window} priority={priority_enabled}: read-only calls must always batch"
            );
            assert!(matches!(
                events.first(),
                Some(ObserverEvent::ToolBatch { batch_size: 1, .. })
            ));
        }
    }
}

/// Read-only calls in one iteration still overlap — deleting the rollout
/// gate removed the ability to *disable* concurrency, not concurrency
/// itself.
#[tokio::test]
async fn read_only_calls_still_run_concurrently_without_any_rollout_gate() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(DelayTool::new(
        "file_read",
        120,
        Arc::clone(&active),
        Arc::clone(&max_active),
    ))];
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(tools, &ledger_dir);
    let calls = vec![
        ParsedToolCall {
            name: "file_read".to_string(),
            arguments: serde_json::json!({"value": "a"}),
        },
        ParsedToolCall {
            name: "file_read".to_string(),
            arguments: serde_json::json!({"value": "b"}),
        },
    ];
    let observer = SchedulerEventObserver::default();
    let recorder = ToolResultRecorder::new(calls.len());
    let results = execute_tools_with_service(
        &calls,
        &[],
        &adapter,
        unlimited_schedule(2),
        None,
        ChatMode::default(),
        &observer,
        &recorder,
    )
    .await
    .expect("scheduler should complete without error");

    assert_eq!(results.len(), 2);
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        2,
        "both read-only calls must be in flight at the same time"
    );
}

#[tokio::test]
async fn production_scheduler_applies_barrier_for_shared_resource_tools() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(DelayTool::new(
        "file_write",
        120,
        Arc::clone(&active),
        Arc::clone(&max_active),
    ))];
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(tools_registry, &ledger_dir);
    let first_calls = single_call("file_write", serde_json::json!({"value":"A"}));
    let second_calls = single_call("file_write", serde_json::json!({"value":"B"}));

    let (first, second) = tokio::join!(
        run_through_production_scheduler(&first_calls, &adapter, unlimited_schedule(1), ChatMode::default()),
        run_through_production_scheduler(&second_calls, &adapter, unlimited_schedule(1), ChatMode::default()),
    );
    assert_eq!(
        first.iter().map(|result| result.content.as_str()).collect::<Vec<_>>(),
        vec!["ok:A"]
    );
    assert_eq!(
        second.iter().map(|result| result.content.as_str()).collect::<Vec<_>>(),
        vec!["ok:B"]
    );
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "barrier should serialize concurrent file_write calls"
    );
}

#[tokio::test(start_paused = true)]
async fn production_scheduler_refuses_a_tool_when_its_shared_resource_barrier_times_out() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let barrier = {
        let mut barriers = TOOL_BARRIERS.lock();
        barriers
            .entry("runtime_config")
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let held = barrier.lock_owned().await;
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(
        vec![Box::new(RecordingTool {
            name: "proxy_config".to_string(),
            execution_order: Arc::clone(&execution_order),
        })],
        &ledger_dir,
    );
    let calls = vec![ParsedToolCall {
        name: "proxy_config".to_string(),
        arguments: serde_json::json!({}),
    }];

    let task = tokio::spawn(async move {
        let recorder = ToolResultRecorder::new(calls.len());
        execute_tools_with_service(
            &calls,
            &[],
            &adapter,
            unlimited_schedule(1),
            None,
            ChatMode::default(),
            &NoopObserver,
            &recorder,
        )
        .await
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(TOOL_BARRIER_TIMEOUT_SECS + 1)).await;
    let results = task
        .await
        .expect("test: scheduler task must join")
        .expect("test: barrier timeout is a failed tool result, not a scheduler failure");
    drop(held);

    assert_eq!(results.len(), 1);
    assert!(
        results[0]
            .content
            .contains("could not acquire shared-resource barrier 'runtime_config' within 60s; tool was not executed"),
        "result: {}",
        results[0].content
    );
    assert!(
        execution_order
            .lock()
            .expect("execution order lock should be valid")
            .is_empty(),
        "a timed-out shared-resource call must not reach its backend"
    );
}

#[tokio::test]
async fn production_scheduler_rejects_action_specific_schema_errors_before_side_effects() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let executions = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(ConditionalSchemaTool {
        executions: Arc::clone(&executions),
    })];
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(tools_registry, &ledger_dir);
    let calls = single_call("conditional_schema", serde_json::json!({"action": "write"}));
    let results = run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::default()).await;
    let result = &results[0].content;

    assert!(result.contains("$.content is required"), "{result}");
    assert!(result.contains("Read the tool schema and retry"), "{result}");
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn production_scheduler_in_plan_mode_intercepts_write_tools() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    // file_write is registered but plan mode should short-circuit before
    // touching it. We confirm via the DelayTool counter that no execution
    // happened.
    let autonomy = crate::config::AutonomyConfig {
        level: crate::security::AutonomyLevel::Full,
        ..crate::config::AutonomyConfig::default()
    };
    let policy = crate::security::SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));
    assert_eq!(
        policy.decide("file_write", "user", "terminal", "chat"),
        crate::security::policy::ToolDecision::Allow,
        "full autonomy is the ceiling, so the policy itself would allow this write"
    );

    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(DelayTool::new(
        "file_write",
        500,
        Arc::clone(&active),
        Arc::clone(&max_active),
    ))];
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(tools_registry, &ledger_dir);
    let calls = single_call("file_write", serde_json::json!({"value": "x"}));

    let started = std::time::Instant::now();
    let results = run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::Plan).await;
    let elapsed = started.elapsed();
    let result = &results[0].content;

    assert!(
        result.starts_with("[plan mode] would call file_write"),
        "expected simulated result, got: {result}"
    );
    assert!(
        elapsed < Duration::from_millis(400),
        "plan-mode short-circuit must NOT execute the real tool (elapsed={elapsed:?})"
    );
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        0,
        "real tool body must not have been invoked under plan mode"
    );
}

#[tokio::test]
async fn production_scheduler_in_plan_mode_allows_read_tools() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    // file_read is in the read-only set — plan mode should let it run.
    // We use a fake DelayTool named "file_read" and assert it ran (active
    // counter ≥ 1 at peak).
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(DelayTool::new(
        "file_read",
        20,
        Arc::clone(&active),
        Arc::clone(&max_active),
    ))];
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(tools_registry, &ledger_dir);
    let calls = single_call("file_read", serde_json::json!({"value": "x"}));
    let results = run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::Plan).await;
    let result = &results[0].content;

    assert!(
        !result.starts_with("[plan mode]"),
        "read tool must not be intercepted in plan mode (got: {result})"
    );
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "read tool body must have been executed once"
    );
}

#[tokio::test]
async fn production_scheduler_in_edit_mode_runs_write_tools_normally() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(DelayTool::new(
        "file_write",
        20,
        Arc::clone(&active),
        Arc::clone(&max_active),
    ))];
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(tools_registry, &ledger_dir);
    let calls = single_call("file_write", serde_json::json!({"value": "y"}));
    let results = run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::Edit).await;
    let result = &results[0].content;

    assert!(
        !result.starts_with("[plan mode]"),
        "edit mode must never produce plan-mode synthesized output"
    );
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "edit mode should execute the real tool"
    );
}

#[tokio::test]
async fn production_scheduler_in_auto_mode_runs_write_tools_normally() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(DelayTool::new(
        "shell",
        20,
        Arc::clone(&active),
        Arc::clone(&max_active),
    ))];
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(tools_registry, &ledger_dir);
    let calls = single_call("shell", serde_json::json!({"value": "echo hi"}));
    let results = run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::Auto).await;
    let result = &results[0].content;

    assert!(
        !result.starts_with("[plan mode]"),
        "auto mode must never produce plan-mode synthesized output"
    );
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "auto mode should execute the real tool"
    );
}

/// A tool that fails with empty output and its reason in `error`.
struct EmptyOutputFailureTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for EmptyOutputFailureTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Fails with empty output"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(crate::tools::ToolResult {
            success: false,
            output: String::new(),
            error: Some("Path not allowed: /etc/shadow".to_string()),
        })
    }
}

/// Live path (the service-backed scheduler every surface runs, the chat Redux
/// driver included): a tool that fails with empty output puts its error reason
/// into the result the model reads, not an empty string. Replaces the deleted
/// legacy executor's `failed_tool_with_empty_output_surfaces_error_in_content`
/// (R-LOOPD5 F-5).
#[tokio::test]
async fn a_failed_tool_with_empty_output_reports_its_reason_to_the_model() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let executions = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EmptyOutputFailureTool {
        executions: Arc::clone(&executions),
    })];
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scheduler_test_adapter(tools, &ledger_dir);
    let calls = vec![ParsedToolCall {
        name: "file_read".to_string(),
        arguments: serde_json::json!({}),
    }];
    let recorder = ToolResultRecorder::new(calls.len());
    let results = execute_tools_with_service(
        &calls,
        &[],
        &adapter,
        unlimited_schedule(1),
        None,
        ChatMode::default(),
        &NoopObserver,
        &recorder,
    )
    .await
    .expect("scheduler should complete without error");

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(results[0].content.starts_with("Error: "), "{}", results[0].content);
    assert!(
        results[0].content.contains("Path not allowed: /etc/shadow"),
        "{}",
        results[0].content
    );
    assert!(!results[0].succeeded(), "a failure must not read as a success");
}

/// Live path: under read-only autonomy an act tool is denied by the policy
/// stage, before approval and before execution. Replaces the deleted legacy
/// executor's `read_only_policy_denies_act_tool_without_prompt_or_execution`
/// (R-LOOPD5 F-5); that the service asks no approval after a policy deny is
/// pinned by `policy_deny_prevents_approval_preparation_and_execution_but_audits`.
#[tokio::test]
async fn read_only_autonomy_denies_an_act_tool_before_approval_or_execution() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "delegate".to_string(),
        execution_order: Arc::clone(&execution_order),
    })];
    let read_only = crate::security::SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::ReadOnly,
        ..crate::security::SecurityPolicy::default()
    };
    let scope_ctx = ScopeContext {
        policy: &read_only,
        sender: "user@example.com",
        channel: "telegram",
        chat_type: "private",
        chat_id: "chat-1",
        session_key: None,
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        config_generation_id: None,
        config_source_revision: None,
    };
    let (policy, tool_execution_context) = agent_tool_execution_context(Some(&scope_ctx), "telegram", None);
    let approvals = Arc::new(ApprovalManager::new());
    let approval_strategy = AgentToolApprovalStrategy {
        manager: Some(Arc::clone(&approvals)),
        resolver: None,
        channel: "telegram".to_string(),
        policy: Arc::clone(&policy),
        cancellation: None,
    };
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let ledger: Arc<dyn crate::memory::Memory> = Arc::new(
        crate::memory::SqliteMemory::new(ledger_dir.path()).expect("test: sqlite ledger should open in a temp dir"),
    );
    let service = crate::tools::ToolExecutionService::from_shared_boxed_registry(
        Arc::new(tools),
        Arc::new(crate::tools::SecurityEffectPolicy::new(policy)),
        Arc::new(approval_strategy),
        Arc::new(crate::tools::AdapterOwnedPreparation),
        Arc::new(crate::tools::TracingToolExecutionAudit),
    )
    .with_idempotency_memory(ledger);
    let adapter = ToolLoopRuntimeAdapter {
        verify_unexecuted_reply: false,
        provider_trace: None,
        events: None,
        stream_provider: false,
        tool_execution_service: Some(Arc::new(service)),
        tool_execution_context,
        allowed_tool_names: None,
    };
    let calls = vec![ParsedToolCall {
        name: "delegate".to_string(),
        arguments: serde_json::json!({}),
    }];
    let recorder = ToolResultRecorder::new(calls.len());
    let results = execute_tools_with_service(
        &calls,
        &[],
        &adapter,
        unlimited_schedule(1),
        None,
        ChatMode::default(),
        &NoopObserver,
        &recorder,
    )
    .await
    .expect("scheduler should complete without error");

    assert!(
        results[0].content.starts_with("Error: "),
        "a denial result: {}",
        results[0].content
    );
    assert!(
        execution_order
            .lock()
            .expect("execution order lock should be valid")
            .is_empty(),
        "the act tool must not run under read-only autonomy"
    );
}

/// Live path: the scheduler's unrecoverable-failure classifier flags
/// policy/permission/approval refusals and unknown tools, and leaves a
/// recoverable failure (a missing file, a timeout, a bad argument) alone, so
/// the loop retries those instead of stopping. Replaces the deleted legacy
/// executor's `recoverable_tool_failure_is_not_flagged_unrecoverable` and
/// `unrecoverable_error_detector_classifies_correctly` (R-LOOPD5 F-5).
#[test]
fn the_unrecoverable_classifier_separates_refusals_from_recoverable_failures() {
    for unrecoverable in [
        "Error: Path not allowed: /etc/shadow",
        "Error: permission denied",
        "Error: operation not permitted",
        "Error: blocked by policy",
        "Error: approval denied by the operator",
        "Error: preparation denied",
        "Error: unknown tool 'frobnicate'",
    ] {
        assert!(shared_unrecoverable_tool_error(unrecoverable), "{unrecoverable}");
    }
    for recoverable in [
        "Error: file not found: notes.txt",
        "Error: request timed out after 30s",
        "Error: $.content is required",
        "Error: connection reset by peer",
    ] {
        assert!(!shared_unrecoverable_tool_error(recoverable), "{recoverable}");
    }
}

/// Counts executions of a `skills_manage` stand-in; the argument contract is
/// the lifecycle one the real tool publishes for the fields the test sends.
struct GrantBoundProbe(Arc<AtomicUsize>);

#[async_trait]
impl Tool for GrantBoundProbe {
    fn name(&self) -> &str {
        "skills_manage"
    }

    fn description(&self) -> &str {
        "PRX grant-bound lifecycle probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "description": "action"},
                "source": {"type": "string", "description": "source"},
                "revision_id": {"type": "string", "description": "revision"}
            }
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(crate::tools::ToolResult {
            success: true,
            output: "ran".to_string(),
            error: None,
        })
    }
}

/// Finding 113.1(c): a session "Always" answer covers a tool, never a call the
/// tool only accepts under its own approval. Every grant-bound lifecycle call
/// is put to the owner, so an "Always" given for one import cannot approve a
/// later import of a different bundle hash. Calls that need no approval are
/// unaffected.
#[tokio::test]
async fn always_allowlisting_never_approves_a_grant_bound_lifecycle_call() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    let executions = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(GrantBoundProbe(Arc::clone(&executions)))];
    let policy = crate::security::SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Full,
        ..crate::security::SecurityPolicy::default()
    };
    let scope_ctx = scheduler_scope(&policy, "user@example.com");
    let manager = Arc::new(ApprovalManager::new());
    manager.record_decision(
        "skills_manage",
        &serde_json::json!({"action": "lifecycle_import_executable"}),
        ApprovalResponse::Always,
        "telegram",
    );
    assert!(manager.is_session_allowlisted("skills_manage"));
    let resolver = FixedApprovalResolver::new(ApprovalDecision::Deny);
    let ledger_dir = TempDir::new().expect("test: temp dir should be creatable");
    let adapter = scoped_scheduler_adapter(
        tools,
        &scope_ctx,
        Some(Arc::clone(&manager)),
        Some(resolver.clone()),
        &ledger_dir,
    );
    for revision in [format!("r1-{}", "a".repeat(64)), format!("r1-{}", "b".repeat(64))] {
        let calls = single_call(
            "skills_manage",
            serde_json::json!({"action": "lifecycle_import_executable", "source": "/srv/bundle", "revision_id": revision}),
        );
        let results =
            run_through_production_scheduler(&calls, &adapter, unlimited_schedule(1), ChatMode::default()).await;
        assert!(
            results[0].content.contains("Denied by user"),
            "the allowlist must not approve the import of {revision}: {}",
            results[0].content
        );
    }
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        2,
        "each grant-bound call is put to the owner"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0, "no import ran");

    // A read of the same tool needs no approval at all.
    let status = single_call("skills_manage", serde_json::json!({"action": "lifecycle_status"}));
    let results = run_through_production_scheduler(&status, &adapter, unlimited_schedule(1), ChatMode::default()).await;
    assert_eq!(results[0].content, "ran");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
}

#[path = "tool_scheduler/tracing_context.rs"]
mod tracing_context;

struct StatusTextTool {
    process_started: Option<bool>,
    success: bool,
    output: &'static str,
}

#[async_trait]
impl Tool for StatusTextTool {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "Returns arbitrary file content"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: self.success,
            output: self.output.to_string(),
            error: None,
        })
    }
    async fn execute_named_observed(
        &self,
        _name: &str,
        args: serde_json::Value,
        _cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<crate::tools::traits::ToolInvocationResult> {
        Ok(crate::tools::traits::ToolInvocationResult {
            result: self.execute(args).await?,
            process: self.process_started.map(|started| {
                Box::new(crate::tools::traits::ProcessExecutionDetails {
                    termination: if started {
                        crate::tools::traits::ProcessTermination::Exited
                    } else {
                        crate::tools::traits::ProcessTermination::NotStarted
                    },
                    started,
                    output_complete: true,
                    diagnostic: None,
                    exit_code: started.then_some(if self.success { 0 } else { 7 }),
                    signal: None,
                    stdout_truncated: false,
                    stderr_truncated: false,
                })
            }),
        })
    }
}

#[tokio::test]
async fn scheduler_uses_typed_status_and_counts_executed_failures() {
    let _test_guard = TOOL_SCHEDULER_TEST_LOCK.lock().await;
    for (success, output, process_started, expected_executed) in [
        (true, "Error is a word in this file", None, true),
        (true, "Denied is another word", None, true),
        (false, "legacy refusal", None, false),
        (false, "partial output", Some(true), true),
        (false, "not started", Some(false), false),
    ] {
        let temp = TempDir::new().unwrap();
        let adapter = scheduler_test_adapter(
            vec![Box::new(StatusTextTool {
                process_started,
                success,
                output,
            })],
            &temp,
        );
        let results = run_through_production_scheduler(
            &single_call("file_read", serde_json::json!({})),
            &adapter,
            unlimited_schedule(1),
            ChatMode::Edit,
        )
        .await;
        assert_eq!(results[0].succeeded(), success);
        assert_eq!(
            results[0].executed, expected_executed,
            "process facts distinguish command failure from refusal"
        );
        assert!(results[0].content.contains(output));
    }
    let temp = TempDir::new().unwrap();
    let adapter = scheduler_test_adapter(vec![], &temp);
    let results = run_through_production_scheduler(
        &single_call("missing_tool", serde_json::json!({})),
        &adapter,
        unlimited_schedule(1),
        ChatMode::Edit,
    )
    .await;
    assert!(!results[0].executed);
    assert!(!results[0].succeeded());
}
