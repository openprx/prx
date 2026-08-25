//! Runtime observability endpoints: what is running, and how to end it.
//!
//! PRX removes timeouts on purpose, so nothing in the process expires on its
//! own. These endpoints are the operator's replacement for that: a live listing
//! of every registered work item with its lineage and elapsed time, a by-id kill
//! that terminates process groups rather than lone pids, and the connection-pool
//! saturation counters that until now had a `snapshot()` but no way out of the
//! process.

use super::{AppState, authorize_resource_mutation};
use crate::runtime::registry;
use crate::security::policy::ResourceRiskLevel;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub(super) struct WorkItemResponse {
    id: String,
    kind: &'static str,
    name: String,
    state: &'static str,
    parent: Option<String>,
    run_id: Option<String>,
    /// Fan-out this item belongs to, when it was started by `spawn_batch`.
    batch_id: Option<String>,
    started_at_unix_ms: u64,
    elapsed_secs: u64,
    elapsed_ms: u128,
    pid: Option<u32>,
    pgid: Option<i32>,
}

impl From<registry::WorkSnapshot> for WorkItemResponse {
    fn from(snapshot: registry::WorkSnapshot) -> Self {
        Self {
            id: snapshot.id.to_string(),
            kind: snapshot.kind.as_str(),
            name: snapshot.name.to_string(),
            state: snapshot.state.as_str(),
            parent: snapshot.parent.map(|parent| parent.to_string()),
            run_id: snapshot.run_id.map(|run_id| run_id.to_string()),
            batch_id: snapshot.batch_id.map(|batch_id| batch_id.to_string()),
            started_at_unix_ms: snapshot.started_at_unix_ms,
            elapsed_secs: snapshot.elapsed.as_secs(),
            elapsed_ms: snapshot.elapsed.as_millis(),
            pid: snapshot.pid,
            pgid: snapshot.pgid,
        }
    }
}

#[derive(Serialize)]
pub(super) struct TasksResponse {
    /// Work items whose owner is still alive.
    running: Vec<WorkItemResponse>,
    /// Children spawned but never reaped: still alive after their owner ended
    /// (`orphaned`), exited but unreaped (`zombie`), or of unknown liveness
    /// (`abandoned`). This is the orphan-hunting view.
    unreaped: Vec<WorkItemResponse>,
    total: usize,
}

pub async fn get_tasks() -> Json<TasksResponse> {
    let (unreaped, running): (Vec<_>, Vec<_>) = registry::snapshot_all().into_iter().partition(|snapshot| {
        matches!(
            snapshot.state,
            registry::WorkState::Abandoned | registry::WorkState::Orphaned | registry::WorkState::Zombie
        )
    });
    let total = running.len().saturating_add(unreaped.len());
    Json(TasksResponse {
        running: running.into_iter().map(WorkItemResponse::from).collect(),
        unreaped: unreaped.into_iter().map(WorkItemResponse::from).collect(),
        total,
    })
}

#[derive(Deserialize)]
pub(super) struct KillQuery {
    /// Take the item's descendants with it. Defaults to true; see
    /// [`registry::kill`] for why.
    cascade: Option<bool>,
}

#[derive(Serialize)]
pub(super) struct KillTargetResponse {
    id: String,
    kind: &'static str,
    name: String,
    outcome: &'static str,
}

#[derive(Serialize)]
pub(super) struct KillResponse {
    requested: String,
    cascade: bool,
    targets: Vec<KillTargetResponse>,
}

