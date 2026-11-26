use super::*;
use crate::runtime::shell_process::spawn_managed_shell_child;
use std::io::Write;
use std::process::Stdio;
use tracing_subscriber::fmt::writer::MakeWriter;

/// Collects formatted `tracing` output so a test can assert that a log line
/// was actually emitted rather than assuming it was.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<parking_lot::Mutex<Vec<u8>>>);

impl CapturedLogs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }
}

impl Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Unique run id per test: the registry is process-wide and tests run
/// concurrently, so an address must not be able to collide with another
/// test's rows.
fn unique_run_id(label: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("run-{label}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[tokio::test]
async fn a_run_is_addressable_by_run_id_and_by_work_id_alike() {
    let run_id = unique_run_id("both-addresses");
    let guard = register_sub_agent("addressable", &run_id, None, None, None);
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    attach_steer_sender(guard.id(), tx);

    assert_eq!(resolve_address(&run_id), Some(guard.id()));
    assert_eq!(resolve_address(&guard.id().to_string()), Some(guard.id()));

    steer(guard.id(), "by run id".to_string()).await.expect("test: deliver");
    assert_eq!(rx.recv().await.as_deref(), Some("by run id"));
}

#[tokio::test]
async fn a_shared_run_id_resolves_to_the_lineage_root() {
    let run_id = unique_run_id("shared");
    let root = register_sub_agent("root run", &run_id, None, None, None);
    // A tool call inside the run carries the same correlating run id.
    let _child = register_tool_call("shell", Some(&run_id), None);

    assert_eq!(
        resolve_address(&run_id),
        Some(root.id()),
        "the run itself must win over the tool calls it correlates with"
    );
}

#[test]
fn unknown_addresses_resolve_to_nothing_in_either_space() {
    assert_eq!(resolve_address("no-such-run-id-at-all"), None);
    assert_eq!(resolve_address("w999999999"), None);
    assert_eq!(resolve_address("   "), None);
}

#[tokio::test]
async fn items_without_a_steer_channel_are_refused_rather_than_dropped() {
    let run_id = unique_run_id("no-channel");
    let guard = register_turn("inbound turn", &run_id, None);
    assert_eq!(
        steer(guard.id(), "nothing listens".to_string()).await,
        Err(SteerRejection::NotSteerable)
    );
}

#[tokio::test]
async fn a_closed_receiver_is_reported_instead_of_silently_succeeding() {
    let run_id = unique_run_id("closed");
    let guard = register_sub_agent("ending run", &run_id, None, None, None);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    attach_steer_sender(guard.id(), tx);
    drop(rx);
    assert_eq!(
        steer(guard.id(), "too late".to_string()).await,
        Err(SteerRejection::ChannelClosed)
    );
}

#[tokio::test]
async fn a_finished_run_stops_being_addressable() {
    let run_id = unique_run_id("finished");
    let id = {
        let guard = register_sub_agent("short run", &run_id, None, None, None);
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        attach_steer_sender(guard.id(), tx);
        assert_eq!(resolve_address(&run_id), Some(guard.id()));
        guard.id()
    };
    assert_eq!(resolve_address(&run_id), None, "the row is released with the guard");
    assert_eq!(
        steer(id, "after the end".to_string()).await,
        Err(SteerRejection::ChannelClosed)
    );
}

/// The queue is bounded on purpose: a target that is not draining slows the
/// sender down instead of losing messages, and nothing here gives up on a
/// clock.
#[tokio::test]
async fn a_full_queue_parks_the_sender_instead_of_dropping_the_message() {
    let run_id = unique_run_id("backpressure");
    let guard = register_sub_agent("busy run", &run_id, None, None, None);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    attach_steer_sender(guard.id(), tx);

    steer(guard.id(), "fills the queue".to_string())
        .await
        .expect("test: first message fits");
    let mut parked = Box::pin(steer(guard.id(), "must wait".to_string()));
    assert!(
        futures::poll!(&mut parked).is_pending(),
        "a full queue must park the sender, not fail or drop"
    );
    assert_eq!(rx.recv().await.as_deref(), Some("fills the queue"));
    parked
        .await
        .expect("test: the parked send completes once space frees up");
    assert_eq!(rx.recv().await.as_deref(), Some("must wait"));
}

/// Run `f` with every `tracing` event on this thread captured.
fn capturing_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let value = tracing::subscriber::with_default(subscriber, f);
    let text = logs.text();
    (value, text)
}

/// Spawn a real child through the production chokepoint, so the test
/// exercises the same registration path the runtime uses.
fn spawn_registered(program: &str, args: &[&str]) -> crate::runtime::shell_process::ManagedShellChild {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    spawn_managed_shell_child(command).expect("test: spawn child")
}

/// Locate a registry row by its exact name.
fn find_by_name(name: &str) -> Option<WorkSnapshot> {
    snapshot_all().into_iter().find(|snapshot| &*snapshot.name == name)
}

/// Direct existence probe, independent of the module's own classifier, so a
/// broken classifier cannot make a kill test pass.
#[allow(unsafe_code)]
fn pid_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs an existence/permission check only; it
    // delivers no signal and dereferences no pointer.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Poll until `predicate` holds or the budget runs out, returning whether
/// it held. Used instead of a fixed sleep so the tests are not timing-flaky.
async fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    predicate()
}

