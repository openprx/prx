//! Background sub-agent approval suspension (NeedsInput).
//!
//! When a chat `/bg` background sub-agent hits a tool call that the side-effect
//! gate would otherwise auto-fail (a Medium/High risk operation in supervised
//! mode with no runtime grant), this module lets it **suspend** instead of
//! failing: the run flips to [`SubAgentStatus::AwaitingInput`], a
//! [`SessionEvent::NeedsInput`] is surfaced to the chat main loop, and the loop
//! awaits an operator decision fed by `/approve <N>` / `/deny <N>` (or a timeout
//! safe-default of deny).
//!
//! ## Why this is chat-only
//!
//! The suspend resolver is built **only** on the chat `/bg` path (which owns a
//! [`SessionEventSink`](super::event::SessionEventSink) and a
//! [`PendingApprovals`] registry). Channels / gateway background spawns thread a
//! `None` resolver, so their sub-agents keep the historical auto-fail-on-gate
//! semantics: no human is at the keyboard to approve, so suspending there would
//! produce a zombie. This invariant is enforced structurally — the factory is
//! only attached by `chat::run`.
//!
//! ## Cross-task wake-up
//!
//! The resolver runs **inside** the background sub-agent's spawned tokio task
//! (deep in `run_tool_call_loop`). The `/approve` handler runs on the **chat
//! main loop** task. They rendezvous through [`PendingApprovals`]: the resolver
//! registers a [`tokio::sync::oneshot::Sender`] keyed by run id and awaits the
//! receiver; `/approve` / `/deny` looks the run id up and sends the decision.
//! All synchronisation is `tokio::sync` (the registry mutex is never held across
//! an `.await`), satisfying the async iron law.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use super::event::SessionEvent;
use super::id::SessionId;
use crate::agent::loop_::{ApprovalDecision, ApprovalResolver, SpawnApprovalResolverFactory};
use crate::approval::ApprovalRequest;
use crate::tools::sessions_spawn::{SubAgentRun, SubAgentStatus};

/// Default approval timeout for a suspended background sub-agent. If no operator
/// decision arrives within this window the resolver wakes itself with the safe
/// default (deny) so a forgotten suspension cannot pin a concurrency slot
/// forever.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Shared registry of pending approval waiters, keyed by run id.
///
/// The resolver (background task) inserts a oneshot sender before it suspends;
/// the `/approve` / `/deny` handler (chat main loop) removes and fires it. Both
/// sides only ever take the `parking_lot` lock for a non-`await` critical
/// section, so it never blocks the runtime.
#[derive(Clone, Default)]
pub struct PendingApprovals {
    inner: Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>,
}

impl PendingApprovals {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter for `run_id`, returning the receiver the resolver awaits.
    ///
    /// If a stale waiter already exists for this run id it is dropped (its
    /// receiver resolves to `Err`, which the prior resolver treats as a timeout
    /// / safe-deny). Returns the fresh receiver.
    fn register(&self, run_id: &str) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        // Replacing any previous sender drops it; the old waiter (if any) sees a
        // closed channel and falls back to its safe default.
        self.inner.lock().insert(run_id.to_string(), tx);
        rx
    }

    /// Remove the waiter for `run_id` (resolver cleanup after it wakes).
    ///
    /// Safe to call from a `Drop` impl: it only takes the `parking_lot` lock for
    /// a non-`await` critical section, never panics, and is idempotent (a no-op
    /// if the entry was already removed by `/approve` / `/deny` / re-register).
    /// Dropping the removed sender (if present) closes the channel, so any waiter
    /// still parked on the receiver wakes with the safe-deny fallback.
    pub fn remove(&self, run_id: &str) {
        self.inner.lock().remove(run_id);
    }

    /// Deliver a decision to the run's pending waiter.
    ///
    /// Returns `true` if a waiter was present and the decision was delivered,
    /// `false` if there was no pending approval for this run (already resolved,
    /// timed out, killed, or never suspended).
    #[must_use]
    pub fn resolve(&self, run_id: &str, decision: ApprovalDecision) -> bool {
        let Some(tx) = self.inner.lock().remove(run_id) else {
            return false;
        };
        // `send` only fails if the resolver's receiver was already dropped (the
        // task ended / timed out between our lock release and send). Treat that
        // as "no live waiter".
        tx.send(decision).is_ok()
    }

    /// Whether a run currently has a pending approval waiter.
    #[must_use]
    pub fn is_pending(&self, run_id: &str) -> bool {
        self.inner.lock().contains_key(run_id)
    }
}