pub async fn post_task_kill(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<KillQuery>,
) -> Result<Json<KillResponse>, (StatusCode, Json<serde_json::Value>)> {
    // Ending live work is a high-risk side effect, so it goes through the same
    // gate as every other mutating gateway operation.
    authorize_resource_mutation(&state, "runtime_task_kill", ResourceRiskLevel::High)?;

    // Accept either address space. A `WorkId` is a process-local monotonic
    // counter, so it is only meaningful to an operator on this box; a caller
    // that reached this endpoint from another process has nothing but the
    // run id, and until now a kill could not be aimed with one.
    let cascade = query.cascade.unwrap_or(true);
    let (requested, results) = match resolve_task_address(&id)? {
        Some(work_id) => (work_id.to_string(), registry::kill(work_id, cascade).await),
        // A third address: a `spawn_batch` id. It denotes a *set* of sibling
        // roots rather than one item, so it cannot resolve to a `WorkId`; the
        // registry ends the whole fan-out with the same per-member cascade.
        None => (id.trim().to_string(), registry::kill_batch(id.trim(), cascade).await),
    };
    if results.is_empty()
        || results
            .iter()
            .all(|result| result.outcome == registry::KillOutcome::Unknown)
    {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("no running work item with id '{id}'")})),
        ));
    }

    Ok(Json(KillResponse {
        requested,
        cascade,
        targets: results
            .into_iter()
            .map(|result| KillTargetResponse {
                id: result.id.to_string(),
                kind: result.kind.as_str(),
                name: result.name.to_string(),
                outcome: result.outcome.as_str(),
            })
            .collect(),
    }))
}

/// Resolve a path id that may be either a run id or a [`registry::WorkId`].
///
/// `Ok(None)` means "well-formed but nothing is running under it", which the
/// callers report as 404. `Err` is reserved for an address that cannot denote a
/// work item in either space, so a typo is never mistaken for a finished task.
fn resolve_task_address(id: &str) -> Result<Option<registry::WorkId>, (StatusCode, Json<serde_json::Value>)> {
    if id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "task id must not be empty"})),
        ));
    }
    Ok(registry::resolve_address(id))
}

#[derive(Deserialize)]
pub(super) struct TaskMessageRequest {
    /// Instruction to hand to the running task.
    message: String,
}

#[derive(Serialize)]
pub(super) struct TaskMessageResponse {
    /// The address as supplied by the caller.
    requested: String,
    /// The work item the address resolved to, in this process's id space.
    id: String,
    /// The run id, which is the address that means anything outside this
    /// process.
    run_id: Option<String>,
    kind: &'static str,
    name: String,
    outcome: &'static str,
}