#[test]
fn work_id_round_trips_both_textual_forms() {
    let id = WorkId(42);
    assert_eq!(id.to_string(), "w42");
    assert_eq!(WorkId::parse("w42"), Some(id));
    assert_eq!(WorkId::parse(" 42 "), Some(id));
    assert_eq!(WorkId::parse("nope"), None);
}

#[test]
fn guard_drop_removes_the_row() {
    let guard = register_tool_call("unit_tool", None, None);
    let id = guard.id();
    assert!(snapshot(id).is_some());
    drop(guard);
    assert!(snapshot(id).is_none());
}

#[test]
fn unreaped_process_row_survives_its_guard() {
    // pid 0 never resolves to a live process, so the row is reported as
    // abandoned rather than being classified against a real pid.
    let guard = register_process("stale", None, None);
    let id = guard.id();
    drop(guard);
    let snapshot = snapshot(id).expect("abandoned child keeps its ledger row");
    assert_eq!(snapshot.state, WorkState::Abandoned);
    // Clean up so other tests see a tidy registry.
    REGISTRY.remove(id.raw());
}

#[test]
fn reaped_process_row_is_retired() {
    let mut guard = register_process("done", None, None);
    let id = guard.id();
    guard.mark_reaped();
    drop(guard);
    assert!(snapshot(id).is_none());
}

/// A batch member's registry row carries the fan-out it belongs to, which
/// is the only thing that ties siblings together once the launching tool
/// call has returned and its row is gone.
#[test]
fn a_batch_member_reports_its_batch_in_the_snapshot() {
    let run_id = format!("run-{}", uuid::Uuid::new_v4());
    let batch_id = format!("batch-{}", uuid::Uuid::new_v4());
    let guard = register_sub_agent("batched run", &run_id, None, Some(&batch_id), None);
    let listed = snapshot(guard.id()).expect("test: the row must be listed");
    assert_eq!(listed.batch_id.as_deref(), Some(batch_id.as_str()));

    let solo = register_sub_agent("solo run", &format!("run-{}", uuid::Uuid::new_v4()), None, None, None);
    assert!(
        snapshot(solo.id()).and_then(|row| row.batch_id).is_none(),
        "a run outside any batch must not be labelled with one"
    );
}