/// Build a [`SpawnApprovalResolverFactory`] for the chat `/bg` path.
///
/// The factory mints a per-run [`SuspendingApprovalResolver`] bound to the run's
/// [`SessionId`], the chat event channel, the shared run registry, and the
/// pending-approval registry.
#[must_use]
pub fn build_resolver_factory(
    event_tx: mpsc::Sender<SessionEvent>,
    active_runs: Arc<tokio::sync::RwLock<Vec<SubAgentRun>>>,
    pending: PendingApprovals,
    timeout: Duration,
) -> SpawnApprovalResolverFactory {
    SpawnApprovalResolverFactory::new(move |run_id: &str| {
        let resolver = SuspendingApprovalResolver {
            run_id: run_id.to_string(),
            session_id: SessionId::from_run_id(run_id),
            event_tx: event_tx.clone(),
            active_runs: Arc::clone(&active_runs),
            pending: pending.clone(),
            timeout,
        };
        Arc::new(resolver) as Arc<dyn ApprovalResolver>
    })
}

/// Per-run resolver that suspends the background sub-agent on the approval gate.
struct SuspendingApprovalResolver {
    run_id: String,
    session_id: SessionId,
    event_tx: mpsc::Sender<SessionEvent>,
    active_runs: Arc<tokio::sync::RwLock<Vec<SubAgentRun>>>,
    pending: PendingApprovals,
    timeout: Duration,
}

impl SuspendingApprovalResolver {
    /// Set the run's registry status to [`SubAgentStatus::AwaitingInput`].
    async fn mark_awaiting(&self, prompt: &str) {
        let mut runs = self.active_runs.write().await;
        if let Some(run) = runs.iter_mut().find(|r| r.id == self.run_id) {
            run.status = SubAgentStatus::AwaitingInput {
                prompt: prompt.to_string(),
            };
        }
    }

    /// Restore the run's registry status to [`SubAgentStatus::Running`] **only**
    /// if it is still `AwaitingInput` (do not clobber a concurrent kill / failure
    /// that already moved it to a terminal state).
    async fn mark_running(&self) {
        let mut runs = self.active_runs.write().await;
        if let Some(run) = runs.iter_mut().find(|r| r.id == self.run_id) {
            if matches!(run.status, SubAgentStatus::AwaitingInput { .. }) {
                run.status = SubAgentStatus::Running;
            }
        }
    }
}

/// RAII cleanup guard for a suspended approval.
///
/// Created right after the resolver registers its pending waiter. On `Drop` —
/// whether the resolver returns normally, its `await` is cancelled by the loop's
/// cancellation token, or the whole spawned task is `abort()`ed / dropped — it:
///
/// 1. removes the run's entry from [`PendingApprovals`] (drops the sender,
///    closing the channel — no stale waiter is ever left in the registry), and
/// 2. best-effort restores the run's registry status from `AwaitingInput` back
///    to `Running` so no zombie "needs-input" run is left behind, and
/// 3. best-effort emits a [`SessionEvent::Resumed`] so the suspend banner clears.
///
/// Step 1 is the leak fix (`PendingApprovals` can never retain a stale sender).
/// Steps 2–3 are best-effort because `Drop` cannot `.await`: the status restore
/// uses `try_write()` on the async registry lock (skipped if contended — a
/// concurrent kill/teardown is already mutating it to a terminal state, which we
/// must not clobber), and the event send uses the non-blocking `try_send`.
struct ApprovalCleanupGuard {
    run_id: String,
    session_id: SessionId,
    pending: PendingApprovals,
    active_runs: Arc<tokio::sync::RwLock<Vec<SubAgentRun>>>,
    event_tx: mpsc::Sender<SessionEvent>,
    /// Set to `true` once the resolver has performed its own async cleanup on the
    /// normal decision path, so `Drop` only needs to guarantee the pending entry
    /// is gone (the status / event were already handled with full `.await`).
    handled: bool,
}

