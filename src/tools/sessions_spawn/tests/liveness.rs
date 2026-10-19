#![cfg(test)]

use super::*;

#[test]
fn isolated_memory_prefixes_key() {
    assert_eq!(memory_key_prefix("alpha", "plan"), "alpha:plan");
    assert_eq!(memory_key_prefix("alpha", "alpha:plan"), "alpha:plan");
}

/// NeedsInput: after a cancel/steer ends a suspended approval, the run must be
/// deterministically restored from `AwaitingInput` back to `Running` (no
/// zombie `AwaitingInput` left behind once it is running again).
#[test]
fn restore_run_downgrades_awaiting_input_to_running() {
    let mut runs = vec![restore_test_run(
        "r1",
        SubAgentStatus::AwaitingInput {
            prompt: "shell(rm -rf /tmp/x)".to_string(),
        },
    )];
    restore_run_to_running(&mut runs, "r1");
    assert!(
        matches!(runs[0].status, SubAgentStatus::Running),
        "AwaitingInput must be restored to Running on resume"
    );
}

/// NeedsInput: a kill that already moved the run to a terminal `Failed` state
/// must NOT be resurrected to `Running` by the resume restore (terminal wins).
#[test]
fn restore_run_does_not_resurrect_terminal_failed() {
    let mut runs = vec![restore_test_run("r2", SubAgentStatus::Failed("killed".to_string()))];
    restore_run_to_running(&mut runs, "r2");
    assert!(
        matches!(runs[0].status, SubAgentStatus::Failed(ref m) if m == "killed"),
        "a terminal Failed (kill) state must never be overwritten by Running"
    );
}

/// NeedsInput: a completed run must likewise stay terminal.
#[test]
fn restore_run_does_not_resurrect_terminal_completed() {
    let mut runs = vec![restore_test_run("r3", SubAgentStatus::Completed("ok".to_string()))];
    restore_run_to_running(&mut runs, "r3");
    assert!(
        matches!(runs[0].status, SubAgentStatus::Completed(ref m) if m == "ok"),
        "a terminal Completed state must never be overwritten by Running"
    );
}

/// NeedsInput: an already-Running run is left as-is (idempotent), and an
/// unknown run id is a harmless no-op.
#[test]
fn restore_run_is_idempotent_and_ignores_unknown_id() {
    let mut runs = vec![restore_test_run("r4", SubAgentStatus::Running)];
    restore_run_to_running(&mut runs, "r4");
    assert!(matches!(runs[0].status, SubAgentStatus::Running));
    // Unknown id: no panic, no change.
    restore_run_to_running(&mut runs, "does-not-exist");
    assert!(matches!(runs[0].status, SubAgentStatus::Running));
}

// ── b1: sub-agent termination must be discoverable ──────────────

/// A run that has produced nothing yet reports its own start as its last
/// progress, so a supervisor never reads a nonsensical "last progress in
/// the future" or an epoch date.
#[test]
fn a_fresh_run_reports_its_start_as_its_last_progress() {
    let run = restore_test_run("fresh", SubAgentStatus::Running);
    assert_eq!(run.progress.events(), 0);
    assert!(
        (run.last_progress_at() - run.started_at).num_seconds().abs() <= 1,
        "a run with no events must not claim progress it never made"
    );
}

/// `last_progress_at` must be driven by real progress events and by nothing
/// else: it goes stale while the run is silent and jumps forward the moment
/// the shared `ProgressKind` vocabulary records an event.
#[tokio::test]
async fn last_progress_at_moves_forward_only_on_real_progress() {
    let run = restore_test_run("beating", SubAgentStatus::Running);
    let first = run.last_progress_at();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let silent_for = run.idle_for();
    assert!(
        silent_for >= std::time::Duration::from_millis(60),
        "silence must accumulate while nothing happens, got {silent_for:?}"
    );
    assert!(
        (run.last_progress_at() - first).num_milliseconds().abs() <= 20,
        "a silent run must not appear to be making progress"
    );

    run.progress.record(crate::agent::idle::ProgressKind::ToolEnd);
    assert!(
        run.idle_for() < silent_for,
        "a recorded event must reset the run's silence"
    );
    assert!(
        run.last_progress_at() > first,
        "a recorded event must advance the run's last-progress stamp"
    );
    assert_eq!(run.progress.events(), 1);
    assert_eq!(run.progress.last_kind(), crate::agent::idle::ProgressKind::ToolEnd);
}