/// Killing by batch id reaches every member *and* what each member started:
/// the per-member cascade is the existing one, applied once per root.
#[tokio::test]
async fn killing_a_batch_reaches_every_member_and_their_lineage() {
    let batch_id = format!("batch-{}", uuid::Uuid::new_v4());
    let first_token = CancellationToken::new();
    let second_token = CancellationToken::new();
    let first = register_sub_agent(
        "batch_member_one_marker",
        &format!("run-{}", uuid::Uuid::new_v4()),
        None,
        Some(&batch_id),
        Some(first_token.clone()),
    );
    let second = register_sub_agent(
        "batch_member_two_marker",
        &format!("run-{}", uuid::Uuid::new_v4()),
        None,
        Some(&batch_id),
        Some(second_token.clone()),
    );
    let first_id = first.id();
    let second_id = second.id();

    let child_token = CancellationToken::new();
    let child = register(
        WorkKind::ToolCall,
        Arc::from("batch_member_tool_marker"),
        Some(first_id),
        None,
        None,
        Some(child_token.clone()),
        None,
        None,
    );
    let child_id = child.id();

    let running_first = tokio::spawn(scoped(first, async move { first_token.cancelled().await }));
    let running_second = tokio::spawn(scoped(second, async move { second_token.cancelled().await }));
    let running_child = tokio::spawn(scoped(child, async move { child_token.cancelled().await }));

    let results = kill_batch(&batch_id, true).await;
    let ids: Vec<WorkId> = results.iter().map(|result| result.id).collect();
    assert!(ids.contains(&first_id), "batch kill missed a member: {ids:?}");
    assert!(ids.contains(&second_id), "batch kill missed a member: {ids:?}");
    assert!(
        ids.contains(&child_id),
        "batch kill must cascade into what a member started: {ids:?}"
    );

    let _ = running_first.await;
    let _ = running_second.await;
    let _ = running_child.await;
    assert!(find_by_name("batch_member_one_marker").is_none());
    assert!(find_by_name("batch_member_two_marker").is_none());
    assert!(find_by_name("batch_member_tool_marker").is_none());
}

/// An unknown batch matches nothing, which is what lets the HTTP layer tell
/// "no such batch" apart from "the batch is already finished" — both are a
/// 404, and neither is a silent success.
#[tokio::test]
async fn an_unknown_batch_kills_nothing() {
    let results = kill_batch(&format!("batch-{}", uuid::Uuid::new_v4()), true).await;
    assert!(results.is_empty(), "an unknown batch must match no work: {results:?}");
}

/// Cascade off narrows a batch kill to the members themselves, leaving what
/// they started running — the same narrowing `kill` offers for one item.
#[tokio::test]
async fn a_batch_kill_without_cascade_spares_what_the_members_started() {
    let batch_id = format!("batch-{}", uuid::Uuid::new_v4());
    let member_token = CancellationToken::new();
    let member = register_sub_agent(
        "narrow_batch_member_marker",
        &format!("run-{}", uuid::Uuid::new_v4()),
        None,
        Some(&batch_id),
        Some(member_token.clone()),
    );
    let member_id = member.id();
    let child = register(
        WorkKind::ToolCall,
        Arc::from("narrow_batch_tool_marker"),
        Some(member_id),
        None,
        None,
        Some(CancellationToken::new()),
        None,
        None,
    );

    let running = tokio::spawn(scoped(member, async move { member_token.cancelled().await }));
    let results = kill_batch(&batch_id, false).await;
    assert_eq!(results.len(), 1, "cascade off must target exactly the members");
    assert_eq!(results.first().map(|result| result.id), Some(member_id));

    let _ = running.await;
    assert!(
        find_by_name("narrow_batch_tool_marker").is_some(),
        "cascade off must leave the member's child registered"
    );
    drop(child);
}

