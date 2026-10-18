#![cfg(test)]

use super::*;

// ── b2: fan-out / join ───────────────────────────────────────

/// A registry row with a batch label and a beat that nobody ever stamps —
/// the shape of a healthy process-mode member, whose worker produces no
/// byte on any pipe until the moment it is finished.
///
/// The `process_control` is what makes it that shape rather than merely
/// claiming to be: it is the field `member_vouches_for_the_waiter` reads to
/// tell a member whose progress the parent can observe from one whose
/// progress lives in another process entirely.
fn batch_test_run(id: &str, batch_id: &str, status: SubAgentStatus) -> SubAgentRun {
    let mut run = task_mode_batch_test_run(id, batch_id, status);
    run.process_control = Some(ProcessRunControl::new());
    run
}

/// A roster of the given run ids, as `spawn_batch` would have recorded it.
///
/// The unit tests below drive `join_batch_members` directly, so they have to
/// supply the membership the tool would have handed it.
fn roster_of(run_ids: &[&str]) -> Vec<BatchMember> {
    run_ids
        .iter()
        .map(|run_id| BatchMember {
            run_id: (*run_id).to_string(),
            task: format!("task for {run_id}"),
        })
        .collect()
}

/// The other shape: a member that runs as a task inside this process, so
/// its beat is a real progress signal the join can read.
fn task_mode_batch_test_run(id: &str, batch_id: &str, status: SubAgentStatus) -> SubAgentRun {
    let mut run = restore_test_run(id, status);
    run.task = format!("task for {id}");
    run.batch_id = Some(batch_id.to_string());
    run
}

/// Core evidence 1: one request starts three sub-agents, and a join on the
/// batch hands the caller all three results.
#[tokio::test]
async fn one_request_fans_out_to_three_subtasks_and_joins_all_three_results() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));

    let spawned = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "action": "spawn_batch",
            "tasks": ["survey the crate", "check the tests", {"task": "read the config"}],
        })),
    )
    .await
    .expect("spawn_batch tool call");
    assert!(spawned.success, "{spawned:?}");
    let payload = parse_json(&spawned.output);
    let batch_id = payload["batch_id"].as_str().expect("a batch id").to_string();
    assert_eq!(payload["started"], 3, "{payload}");
    assert_eq!(batch_run_ids(&payload).len(), 3);

    // Every member carries the batch label on its own registry row; that is
    // the only thing join, sessions_list and a cascade kill select on.
    {
        let runs = tool.active_runs_snapshot().await;
        assert_eq!(runs.len(), 3);
        assert!(
            runs.iter()
                .all(|run| run.batch_id.as_deref() == Some(batch_id.as_str()))
        );
    }

    let joined = tool
        .execute(json!({"action": "join", "batch_id": batch_id}))
        .await
        .expect("join tool call");
    assert!(joined.success, "{joined:?}");
    let summary = parse_json(&joined.output);
    assert_eq!(summary["total"], 3, "{summary}");
    assert_eq!(
        summary["completed"].as_array().map(Vec::len),
        Some(3),
        "a join must return one result per member: {summary}"
    );
    assert!(summary["failed"].as_array().is_some_and(Vec::is_empty));
    assert!(summary["killed"].as_array().is_some_and(Vec::is_empty));
    assert!(summary["no_result"].as_array().is_some_and(Vec::is_empty));
}

/// A member that finished and was then retired from `active_runs` — what
/// the chat session reaper does, on its own schedule, to any run that has
/// reached a terminal state — is still a member of the batch it was
/// launched in.
///
/// Before the roster, `join` derived its membership by scanning
/// `active_runs` for the batch label, so a reaped member was not reported as
/// missing: it was never in the batch at all. `total` quietly became 1, the
/// caller was handed a complete-looking account of a two-member fan-out, and
/// the identity assertion in `join_summary` saw nothing wrong because the
/// buckets still summed to the (shrunken) total — and is a `debug_assert`
/// that a release build does not run in the first place.
///
/// MUTATION GUARD: drop the roster lookup from
/// `SessionsSpawnTool::batch_members` so it falls back to the registry scan,
/// and `total` here is 2 -> 1 with the reaped member gone without trace.
#[tokio::test]
async fn a_member_reaped_out_of_the_registry_is_still_counted_in_its_batch() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));

    let spawned = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "action": "spawn_batch",
            "tasks": ["the member that survives", "the member that gets reaped"],
        })),
    )
    .await
    .expect("spawn_batch tool call");
    let payload = parse_json(&spawned.output);
    let batch_id = payload["batch_id"].as_str().expect("a batch id").to_string();
    let run_ids = batch_run_ids(&payload);
    assert_eq!(run_ids.len(), 2, "{payload}");
    for run_id in &run_ids {
        let _ = await_terminal_reason(&tool.active_runs, run_id).await;
    }

    // Exactly what `crate::chat::sessions::runtime`'s reaper does to a
    // finished run: `runs.retain(...)`, with no notice to anything holding
    // the batch id.
    let reaped = run_ids.last().expect("two members were launched").clone();
    {
        let mut runs = tool.active_runs.write().await;
        runs.retain(|run| run.id != reaped);
    }

    let joined = tool
        .execute(json!({"action": "join", "batch_id": batch_id}))
        .await
        .expect("join tool call");
    let summary = parse_json(&joined.output);
    assert_eq!(summary["total"], 2, "the batch launched two members: {summary}");
    assert_eq!(summary["completed"].as_array().map(Vec::len), Some(1), "{summary}");
    assert_eq!(
        summary["no_result"].as_array().map(Vec::len),
        Some(1),
        "the reaped member must be reported as a member without a result: {summary}"
    );
    let missing = &summary["no_result"][0];
    assert_eq!(missing["run_id"], reaped, "{summary}");
    assert_eq!(
        missing["task"], "the member that gets reaped",
        "the roster keeps what the member was asked to do, so the caller can retry it: {summary}"
    );
}