/// Output arriving from a process-mode worker is the only progress signal
/// that crosses the process boundary, so the drain must stamp the run's beat
/// as the bytes arrive rather than once at EOF.
#[cfg(unix)]
#[tokio::test]
async fn worker_output_refreshes_the_run_beat_while_it_is_still_arriving() {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg("printf one; sleep 0.3; printf two; sleep 0.3; printf three")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("stand-in worker spawns");
    let stdout = child.stdout.take().expect("worker stdout");
    let progress = crate::agent::idle::child_beat();

    let bytes = drain_worker_stream(stdout, "stdout", Some(Arc::clone(&progress)))
        .await
        .expect("drain succeeds");

    assert_eq!(String::from_utf8_lossy(&bytes), "onetwothree");
    assert!(
        progress.events() >= 2,
        "each arrival is one progress event, got {}",
        progress.events()
    );
    assert!(
        progress.idle_for() < std::time::Duration::from_millis(250),
        "the last arrival must have reset the window"
    );
    let _ = child.wait().await;
}

/// Core evidence 1, at the level that owns the decision: a worker killed
/// from outside (SIGKILL / OOM killer / segfault) leaves the parent with a
/// closed pipe and nothing else, and that must classify as a terminal
/// failure that names the fault.
#[cfg(unix)]
#[tokio::test]
async fn a_sigkilled_worker_is_classified_as_exited_without_result() {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg("exec sleep 300")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().expect("stand-in worker spawns");
    let pid = child.id().expect("live child pid");
    let mut process_group = OwnedProcessGroup::from_child(&child).expect("group ownership");
    let control = ProcessRunControl::new();
    let progress = crate::agent::idle::child_beat();

    // External kill: nothing in this process asks for it, exactly like an
    // OOM kill. The parent's only notification is EOF on the pipes.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = tokio::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status()
            .await;
    });

    let phase = run_spawned_child_lifecycle(
        &mut child,
        &mut process_group,
        "{}",
        &control,
        None,
        Some(progress),
        false,
        None,
    )
    .await
    .expect("the lifecycle observes the child's death");

    let OwnedProcessPhase::Exited { status, stdout, stderr } = phase else {
        panic!("an externally killed worker exits; it is not a requested termination");
    };
    let stdout_raw = String::from_utf8_lossy(&stdout).trim().to_string();
    let stderr_raw = String::from_utf8_lossy(&stderr).trim().to_string();
    assert!(stdout_raw.is_empty(), "the worker never got to write a result");

    match classify_worker_output(status, &stdout_raw, &stderr_raw) {
        Ok(ProcessWorkerOutcome::ExitedWithoutResult(detail)) => {
            assert!(detail.contains("signal 9"), "the fault must be named: {detail}");
        }
        Ok(_) => panic!("a killed worker must never be reported as a finished or terminated run"),
        Err(error) => panic!("a killed worker must be a classified outcome, not an opaque error: {error}"),
    }
}

/// The same classifier must leave a healthy worker completely alone.
#[test]
fn a_worker_that_reports_a_result_is_unaffected_by_the_eof_mapping() {
    let status = std::process::Command::new("true")
        .status()
        .expect("test: /bin/true runs");
    let payload = serde_json::json!({"success": true, "output": "done"}).to_string();
    match classify_worker_output(status, &payload, "") {
        Ok(ProcessWorkerOutcome::Finished(result)) => {
            assert!(result.success);
            assert_eq!(result.output, "done");
        }
        _ => panic!("a normal worker result must still be a Finished outcome"),
    }
}

/// Core evidence 2, through the production spawn path: `registry::kill` —
/// the exact call the idle detector makes from `kill_turn_subtree` when it
/// terminates a wedged *parent* turn, and the one behind `prx tasks kill` —
/// aborts the sub-agent's monitor task through the abort handle on its
/// registry row. An aborted task runs no further code, so before b1 the
/// registry recorded the termination while the run's own row stayed
/// `Running` forever. It must now reach a terminal state.
///
/// MUTATION GUARD: drop the `watch_task_mode_monitor` call in the spawn
/// path and this hangs until `await_terminal_reason`'s bound and then fails.
#[tokio::test]
async fn registry_kill_of_a_spawned_run_makes_its_row_terminal() {
    let (ch, _) = RecordingChannel::new();
    // Parked in the provider, so the run is genuinely live when it is killed.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
            entered: None,
        }),
    );

    let spawned = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({"task": "about to be killed from the registry"})),
    )
    .await
    .expect("spawn tool call");
    assert!(spawned.success, "{spawned:?}");
    let run_id = {
        let runs = tool.active_runs_snapshot().await;
        runs.first().expect("the spawned run is registered").id.clone()
    };

    let work_id = crate::runtime::registry::resolve_address(&run_id)
        .expect("a spawned run must be addressable in the work registry");
    let _ = crate::runtime::registry::kill(work_id, true).await;

    let reason = await_terminal_reason(&tool.active_runs, &run_id).await;
    assert!(
        reason.contains("terminated before it could report a result"),
        "a registry kill must be visible on the run row itself, got: {reason}"
    );
    gate.add_permits(4);
}