#[test]
fn lineage_collects_transitive_descendants() {
    let agent = register_sub_agent("agent", "run-1", None, None, None);
    let tool = register(
        WorkKind::ToolCall,
        Arc::from("shell"),
        Some(agent.id()),
        None,
        None,
        None,
        None,
        None,
    );
    let process = register(
        WorkKind::Process,
        Arc::from("sleep"),
        Some(tool.id()),
        None,
        None,
        None,
        None,
        None,
    );
    let unrelated = register_tool_call("elsewhere", None, None);

    let all = REGISTRY.slots();
    let lineage = collect_lineage(&all, agent.id().raw());
    let ids: Vec<u64> = lineage.iter().map(|slot| slot.id).collect();
    assert!(ids.contains(&agent.id().raw()));
    assert!(ids.contains(&tool.id().raw()));
    assert!(ids.contains(&process.id().raw()));
    assert!(!ids.contains(&unrelated.id().raw()));
}

#[test]
fn long_task_threshold_resolution() {
    assert_eq!(long_task_warn_threshold(Some(0)), None);
    assert_eq!(long_task_warn_threshold(Some(30)), Some(Duration::from_secs(30)));
    assert_eq!(
        long_task_warn_threshold(None),
        Some(Duration::from_secs(DEFAULT_LONG_TASK_WARN_SECS))
    );
}

#[test]
fn warning_never_removes_or_kills_the_entry() {
    let guard = register_tool_call("slow_tool", None, Some(CancellationToken::new()));
    let id = guard.id();
    // Threshold of zero makes every entry immediately overdue.
    warn_long_running(Duration::from_secs(0));
    let after = snapshot(id).expect("warned entry must still be registered");
    assert_eq!(after.state, WorkState::Running);
    assert!(!guard.kill_requested());
    drop(guard);
}

// ── Behavioural tests against real OS processes ──────────────────────

#[tokio::test]
async fn kill_by_id_actually_ends_a_real_child_process() {
    // A distinctive duration keeps this child's registry row unambiguous
    // while the rest of the suite runs in parallel.
    let mut child = spawn_registered("sleep", &["300.11"]);
    let row = find_by_name("sleep 300.11").expect("test: child must be registered on spawn");
    let pid = row.pid.expect("test: registered child must carry its pid");
    assert_eq!(row.kind, WorkKind::Process);
    assert_eq!(row.state, WorkState::Running);
    assert!(pid_exists(pid), "test: child should be alive before the kill");

    let results = kill(row.id, false).await;
    assert_eq!(results.len(), 1);
    let outcome = results.first().map(|result| result.outcome);
    assert_eq!(
        outcome,
        Some(KillOutcome::Killed),
        "kill must confirm the child is gone"
    );

    // Independent verification: the process is no longer executing. It may
    // linger as a zombie until we reap it below, which is why the check is
    // on `/proc` state rather than on mere pid existence.
    assert_ne!(
        probe_process(pid),
        ProcessLiveness::Running,
        "the child must not still be running after the kill"
    );

    // Reap as the owner, then the pid must be gone outright.
    let _ = child.wait().await;
    child.mark_complete();
    assert!(
        eventually(|| !pid_exists(pid)).await,
        "the reaped child's pid must disappear entirely"
    );
}