/// Inject an operator message into a running task.
///
/// This is the control-plane half of `sessions_send`: same bounded channel,
/// same run, reached by run id from a different entry point. Nothing here is
/// bounded by a clock — a busy target parks the caller, which is the intended
/// backpressure and not a failure.
pub async fn post_task_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<TaskMessageRequest>,
) -> Result<Json<TaskMessageResponse>, (StatusCode, Json<serde_json::Value>)> {
    // Medium, where a kill is High. Killing destroys in-flight work and cannot
    // be undone; a message is additive, and every side effect the target then
    // takes is re-authorized through this same gate under the run's own policy,
    // so the message cannot widen what the run was already allowed to do. It is
    // not Low either: it redirects an autonomous agent that another entry point
    // owns, which is exactly the operator-plane action this gate exists for.
    authorize_resource_mutation(&state, "runtime_task_message", ResourceRiskLevel::Medium)?;

    let message = request.message.trim();
    if message.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "message must not be empty"})),
        ));
    }

    let Some(work_id) = resolve_task_address(&id)? else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("no running work item with id '{id}'")})),
        ));
    };
    // Snapshot before the send so the report names the target even if the run
    // finishes while the message is in flight.
    let Some(target) = registry::snapshot(work_id) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("no running work item with id '{id}'")})),
        ));
    };

    // Register the delivery itself, for its whole duration.
    //
    // The target's steering queue is bounded and this runtime has no wall
    // clock, so a send to a busy — or wedged — run legitimately parks with no
    // upper bound. Without a row of its own, that parked delivery is invisible
    // everywhere: it is not the target (which looks idle), it is not the
    // caller's turn (the caller is another process), and no timeout will ever
    // end it. Since a manual kill is the only backstop left once nothing
    // expires by itself, being *seen* is the precondition for being ended, and
    // the cancellation token below is what makes the seen row actually
    // killable rather than merely observable.
    //
    // The guard is dropped the instant the send settles, so a delivery that
    // does not park never lingers in the listing.
    let delivery_cancel = tokio_util::sync::CancellationToken::new();
    let delivery_name = format!("steer → {work_id} {}", target.name);
    let delivery =
        registry::register_tool_call(&delivery_name, target.run_id.as_deref(), Some(delivery_cancel.clone()));
    let delivered = tokio::select! {
        biased;
        // A completed send wins a tie: once the message is in the queue it has
        // been delivered, and reporting it as killed would be a lie the caller
        // could act on by sending it twice.
        result = registry::steer(work_id, message.to_string()) => Some(result),
        () = delivery_cancel.cancelled() => None,
    };
    drop(delivery);

    let Some(outcome) = delivered else {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "delivery to work item '{work_id}' was ended by an operator before the message was accepted"
                ),
                "outcome": "killed",
            })),
        ));
    };

    if let Err(rejection) = outcome {
        let detail = match rejection {
            registry::SteerRejection::NotSteerable => format!(
                "work item '{work_id}' ({}) exposes no message channel; only sub-agent runs can be steered",
                target.kind.as_str()
            ),
            registry::SteerRejection::ChannelClosed => {
                format!("work item '{work_id}' finished before the message could be delivered")
            }
        };
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": detail, "outcome": rejection.as_str()})),
        ));
    }

    Ok(Json(TaskMessageResponse {
        requested: id,
        id: work_id.to_string(),
        run_id: target.run_id.map(|run_id| run_id.to_string()),
        kind: target.kind.as_str(),
        name: target.name.to_string(),
        outcome: "delivered",
    }))
}

#[derive(Serialize)]
pub(super) struct PoolResponse {
    kind: &'static str,
    name: String,
    metrics: serde_json::Value,
}

#[derive(Serialize)]
pub(super) struct PoolsResponse {
    pools: Vec<PoolResponse>,
}