/// Core evidence 2: a member killed from outside — `registry::kill`, the
/// call behind `prx tasks kill` and behind the idle detector's
/// `kill_turn_subtree` — must not strand the join. It returns, and the
/// killed member is reported as killed rather than as a task that failed.
///
/// This is the b1 propagation seen from the caller's side: without it the
/// killed row would stay `Running` and this join would never return.
///
/// MUTATION GUARD: make `failure_is_kill` return `false` and the killed
/// member lands in `failed`, which is the assertion below.
#[tokio::test]
async fn a_member_killed_from_outside_still_lets_the_join_return_and_is_reported_as_killed() {
    let (ch, _) = RecordingChannel::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
        }),
    );

    let spawned = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "action": "spawn_batch",
            "tasks": ["member one", "member two", "member three"],
        })),
    )
    .await
    .expect("spawn_batch tool call");
    let payload = parse_json(&spawned.output);
    let batch_id = payload["batch_id"].as_str().expect("a batch id").to_string();
    let run_ids = batch_run_ids(&payload);
    assert_eq!(run_ids.len(), 3);

    let victim = run_ids.first().expect("three members were started").clone();
    let work_id = crate::runtime::registry::resolve_address(&victim)
        .expect("a spawned run must be addressable in the work registry");
    let _ = crate::runtime::registry::kill(work_id, true).await;
    // The survivors are released; the killed one no longer has a monitor to
    // consume a permit.
    gate.add_permits(8);

    let joined = tool
        .execute(json!({"action": "join", "batch_id": batch_id}))
        .await
        .expect("join tool call");
    let summary = parse_json(&joined.output);
    assert_eq!(summary["total"], 3, "{summary}");
    let killed = summary["killed"].as_array().expect("a killed bucket");
    assert_eq!(
        killed.len(),
        1,
        "the killed member must be reported as killed: {summary}"
    );
    assert_eq!(killed[0]["run_id"].as_str(), Some(victim.as_str()));
    assert_eq!(
        summary["completed"].as_array().map(Vec::len),
        Some(2),
        "killing one member must not disturb the others: {summary}"
    );
    assert!(summary["failed"].as_array().is_some_and(Vec::is_empty), "{summary}");
}

/// A tool whose only configured agent carries a `memory_scope` typo — the
/// cheapest way to make `execute_spawn` fail *after* the point at which the
/// run's row used to be pushed.
fn tool_with_broken_agent(agent: &str) -> SessionsSpawnTool {
    let (ch, _) = RecordingChannel::new();
    let mut agents = HashMap::new();
    let mut cfg = make_agent_config(None);
    // A plausible typo for "isolated": `parse_memory_scope` refuses it.
    cfg.memory_scope = Some("isolate".to_string());
    agents.insert(agent.to_string(), cfg);
    SessionsSpawnTool::new(
        Arc::new(ch),
        Arc::new(EchoProvider { response: "ok".into() }),
        "ollama",
        "prx-local-model",
        0.7,
        test_security(),
        std::path::PathBuf::from("/tmp"),
        crate::config::MultimodalConfig::default(),
        agents,
        None,
        crate::providers::ProviderRuntimeOptions::default(),
        crate::config::SessionsSpawnConfig::default(),
    )
}