#[tokio::test]
async fn kill_reaches_grandchildren_through_the_process_group() {
    // Whole-test safety net only: PID publication and OS reaping decide readiness.
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let long_dir = dir.path().join("process-group-".repeat(12));
        std::fs::create_dir(&long_dir).expect("test: long PID directory");
        let pid_file = long_dir.join("grandchild.pid");
        // `sh` forks `sleep` and then waits: killing only the direct child
        // would leave the grandchild running, which is exactly what `killpg`
        // is here to prevent.
        let script = format!(
            "sleep 300 & echo $! > '{0}.tmp' && mv '{0}.tmp' '{0}'; wait",
            pid_file.display()
        );
        let mut child = spawn_registered("sh", &["-c", &script]);

        let grandchild = loop {
            match std::fs::read_to_string(&pid_file) {
                Ok(line) => {
                    if line.ends_with('\n')
                        && let Ok(pid) = line.trim().parse::<u32>()
                        && pid > 0
                    {
                        break pid;
                    }
                }
                Err(error) => assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::NotFound,
                    "test: read published descendant pid: {error}"
                ),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(pid_exists(grandchild), "test: grandchild should be alive to start with");

        // Registry labels may end at a UTF-8 boundary before an ellipsis.
        // Compare the full command against the registered prefix, including its temp directory.
        let label = format!("sh -c {script}");
        let leader = snapshot_all()
            .into_iter()
            .find(|row| {
                *row.name == label
                    || row
                        .name
                        .strip_suffix('…')
                        .is_some_and(|prefix| label.starts_with(prefix))
            })
            .expect("test: leader must be registered");
        assert!(label.len() > 200, "test: command must reach the truncation branch");
        assert!(
            leader.name.ends_with('…') && leader.name.len() <= 203 && leader.name.len() < label.len(),
            "test: registered command must actually be truncated: {} bytes",
            leader.name.len()
        );
        assert!(leader.pgid.is_some(), "the leader must lead its own process group");

        let results = kill(leader.id, false).await;
        assert_eq!(results.first().map(|result| result.outcome), Some(KillOutcome::Killed));

        child.wait().await.expect("test: reap killed group leader");

        // The leader's wait cannot reap its child: init owns the grandchild now.
        // Observe that independent OS signal under the same whole-test safety net.
        while pid_exists(grandchild) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        child.mark_complete();
    })
    .await
    .expect("test: process-group kill whole-test safety net expired");
}

#[tokio::test]
async fn panicking_task_leaves_no_ghost_entry() {
    let handle = tokio::spawn(scoped(register_tool_call("panicky_tool_marker", None, None), async {
        panic!("test: deliberate panic inside a registered tool call");
    }));
    assert!(handle.await.is_err(), "test: the task must actually have panicked");
    assert!(
        find_by_name("panicky_tool_marker").is_none(),
        "a panicking tool call must not leave a registry entry behind"
    );
}

#[tokio::test]
async fn dropped_future_leaves_no_ghost_entry() {
    let guard = register_tool_call("cancelled_tool_marker", None, None);
    let id = guard.id();
    {
        let running = scoped(guard, std::future::pending::<()>());
        tokio::pin!(running);
        // Poll once so the future is genuinely in flight and owns the guard.
        let polled = tokio::time::timeout(Duration::from_millis(20), &mut running).await;
        assert!(polled.is_err(), "test: the future must still be pending");
        assert!(snapshot(id).is_some(), "test: entry must exist while the future lives");
    }
    assert!(
        find_by_name("cancelled_tool_marker").is_none(),
        "dropping the future must deregister the work item"
    );
    assert!(snapshot(id).is_none());
}

#[tokio::test]
async fn cascade_kill_takes_down_the_agent_lineage() {
    let agent_token = CancellationToken::new();
    let agent = register_sub_agent(
        "cascade_agent_marker",
        "run-cascade",
        None,
        None,
        Some(agent_token.clone()),
    );
    let agent_id = agent.id();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<u32>();

    let run = tokio::spawn(scoped(agent, async move {
        let tool_token = CancellationToken::new();
        let tool = register_tool_call("cascade_tool_marker", None, Some(tool_token.clone()));
        scoped(tool, async move {
            let mut child = spawn_registered("sleep", &["300.22"]);
            let pid = find_by_name("sleep 300.22")
                .and_then(|snapshot| snapshot.pid)
                .expect("test: nested child must be registered");
            let _ = ready_tx.send(pid);
            // Hold the child until this tool call is cancelled, mirroring a
            // tool that owns a subprocess for its whole duration.
            tool_token.cancelled().await;
            let _ = child.wait().await;
            child.mark_complete();
        })
        .await;
    }));

    let pid = ready_rx.await.expect("test: nested child pid");
    assert!(pid_exists(pid));

    let results = kill(agent_id, true).await;
    let kinds: Vec<WorkKind> = results.iter().map(|result| result.kind).collect();
    assert!(kinds.contains(&WorkKind::SubAgent), "the agent itself must be a target");
    assert!(kinds.contains(&WorkKind::ToolCall), "its tool call must be a target");
    assert!(kinds.contains(&WorkKind::Process), "its child process must be a target");
    assert!(
        results.iter().all(|result| result.outcome == KillOutcome::Killed),
        "every lineage member must be confirmed dead: {results:?}"
    );

    assert!(
        eventually(|| probe_process(pid) != ProcessLiveness::Running).await,
        "the agent's grandchild process must not survive a cascading kill"
    );
    let _ = run.await;
    assert!(find_by_name("cascade_agent_marker").is_none());
    assert!(find_by_name("cascade_tool_marker").is_none());
}

