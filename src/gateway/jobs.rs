//! Long-running gateway work, detached from the HTTP request that asked for it.
//!
//! PRX has no request timeout, so an agent turn started over HTTP may run for
//! minutes or hours. That makes the obvious shape — `await` the whole turn
//! inside the handler future — wrong, because the work then inherits the
//! *connection's* lifetime rather than its own: a closed browser tab, a proxy
//! hang-up, or a client-side abort drops the handler future and takes the turn
//! with it, halfway through whatever side effects it had already committed.
//!
//! Every long route therefore submits a job. The job runs in its own task,
//! registered in [`crate::runtime::registry`] so `prx tasks list` shows it and
//! `prx tasks kill <id>` ends it, and its result lands in this store. Callers
//! choose how to collect it:
//!
//! - **wait** (default): the handler awaits the job and answers exactly as it
//!   did before, so existing clients keep their synchronous contract — but the
//!   job outlives the handler, so a disconnect no longer destroys it.
//! - **async** (`?mode=async`, or `Prefer: respond-async`): the handler returns
//!   `202 Accepted` with the job id immediately, and the caller polls
//!   `GET /api/jobs/{id}` for status and result.
//!
//! Retention is bounded, but only over *finished* jobs: a running job is never
//! evicted, because evicting the record of live work would recreate the blind
//! spot this store exists to remove.

use crate::runtime::registry::{self, WorkId};
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How long a finished job's result stays queryable. A client that submits in
/// async mode and polls later must still find its answer; an hour covers a
/// reconnecting console without retaining results indefinitely.
const JOB_RETENTION: Duration = Duration::from_hours(1);

/// Ceiling on retained job records. Only *finished* records are ever evicted,
/// oldest first, so this bounds memory without bounding concurrency.
const MAX_JOB_RECORDS: usize = 512;

/// Job kind tags. Stable strings: operators and the console filter on them.
pub(super) const KIND_SESSION_MESSAGE: &str = "session_message";
pub(super) const KIND_WEBHOOK: &str = "webhook";
pub(super) const KIND_CHANNEL_WEBHOOK: &str = "channel_webhook";
pub(super) const KIND_MCP_TOOL_CALL: &str = "mcp_tool_call";

/// Lifecycle state of a submitted job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobPhase {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobPhase {
    /// Stable lowercase tag used by the HTTP API.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// The HTTP answer a job produces, held verbatim so that waiting and polling
/// callers observe the same status code and body.
#[derive(Debug, Clone)]
pub(super) struct JobOutput {
    pub status: StatusCode,
    pub body: serde_json::Value,
}

impl JobOutput {
    pub(super) const fn new(status: StatusCode, body: serde_json::Value) -> Self {
        Self { status, body }
    }
}

/// Terminal result of a job, as seen by a caller that waited for it.
#[derive(Debug)]
pub(super) enum JobOutcome {
    Completed(JobOutput),
    Failed(String),
    Cancelled,
}

impl JobOutcome {
    /// Render as the `(status, body)` pair the gateway handlers return.
    pub(super) fn into_parts(self) -> (StatusCode, Json<serde_json::Value>) {
        match self {
            Self::Completed(output) => (output.status, Json(output.body)),
            Self::Failed(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": message })),
            ),
            Self::Cancelled => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "This request was cancelled while running",
                    "status": "cancelled",
                })),
            ),
        }
    }
}

impl IntoResponse for JobOutcome {
    fn into_response(self) -> Response {
        self.into_parts().into_response()
    }
}

/// Handle returned by [`submit`]: enough to answer immediately (async mode) or
/// to wait for the result (wait mode).
#[derive(Debug)]
pub(super) struct JobHandle {
    id: Uuid,
    work_id: WorkId,
    kind: &'static str,
    phase: watch::Receiver<JobPhase>,
}

impl JobHandle {
    /// `202 Accepted` payload for async submission. Carries both identifier
    /// spaces: the job id for `GET /api/jobs/{id}`, and the registry id so an
    /// operator can go straight to `prx tasks kill <id>`.
    pub(super) fn accepted_body(&self) -> serde_json::Value {
        serde_json::json!({
            "status": "accepted",
            "job_id": self.id.to_string(),
            "work_id": self.work_id.to_string(),
            "kind": self.kind,
            "poll_url": format!("/api/jobs/{}", self.id),
            "cancel_url": format!("/api/jobs/{}/cancel", self.id),
        })
    }