pub async fn get_pools() -> Json<PoolsResponse> {
    Json(PoolsResponse {
        pools: registry::pool_snapshots()
            .into_iter()
            .map(|snapshot| PoolResponse {
                kind: snapshot.kind,
                name: snapshot.name,
                metrics: snapshot.metrics,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
    use crate::config::Config;
    use crate::gateway::{GatewayRateLimiter, IdempotencyStore};
    use crate::hooks::HookManager;
    use crate::memory::SqliteMemory;
    use crate::observability::NoopObserver;
    use crate::providers::Provider;
    use crate::runtime::tasks_cli::{self, TasksEndpoint};
    use crate::security::pairing::PairingGuard;
    use crate::security::policy::AutonomyLevel;
    use crate::tools::traits::Tool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::broadcast;

    /// The steering wording a real run injects. Asserting on the shared
    /// constructor rather than a literal keeps this test honest if the wording
    /// ever moves.
    use crate::tools::sessions_spawn::{SessionsSpawnTool, steering_instruction};

    /// Minimal channel: `sessions_spawn` needs one to announce results to, and
    /// nothing in these tests reads what it would have sent.
    struct SilentChannel;

    #[async_trait::async_trait]
    impl Channel for SilentChannel {
        fn name(&self) -> &str {
            "silent"
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Provider that parks inside every call until the test hands out permits.
    /// That is what makes the spawned sub-agent a *long* task: it is genuinely
    /// still running while the control plane addresses it.
    struct GatedProvider {
        gate: Arc<tokio::sync::Semaphore>,
    }

    impl GatedProvider {
        async fn park(&self) {
            if let Ok(permit) = self.gate.acquire().await {
                drop(permit);
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for GatedProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.park().await;
            Ok("gated".to_string())
        }

        async fn chat(
            &self,
            _request: crate::providers::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            self.park().await;
            Ok(crate::providers::ChatResponse {
                text: Some("gated".to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
    }

    fn test_app_state(autonomy: AutonomyLevel, workspace: &std::path::Path) -> AppState {
        let mut config = Config::default();
        config.autonomy.level = autonomy;
        config.workspace_dir = workspace.to_path_buf();
        let memory = SqliteMemory::new(workspace).expect("test: sqlite memory");
        AppState {
            config: crate::config::new_shared(config),
            provider: Arc::new(GatedProvider {
                gate: Arc::new(tokio::sync::Semaphore::new(usize::from(u8::MAX))),
            }),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(memory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            turn_runtime: None,
            hooks: Arc::new(HookManager::new(workspace.to_path_buf())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(10_000, 10_000, 10_000, 10_000)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_runtime: None,
        }
    }

    /// Serve the real API router on an ephemeral port and return its endpoint.
    ///
    /// The whole stack is genuine: the same `router()` the daemon mounts, the
    /// same auth and rate-limit layers, reached over a real socket by the same
    /// client `prx tasks` uses.
    async fn serve(state: AppState) -> (TasksEndpoint, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .nest("/api", super::super::router(state.clone()))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind ephemeral port");
        let port = listener.local_addr().expect("test: local addr").port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut config = Config::default();
        config.gateway.host = "127.0.0.1".to_string();
        config.gateway.port = port;
        (TasksEndpoint::resolve(&config, None, None), handle)
    }

    /// Spawn a genuinely long-running sub-agent and return `(tool, run_id)`.
    async fn spawn_long_running_sub_agent(
        workspace: &std::path::Path,
        gate: &Arc<tokio::sync::Semaphore>,
    ) -> (SessionsSpawnTool, String) {
        let tool = SessionsSpawnTool::new(
            Arc::new(SilentChannel) as Arc<dyn Channel>,
            Arc::new(GatedProvider { gate: Arc::clone(gate) }),
            "test-provider",
            "test-model",
            0.0,
            Arc::new(crate::security::SecurityPolicy {
                workspace_dir: workspace.to_path_buf(),
                ..crate::security::SecurityPolicy::default()
            }),
            workspace.to_path_buf(),
            crate::config::MultimodalConfig::default(),
            crate::config::AgentCompactionConfig::default(),
            std::collections::HashMap::new(),
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            crate::config::SessionsSpawnConfig::default(),
        );
        // A run only enters the steerable agent loop when it has a tools
        // registry: `run_sub_agent_task` short-circuits a registry-less spawn
        // into a single provider call that never reads its steer channel. An
        // empty registry is enough to take the real path.
        tool.tools_handle()
            .set(Arc::new(Vec::new()))
            .ok()
            .expect("test: tools registry must be injectable once");
        tool.set_default_recipient(Some("test-recipient".to_string())).await;
        let result = tool
            .execute(serde_json::json!({
                "task": "a long task the operator will interrupt",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "sender": "operator",
                    "channel": "silent",
                    "chat_type": "direct",
                    "chat_id": "chat-1"
                }
            }))
            .await
            .expect("test: spawn must be accepted");
        assert!(result.success, "test: spawn failed: {:?}", result.error);
        let runs = tool.active_runs_snapshot().await;
        let run_id = runs
            .first()
            .map(|run| run.id.clone())
            .expect("test: the spawn must have registered a run");
        (tool, run_id)
    }

    /// Wait until the run's row (and therefore its address) exists.
    async fn await_addressable(run_id: &str) -> registry::WorkId {
        for _ in 0..400 {
            if let Some(id) = registry::resolve_address(run_id) {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("test: run {run_id} never became addressable in the work registry");
    }

    async fn await_history_containing(
        runs: &Arc<tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>>,
        run_id: &str,
        needle: &str,
    ) -> bool {
        for _ in 0..400 {
            let history = {
                let guard = runs.read().await;
                match guard.iter().find(|run| run.id == run_id) {
                    Some(run) => Arc::clone(&run.history),
                    None => break,
                }
            };
            if history.read().await.iter().any(|entry| entry.content.contains(needle)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// Evidence 1: a long task started from the tool plane is interrupted from
    /// the control plane, end to end — real CLI transport, real socket, real
    /// router, real registry, real sub-agent — and the target actually takes
    /// the message on board as an operator turn in its history.
    #[tokio::test]
    async fn message_reaches_a_live_sub_agent_addressed_by_run_id() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (tool, run_id) = spawn_long_running_sub_agent(workspace.path(), &gate).await;
        await_addressable(&run_id).await;

        let (endpoint, server) = serve(test_app_state(AutonomyLevel::Full, workspace.path())).await;
        let report = tasks_cli::request_message(&endpoint, &run_id, "also check Y")
            .await
            .expect("test: the control plane must accept the message");

        assert_eq!(report.outcome, "delivered");
        assert_eq!(report.run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(report.requested, run_id);

        let injected = steering_instruction("also check Y");
        assert!(
            await_history_containing(&tool.active_runs_arc(), &run_id, &injected).await,
            "the steered run must have taken the operator message into its history"
        );

        gate.add_permits(64);
        server.abort();
    }

    /// Evidence 2: both address spaces resolve to the same run. The run id is
    /// what a caller outside this process has; `w42` is the local convenience.
    #[tokio::test]
    async fn run_id_and_work_id_address_the_same_run() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (tool, run_id) = spawn_long_running_sub_agent(workspace.path(), &gate).await;
        let work_id = await_addressable(&run_id).await;

        let (endpoint, server) = serve(test_app_state(AutonomyLevel::Full, workspace.path())).await;

        let by_run_id = tasks_cli::request_message(&endpoint, &run_id, "first, by run id")
            .await
            .expect("test: run id must be a valid address");
        let by_work_id = tasks_cli::request_message(&endpoint, &work_id.to_string(), "second, by work id")
            .await
            .expect("test: work id must stay a valid address");

        assert_eq!(by_run_id.id, by_work_id.id, "both addresses must denote one run");
        assert_eq!(by_run_id.run_id, by_work_id.run_id);
        assert_eq!(by_work_id.run_id.as_deref(), Some(run_id.as_str()));

        let runs = tool.active_runs_arc();
        assert!(
            await_history_containing(&runs, &run_id, &steering_instruction("second, by work id")).await,
            "the message sent by work id must reach the same run"
        );

        gate.add_permits(64);
        server.abort();
    }

    /// Evidence 3: `kill` learned the portable address without losing the local
    /// one.
    #[tokio::test]
    async fn kill_accepts_a_run_id_and_still_accepts_a_work_id() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (_tool, run_id) = spawn_long_running_sub_agent(workspace.path(), &gate).await;
        let work_id = await_addressable(&run_id).await;

        let (endpoint, server) = serve(test_app_state(AutonomyLevel::Full, workspace.path())).await;
        let report = tasks_cli::request_kill(&endpoint, &run_id, false)
            .await
            .expect("test: a kill addressed by run id must be accepted");
        assert_eq!(report.requested, work_id.to_string());
        assert!(
            report.targets.iter().any(|target| target.id == work_id.to_string()),
            "the run id must have resolved to the same work item"
        );

        // And the legacy address space still works: a second, unrelated run
        // killed by `w<n>` behaves exactly as before.
        let (_tool2, run_id2) = spawn_long_running_sub_agent(workspace.path(), &gate).await;
        let work_id2 = await_addressable(&run_id2).await;
        let report2 = tasks_cli::request_kill(&endpoint, &work_id2.to_string(), false)
            .await
            .expect("test: a kill addressed by work id must still be accepted");
        assert_eq!(report2.requested, work_id2.to_string());

        gate.add_permits(64);
        server.abort();
    }

    /// Evidence 4: the gate is real. Under `read_only` the control plane refuses
    /// to steer, and nothing reaches the run.
    #[tokio::test]
    async fn message_is_refused_when_the_policy_forbids_acting() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (tool, run_id) = spawn_long_running_sub_agent(workspace.path(), &gate).await;
        await_addressable(&run_id).await;

        let (endpoint, server) = serve(test_app_state(AutonomyLevel::ReadOnly, workspace.path())).await;
        let error = tasks_cli::request_message(&endpoint, &run_id, "you must not hear this")
            .await
            .expect_err("test: a read-only policy must refuse to steer");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("403"),
            "the refusal must be an authorization failure, got: {rendered}"
        );

        let runs = tool.active_runs_arc();
        let run_history = {
            let guard = runs.read().await;
            guard
                .iter()
                .find(|run| run.id == run_id)
                .map(|run| Arc::clone(&run.history))
                .expect("test: the run must still be registered")
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !run_history
                .read()
                .await
                .iter()
                .any(|entry| entry.content.contains("you must not hear this")),
            "a refused message must not reach the run"
        );

        gate.add_permits(64);
        server.abort();
    }

    /// Evidence 5: an address that denotes nothing is an explicit error, never a
    /// silent success — in either address space.
    #[tokio::test]
    async fn unknown_addresses_fail_loudly() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let (endpoint, server) = serve(test_app_state(AutonomyLevel::Full, workspace.path())).await;

        for address in ["7b1f0f2a-0000-4000-8000-000000000000", "w999999999"] {
            let error = tasks_cli::request_message(&endpoint, address, "nobody is listening")
                .await
                .expect_err("test: an unknown address must be an error");
            let rendered = format!("{error:#}");
            assert!(
                rendered.contains("404") && rendered.contains(address),
                "the error must name the missing address, got: {rendered}"
            );

            let error = tasks_cli::request_kill(&endpoint, address, true)
                .await
                .expect_err("test: an unknown kill address must be an error");
            assert!(
                format!("{error:#}").contains("404"),
                "kill must report an unknown address as missing"
            );
        }

        server.abort();
    }

    /// An empty message is rejected before anything is delivered: a blank steer
    /// would otherwise cancel and restart the target's current segment for
    /// nothing.
    #[tokio::test]
    async fn blank_messages_are_rejected() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (_tool, run_id) = spawn_long_running_sub_agent(workspace.path(), &gate).await;
        await_addressable(&run_id).await;

        let (endpoint, server) = serve(test_app_state(AutonomyLevel::Full, workspace.path())).await;
        let error = tasks_cli::request_message(&endpoint, &run_id, "   ")
            .await
            .expect_err("test: a blank message must be rejected");
        assert!(format!("{error:#}").contains("400"));

        gate.add_permits(64);
        server.abort();
    }

    /// A live item with no message channel — an agent turn, a tool call — is a
    /// distinct, explicit refusal rather than a 404 or a quiet success.
    #[tokio::test]
    async fn items_without_a_message_channel_are_refused_explicitly() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let run_id = format!("turn-{}", uuid::Uuid::new_v4());
        let guard = registry::register_turn("inbound turn", &run_id, None);
        let work_id = guard.id();

        let (endpoint, server) = serve(test_app_state(AutonomyLevel::Full, workspace.path())).await;
        let error = tasks_cli::request_message(&endpoint, &work_id.to_string(), "steer a turn")
            .await
            .expect_err("test: a turn exposes no message channel");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("409"), "expected a conflict, got: {rendered}");
        assert!(rendered.contains("no message channel"), "got: {rendered}");

        drop(guard);
        server.abort();
    }

    /// Register a run whose steering queue is already full and whose receiver is
    /// never drained, so the next delivery genuinely parks on backpressure.
    ///
    /// The receiver is returned rather than dropped: dropping it would close the
    /// channel and turn the parked send into an immediate `ChannelClosed`, which
    /// is the opposite of what these tests need to observe.
    async fn wedged_target() -> (registry::WorkGuard, String, tokio::sync::mpsc::Receiver<String>) {
        let run_id = format!("wedged-{}", uuid::Uuid::new_v4());
        let guard = registry::register_sub_agent("wedged run", &run_id, None, None, None);
        // Capacity one, already occupied: the queue is full by construction, the
        // same state a real run reaches when it stops draining.
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(1);
        tx.send("already queued".to_string())
            .await
            .expect("test: the first message fits in the queue");
        registry::attach_steer_sender(guard.id(), tx);
        (guard, run_id, rx)
    }

    /// Find the registry row a parked delivery to `target` registered, if any.
    fn delivery_row(target: registry::WorkId) -> Option<registry::WorkSnapshot> {
        let needle = format!("steer → {target} ");
        registry::snapshot_all()
            .into_iter()
            .find(|snapshot| snapshot.name.starts_with(&needle))
    }

    async fn await_delivery_row(target: registry::WorkId) -> registry::WorkSnapshot {
        for _ in 0..400 {
            if let Some(row) = delivery_row(target) {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("test: a parked delivery to {target} never appeared in the work registry");
    }

    /// Core evidence for §7.2: a cross-entry delivery that parks on backpressure
    /// is **visible** for exactly as long as it parks.
    ///
    /// This is the blind spot the registration exists to close. Nothing in this
    /// runtime expires on a clock, so a delivery wedged behind a full steering
    /// queue would otherwise be invisible everywhere at once: the target looks
    /// idle, the caller is a different process, and no timeout will ever end it.
    ///
    /// MUTATION GUARD: delete the `register_tool_call` in `post_task_message`
    /// and this test fails — the row never appears.
    #[tokio::test]
    async fn a_parked_delivery_is_visible_while_it_parks_and_gone_once_it_lands() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let (guard, run_id, mut rx) = wedged_target().await;
        let target = guard.id();

        let (endpoint, server) = serve(test_app_state(AutonomyLevel::Full, workspace.path())).await;
        let endpoint = Arc::new(endpoint);
        let call = tokio::spawn({
            let endpoint = Arc::clone(&endpoint);
            let run_id = run_id.clone();
            async move { tasks_cli::request_message(&endpoint, &run_id, "you will have to wait").await }
        });

        let row = await_delivery_row(target).await;
        assert_eq!(
            row.run_id.as_deref(),
            Some(run_id.as_str()),
            "the delivery row must carry the target's run id so an operator can tell what it is waiting on"
        );
        assert_eq!(row.kind, registry::WorkKind::ToolCall);

        // Drain one slot: the parked send now completes.
        assert_eq!(rx.recv().await.as_deref(), Some("already queued"));
        let report = call
            .await
            .expect("test: the delivery task must not panic")
            .expect("test: the delivery must succeed once the queue drains");
        assert_eq!(report.outcome, "delivered");
        assert_eq!(rx.recv().await.as_deref(), Some("you will have to wait"));

        // And it stops being visible the moment it settles: the row is a report
        // of work in flight, not a log.
        for _ in 0..400 {
            if delivery_row(target).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            delivery_row(target).is_none(),
            "a settled delivery must not linger in the listing"
        );

        drop(guard);
        server.abort();
    }

    /// Being seen is only half a backstop: the visible row must also be the
    /// thing an operator can end. Killing the delivery row releases the caller
    /// without the message ever reaching the target.
    #[tokio::test]
    async fn a_parked_delivery_can_be_killed_by_the_operator() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let (guard, run_id, mut rx) = wedged_target().await;
        let target = guard.id();

        let (endpoint, server) = serve(test_app_state(AutonomyLevel::Full, workspace.path())).await;
        let endpoint = Arc::new(endpoint);
        let call = tokio::spawn({
            let endpoint = Arc::clone(&endpoint);
            let run_id = run_id.clone();
            async move { tasks_cli::request_message(&endpoint, &run_id, "abandon me").await }
        });

        let row = await_delivery_row(target).await;
        let report = tasks_cli::request_kill(&endpoint, &row.id.to_string(), false)
            .await
            .expect("test: the delivery row must be killable like any other work item");
        assert!(
            report.targets.iter().any(|item| item.id == row.id.to_string()),
            "the kill must have named the delivery row: {report:?}"
        );

        let error = call
            .await
            .expect("test: the delivery task must not panic")
            .expect_err("test: a killed delivery must report failure, never a silent success");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("409") && rendered.contains("ended by an operator"),
            "the caller must be told the delivery was ended, got: {rendered}"
        );

        // The queue still holds only what was there before: a killed delivery
        // delivered nothing.
        assert_eq!(rx.recv().await.as_deref(), Some("already queued"));

        drop(guard);
        server.abort();
    }

    /// §7.1 end to end: a `spawn_batch` fan-out is visible as a batch through
    /// the very listing `prx tasks list` renders, and the batch id is a kill
    /// address that ends every member.
    #[tokio::test]
    async fn a_batch_is_listed_as_one_unit_and_killable_by_its_batch_id() {
        let workspace = tempfile::TempDir::new().expect("test: tempdir");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (tool, _) = spawn_long_running_sub_agent(workspace.path(), &gate).await;

        let spawned = tool
            .execute(serde_json::json!({
                "action": "spawn_batch",
                "tasks": ["alpha", "beta", "gamma"],
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "sender": "operator",
                    "channel": "silent",
                    "chat_type": "direct",
                    "chat_id": "chat-1"
                }
            }))
            .await
            .expect("test: spawn_batch must be accepted");
        let payload: serde_json::Value =
            serde_json::from_str(&spawned.output).expect("test: spawn_batch output must be JSON");
        let batch_id = payload
            .get("batch_id")
            .and_then(serde_json::Value::as_str)
            .expect("test: spawn_batch must return a batch id")
            .to_string();

        let (endpoint, server) = serve(test_app_state(AutonomyLevel::Full, workspace.path())).await;

        // Wait for all three rows to be registered, then read the listing the
        // CLI reads.
        let mut listing = tasks_cli::fetch_tasks(&endpoint).await.expect("test: listing");
        for _ in 0..400 {
            if listing
                .running
                .iter()
                .filter(|item| item.batch_id.as_deref() == Some(batch_id.as_str()))
                .count()
                == 3
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            listing = tasks_cli::fetch_tasks(&endpoint).await.expect("test: listing");
        }
        let members: Vec<_> = listing
            .running
            .iter()
            .filter(|item| item.batch_id.as_deref() == Some(batch_id.as_str()))
            .collect();
        assert_eq!(
            members.len(),
            3,
            "every batch member must report its batch in the operator listing: {:?}",
            listing.running
        );

        // And the CLI's grouping puts them under one heading.
        let groups = tasks_cli::group_by_batch(&listing.running);
        let batch_group = groups
            .iter()
            .find(|(id, _)| *id == Some(batch_id.as_str()))
            .expect("test: the batch must form a group of its own");
        assert_eq!(batch_group.1.len(), 3);

        // The batch id is a third kill address, and it reaches every member.
        let report = tasks_cli::request_kill(&endpoint, &batch_id, true)
            .await
            .expect("test: a batch id must be a valid kill address");
        assert_eq!(report.requested, batch_id);
        for member in &members {
            assert!(
                report.targets.iter().any(|target| target.id == member.id),
                "the batch kill must have reached {}: {report:?}",
                member.id
            );
        }

        gate.add_permits(64);
        server.abort();
    }
}