/// A channel turn is registered as a lineage root: killing it with cascade
/// must reach the tool call it started and the child process under that
/// tool call, which is the whole reason the turn is in the registry at all.
#[tokio::test]
async fn turn_is_a_lineage_root_that_cascades_to_its_tools_and_processes() {
    assert_eq!(WorkKind::Turn.as_str(), "turn");

    let turn_token = CancellationToken::new();
    let turn = register_turn("turn_cascade_marker", "run-turn-cascade", Some(turn_token.clone()));
    let turn_id = turn.id();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<u32>();

    let run = tokio::spawn(scoped(turn, async move {
        let tool_token = CancellationToken::new();
        let tool = register_tool_call("turn_cascade_tool_marker", None, Some(tool_token.clone()));
        scoped(tool, async move {
            let mut child = spawn_registered("sleep", &["300.31"]);
            let pid = find_by_name("sleep 300.31")
                .and_then(|snapshot| snapshot.pid)
                .expect("test: the turn's grandchild must be registered");
            let _ = ready_tx.send(pid);
            tool_token.cancelled().await;
            let _ = child.wait().await;
            child.mark_complete();
        })
        .await;
    }));

    let pid = ready_rx.await.expect("test: grandchild pid");
    let listed = find_by_name("turn_cascade_marker").expect("test: the turn must be listed");
    assert_eq!(listed.kind, WorkKind::Turn);
    assert_eq!(
        listed.run_id.as_deref(),
        Some("run-turn-cascade"),
        "the turn row must carry the run id its storage records use"
    );

    let results = kill(turn_id, true).await;
    let kinds: Vec<WorkKind> = results.iter().map(|result| result.kind).collect();
    assert!(kinds.contains(&WorkKind::Turn), "the turn itself must be a target");
    assert!(kinds.contains(&WorkKind::ToolCall), "its tool call must be a target");
    assert!(kinds.contains(&WorkKind::Process), "its child process must be a target");
    assert!(
        results.iter().all(|result| result.outcome == KillOutcome::Killed),
        "every member of the turn's lineage must be confirmed dead: {results:?}"
    );
    assert!(
        eventually(|| probe_process(pid) != ProcessLiveness::Running).await,
        "a killed turn must not leave its grandchild process running"
    );
    let _ = run.await;
    assert!(find_by_name("turn_cascade_marker").is_none());
    assert!(find_by_name("turn_cascade_tool_marker").is_none());
}

#[tokio::test]
async fn non_cascading_kill_leaves_the_children_alone() {
    let agent_token = CancellationToken::new();
    let agent = register_sub_agent("solo_agent_marker", "run-solo", None, None, Some(agent_token.clone()));
    let agent_id = agent.id();
    let child_guard = register(
        WorkKind::ToolCall,
        Arc::from("solo_tool_marker"),
        Some(agent_id),
        None,
        None,
        Some(CancellationToken::new()),
        None,
        None,
    );

    let run = tokio::spawn(scoped(agent, async move { agent_token.cancelled().await }));
    let results = kill(agent_id, false).await;
    assert_eq!(results.len(), 1, "cascade off must target exactly one item");
    let _ = run.await;

    assert!(
        find_by_name("solo_tool_marker").is_some(),
        "cascade off must leave the child registered"
    );
    drop(child_guard);
}