    /// Block until the job reaches a terminal phase and return its result.
    ///
    /// Dropping this future does **not** stop the job: that is the whole point
    /// of the indirection.
    pub(super) async fn wait(mut self) -> JobOutcome {
        // A send error means the record was dropped, which can only happen after
        // the job finished; fall through to the store read either way.
        let _ = self.phase.wait_for(|phase| phase.is_terminal()).await;
        JOBS.take_outcome(self.id)
            .unwrap_or_else(|| JobOutcome::Failed("The job finished but its result is no longer available".to_string()))
    }
}

/// Public view of one job.
#[derive(Debug, Clone, Serialize)]
pub(super) struct JobSnapshot {
    pub job_id: String,
    pub kind: &'static str,
    pub label: String,
    /// Registry id, for `prx tasks list` / `prx tasks kill` correlation.
    pub work_id: String,
    pub status: &'static str,
    pub submitted_at_unix_ms: u64,
    pub elapsed_ms: u128,
    /// HTTP status the job produced, once it has one.
    pub http_status: Option<u16>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// One stored job. `phase_tx` stays alive as long as the record does, which is
/// what lets a waiter observe the transition to a terminal phase.
struct JobRecord {
    id: Uuid,
    kind: &'static str,
    label: Arc<str>,
    work_id: WorkId,
    submitted_at_unix_ms: u64,
    started: Instant,
    phase: JobPhase,
    finished_at: Option<Instant>,
    output: Option<JobOutput>,
    error: Option<String>,
    phase_tx: watch::Sender<JobPhase>,
}

impl JobRecord {
    fn snapshot(&self) -> JobSnapshot {
        let elapsed = self
            .finished_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started);
        JobSnapshot {
            job_id: self.id.to_string(),
            kind: self.kind,
            label: self.label.to_string(),
            work_id: self.work_id.to_string(),
            status: self.phase.as_str(),
            submitted_at_unix_ms: self.submitted_at_unix_ms,
            elapsed_ms: elapsed.as_millis(),
            http_status: self.output.as_ref().map(|output| output.status.as_u16()),
            result: self.output.as_ref().map(|output| output.body.clone()),
            error: self.error.clone(),
        }
    }
}

/// How a job ended, as reported by its supervising task.
enum Terminal {
    Succeeded(JobOutput),
    Failed(String),
    Cancelled,
}

#[derive(Default)]
struct JobStore {
    records: Mutex<HashMap<Uuid, JobRecord>>,
}

static JOBS: LazyLock<JobStore> = LazyLock::new(JobStore::default);

impl JobStore {
    fn insert(&self, record: JobRecord) {
        let mut records = self.records.lock();
        Self::prune(&mut records);
        records.insert(record.id, record);
    }

    /// Retire expired finished records, then, if still over the ceiling, the
    /// oldest finished ones. Running jobs are never touched.
    fn prune(records: &mut HashMap<Uuid, JobRecord>) {
        let now = Instant::now();
        records.retain(|_, record| {
            record
                .finished_at
                .is_none_or(|finished| now.saturating_duration_since(finished) < JOB_RETENTION)
        });
        while records.len() >= MAX_JOB_RECORDS {
            let Some(oldest) = records
                .values()
                .filter(|record| record.finished_at.is_some())
                .min_by_key(|record| record.started)
                .map(|record| record.id)
            else {
                // Everything left is still running; refuse to forget live work.
                break;
            };
            records.remove(&oldest);
        }
    }

    fn finish(&self, id: Uuid, terminal: Terminal) {
        let mut records = self.records.lock();
        let Some(record) = records.get_mut(&id) else {
            return;
        };
        record.finished_at = Some(Instant::now());
        record.phase = match terminal {
            Terminal::Succeeded(output) => {
                record.output = Some(output);
                JobPhase::Succeeded
            }
            Terminal::Failed(message) => {
                record.error = Some(message);
                JobPhase::Failed
            }
            Terminal::Cancelled => JobPhase::Cancelled,
        };
        // Waiters observe the terminal phase only after the result is stored.
        let _ = record.phase_tx.send(record.phase);
    }