impl Drop for ApprovalCleanupGuard {
    fn drop(&mut self) {
        // (1) Always clear the pending entry — the core leak fix. Idempotent.
        self.pending.remove(&self.run_id);
        if self.handled {
            // Normal path already restored status + emitted Resumed with full
            // `.await`; nothing more to do.
            return;
        }
        // (2) Best-effort status restore without awaiting. Only downgrade
        //     `AwaitingInput` -> `Running`; never clobber a terminal state set by
        //     a concurrent kill/failure. If the lock is contended right now,
        //     skip: whoever holds it is mutating the run already.
        //
        //     NOTE: this is a *fallback* only. `Drop` cannot `.await`, so this
        //     `try_write` is skipped under contention — which previously left a
        //     zombie `AwaitingInput` run that was in fact running again. The
        //     authoritative, contention-proof restore now lives on the async
        //     cancel-and-resume path (`run_sub_agent_task::restore_running`,
        //     `tools/sessions_spawn.rs`), which always re-runs after the resolver
        //     future is dropped on cancel. This `try_write` is retained as a
        //     harmless, idempotent best-effort for any path that wakes the banner
        //     before that async restore runs; both only do `AwaitingInput` ->
        //     `Running`, so they can never conflict or double-apply.
        if let Ok(mut runs) = self.active_runs.try_write() {
            if let Some(run) = runs.iter_mut().find(|r| r.id == self.run_id) {
                if matches!(run.status, SubAgentStatus::AwaitingInput { .. }) {
                    run.status = SubAgentStatus::Running;
                }
            }
        }
        // (3) Best-effort resume banner clear.
        let _ = self.event_tx.try_send(SessionEvent::Resumed {
            id: self.session_id.clone(),
        });
    }
}

/// Build a short, human-readable prompt from an approval request: the tool name
/// plus a compact, truncated argument digest.
fn summarize_request(request: &ApprovalRequest) -> String {
    const MAX_ARGS: usize = 120;
    let args = request.arguments.to_string();
    let args_digest: String = if args.chars().count() > MAX_ARGS {
        let head: String = args.chars().take(MAX_ARGS).collect();
        format!("{head}…")
    } else {
        args
    };
    if args_digest.is_empty() || args_digest == "null" {
        request.tool_name.clone()
    } else {
        format!("{}({args_digest})", request.tool_name)
    }
}

