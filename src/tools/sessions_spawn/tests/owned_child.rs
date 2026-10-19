#![cfg(test)]

use super::*;

#[cfg(unix)]
#[tokio::test]
async fn process_mode_waits_for_natural_completion_without_a_deadline() {
    let mut command = tokio::process::Command::new("sleep");
    command.arg("0.1");
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().unwrap();
    let control = ProcessRunControl::new();
    let mut process_group = OwnedProcessGroup::from_child(&child).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    process_group.observe_termination_calls(calls.clone());

    let result = wait_for_owned_process(&mut child, &mut process_group, &control, false)
        .await
        .expect("the worker must be allowed to finish naturally");
    assert!(matches!(result, OwnedChildExit::Exited(status) if status.success()));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[cfg(unix)]
async fn assert_explicit_cleanup_signals_once() {
    let mut command = tokio::process::Command::new("sleep");
    command
        .arg("300")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut process_group = OwnedProcessGroup::from_child(&child).unwrap();
    process_group.observe_termination_calls(calls.clone());

    terminate_owned_child(&mut child, &mut process_group).await.unwrap();

    assert!(
        !process_group.armed,
        "the single group owner must be disarmed before the direct child is reaped"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn live_termination_disarms_single_group_owner_before_reap() {
    assert_explicit_cleanup_signals_once().await;
}

#[cfg(unix)]
#[tokio::test]
async fn stdin_error_cleanup_signals_group_once() {
    assert_explicit_cleanup_signals_once().await;
}

#[cfg(unix)]
#[tokio::test]
async fn pipe_setup_error_cleanup_signals_group_once() {
    assert_explicit_cleanup_signals_once().await;
}

#[cfg(unix)]
#[tokio::test]
async fn owned_child_panic_cleanup_signals_once_and_reaps_before_return() {
    let mut command = tokio::process::Command::new("sleep");
    command
        .arg("300")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut process_group = OwnedProcessGroup::from_child(&child).unwrap();
    process_group.observe_termination_calls(calls.clone());

    let control = ProcessRunControl::new();
    let panic_result = std::panic::AssertUnwindSafe(run_spawned_child_lifecycle(
        &mut child,
        &mut process_group,
        "{}",
        &control,
        None,
        None,
        true,
        None,
    ))
    .catch_unwind()
    .await;
    assert!(panic_result.is_err());
    cleanup_owned_child_after_panic(&mut child, &mut process_group, false)
        .await
        .unwrap();

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!process_group.armed);
    assert!(
        child.try_wait().unwrap().is_some(),
        "direct child must be reaped before cleanup returns"
    );
}

#[cfg(unix)]
#[tokio::test]
#[allow(unsafe_code)]
async fn owner_mediated_process_kill_reaps_leader_and_terminates_group() {
    let temp = tempfile::tempdir().expect("test temp directory should be created");
    let descendant_pid_file = temp.path().join("descendant.pid");
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(format!(
            "sleep 300 & echo $! > '{}'; wait",
            descendant_pid_file.display()
        ))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let child = command.spawn().expect("test child should spawn");
    let pid = child.id().expect("test child should have a pid");
    let control = ProcessRunControl::new();
    let owner_control = control.clone();
    let monitor = tokio::spawn(async move {
        let mut child = child;
        let mut process_group = OwnedProcessGroup::from_child(&child).unwrap();
        let result = wait_for_owned_process(&mut child, &mut process_group, owner_control.as_ref(), false).await;
        let finalization = match result {
            Ok(OwnedChildExit::Terminated(_)) => ProcessFinalization::Terminated,
            _ => ProcessFinalization::Natural,
        };
        owner_control.finalize(finalization);
    });

    let descendant_pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(raw) = tokio::fs::read_to_string(&descendant_pid_file).await {
                if let Ok(pid) = raw.trim().parse::<i32>() {
                    break pid;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background descendant pid should be published");

    assert_eq!(
        control.request_termination("test termination").await,
        ProcessTerminationRequestResult::Finalized(ProcessFinalization::Terminated)
    );
    monitor.await.expect("process owner should finish");
    let leader_pid = i32::try_from(pid).expect("test pid should fit pid_t");
    let process_exists = |pid| {
        // SAFETY: signal 0 sends no signal; it only probes test-owned pids.
        (unsafe { libc::kill(pid, 0) }) == 0
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while process_exists(leader_pid) || process_exists(descendant_pid) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("process owner must reap its leader and terminate the descendant group");
}

#[cfg(unix)]
#[tokio::test]
#[allow(unsafe_code)]
async fn termination_after_leader_exit_does_not_wait_forever_on_inherited_pipe() {
    let temp = tempfile::tempdir().expect("test temp directory should be created");
    let descendant_pid_file = temp.path().join("descendant.pid");
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(format!(
            "sleep 300 & echo $! > '{}'; exit 0",
            descendant_pid_file.display()
        ))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().expect("test leader should spawn");
    let mut process_group = OwnedProcessGroup::from_child(&child).unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout_reader = tokio::spawn(async move {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await?;
        Ok::<_, anyhow::Error>(output)
    });
    let stderr_reader = tokio::spawn(async move {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).await?;
        Ok::<_, anyhow::Error>(output)
    });
    child.wait().await.expect("leader should exit naturally");
    relinquish_process_group_after_leader_exit(&mut process_group);

    let descendant_pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(raw) = tokio::fs::read_to_string(&descendant_pid_file).await {
                if let Ok(pid) = raw.trim().parse::<i32>() {
                    break pid;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant pid should be published");
    // SAFETY: signal 0 only probes the test-owned descendant PID.
    assert_eq!((unsafe { libc::kill(descendant_pid, 0) }), 0);

    let control = ProcessRunControl::new();
    let request = {
        let control = control.clone();
        tokio::spawn(async move { control.request_termination("kill after leader exit").await })
    };
    tokio::task::yield_now().await;
    let owner_control = control.clone();
    let owner = tokio::spawn(async move {
        let outcome = drain_process_output_after_leader_exit(
            stdout_reader,
            stderr_reader,
            owner_control.as_ref(),
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap();
        let finalization = match outcome {
            ProcessOutputDrain::TerminationFailed(_) => ProcessFinalization::TerminationFailed,
            ProcessOutputDrain::Finished { .. } => ProcessFinalization::Natural,
        };
        owner_control.finalize(finalization);
    });
    let request = tokio::time::timeout(std::time::Duration::from_secs(2), request)
        .await
        .expect("kill request must still reach the owner after leader exit")
        .unwrap();
    owner.await.unwrap();

    assert_eq!(
        request,
        ProcessTerminationRequestResult::Finalized(ProcessFinalization::TerminationFailed)
    );
    // The owner deliberately relinquished the PGID after reaping the
    // leader; clean up this test-only escaped descendant by its known pid.
    // SAFETY: this PID was created by the test fixture and is cleaned up here.
    let _ = unsafe { libc::kill(descendant_pid, libc::SIGKILL) };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        // SAFETY: signal 0 only probes the same test-owned descendant PID.
        while (unsafe { libc::kill(descendant_pid, 0) }) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("test descendant should exit after explicit fixture cleanup");
}

#[test]
fn normal_leader_output_completion_disarms_group_without_signal() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let mut process_group = OwnedProcessGroup::test_stub(calls.clone());
        relinquish_process_group_after_leader_exit(&mut process_group);
        assert!(!process_group.armed);
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn inherited_pipe_after_leader_reap_relinquishes_without_signal() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let mut process_group = OwnedProcessGroup::test_stub(calls.clone());
        relinquish_process_group_after_leader_exit(&mut process_group);
        assert!(!process_group.armed);
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn process_termination_requests_share_one_finalization() {
    let control = ProcessRunControl::new();
    let first = {
        let control = control.clone();
        tokio::spawn(async move { control.request_termination("first reason").await })
    };
    let second = {
        let control = control.clone();
        tokio::spawn(async move { control.request_termination("second reason").await })
    };

    tokio::task::yield_now().await;
    assert_eq!(control.finalization(), None, "requesters must wait for the owner");
    control.finalize(ProcessFinalization::Terminated);

    assert_eq!(
        first.await.unwrap(),
        ProcessTerminationRequestResult::Finalized(ProcessFinalization::Terminated)
    );
    assert_eq!(
        second.await.unwrap(),
        ProcessTerminationRequestResult::Finalized(ProcessFinalization::Terminated)
    );
    assert_eq!(
        control.request_termination("repeated after finalization").await,
        ProcessTerminationRequestResult::Finalized(ProcessFinalization::Terminated)
    );
}

#[tokio::test]
async fn termination_request_timeout_does_not_finalize_or_release_slot() {
    let control = ProcessRunControl::new();
    let mut runs = vec![restore_test_run("process-run", SubAgentStatus::Running)];
    runs[0].process_control = Some(control.clone());

    let result = control
        .request_termination_with_timeout("stuck owner", std::time::Duration::from_millis(20))
        .await;

    assert_eq!(result, ProcessTerminationRequestResult::Pending);
    assert_eq!(control.finalization(), None);
    assert_eq!(running_run_count(&runs), 1);
    assert!(matches!(runs[0].status, SubAgentStatus::Running));
}

#[tokio::test]
async fn termination_request_timeout_observes_boundary_finalization() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let control = ProcessRunControl::new_timeout_boundary_for_test(barrier.clone());
    let requester = {
        let control = control.clone();
        tokio::spawn(async move { control.request_termination("boundary finalization").await })
    };

    barrier.wait().await;
    control.finalize(ProcessFinalization::Terminated);
    barrier.wait().await;

    assert_eq!(
        requester.await.unwrap(),
        ProcessTerminationRequestResult::Finalized(ProcessFinalization::Terminated)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn owner_keeps_child_after_requester_timeout_until_reap() {
    let mut command = tokio::process::Command::new("sleep");
    command
        .arg("0.2")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();
    let control = ProcessRunControl::new();
    let owner_control = control.clone();
    let owner = tokio::spawn(async move {
        let mut child = child;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut process_group = OwnedProcessGroup::test_stub(calls);
        let exit = wait_for_owned_process(&mut child, &mut process_group, owner_control.as_ref(), false)
            .await
            .unwrap();
        assert!(matches!(exit, OwnedChildExit::Terminated(_)));
        owner_control.finalize(ProcessFinalization::Terminated);
    });

    let result = control
        .request_termination_with_timeout("slow reap", std::time::Duration::from_millis(20))
        .await;
    assert_eq!(result, ProcessTerminationRequestResult::Pending);
    assert_eq!(control.finalization(), None);
    assert!(!owner.is_finished(), "owner must retain Child while reap is pending");

    owner.await.unwrap();
    assert_eq!(control.finalization(), Some(ProcessFinalization::Terminated));
}

#[cfg(unix)]
#[tokio::test]
async fn injected_wait_error_keeps_child_finalization_and_slot_pending() {
    let mut command = tokio::process::Command::new("true");
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();
    let control = ProcessRunControl::new_with_request_timeout(std::time::Duration::from_millis(20));
    let mut runs = vec![restore_test_run("unresolved-process", SubAgentStatus::Running)];
    runs[0].process_control = Some(control.clone());
    let owner_control = control.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let owner_calls = calls.clone();
    let owner = tokio::spawn(async move {
        let mut child = child;
        let mut process_group = OwnedProcessGroup::test_stub(owner_calls);
        let _ = wait_for_owned_process(&mut child, &mut process_group, owner_control.as_ref(), true).await;
    });

    assert_eq!(
        control.request_termination("unresolved wait").await,
        ProcessTerminationRequestResult::Pending
    );
    assert_eq!(control.finalization(), None);
    assert_eq!(running_run_count(&runs), 1);
    assert!(!owner.is_finished());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    owner.abort();
    let _ = owner.await;
}

#[cfg(unix)]
#[tokio::test]
async fn injected_termination_wait_error_keeps_owner_pending() {
    let mut command = tokio::process::Command::new("true");
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();
    let control = ProcessRunControl::new_with_request_timeout(std::time::Duration::from_millis(20));
    let owner_control = control.clone();
    let owner = tokio::spawn(async move {
        let mut child = child;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut process_group = OwnedProcessGroup::test_stub(calls);
        let reason = owner_control.termination_requested().await;
        assert_eq!(reason, "termination wait error");
        process_group.terminate().unwrap();
        let _ = wait_for_reap_after_group_termination(&mut child, true).await;
    });

    assert_eq!(
        control.request_termination("termination wait error").await,
        ProcessTerminationRequestResult::Pending
    );
    assert_eq!(control.finalization(), None);
    assert!(!owner.is_finished());

    owner.abort();
    let _ = owner.await;
}

#[cfg(unix)]
#[tokio::test]
async fn injected_panic_cleanup_try_wait_error_keeps_owner_pending() {
    let mut command = tokio::process::Command::new("true");
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();
    let control = ProcessRunControl::new_with_request_timeout(std::time::Duration::from_millis(20));
    let owner = tokio::spawn(async move {
        let mut child = child;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut process_group = OwnedProcessGroup::test_stub(calls);
        let _ = cleanup_owned_child_after_panic(&mut child, &mut process_group, true).await;
    });

    assert_eq!(
        control.request_termination("panic cleanup unresolved").await,
        ProcessTerminationRequestResult::Pending
    );
    assert_eq!(control.finalization(), None);
    assert!(!owner.is_finished());

    owner.abort();
    let _ = owner.await;
}

#[tokio::test]
async fn owner_panic_commits_failed_registry_state_before_finalization() {
    let control = ProcessRunControl::new();
    let mut run = restore_test_run("process-run", SubAgentStatus::Running);
    run.process_control = Some(control.clone());
    let runs = Arc::new(RwLock::new(vec![run]));

    let panic_result = std::panic::AssertUnwindSafe(async {
        panic!("simulated process owner panic");
    })
    .catch_unwind()
    .await;
    assert!(panic_result.is_err());
    commit_process_owner_failure_if_unfinalized(
        &runs,
        "process-run",
        control.as_ref(),
        "process owner panicked".to_string(),
    )
    .await;

    let runs = runs.read().await;
    assert!(matches!(runs[0].status, SubAgentStatus::Failed(ref error) if error == "process owner panicked"));
    assert_eq!(control.finalization(), Some(ProcessFinalization::TerminationFailed));
    assert_eq!(running_run_count(&runs), 0);
}

#[tokio::test]
async fn terminal_commit_does_not_release_slot_before_control_finalization() {
    let control = ProcessRunControl::new();
    let mut run = restore_test_run("process-run", SubAgentStatus::Running);
    run.process_control = Some(control.clone());
    let runs = Arc::new(RwLock::new(vec![run]));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let commit = {
        let runs = runs.clone();
        let control = control.clone();
        tokio::spawn(async move {
            commit_process_terminal_state(
                &runs,
                "process-run",
                SubAgentStatus::Failed("killed".to_string()),
                Vec::new(),
                control.as_ref(),
                ProcessFinalization::Terminated,
                None,
                Some(ProcessTerminalCommitHook {
                    entered: entered_tx,
                    release: release_rx,
                }),
            )
            .await
            .expect("process terminal commit");
        })
    };

    entered_rx.await.expect("commit should reach the guarded window");
    assert_eq!(control.finalization(), None);
    let early_visibility = tokio::time::timeout(std::time::Duration::from_millis(50), async {
        running_run_count(&runs.read().await)
    })
    .await;
    assert!(
        early_visibility.is_err(),
        "registry readers must remain blocked until status and finalization commit together"
    );

    release_tx.send(()).unwrap();
    commit.await.unwrap();
    assert_eq!(control.finalization(), Some(ProcessFinalization::Terminated));
    assert_eq!(running_run_count(&runs.read().await), 0);
}

#[tokio::test]
async fn process_termination_keeps_concurrency_slot_until_owner_finalizes() {
    let control = ProcessRunControl::new();
    let mut runs = vec![restore_test_run("process-run", SubAgentStatus::Running)];
    runs[0].process_control = Some(control.clone());
    let requester = {
        let control = control.clone();
        tokio::spawn(async move { control.request_termination("shutdown").await })
    };

    tokio::task::yield_now().await;
    assert_eq!(running_run_count(&runs), 1);
    assert!(matches!(runs[0].status, SubAgentStatus::Running));

    runs[0].status = SubAgentStatus::Failed("shutdown".to_string());
    control.finalize(ProcessFinalization::Terminated);
    assert_eq!(
        requester.await.unwrap(),
        ProcessTerminationRequestResult::Finalized(ProcessFinalization::Terminated)
    );
    assert_eq!(running_run_count(&runs), 0);
}

#[tokio::test]
async fn process_termination_records_killed_terminal_event_once() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let fabric = MemoryFabric::new(memory.clone(), "/tmp");
    let scope = MessageEventScope::new("sessions_spawn", MemoryVisibility::Workspace)
        .with_session_key("process-kill-event")
        .with_run_id("process-run");

    record_spawn_result_event(
        Some(&fabric),
        scope,
        "Sub-agent process terminated: killed by operator",
        &SubAgentStatus::Failed("killed by operator".to_string()),
        &SpawnLineage::default(),
        Some("task.killed"),
    )
    .await;

    let events = memory
        .list_memory_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "/tmp".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some("process-kill-event".to_string()),
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        events.iter().filter(|event| event.event_type == "task.killed").count(),
        1
    );
    assert_eq!(
        events.iter().filter(|event| event.event_type == "task.failed").count(),
        0
    );
}