/// b2: a spawn that fails while preparing the sub-agent must leave nothing
/// behind.
///
/// The failure happens after the announce channel is resolved and after
/// every clone the driver task needs has been made — exactly where the run
/// used to be registered already. A row published there has no abort
/// handle, no driver task and no observer, so nothing will ever move it off
/// `Running`.
///
/// MUTATION GUARD: move the `active_runs` push in `execute_spawn` back above
/// the agent prompt/tool resolution and this assertion finds one `Running`
/// row that no code will ever finish.
#[tokio::test]
async fn a_spawn_that_fails_while_preparing_the_agent_registers_no_run() {
    let tool = tool_with_broken_agent("alpha");

    let error = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "hello", "agent": "alpha"})))
        .await
        .expect_err("an unparseable memory_scope must fail the spawn");
    assert!(error.to_string().contains("memory_scope"), "{error}");

    let runs = tool.active_runs_snapshot().await;
    assert!(
        runs.is_empty(),
        "a spawn that never started a driver task must not leave a row behind: {:?}",
        runs.iter()
            .map(|run| (run.id.clone(), format!("{:?}", run.status)))
            .collect::<Vec<_>>()
    );
}

/// b2, from the joining caller's side: the risk that actually bites.
///
/// A fan-out where one member cannot be prepared reports that member in
/// `rejected`, and the caller then joins the batch. If the failed member
/// left a `Running` row carrying the batch id, `join_batch_members` selects
/// it, never sees it settle, and polls for it forever — this runtime has no
/// wall clock that would ever break the tie.
///
/// The timeout below is the test's assertion, not a production mechanism:
/// it exists so a regression fails in seconds instead of hanging the suite.
#[tokio::test]
async fn a_batch_member_that_fails_preparation_does_not_park_the_join_forever() {
    let tool = tool_with_broken_agent("broken");

    let spawned = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "action": "spawn_batch",
            "tasks": ["a member that starts", {"task": "a member that cannot", "agent": "broken"}],
        })),
    )
    .await
    .expect("spawn_batch tool call");
    let payload = parse_json(&spawned.output);
    let batch_id = payload["batch_id"].as_str().expect("a batch id").to_string();
    assert_eq!(payload["started"], 1, "{payload}");
    assert_eq!(
        payload["rejected"].as_array().map(Vec::len),
        Some(1),
        "the unpreparable member belongs in rejected: {payload}"
    );

    let joined = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tool.execute(json!({"action": "join", "batch_id": batch_id})),
    )
    .await
    .expect("join must converge: a member that never started a driver task must not be joinable")
    .expect("join tool call");
    let summary = parse_json(&joined.output);
    assert_eq!(
        summary["total"], 1,
        "only the member that actually started is part of the batch: {summary}"
    );
    assert_eq!(summary["completed"].as_array().map(Vec::len), Some(1), "{summary}");
}

/// Core evidence 3, through the real hang detector: a turn parked on a
/// member that emits nothing at all survives an idle window several times
/// shorter than the wait.
///
/// This is the one limitation b1 left behind, seen from the join's side: a
/// process-mode member's only in-band signal is the bytes its worker
/// writes, and a healthy `session-worker` writes exactly one line, at the
/// very end. The run modelled here therefore never stamps its beat — and
/// the joining turn must still not be judged wedged.
///
/// MUTATION GUARD: delete the `ProgressKind::SubtaskAlive` line in
/// `join_batch_members` and `run_guarded` terminates this turn after 300ms
/// with `IdleHangTerminated`.
#[tokio::test]
async fn a_join_on_a_member_that_emits_nothing_is_not_mistaken_for_a_hang() {
    let active_runs: Arc<RwLock<Vec<SubAgentRun>>> = Arc::new(RwLock::new(vec![batch_test_run(
        "silent-worker",
        "batch-silent",
        SubAgentStatus::Running,
    )]));
    let finisher = Arc::clone(&active_runs);
    tokio::spawn(async move {
        // Far beyond the idle window below, and with not one progress event
        // in between.
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        let mut runs = finisher.write().await;
        if let Some(run) = runs.iter_mut().find(|run| run.id == "silent-worker") {
            run.finished_at = Some(Utc::now());
            run.status = SubAgentStatus::Completed("worker result".to_string());
        }
    });

    let guard = crate::agent::idle::IdleGuard {
        idle: Some(std::time::Duration::from_millis(300)),
    };
    let members = roster_of(&["silent-worker"]);
    let joined = crate::agent::idle::run_guarded(guard, "turn joining a silent batch", None, async {
        Ok(join_batch_members(&active_runs, &members).await)
    })
    .await
    .expect("a turn parked on a working sub-agent must never be terminated as hung");

    assert!(
        matches!(
            joined.first().map(|member| &member.verdict),
            Some(JoinedVerdict::Completed(output)) if output == "worker result"
        ),
        "the join must return the member's real result"
    );
}