/// The observer must never invent a failure for a run that finished
/// normally: a monitor that returns after committing its own terminal state
/// leaves that state exactly as it was.
#[tokio::test]
async fn a_run_that_finishes_normally_is_left_alone_by_the_observer() {
    let active_runs: Arc<RwLock<Vec<SubAgentRun>>> = Arc::new(RwLock::new(vec![restore_test_run(
        "happy-run",
        SubAgentStatus::Running,
    )]));
    let commit_runs = Arc::clone(&active_runs);
    let handle = tokio::spawn(async move {
        let mut runs = commit_runs.write().await;
        if let Some(run) = runs.iter_mut().find(|run| run.id == "happy-run") {
            run.status = SubAgentStatus::Completed("all good".to_string());
            run.finished_at = Some(Utc::now());
        }
    });
    watch_task_mode_monitor(
        handle,
        Arc::clone(&active_runs),
        "happy-run".to_string(),
        None,
        None,
        None,
        None,
    );

    // Give the observer every chance to misbehave.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let runs = active_runs.read().await;
        let run = runs.iter().find(|run| run.id == "happy-run").expect("run present");
        assert!(
            matches!(run.status, SubAgentStatus::Completed(ref text) if text == "all good"),
            "a completed run must not be rewritten by the termination observer"
        );
    }
}

/// A terminal status already published by the monitor always wins the race
/// against the observer.
#[tokio::test]
async fn a_terminal_status_is_never_overwritten_by_the_termination_commit() {
    let active_runs: Arc<RwLock<Vec<SubAgentRun>>> = Arc::new(RwLock::new(vec![
        restore_test_run("done", SubAgentStatus::Completed("output".to_string())),
        restore_test_run("failed", SubAgentStatus::Failed("real cause".to_string())),
        restore_test_run("live", SubAgentStatus::Running),
    ]));

    assert!(!commit_run_termination_if_unfinished(&active_runs, "done", "late verdict").await);
    assert!(!commit_run_termination_if_unfinished(&active_runs, "failed", "late verdict").await);
    assert!(!commit_run_termination_if_unfinished(&active_runs, "absent", "late verdict").await);
    assert!(commit_run_termination_if_unfinished(&active_runs, "live", "late verdict").await);

    let runs = active_runs.read().await;
    assert!(matches!(runs[0].status, SubAgentStatus::Completed(ref text) if text == "output"));
    assert!(matches!(runs[1].status, SubAgentStatus::Failed(ref text) if text == "real cause"));
    assert!(matches!(runs[2].status, SubAgentStatus::Failed(ref text) if text == "late verdict"));
    assert!(runs[2].finished_at.is_some());
    assert!(runs[2].steer_tx.is_none());
}

/// Core evidence 3, through the production spawn path: a run's registry row
/// reports staleness while the run is genuinely silent, and its
/// `last_progress_at` jumps forward exactly when the run produces something
/// real. Nothing here is simulated — the run is parked inside the provider
/// and released by the test.
#[tokio::test]
async fn a_spawned_run_row_tracks_its_own_real_progress() {
    let (ch, _) = RecordingChannel::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
            entered: Some(Arc::clone(&entered)),
        }),
    );

    let spawned = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "watch me work"})))
        .await
        .expect("spawn tool call");
    assert!(spawned.success, "{spawned:?}");

    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .expect("the real provider must be parked before measuring silence");
    // While parked in the provider the run really has made no progress, and
    // the row must say so instead of inventing activity.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (events_before, idle_before, stamp_before) = {
        let runs = tool.active_runs_snapshot().await;
        let run = runs.first().expect("the spawned run is registered");
        assert!(matches!(run.status, SubAgentStatus::Running));
        (run.progress.events(), run.idle_for(), run.last_progress_at())
    };
    assert_eq!(
        events_before, 1,
        "only completed request preparation may precede provider output"
    );
    assert!(
        idle_before >= std::time::Duration::from_millis(150),
        "silence must accumulate, got {idle_before:?}"
    );

    // Release it: the provider round-trip is a real ProgressKind event.
    gate.add_permits(4);
    let mut observed = None;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let runs = tool.active_runs_snapshot().await;
        let Some(run) = runs.first() else { continue };
        if run.progress.events() > events_before {
            observed = Some((run.progress.events(), run.idle_for(), run.last_progress_at()));
            break;
        }
    }
    let (events_after, idle_after, stamp_after) = observed.expect("a released run must record progress on its own row");
    assert!(events_after >= 1);
    assert!(
        events_after > events_before,
        "provider release must produce a new progress event"
    );
    assert!(
        idle_after < idle_before,
        "real progress must reset the run's silence: {idle_after:?} vs {idle_before:?}"
    );
    assert!(
        stamp_after > stamp_before,
        "last_progress_at must move forward, and only forward"
    );
}