#[tokio::test]
async fn long_task_warning_only_warns_and_never_terminates() {
    let (mut child, row, pid, warned, logs) = {
        // Registration and the observed sweep belong to one tick. Another
        // test or background sweeper cannot consume this entry's warning.
        let tick = WARNING_TICK.lock();
        let child = spawn_registered("sleep", &["300.33"]);
        let row = find_by_name("sleep 300.33").expect("test: child must be registered");
        let pid = row.pid.expect("test: pid");
        let (returned, returned_rx) = std::sync::mpsc::channel();
        let competitor = std::thread::spawn(move || {
            warn_long_running(Duration::ZERO);
            returned.send(()).unwrap();
        });
        assert_eq!(
            returned_rx.recv_timeout(Duration::from_millis(200)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "test: competing warning sweep must block until the observed tick is released"
        );
        let (warned, logs) = capturing_logs(|| warn_long_running(Duration::ZERO));
        drop(tick);
        competitor.join().unwrap();
        (child, row, pid, warned, logs)
    };

    // A zero threshold makes every live item immediately overdue.
    assert!(
        logs.contains("sleep 300.33"),
        "test: warning must describe this child: {logs}"
    );
    assert!(warned > 0, "test: at least this child should have been warned about");
    assert!(
        logs.contains("has been running for a long time"),
        "the sweeper must actually emit a warning: {logs}"
    );
    assert!(
        logs.contains("nothing was terminated"),
        "the warning must say plainly that it did not kill anything: {logs}"
    );
    assert!(logs.contains("WARN"), "it must be logged at warn level: {logs}");

    // The whole point: the task is untouched.
    assert!(pid_exists(pid), "the warned child must still be alive");
    assert_eq!(probe_process(pid), ProcessLiveness::Running);
    let after = snapshot(row.id).expect("the warned entry must still be registered");
    assert_eq!(
        after.state,
        WorkState::Running,
        "warning must not mark the item as killed"
    );

    // Clean up the still-running child the warning deliberately spared.
    let _ = kill(row.id, false).await;
    let _ = child.wait().await;
    child.mark_complete();
}

#[tokio::test]
async fn unreaped_child_is_reported_as_an_orphan_then_reclaimed() {
    // Deliberately spawned outside the managed chokepoint so the guard can
    // be dropped while the process is still alive — the exact shape of a
    // child that outlives its Rust owner.
    let mut command = tokio::process::Command::new("sleep");
    command
        .arg("300.44")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().expect("test: spawn detached child");
    let pid = child.id().expect("test: child pid");
    let pgid = i32::try_from(pid).ok().filter(|pid| *pid > 0);

    let guard = register_process("orphan_marker", Some(pid), pgid);
    let id = guard.id();
    assert_eq!(snapshot(id).map(|snapshot| snapshot.state), Some(WorkState::Running));
    drop(guard);

    let orphan = snapshot(id).expect("an unreaped child keeps its ledger row");
    assert_eq!(
        orphan.state,
        WorkState::Orphaned,
        "a live child whose owner is gone must be reported as an orphan"
    );

    #[allow(unsafe_code)]
    if let Some(pgid) = pgid {
        // SAFETY: `pgid` is the group this test created; `killpg` only
        // delivers a signal and dereferences no pointer.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
    // Before reaping, the row must report the child as a zombie rather than
    // pretending it is gone.
    assert!(
        eventually(|| matches!(
            snapshot(id).map(|snapshot| snapshot.state),
            Some(WorkState::Zombie) | None
        ))
        .await,
        "an exited but unreaped child must be reported as a zombie"
    );

    let _ = child.wait().await;
    assert!(
        eventually(|| !pid_exists(pid) && snapshot(id).is_none()).await,
        "a reclaimed child must leave the ledger on its own"
    );
}

#[test]
fn blocking_pool_is_always_reported() {
    let snapshots = pool_snapshots();
    assert!(snapshots.iter().any(|snapshot| snapshot.kind == "blocking"));
}