/// Core evidence 3 against a **real OS process**: a genuinely silent child
/// — a separate process that writes not one byte for its whole life, which
/// is exactly what a healthy `session-worker` does — is joined to
/// completion, and the joining turn survives an idle window four times
/// shorter than the wait.
///
/// The member's beat is fed by [`drain_worker_stream`], the same function
/// the production process-mode path uses, so the silence measured here is
/// the production silence and not a stand-in for it. The assertion that the
/// beat recorded **zero** events is the limitation itself, stated as a
/// number: there is no in-band signal to have, which is why the join has to
/// supply the evidence out of band.
#[cfg(unix)]
#[tokio::test]
async fn a_join_survives_a_real_child_process_that_never_writes_anything() {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg("exec sleep 2")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("stand-in worker spawns");
    let stdout = child.stdout.take().expect("worker stdout");

    let run = batch_test_run("silent-child", "batch-real", SubAgentStatus::Running);
    let member_beat = Arc::clone(&run.progress);
    let active_runs: Arc<RwLock<Vec<SubAgentRun>>> = Arc::new(RwLock::new(vec![run]));

    // The production shape: one task owns the child, stamps the run's beat
    // from the worker's output stream, and commits a terminal status at EOF.
    let monitor_runs = Arc::clone(&active_runs);
    let monitor_beat = Arc::clone(&member_beat);
    tokio::spawn(async move {
        let drained = drain_worker_stream(stdout, "stdout", Some(monitor_beat)).await;
        let _ = child.wait().await;
        let mut runs = monitor_runs.write().await;
        if let Some(run) = runs.iter_mut().find(|run| run.id == "silent-child") {
            run.finished_at = Some(Utc::now());
            run.status = match drained {
                Ok(_) => SubAgentStatus::Completed("child exited".to_string()),
                Err(error) => SubAgentStatus::Failed(error.to_string()),
            };
        }
    });

    let guard = crate::agent::idle::IdleGuard {
        idle: Some(std::time::Duration::from_millis(500)),
    };
    let members = roster_of(&["silent-child"]);
    let joined = crate::agent::idle::run_guarded(guard, "turn joining a real silent child", None, async {
        Ok(join_batch_members(&active_runs, &members).await)
    })
    .await
    .expect("a turn joining a live child process must not be terminated as hung");

    assert!(
        matches!(
            joined.first().map(|member| &member.verdict),
            Some(JoinedVerdict::Completed(output)) if output == "child exited"
        ),
        "the join must observe the child's real ending"
    );
    assert_eq!(
        member_beat.events(),
        0,
        "the limitation, measured: a silent process-mode member has no in-band progress signal at all"
    );
}

/// The tightening, stated as the case it exists for: a member whose
/// progress the parent *can* read went silent for longer than its own idle
/// window, and its own watchdog did not end it. The premise that every
/// member is bounded on its own terms is false for that member, so the join
/// stops vouching and the caller's detector reaches the verdict it exists
/// to reach.
///
/// Before this rule the join stamped `SubtaskAlive` on every poll for as
/// long as any member's row was not terminal, which made the caller
/// immortal for as long as one member failed to die — the one thing an
/// unbounded runtime cannot afford, because the idle detector is its only
/// automatic recovery.
///
/// MUTATION GUARD: make `member_vouches_for_the_waiter` return `true`
/// unconditionally and this test never reaches the expected hang verdict.
#[tokio::test]
async fn a_join_stops_vouching_for_an_observable_member_that_went_silent() {
    let active_runs: Arc<RwLock<Vec<SubAgentRun>>> = Arc::new(RwLock::new(vec![task_mode_batch_test_run(
        "wedged-task-member",
        "batch-wedged",
        SubAgentStatus::Running,
    )]));

    let guard = crate::agent::idle::IdleGuard {
        idle: Some(std::time::Duration::from_millis(300)),
    };
    let members = roster_of(&["wedged-task-member"]);
    // `with_guard` installs the same thresholds the member's own
    // `run_guarded` would resolve, which is what the join reads to decide
    // when that member was due to be judged.
    let outcome: anyhow::Result<Vec<JoinedMember>> = crate::agent::idle::with_guard(guard, async {
        crate::agent::idle::run_guarded(guard, "turn joining a wedged task member", None, async {
            Ok(join_batch_members(&active_runs, &members).await)
        })
        .await
    })
    .await;

    let error = outcome
        .err()
        .expect("a join underwritten by nothing must not run forever");
    let terminated = error
        .downcast_ref::<crate::agent::idle::IdleHangTerminated>()
        .expect("the turn must end as a hang, not as a task failure");
    assert_eq!(terminated.reason, crate::agent::idle::HangReason::NoProgress);
}