#[async_trait::async_trait]
impl ApprovalResolver for SuspendingApprovalResolver {
    async fn resolve(&self, request: &ApprovalRequest, _channel: &str) -> ApprovalDecision {
        let prompt = summarize_request(request);

        // 1. Register the waiter BEFORE announcing, so an immediate `/approve`
        //    after the NeedsInput banner always finds a live waiter.
        let rx = self.pending.register(&self.run_id);

        // 1b. Install the RAII cleanup guard immediately. From here on, no early
        //     return / cancellation / task-abort can leak the pending entry or
        //     leave a zombie `AwaitingInput` run: the guard's `Drop` clears them.
        //     Crucially, if the surrounding `resolve()` future is *dropped* (the
        //     loop `select!`s it against its cancellation token, or the spawned
        //     task is `abort()`ed mid-await), this guard still runs.
        let mut guard = ApprovalCleanupGuard {
            run_id: self.run_id.clone(),
            session_id: self.session_id.clone(),
            pending: self.pending.clone(),
            active_runs: Arc::clone(&self.active_runs),
            event_tx: self.event_tx.clone(),
            handled: false,
        };

        // 2. Flip registry status -> AwaitingInput (drives the `❓` glyph + the
        //    status-line `needs-input` counter via `project_status`).
        self.mark_awaiting(&prompt).await;

        // 3. Surface a NeedsInput event to the chat main loop (non-intrusive
        //    banner + status refresh). The registry status already reflects
        //    the suspension; if the bounded UI channel is full, log the missed
        //    banner instead of discarding it invisibly.
        if let Err(error) = self.event_tx.try_send(SessionEvent::NeedsInput {
            id: self.session_id.clone(),
            prompt: prompt.clone(),
        }) {
            tracing::warn!(
                run_id = %self.run_id,
                session_id = %self.session_id,
                error = %error,
                "failed to enqueue NeedsInput banner"
            );
        }

        tracing::info!(run_id = %self.run_id, tool = %request.tool_name, "background sub-agent suspended awaiting approval");

        // 4. Await the operator decision, bounded by the approval timeout. A
        //    timeout or a dropped sender (kill/teardown) is treated as a safe
        //    deny so the run can never hang forever holding a concurrency slot.
        //    Cancellation (steer / send / kill) is handled one level up by the
        //    loop, which `select!`s this whole future against its cancellation
        //    token and drops it on cancel — the guard above performs cleanup.
        let decision = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_recv_closed)) => {
                tracing::warn!(run_id = %self.run_id, "approval waiter channel closed; defaulting to deny");
                ApprovalDecision::Deny
            }
            Err(_elapsed) => {
                tracing::warn!(run_id = %self.run_id, timeout_secs = self.timeout.as_secs(), "approval timed out; defaulting to deny");
                ApprovalDecision::Deny
            }
        };

        // 5. Normal-path cleanup with full `.await`: remove the registry entry
        //    (no-op if `/approve` already took it), restore Running status, and
        //    signal resume so the suspend banner clears. Mark the guard handled
        //    so its `Drop` only needs to guarantee the pending entry is gone.
        self.pending.remove(&self.run_id);
        self.mark_running().await;
        let _ = self.event_tx.try_send(SessionEvent::Resumed {
            id: self.session_id.clone(),
        });
        guard.handled = true;

        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::RwLock;

    /// Read the status of the first (and, in these tests, only) run without
    /// index-panicking (satisfies `clippy::indexing_slicing`).
    async fn first_status(runs: &Arc<RwLock<Vec<SubAgentRun>>>) -> SubAgentStatus {
        runs.read()
            .await
            .first()
            .map(|r| r.status.clone())
            .expect("test: at least one run present")
    }

    fn make_run(id: &str) -> SubAgentRun {
        SubAgentRun {
            id: id.to_string(),
            task: "t".into(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            status: SubAgentStatus::Running,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            process_control: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "s".into(),
            spawn_depth: 0,
            token_usage_records: Vec::new(),
        }
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            tool_name: "shell".into(),
            arguments: json!({"command": "rm -rf /tmp/x"}),
        }
    }

    fn resolver(
        run_id: &str,
        runs: Arc<RwLock<Vec<SubAgentRun>>>,
        pending: PendingApprovals,
        timeout: Duration,
    ) -> (SuspendingApprovalResolver, mpsc::Receiver<SessionEvent>) {
        let (tx, rx) = mpsc::channel(16);
        let r = SuspendingApprovalResolver {
            run_id: run_id.to_string(),
            session_id: SessionId::from_run_id(run_id),
            event_tx: tx,
            active_runs: runs,
            pending,
            timeout,
        };
        (r, rx)
    }

    #[tokio::test]
    async fn approve_grant_wakes_resolver_and_clears_status() {
        let runs = Arc::new(RwLock::new(vec![make_run("r1")]));
        let pending = PendingApprovals::new();
        let (r, mut events) = resolver("r1", Arc::clone(&runs), pending.clone(), Duration::from_secs(30));

        let pending_for_task = pending.clone();
        let req = request();
        let handle = tokio::spawn(async move { r.resolve(&req, "sessions_spawn").await });

        // Wait until the resolver has registered + emitted NeedsInput.
        let evt = events.recv().await.expect("test: needs-input event");
        assert!(matches!(evt, SessionEvent::NeedsInput { .. }));
        assert!(matches!(
            first_status(&runs).await,
            SubAgentStatus::AwaitingInput { .. }
        ));

        // Operator approves with a grant.
        assert!(pending_for_task.resolve("r1", ApprovalDecision::Grant));

        let decision = handle.await.expect("test: join");
        assert!(matches!(decision, ApprovalDecision::Grant));

        // Status restored + resumed event emitted.
        assert!(matches!(first_status(&runs).await, SubAgentStatus::Running));
        let resumed = events.recv().await.expect("test: resumed event");
        assert!(matches!(resumed, SessionEvent::Resumed { .. }));
        assert!(!pending_for_task.is_pending("r1"));
    }

    #[tokio::test]
    async fn deny_wakes_resolver_with_deny() {
        let runs = Arc::new(RwLock::new(vec![make_run("r2")]));
        let pending = PendingApprovals::new();
        let (r, mut events) = resolver("r2", Arc::clone(&runs), pending.clone(), Duration::from_secs(30));
        let req = request();
        let handle = tokio::spawn(async move { r.resolve(&req, "sessions_spawn").await });

        let _ = events.recv().await.expect("test: needs-input");
        assert!(pending.resolve("r2", ApprovalDecision::Deny));
        let decision = handle.await.expect("test: join");
        assert!(matches!(decision, ApprovalDecision::Deny));
    }

    #[tokio::test]
    async fn timeout_defaults_to_deny_and_releases_slot() {
        let runs = Arc::new(RwLock::new(vec![make_run("r3")]));
        let pending = PendingApprovals::new();
        // Very short timeout; no operator decision ever arrives.
        let (r, mut events) = resolver("r3", Arc::clone(&runs), pending.clone(), Duration::from_millis(50));
        let req = request();
        let handle = tokio::spawn(async move { r.resolve(&req, "sessions_spawn").await });

        let _ = events.recv().await.expect("test: needs-input");
        let decision = handle.await.expect("test: join");
        assert!(matches!(decision, ApprovalDecision::Deny));
        // No leaked waiter; status restored.
        assert!(!pending.is_pending("r3"));
        assert!(matches!(first_status(&runs).await, SubAgentStatus::Running));
    }

    #[tokio::test]
    async fn cancellation_drops_resolver_and_clears_pending_without_timeout() {
        // Fix #1+#2: when the loop races the (suspended) resolver future against
        // its cancellation token and cancels, dropping the future must run the
        // RAII guard — clearing the pending waiter and restoring Running status
        // PROMPTLY (not after the 300s timeout). We use a long timeout so a leak
        // would visibly hang the test rather than self-heal via timeout.
        use tokio_util::sync::CancellationToken;

        let runs = Arc::new(RwLock::new(vec![make_run("rc")]));
        let pending = PendingApprovals::new();
        let (r, mut events) = resolver(
            "rc",
            Arc::clone(&runs),
            pending.clone(),
            Duration::from_secs(300), // long: only cancellation can wake us
        );
        let req = request();
        let token = CancellationToken::new();

        // Mirror the loop call site: select! resolver future vs cancellation.
        let token_for_task = token.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                biased;
                () = token_for_task.cancelled() => None,
                decision = r.resolve(&req, "sessions_spawn") => Some(decision),
            }
        });

        // Wait until suspended (NeedsInput emitted, pending registered).
        let evt = events.recv().await.expect("test: needs-input");
        assert!(matches!(evt, SessionEvent::NeedsInput { .. }));
        assert!(pending.is_pending("rc"));
        assert!(matches!(
            first_status(&runs).await,
            SubAgentStatus::AwaitingInput { .. }
        ));

        // Cancel — the resolver future is dropped; its guard must clean up.
        token.cancel();
        let out = handle.await.expect("test: join");
        assert!(out.is_none(), "cancelled select! yields None");

        // No leaked waiter and status restored — promptly, well under the 300s
        // timeout (the test would hang here if cleanup depended on the timeout).
        assert!(!pending.is_pending("rc"));
        assert!(matches!(first_status(&runs).await, SubAgentStatus::Running));
    }

    #[tokio::test]
    async fn abort_runs_drop_guard_and_clears_pending() {
        // Fix #2: kill -> `abort_handle.abort()` drops the spawned task while it
        // is parked on `resolve()`. The RAII guard must still fire, leaving no
        // stale sender in `PendingApprovals` (the leak this guards against).
        let runs = Arc::new(RwLock::new(vec![make_run("ra")]));
        let pending = PendingApprovals::new();
        let (r, mut events) = resolver("ra", Arc::clone(&runs), pending.clone(), Duration::from_secs(300));
        let req = request();
        let handle = tokio::spawn(async move { r.resolve(&req, "sessions_spawn").await });

        // Wait until suspended.
        let _ = events.recv().await.expect("test: needs-input");
        assert!(pending.is_pending("ra"));

        // Abort the task (kill path). Joining an aborted task yields a JoinError.
        handle.abort();
        let join = handle.await;
        assert!(join.is_err(), "aborted task should not complete normally");

        // The guard's Drop cleared the pending entry — no leaked sender.
        assert!(!pending.is_pending("ra"));
    }

    #[test]
    fn resolve_unknown_run_is_false() {
        let pending = PendingApprovals::new();
        assert!(!pending.resolve("nope", ApprovalDecision::Allow));
    }

    #[tokio::test]
    async fn register_then_resolve_delivers_decision() {
        // The resolver-side registration must hand the decision to a live waiter.
        let pending = PendingApprovals::new();
        let mut rx = pending.register("r-rt");
        assert!(pending.is_pending("r-rt"));
        assert!(pending.resolve("r-rt", ApprovalDecision::Grant));
        let got = rx.try_recv().expect("test: decision delivered");
        assert!(matches!(got, ApprovalDecision::Grant));
        // Consumed: no longer pending and a second resolve is a no-op.
        assert!(!pending.is_pending("r-rt"));
        assert!(!pending.resolve("r-rt", ApprovalDecision::Deny));
    }

    #[tokio::test]
    async fn re_register_drops_stale_waiter_to_safe_deny() {
        // A second suspension for the same run id replaces the prior waiter; the
        // old receiver must observe a closed channel (resolver falls back to deny).
        let pending = PendingApprovals::new();
        let mut stale = pending.register("r-dup");
        let _fresh = pending.register("r-dup");
        // The stale sender was dropped on re-register -> Err on the old receiver.
        assert!(stale.try_recv().is_err());
        // The fresh waiter is the one that resolves.
        assert!(pending.resolve("r-dup", ApprovalDecision::Allow));
    }

    #[test]
    fn summarize_truncates_long_args() {
        let req = ApprovalRequest {
            tool_name: "http_request".into(),
            arguments: json!({"url": "x".repeat(500)}),
        };
        let s = summarize_request(&req);
        assert!(s.starts_with("http_request("));
        // The argument digest is truncated with an ellipsis, then wrapped in the
        // `tool_name(args)` form, so the ellipsis precedes the closing paren.
        assert!(s.contains('…'));
        assert!(s.ends_with(')'));
        // Truncated well below the raw 500-char argument length.
        assert!(s.chars().count() < 200);
    }
}