    /// Terminal outcome of a job, for a caller that waited on it.
    fn take_outcome(&self, id: Uuid) -> Option<JobOutcome> {
        let records = self.records.lock();
        let record = records.get(&id)?;
        match record.phase {
            JobPhase::Running => None,
            JobPhase::Succeeded => record.output.clone().map(JobOutcome::Completed),
            JobPhase::Failed => {
                Some(JobOutcome::Failed(record.error.clone().unwrap_or_else(|| {
                    "The job failed without a reported reason".to_string()
                })))
            }
            JobPhase::Cancelled => Some(JobOutcome::Cancelled),
        }
    }
}

/// Submit `work` as a detached, registry-visible job.
///
/// `label` is what an operator sees in `prx tasks list`, so it should identify
/// the request (route plus session/tool), not just the route.
pub(super) fn submit<F>(kind: &'static str, label: impl Into<Arc<str>>, work: F) -> JobHandle
where
    F: Future<Output = Result<JobOutput, String>> + Send + 'static,
{
    let id = Uuid::new_v4();
    let label: Arc<str> = label.into();
    let job_ref = id.to_string();
    let cancel = CancellationToken::new();

    // Registration happens before the spawn so the work id is known
    // synchronously and can be handed back in the 202 body; the guard itself
    // moves into the task, so the row disappears exactly when the work does.
    let guard = registry::register_sub_agent(&label, &job_ref, registry::current_work_id(), Some(cancel.clone()));
    let work_id = guard.id();
    let (phase_tx, phase_rx) = watch::channel(JobPhase::Running);

    JOBS.insert(JobRecord {
        id,
        kind,
        label: Arc::clone(&label),
        work_id,
        submitted_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX)),
        started: Instant::now(),
        phase: JobPhase::Running,
        finished_at: None,
        output: None,
        error: None,
        phase_tx,
    });

    let inner = tokio::spawn(registry::scoped(guard, async move {
        tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = work => Some(result),
        }
    }));
    registry::attach_abort_handle(work_id, inner.abort_handle());

    tokio::spawn(async move {
        let terminal = match inner.await {
            Ok(Some(Ok(output))) => Terminal::Succeeded(output),
            Ok(Some(Err(message))) => Terminal::Failed(message),
            Ok(None) => Terminal::Cancelled,
            Err(error) if error.is_cancelled() => Terminal::Cancelled,
            Err(error) => Terminal::Failed(format!("The job task ended abnormally: {error}")),
        };
        JOBS.finish(id, terminal);
    });

    JobHandle {
        id,
        work_id,
        kind,
        phase: phase_rx,
    }
}

/// Every retained job, newest submission first.
pub(super) fn snapshot_all() -> Vec<JobSnapshot> {
    let mut snapshots = {
        let records = JOBS.records.lock();
        records.values().map(JobRecord::snapshot).collect::<Vec<_>>()
    };
    snapshots.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.submitted_at_unix_ms));
    snapshots
}

/// One job by id.
pub(super) fn snapshot(id: Uuid) -> Option<JobSnapshot> {
    let records = JOBS.records.lock();
    records.get(&id).map(JobRecord::snapshot)
}

/// Registry id of a job, so a caller holding only a job id can reach the kill
/// path without knowing how the two identifier spaces relate.
pub(super) fn work_id_of(id: Uuid) -> Option<WorkId> {
    let records = JOBS.records.lock();
    records.get(&id).map(|record| record.work_id)
}