/// The same member, still silent, but blocked on an operator decision. A
/// pending approval is a positive statement about where the run is, so the
/// join keeps vouching and the caller survives an idle window many times
/// shorter than the wait.
#[tokio::test]
async fn a_member_awaiting_an_operator_decision_still_vouches_for_the_waiter() {
    let mut awaiting = task_mode_batch_test_run("needs-approval", "batch-approval", SubAgentStatus::Running);
    awaiting.status = SubAgentStatus::AwaitingInput {
        prompt: "may I write the file?".to_string(),
    };
    let active_runs: Arc<RwLock<Vec<SubAgentRun>>> = Arc::new(RwLock::new(vec![awaiting]));
    let finisher = Arc::clone(&active_runs);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        let mut runs = finisher.write().await;
        if let Some(run) = runs.iter_mut().find(|run| run.id == "needs-approval") {
            run.status = SubAgentStatus::Completed("approved and finished".to_string());
        }
    });

    let guard = crate::agent::idle::IdleGuard {
        idle: Some(std::time::Duration::from_millis(300)),
    };
    let members = roster_of(&["needs-approval"]);
    let joined = crate::agent::idle::with_guard(guard, async {
        crate::agent::idle::run_guarded(guard, "turn joining a member awaiting input", None, async {
            Ok(join_batch_members(&active_runs, &members).await)
        })
        .await
    })
    .await
    .expect("a turn waiting on an operator decision must not be terminated as hung");

    assert!(
        matches!(
            joined.first().map(|member| &member.verdict),
            Some(JoinedVerdict::Completed(output)) if output == "approved and finished"
        ),
        "the join must return the member's real result"
    );
}

/// The complement of the test above: the liveness evidence is recorded on
/// the *caller's* beat, and only while a member is genuinely non-terminal.
/// Once the batch settles the join stops vouching for anything.
#[tokio::test]
async fn join_liveness_is_recorded_on_the_caller_and_stops_when_the_batch_settles() {
    let active_runs: Arc<RwLock<Vec<SubAgentRun>>> = Arc::new(RwLock::new(vec![batch_test_run(
        "member",
        "batch-liveness",
        SubAgentStatus::Running,
    )]));
    let caller_beat = crate::agent::idle::child_beat();
    let finisher = Arc::clone(&active_runs);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let mut runs = finisher.write().await;
        if let Some(run) = runs.iter_mut().find(|run| run.id == "member") {
            run.status = SubAgentStatus::Completed("done".to_string());
        }
    });

    let members = roster_of(&["member"]);
    let settled = crate::agent::idle::scope_beat(
        Some(Arc::clone(&caller_beat)),
        join_batch_members(&active_runs, &members),
    )
    .await;
    assert_eq!(settled.len(), 1);

    let events_at_return = caller_beat.events();
    assert!(
        events_at_return > 0,
        "waiting on a live member must refresh the caller's window"
    );
    assert_eq!(
        caller_beat.last_kind(),
        crate::agent::idle::ProgressKind::SubtaskAlive,
        "the evidence must name itself for the operator"
    );

    // Nothing is vouched for after the batch settles.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(
        caller_beat.events(),
        events_at_return,
        "a settled batch must stop refreshing the caller"
    );
}

/// Core evidence 4: what a kill of the parent *turn* reaches, and what it
/// does not.
///
/// The members are not children of the turn. Their registry parent is the
/// `sessions_spawn` tool call that launched them, and `ToolExecutionService`
/// retires that row when the call returns — so `collect_lineage` walking
/// down from the turn never reaches them. This test reproduces that shape
/// rather than asserting around it: the spawn runs inside a tool-call row
/// that is dropped before the kill, exactly as production does.
///
/// This used to be asserted the other way round, and passed only because the
/// test called `Tool::execute` directly and so left the turn itself as the
/// members' registry parent — a lineage that no production call ever has.
/// The batch kill is `prx tasks kill <batch-id>`, and that is the half of
/// this test that terminates anything.
///
/// MUTATION GUARD: drop the `register_tool_call` scope below and the members
/// become direct children of the turn again, so the first half goes red — a
/// green assertion about a lineage production never builds.
#[tokio::test]
async fn a_batch_outlives_a_kill_of_its_parent_turn_and_dies_on_its_batch_id() {
    let (ch, _) = RecordingChannel::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
        }),
    );

    let turn = crate::runtime::registry::register_turn("parent turn", "parent-turn-run", None);
    let parent_id = turn.id();
    let (batch_id, run_ids) = crate::runtime::registry::scoped(turn, async {
        // The production shape: every tool call gets its own registry row,
        // scoped so what it starts resolves it as the parent, and retired
        // when the call returns. `registry::scoped` is the same helper
        // `ToolExecutionService` uses, and dropping the guard here is the
        // link that breaks.
        let call = crate::runtime::registry::register_tool_call("sessions_spawn", None, None);
        let spawned = crate::runtime::registry::scoped(
            call,
            execute_admitted_spawn(
                &tool,
                with_spawn_grant(json!({
                    "action": "spawn_batch",
                    "tasks": ["one", "two", "three"],
                })),
            ),
        )
        .await
        .expect("spawn_batch tool call");
        let payload = parse_json(&spawned.output);
        let batch_id = payload["batch_id"].as_str().expect("a batch id").to_string();
        let run_ids = batch_run_ids(&payload);

        let killed = crate::runtime::registry::kill(parent_id, true).await;
        assert!(
            !killed
                .iter()
                .any(|result| result.kind == crate::runtime::registry::WorkKind::SubAgent),
            "the turn's lineage cannot contain the members: {killed:?}"
        );
        (batch_id, run_ids)
    })
    .await;
    assert_eq!(run_ids.len(), 3);

    // The members survive their launching turn, which is what makes a spawn
    // able to report back after the turn that asked for it has ended.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    {
        let runs = tool.active_runs_snapshot().await;
        assert_eq!(
            runs.iter()
                .filter(|run| matches!(run.status, SubAgentStatus::Running))
                .count(),
            3,
            "a kill of the parent turn must not reach rows that are not in its lineage"
        );
    }

    // The batch label is the address that does reach them: `prx tasks kill
    // <batch-id>`, which resolves the label to the member rows.
    let killed = crate::runtime::registry::kill_batch(&batch_id, true).await;
    assert_eq!(killed.len(), 3, "every member must be a target: {killed:?}");

    for run_id in &run_ids {
        let reason = await_terminal_reason(&tool.active_runs, run_id).await;
        assert!(
            reason.contains("terminated before it could report a result"),
            "a batch kill must reach every batch member, got: {reason}"
        );
    }

    let joined = tool
        .execute(json!({"action": "join", "batch_id": batch_id}))
        .await
        .expect("join tool call");
    let summary = parse_json(&joined.output);
    assert_eq!(summary["total"], 3, "{summary}");
    assert_eq!(summary["killed"].as_array().map(Vec::len), Some(3), "{summary}");
    gate.add_permits(8);
}