/// Whether the caller asked for async submission.
///
/// Accepts `?mode=async` and the RFC 7240 `Prefer: respond-async` header, so a
/// client that cannot alter the URL can still opt in.
pub(super) fn wants_async(query: Option<&str>, headers: &HeaderMap) -> bool {
    let mode = query.and_then(|query| {
        query.split('&').find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name.trim() == "mode").then_some(value)
        })
    });
    if mode.is_some_and(|value| value.trim().eq_ignore_ascii_case("async")) {
        return true;
    }
    headers
        .get("prefer")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("respond-async"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_output(marker: &str) -> JobOutput {
        JobOutput::new(StatusCode::OK, serde_json::json!({ "marker": marker }))
    }

    #[tokio::test]
    async fn submitted_job_result_is_observable_by_a_waiter() {
        let handle = super::submit(KIND_SESSION_MESSAGE, "test:wait", async { Ok(ok_output("done")) });
        let id = handle.id;
        let outcome = handle.wait().await;
        match outcome {
            JobOutcome::Completed(output) => {
                assert_eq!(output.status, StatusCode::OK);
                assert_eq!(
                    output.body.get("marker").and_then(serde_json::Value::as_str),
                    Some("done")
                );
            }
            other => panic!("expected completion, got {other:?}"),
        }
        let snapshot = snapshot(id).expect("record retained");
        assert_eq!(snapshot.status, "succeeded");
        assert_eq!(snapshot.http_status, Some(200));
    }

    #[tokio::test]
    async fn job_survives_the_submitter_dropping_its_handle() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = super::submit(KIND_WEBHOOK, "test:detached", async move {
            let _ = rx.await;
            let _ = done_tx.send(());
            Ok(ok_output("late"))
        });
        let id = handle.id;
        // The "client" walks away before the work finishes.
        drop(handle);
        let _ = tx.send(());
        done_rx.await.expect("job body ran to completion");

        for _ in 0..200u32 {
            if snapshot(id).is_some_and(|snapshot| snapshot.status == "succeeded") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("detached job never recorded its result");
    }

    #[tokio::test]
    async fn failed_job_reports_its_message() {
        let handle = super::submit(KIND_MCP_TOOL_CALL, "test:fail", async {
            Err("tool exploded".to_string())
        });
        match handle.wait().await {
            JobOutcome::Failed(message) => assert_eq!(message, "tool exploded"),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn killing_the_registry_row_cancels_the_job() {
        let handle = super::submit(KIND_SESSION_MESSAGE, "test:cancel", async {
            std::future::pending::<()>().await;
            Ok(ok_output("unreachable"))
        });
        let id = handle.id;
        let work_id = handle.work_id;
        assert_eq!(work_id_of(id), Some(work_id));

        let waiter = tokio::spawn(handle.wait());
        registry::kill(work_id, true).await;

        let outcome = waiter.await.expect("waiter completed");
        assert!(matches!(outcome, JobOutcome::Cancelled), "got {outcome:?}");
        assert_eq!(snapshot(id).map(|snapshot| snapshot.status), Some("cancelled"));
    }

    #[tokio::test]
    async fn running_job_is_visible_in_the_runtime_registry() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = super::submit(KIND_SESSION_MESSAGE, "test:visible-in-registry", async move {
            let _ = rx.await;
            Ok(ok_output("done"))
        });
        let work_id = handle.work_id;
        assert!(
            registry::snapshot(work_id).is_some_and(|snapshot| snapshot.name.as_ref() == "test:visible-in-registry"),
            "job should appear in `prx tasks list`"
        );
        let _ = tx.send(());
        let _ = handle.wait().await;
    }

    #[test]
    fn async_mode_is_opt_in() {
        let mut headers = HeaderMap::new();
        assert!(!wants_async(None, &headers));
        assert!(!wants_async(Some("mode=wait"), &headers));
        assert!(!wants_async(Some("other=async"), &headers));
        assert!(wants_async(Some("mode=async"), &headers));
        assert!(wants_async(Some("limit=1&mode=ASYNC"), &headers));

        headers.insert("prefer", axum::http::HeaderValue::from_static("respond-async"));
        assert!(wants_async(None, &headers));
    }

    #[test]
    fn pruning_never_evicts_a_running_job() {
        let mut records = HashMap::new();
        for index in 0..(MAX_JOB_RECORDS + 8) {
            let id = Uuid::new_v4();
            let (phase_tx, _phase_rx) = watch::channel(JobPhase::Running);
            records.insert(
                id,
                JobRecord {
                    id,
                    kind: KIND_SESSION_MESSAGE,
                    label: Arc::from(format!("running-{index}").as_str()),
                    work_id: WorkId::parse("w1").expect("valid id"),
                    submitted_at_unix_ms: 0,
                    started: Instant::now(),
                    phase: JobPhase::Running,
                    finished_at: None,
                    output: None,
                    error: None,
                    phase_tx,
                },
            );
        }
        let before = records.len();
        JobStore::prune(&mut records);
        assert_eq!(records.len(), before, "running jobs must not be evicted");
    }
}