/// `--no-cascade` on a batch means "end the members, leave what they
/// started" — and for a process-mode member that used to mean nothing at
/// all. Its row published no abort handle (the monitor is the sole owner of
/// the OS child) and no token, so `request_termination` found no mechanism
/// and the CLI answered `not_killable`; only the default cascade reached it,
/// through the worker's own process row one level down.
///
/// The row's cooperative token now reaches the owner instead, so the
/// mechanism a `kill` action would have used is the mechanism the registry
/// uses. The owner below stands in for the real monitor at exactly the two
/// points that matter: it is the only thing that acts on the termination
/// request, and the registry row lives for precisely as long as it does.
///
/// MUTATION GUARD: pass `None` for the token in the process branch's
/// `register_sub_agent` call and the outcome below is `NotKillable`.
#[tokio::test]
async fn a_process_mode_member_answers_a_kill_of_its_own_row() {
    let control = ProcessRunControl::new_for_test();
    let batch_id = format!("batch-{}", Uuid::new_v4());
    let run_id = format!("run-{}", Uuid::new_v4());
    // The production entry point, not a copy of it: this is the same call
    // the process branch of `execute_spawn` makes.
    let work = register_killable_process_run("sub-agent (process)", &run_id, None, Some(&batch_id), &control);

    let owner_control = Arc::clone(&control);
    let owner = tokio::spawn(crate::runtime::registry::scoped(work, async move {
        let reason = owner_control.wait_for_termination_for_test().await;
        owner_control.finalize_for_test(ProcessFinalization::Terminated);
        reason
    }));

    let killed = crate::runtime::registry::kill_batch(&batch_id, false).await;
    assert_eq!(killed.len(), 1, "the member is the only target: {killed:?}");
    assert_eq!(
        killed[0].outcome,
        crate::runtime::registry::KillOutcome::Killed,
        "a process-mode member must be endable by its own row, not only by a cascade: {killed:?}"
    );
    assert_eq!(
        owner.await.expect("test: the owner task must not panic"),
        KILLED_BY_USER_REASON,
        "the owner must be asked to stop for the reason an operator kill records"
    );
    assert_eq!(control.finalization(), Some(ProcessFinalization::Terminated));
}

/// Core evidence 5: the join has no deadline.
///
/// Time is paused, so the runtime advances its clock as fast as the tasks
/// allow: this batch is waited on across a *virtual hour* and the join must
/// still return the member's real result. Any deadline shorter than an hour
/// — which is to say any deadline anyone would plausibly add — makes this
/// fail, and it costs milliseconds of real time to prove it.
///
/// MUTATION GUARD: wrap the `join_batch_members` call in a
/// `tokio::time::timeout` of any length and this test goes red.
#[tokio::test(start_paused = true)]
async fn a_join_survives_an_hour_of_waiting_because_it_has_no_deadline() {
    let active_runs: Arc<RwLock<Vec<SubAgentRun>>> = Arc::new(RwLock::new(vec![batch_test_run(
        "long-hauler",
        "batch-long",
        SubAgentStatus::Running,
    )]));
    let finisher = Arc::clone(&active_runs);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_hours(1)).await;
        let mut runs = finisher.write().await;
        if let Some(run) = runs.iter_mut().find(|run| run.id == "long-hauler") {
            run.status = SubAgentStatus::Completed("an hour of honest work".to_string());
        }
    });

    let members = roster_of(&["long-hauler"]);
    let settled = join_batch_members(&active_runs, &members).await;

    assert!(
        matches!(
            settled.first().map(|member| &member.verdict),
            Some(JoinedVerdict::Completed(output)) if output == "an hour of honest work"
        ),
        "a batch that takes an hour must still be joined, not abandoned"
    );
}

/// A member whose registry row is gone reports the absence instead of
/// hanging the join on a row that will never change again.
#[tokio::test]
async fn a_member_whose_row_disappeared_is_reported_rather_than_waited_on() {
    let active_runs: Arc<RwLock<Vec<SubAgentRun>>> = Arc::new(RwLock::new(vec![batch_test_run(
        "present",
        "batch-gap",
        SubAgentStatus::Completed("fine".to_string()),
    )]));
    let members = roster_of(&["present", "vanished"]);
    let settled = join_batch_members(&active_runs, &members).await;
    let summary = join_summary("batch-gap", &settled);
    assert_eq!(summary["total"], 2, "{summary}");
    assert_eq!(summary["completed"].as_array().map(Vec::len), Some(1), "{summary}");
    assert_eq!(summary["no_result"].as_array().map(Vec::len), Some(1), "{summary}");
}

/// Both wordings a kill can arrive as are classified as kills, and a
/// genuine task failure is not.
#[test]
fn a_kill_is_never_reported_as_an_ordinary_failure() {
    assert!(failure_is_kill(KILLED_BY_USER_REASON));
    assert!(failure_is_kill(&vanished_monitor_reason(
        &cancelled_join_error(),
        "sub-agent run"
    )));
    assert!(!failure_is_kill("the compiler rejected the patch"));
    assert!(!failure_is_kill("worker exited without result (killed by signal 9)"));
}

/// Every member of a joined batch appears in exactly one bucket.
#[test]
fn a_join_summary_accounts_for_every_member_exactly_once() {
    let settled = vec![
        JoinedMember {
            run_id: "a".to_string(),
            task: "a".to_string(),
            verdict: JoinedVerdict::Completed("out".to_string()),
        },
        JoinedMember {
            run_id: "b".to_string(),
            task: "b".to_string(),
            verdict: JoinedVerdict::Failed("boom".to_string()),
        },
        JoinedMember {
            run_id: "c".to_string(),
            task: "c".to_string(),
            verdict: JoinedVerdict::Killed(KILLED_BY_USER_REASON.to_string()),
        },
        JoinedMember {
            run_id: "d".to_string(),
            task: "d".to_string(),
            verdict: JoinedVerdict::NoResult("gone".to_string()),
        },
    ];
    let summary = join_summary("batch-x", &settled);
    assert_eq!(summary["total"], 4);
    let counted = ["completed", "failed", "killed", "no_result"]
        .iter()
        .filter_map(|bucket| summary[*bucket].as_array().map(Vec::len))
        .sum::<usize>();
    assert_eq!(counted, 4, "{summary}");
}

/// A cancelled `JoinHandle` error, for the classification test above.
fn cancelled_join_error() -> tokio::task::JoinError {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let handle = tokio::spawn(std::future::pending::<()>());
        handle.abort();
        handle.await.expect_err("an aborted task reports a cancellation")
    })
}

/// Batch entries inherit the batch-level defaults and may override them,
/// and the trusted per-turn scope travels with every member.
#[test]
fn batch_entries_inherit_batch_defaults_and_may_override_them() {
    let batch_args = json!({
        "action": "spawn_batch",
        "tasks": ["ignored"],
        "mode": "task",
        "model": "batch-model",
        "_zc_scope": {"channel": "signal", "chat_id": "+100"},
    });
    let member = SessionsSpawnTool::batch_member_args(&batch_args, &json!({"task": "t", "model": "member-model"}))
        .expect("a well-formed entry");
    assert_eq!(member["task"], "t");
    assert_eq!(member["model"], "member-model", "an entry overrides the batch default");
    assert_eq!(member["mode"], "task", "an entry inherits the batch default");
    assert_eq!(
        member["_zc_scope"], batch_args["_zc_scope"],
        "the trusted scope must travel with every member"
    );
    assert!(member.get("action").is_none() && member.get("tasks").is_none());

    let bare = SessionsSpawnTool::batch_member_args(&batch_args, &json!("just a string"))
        .expect("a bare task string is a valid entry");
    assert_eq!(bare["task"], "just a string");
    assert!(SessionsSpawnTool::batch_member_args(&batch_args, &json!(7)).is_err());
}

/// A batch entry is written by the model, so it must not be able to name
/// the runtime-only keys that decide *who* a sub-agent speaks as — and the
/// attempt has to be refused out loud, not dropped in silence, or a forged
/// scope looks exactly like an ordinary spawn.
#[test]
fn batch_entry_cannot_forge_a_trusted_scope_or_an_approval_grant() {
    let batch_args = json!({
        "action": "spawn_batch",
        "tasks": ["ignored"],
        "_zc_scope_trusted": true,
        "_zc_scope": {"sender": "+100", "channel": "signal", "chat_type": "dm", "chat_id": "+100"},
    });

    for forged in [
        "_zc_scope",
        "_zc_scope_trusted",
        "_prx_scope_trusted",
        crate::security::policy::RUNTIME_APPROVAL_GRANTED_ARG,
        crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG,
        "action",
        "an_unknown_key",
    ] {
        let mut fields = serde_json::Map::new();
        fields.insert("task".to_string(), json!("t"));
        fields.insert(forged.to_string(), json!("attacker-chosen-value"));
        let entry = serde_json::Value::Object(fields);

        let error = SessionsSpawnTool::batch_member_args(&batch_args, &entry)
            .expect_err("a key outside the member schema must be refused, not merged")
            .to_string();
        assert!(
            error.contains(forged),
            "the refusal names the offending key, got: {error}"
        );
        assert!(
            !error.contains("attacker-chosen-value"),
            "the refusal must not echo the key's value, got: {error}"
        );
    }

    // The inherited scope is still the batch's own, unforged.
    let member = SessionsSpawnTool::batch_member_args(&batch_args, &json!({"task": "t"})).expect("a well-formed entry");
    assert_eq!(member["_zc_scope"], batch_args["_zc_scope"]);
    assert_eq!(member["_zc_scope_trusted"], json!(true));
}

/// The list the merge enforces and the list the model is shown are one
/// list — derived, not restated, so they cannot drift.
#[test]
fn batch_member_overridable_keys_are_derived_from_the_advertised_entry_schema() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let schema = tool.parameters_schema();
    let advertised = schema["properties"]["tasks"]["items"]["anyOf"][1].clone();
    assert_eq!(
        advertised,
        SessionsSpawnTool::batch_member_entry_schema(),
        "the model is shown exactly the entry schema the merge enforces"
    );

    let declared = advertised["properties"]
        .as_object()
        .expect("the entry schema declares its properties")
        .keys()
        .cloned()
        .collect::<BTreeSet<String>>();
    assert_eq!(*SessionsSpawnTool::batch_member_overridable_keys(), declared);
    assert!(!declared.is_empty());
    assert!(
        declared.contains("recipient"),
        "recipient is a declared member parameter and stays model-settable"
    );
    for runtime_only in [
        "_zc_scope",
        "_zc_scope_trusted",
        "_prx_scope_trusted",
        crate::security::policy::RUNTIME_APPROVAL_GRANTED_ARG,
        crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG,
    ] {
        assert!(
            !declared.contains(runtime_only),
            "'{runtime_only}' is runtime-only and must never be declared as a member parameter"
        );
    }
}

/// Argument validation for the two new actions.
#[tokio::test]
async fn the_new_actions_reject_malformed_requests_without_starting_anything() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));

    let missing = execute_admitted_spawn(&tool, with_spawn_grant(json!({"action": "spawn_batch"})))
        .await
        .expect_err("the service contract must reject a missing tasks array");
    assert!(missing.to_string().contains("$.tasks"), "{missing}");
    let empty = execute_admitted_spawn(&tool, with_spawn_grant(json!({"action": "spawn_batch", "tasks": []})))
        .await
        .expect_err("the service contract must reject an empty tasks array");
    assert!(empty.to_string().contains("$.tasks"), "{empty}");

    let no_batch = tool.execute(json!({"action": "join"})).await;
    assert!(no_batch.is_err(), "join without a batch_id is a caller error");
    let unknown = tool
        .execute(json!({"action": "join", "batch_id": "batch-does-not-exist"}))
        .await
        .expect("tool call");
    assert!(!unknown.success, "{unknown:?}");
    assert!(tool.active_runs_snapshot().await.is_empty());
}

/// The two new actions must be discoverable, or the model can never use
/// them.
#[test]
fn schema_advertises_the_fan_out_and_join_actions() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let schema = tool.parameters_schema();
    let actions = schema["properties"]["action"]["enum"]
        .as_array()
        .expect("the action enum")
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>();
    assert!(actions.contains(&"spawn_batch"), "{actions:?}");
    assert!(actions.contains(&"join"), "{actions:?}");
    assert!(schema["properties"]["tasks"].is_object());
    assert!(schema["properties"]["batch_id"].is_object());
    assert!(tool.description().contains("spawn_batch") && tool.description().contains("join"));
}
