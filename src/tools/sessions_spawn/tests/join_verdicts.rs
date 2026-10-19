#![cfg(test)]

use super::*;

// ── b3: partial-success semantics ────────────────────────────

/// A provider whose behaviour is chosen by the task text it is handed, so a
/// single fan-out can hold a member that succeeds, one that concludes it
/// cannot, and members that stay live until the test ends them from
/// outside. Without this, a mixed batch would have to be assembled by
/// hand-writing registry rows, and the buckets would then only prove that
/// `join_summary` sorts strings.
struct TaskRoutingProvider {
    /// Members whose task carries [`Self::PARKS`] wait here and are never
    /// released.
    gate: Arc<tokio::sync::Semaphore>,
}

impl TaskRoutingProvider {
    const FAILS: &'static str = "MEMBER-CONCLUDES-FAILURE";
    const PARKS: &'static str = "MEMBER-STAYS-LIVE";
    const FAILURE_TEXT: &'static str = "I looked at this and it cannot be done";

    async fn route(&self, prompt: &str) -> anyhow::Result<String> {
        if prompt.contains(Self::FAILS) {
            anyhow::bail!(Self::FAILURE_TEXT);
        }
        if prompt.contains(Self::PARKS) {
            if let Ok(permit) = self.gate.acquire().await {
                drop(permit);
            }
        }
        Ok("member finished".to_string())
    }

    fn prompt_of(request: &crate::providers::ChatRequest<'_>) -> String {
        request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait::async_trait]
impl crate::providers::Provider for TaskRoutingProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.route(message).await
    }

    async fn chat(
        &self,
        request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        let text = self.route(&Self::prompt_of(&request)).await?;
        Ok(crate::providers::ChatResponse {
            text: Some(text),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    async fn chat_traced(
        &self,
        request: crate::providers::ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        let started_at = chrono::Utc::now();
        let text = self.route(&Self::prompt_of(&request)).await?;
        Ok(crate::providers::traits::ChatTrace {
            response: crate::providers::ChatResponse {
                text: Some(text),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            attempts: vec![crate::llm::route_decision::ProviderAttempt {
                seq: 1,
                provider: "routing".to_string(),
                model: model.to_string(),
                started_at,
                finished_at: chrono::Utc::now(),
                status: crate::llm::route_decision::AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "routing".to_string(),
            final_model: model.to_string(),
            served_member: None,
            tokens_used: crate::llm::route_decision::TokenUsage::default(),
        })
    }
}

/// The exact wording production stamps on a run whose worker was killed
/// before it wrote anything — derived from a *real* `SIGKILL`ed child
/// through the same two production functions the process-mode monitor uses
/// (`classify_worker_output` -> `exited_without_result_reason`), never
/// re-typed here.
#[cfg(unix)]
async fn silent_death_reason_from_a_real_sigkill() -> String {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("exec sleep 300")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("stand-in worker spawns");
    let pid = child.id().expect("live child pid");
    let killed = tokio::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .await
        .expect("kill -9 runs");
    assert!(killed.success(), "the stand-in worker must actually be killed");
    let status = child.wait().await.expect("the killed child is reaped");
    // The parent's whole observation: closed pipes, nothing written.
    match classify_worker_output(status, "", "") {
        Ok(ProcessWorkerOutcome::ExitedWithoutResult(detail)) => {
            assert!(detail.contains("signal 9"), "the fault must be named: {detail}");
            exited_without_result_reason(&detail)
        }
        Ok(_) => panic!("a killed worker must never classify as a finished or terminated run"),
        Err(error) => panic!("a killed worker must be a classified outcome, not an opaque error: {error}"),
    }
}

/// Core evidence 1: a batch that ends four different ways accounts for every
/// member exactly once, and `total` is the sum of the four buckets.
///
/// All four endings are produced by production code, not by hand-written
/// rows: the first two by the sub-agents themselves, the third by
/// `registry::kill` (the call behind `prx tasks kill`), the fourth by
/// classifying a genuinely `SIGKILL`ed child and committing the result
/// through `commit_run_termination_if_unfinished`, which is the same
/// function b1's monitor watchers use.
///
/// MUTATION GUARD: drop any one bucket from `join_summary` and the identity
/// assertion at the end fails.
#[cfg(unix)]
#[tokio::test]
async fn a_mixed_batch_accounts_for_every_member_in_exactly_one_bucket() {
    let silent_death_reason = silent_death_reason_from_a_real_sigkill().await;

    let (ch, _) = RecordingChannel::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(TaskRoutingProvider {
            gate: Arc::clone(&gate),
        }),
    );

    let spawned = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "action": "spawn_batch",
            "tasks": [
                "this one just works",
                format!("this one {}", TaskRoutingProvider::FAILS),
                format!("this one {} until it is killed", TaskRoutingProvider::PARKS),
                format!("this one {} until its worker dies", TaskRoutingProvider::PARKS),
            ],
        })),
    )
    .await
    .expect("spawn_batch tool call");
    assert!(spawned.success, "{spawned:?}");
    let payload = parse_json(&spawned.output);
    let batch_id = payload["batch_id"].as_str().expect("a batch id").to_string();
    let run_ids = batch_run_ids(&payload);
    assert_eq!(run_ids.len(), 4, "{payload}");

    let killed_id = run_ids[2].clone();
    let silent_id = run_ids[3].clone();

    // Ending #3: an operator kill that reaches the run through the registry
    // and never lets it write a result of its own.
    let work_id = crate::runtime::registry::resolve_address(&killed_id)
        .expect("a spawned run must be addressable in the work registry");
    let _ = crate::runtime::registry::kill(work_id, true).await;

    // Ending #4: the silent death. Committed exactly the way b1's watcher
    // commits one, with the wording a real SIGKILL produced above.
    let killed_reason = await_terminal_reason(&tool.active_runs, &killed_id).await;
    assert!(
        killed_reason.ends_with(TERMINATED_BEFORE_RESULT_SUFFIX),
        "a registry kill must land as a kill, not as something else: {killed_reason}"
    );
    assert!(
        commit_run_termination_if_unfinished(&tool.active_runs, &silent_id, &silent_death_reason).await,
        "the parked member must still be non-terminal when its worker dies"
    );

    let joined = tool
        .execute(json!({"action": "join", "batch_id": batch_id}))
        .await
        .expect("join tool call");
    assert!(
        joined.success,
        "partial success is data, not a tool failure: {joined:?}"
    );
    let summary = parse_json(&joined.output);

    let bucket = |name: &str| -> Vec<serde_json::Value> {
        summary[name]
            .as_array()
            .unwrap_or_else(|| panic!("join must always render a '{name}' bucket: {summary}"))
            .clone()
    };
    let completed = bucket("completed");
    let failed = bucket("failed");
    let killed = bucket("killed");
    let no_result = bucket("no_result");

    assert_eq!(completed.len(), 1, "one member simply worked: {summary}");
    assert_eq!(completed[0]["run_id"].as_str(), Some(run_ids[0].as_str()));
    assert_eq!(failed.len(), 1, "one member concluded a failure: {summary}");
    assert_eq!(failed[0]["run_id"].as_str(), Some(run_ids[1].as_str()));
    assert_eq!(killed.len(), 1, "one member was killed: {summary}");
    assert_eq!(killed[0]["run_id"].as_str(), Some(killed_id.as_str()));
    assert_eq!(no_result.len(), 1, "one member died silently: {summary}");
    assert_eq!(no_result[0]["run_id"].as_str(), Some(silent_id.as_str()));

    // The identity, spelled out: nothing is hidden, nothing is counted
    // twice, and `total` is not an independently maintained number.
    assert_eq!(summary["total"].as_u64(), Some(4), "{summary}");
    assert_eq!(
        summary["total"].as_u64(),
        Some((completed.len() + failed.len() + killed.len() + no_result.len()) as u64),
        "total must equal completed + failed + killed + no_result: {summary}"
    );
    gate.add_permits(8);
}

/// Core evidence 2: the two ways a run can stop without succeeding are not
/// the same thing, and `join` must not let a caller mistake one for the
/// other. A task that reported "this cannot be done" carries a judgement; a
/// task whose worker was `SIGKILL`ed never reached one.
///
/// MUTATION GUARD: fold `no_result` into `failed` — by making
/// `failure_gave_no_conclusion` return `false`, or by pushing `NoResult`
/// into the `failed` vector in `join_summary` — and this fails.
#[cfg(unix)]
#[tokio::test]
async fn a_silent_death_is_reported_apart_from_a_concluded_failure() {
    let silent_death_reason = silent_death_reason_from_a_real_sigkill().await;

    let (ch, _) = RecordingChannel::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(TaskRoutingProvider {
            gate: Arc::clone(&gate),
        }),
    );

    let spawned = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "action": "spawn_batch",
            "tasks": [
                format!("verdict member {}", TaskRoutingProvider::FAILS),
                format!("silent member {}", TaskRoutingProvider::PARKS),
            ],
        })),
    )
    .await
    .expect("spawn_batch tool call");
    let payload = parse_json(&spawned.output);
    let batch_id = payload["batch_id"].as_str().expect("a batch id").to_string();
    let run_ids = batch_run_ids(&payload);
    assert_eq!(run_ids.len(), 2, "{payload}");
    let verdict_id = run_ids[0].clone();
    let silent_id = run_ids[1].clone();

    assert!(
        commit_run_termination_if_unfinished(&tool.active_runs, &silent_id, &silent_death_reason).await,
        "the parked member must still be non-terminal when its worker dies"
    );

    let joined = tool
        .execute(json!({"action": "join", "batch_id": batch_id}))
        .await
        .expect("join tool call");
    let summary = parse_json(&joined.output);

    let failed = summary["failed"].as_array().expect("a failed bucket");
    let no_result = summary["no_result"].as_array().expect("a no_result bucket");

    assert_eq!(
        failed.len(),
        1,
        "the member that reached a verdict belongs there: {summary}"
    );
    assert_eq!(failed[0]["run_id"].as_str(), Some(verdict_id.as_str()));
    assert!(
        failed[0]["error"]
            .as_str()
            .is_some_and(|error| error.contains(TaskRoutingProvider::FAILURE_TEXT)),
        "a concluded failure must carry the sub-agent's own words: {summary}"
    );

    assert_eq!(
        no_result.len(),
        1,
        "the killed worker never reached a verdict: {summary}"
    );
    assert_eq!(no_result[0]["run_id"].as_str(), Some(silent_id.as_str()));
    assert!(
        no_result[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("signal 9")),
        "a silent death must name the fault instead of pretending to be a verdict: {summary}"
    );

    // And the crossings, stated directly: neither member appears in the
    // other's bucket.
    assert!(
        !failed
            .iter()
            .any(|entry| entry["run_id"].as_str() == Some(silent_id.as_str())),
        "a SIGKILLed member must never be reported as a task that decided it failed: {summary}"
    );
    assert!(
        !no_result
            .iter()
            .any(|entry| entry["run_id"].as_str() == Some(verdict_id.as_str())),
        "a task that reported a failure must never be reported as having said nothing: {summary}"
    );
    gate.add_permits(8);
}

/// Every wording production writes for a run that ended without a verdict
/// must classify as `no_result`, and an ordinary task failure must not.
///
/// Each input below is built by the same production function that writes it
/// on the row, so this cannot pass by agreeing with a literal that has
/// since drifted.
#[test]
fn every_silent_death_wording_is_recognised_as_a_missing_conclusion() {
    // b1: a process-mode worker that died on a signal.
    assert!(failure_gave_no_conclusion(&exited_without_result_reason(
        "killed by signal 9"
    )));
    // b1: the run's monitor task panicked instead of reporting.
    assert!(failure_gave_no_conclusion(&format!(
        "sub-agent run {PANICKED_BEFORE_RESULT_SUFFIX}"
    )));
    assert!(failure_gave_no_conclusion(&format!(
        "sub-agent process monitor {PANICKED_BEFORE_RESULT_SUFFIX}"
    )));
    // The process owner blew up while holding the OS child.
    assert!(failure_gave_no_conclusion(PROCESS_OWNER_PANICKED_PREFIX));
    assert!(failure_gave_no_conclusion(&format!(
        "{PROCESS_OWNER_PANICKED_PREFIX} while owning session-worker child"
    )));
    // The sub-agent's own hang detector ended it — rendered by the real
    // `IdleHangTerminated`, not by a hand-copied phrase.
    let terminated = crate::agent::idle::IdleHangTerminated {
        reason: crate::agent::idle::HangReason::NoProgress,
        label: Arc::from("sub-agent turn"),
        idle: std::time::Duration::from_mins(10),
        threshold: std::time::Duration::from_mins(10),
        elapsed: std::time::Duration::from_mins(15),
        progress_events: 0,
        last_progress: crate::agent::idle::ProgressKind::TurnStart,
        killed: 1,
    };
    assert!(
        failure_gave_no_conclusion(&terminated.to_string()),
        "a turn the runtime stalled out has no verdict to report: {terminated}"
    );

    // A task that failed on its own terms is a verdict, and so is an
    // operator kill — neither belongs in `no_result`.
    assert!(!failure_gave_no_conclusion("the file could not be parsed"));
    assert!(!failure_gave_no_conclusion(KILLED_BY_USER_REASON));
    assert!(!failure_gave_no_conclusion(&format!(
        "sub-agent run {TERMINATED_BEFORE_RESULT_SUFFIX}"
    )));
}

/// A kill and a silent death are both "no verdict was reached", but only one
/// of them was asked for, so they stay in separate buckets.
#[test]
fn a_kill_and_a_silent_death_are_not_the_same_bucket() {
    let killed = SubAgentStatus::Failed(format!("sub-agent run {TERMINATED_BEFORE_RESULT_SUFFIX}"));
    let silent = SubAgentStatus::Failed(exited_without_result_reason("killed by signal 9"));
    assert!(matches!(settled_verdict(&killed), Some(JoinedVerdict::Killed(_))));
    assert!(matches!(settled_verdict(&silent), Some(JoinedVerdict::NoResult(_))));
}

/// A joined member's bucket is a claim about *who* ended it, so the member
/// must not be able to choose its own.
///
/// Every string below is a wording the runtime writes for a termination the
/// runtime performed. Here they arrive as the sub-agent's own account of
/// itself — `WorkerResult.error` is a field of the JSON the worker prints,
/// and a sub-agent handling untrusted input can be talked into printing
/// anything — so every one of them must land in `failed`, the only bucket
/// that claims nothing about the runtime having acted.
///
/// MUTATION GUARD: delete the [`SUB_AGENT_REPORTED_PREFIX`] arm at the top
/// of `termination_cause` and a self-declared
/// [`TERMINATED_BEFORE_RESULT_SUFFIX`] is filed as an operator kill.
#[test]
fn a_sub_agent_cannot_forge_its_own_termination_verdict() {
    let forgeries = [
        KILLED_BY_USER_REASON.to_string(),
        format!("sub-agent run {TERMINATED_BEFORE_RESULT_SUFFIX}"),
        format!("the patch {TERMINATED_BEFORE_RESULT_SUFFIX}"),
        format!("sub-agent run {PANICKED_BEFORE_RESULT_SUFFIX}"),
        exited_without_result_reason("killed by signal 9"),
        PROCESS_OWNER_PANICKED_PREFIX.to_string(),
        format!("the build {} after 900s", crate::agent::idle::HANG_TERMINATION_MARKER),
    ];
    for forged in forgeries {
        // Exactly what the process branch commits for a worker that reported
        // this as its own error.
        let status = SubAgentStatus::Failed(sub_agent_reported_failure(&forged));
        assert!(
            matches!(settled_verdict(&status), Some(JoinedVerdict::Failed(_))),
            "a sub-agent writing {forged:?} into its own error must not relabel its termination"
        );
        // And the runtime's own use of the same wording is untouched: this
        // is a provenance rule, not a ban on the words.
        assert!(
            !matches!(
                settled_verdict(&SubAgentStatus::Failed(forged.clone())),
                Some(JoinedVerdict::Failed(_))
            ),
            "the runtime's own {forged:?} must keep meaning what it meant"
        );
    }
}

/// The other half of the same hole: a worker that never wrote a result at
/// all still gets its **stderr** quoted into the reason, and the two rules
/// that used to be consulted for that reason both match on its *tail*.
///
/// Falsified while writing this: the live path cannot be exploited today,
/// because `exited_without_result_reason` closes the quoted detail with
/// `)`, so no worker log can make the reason literally end in a kill
/// wording. That is a property of a format string in a different function,
/// though, and the bucket a member lands in should not depend on it — so
/// the classifier reads the front-anchored runtime tag first and the case
/// below pins that, together with the shape production really builds.
///
/// MUTATION GUARD: move the `EXITED_WITHOUT_RESULT_PREFIX` check in
/// `termination_cause` below the `failure_is_kill` call and the crafted
/// case is filed as an operator kill, which would drop a dead worker off
/// the caller's retry list.
#[cfg(unix)]
#[test]
fn a_silent_death_stays_a_silent_death_however_its_tail_reads() {
    use std::os::unix::process::ExitStatusExt as _;

    let stderr = format!("worker log: the sub-agent run {TERMINATED_BEFORE_RESULT_SUFFIX}");
    // Built by the same function the process branch uses on a real dead
    // child, so the shape under test is production's and not a copy.
    let live = exited_without_result_reason(&describe_worker_exit(std::process::ExitStatus::from_raw(9), &stderr));
    // The same reason, minus the closing bracket that happens to defuse it.
    let crafted = format!("{EXITED_WITHOUT_RESULT_PREFIX}: {stderr}");
    assert!(
        crafted.ends_with(TERMINATED_BEFORE_RESULT_SUFFIX),
        "the crafted case must really end in the kill wording: {crafted}"
    );
    for reason in [live, crafted] {
        assert!(
            matches!(
                settled_verdict(&SubAgentStatus::Failed(reason.clone())),
                Some(JoinedVerdict::NoResult(_))
            ),
            "a worker killed by a signal produced no verdict, whatever it logged: {reason}"
        );
    }
}

/// Task mode has the typed error in hand at the moment the run ends, so the
/// classification is made there, from the type, and the text is only the
/// explanation that comes with it.
///
/// MUTATION GUARD: make `task_failure_reason` return `error.to_string()`
/// and the third case files a tool error that merely quotes the hang
/// detector as `no_result`.
#[test]
fn a_task_mode_failure_is_classified_by_the_error_type_not_its_words() {
    let hang = crate::agent::idle::IdleHangTerminated {
        reason: crate::agent::idle::HangReason::NoProgress,
        label: Arc::from("sub-agent turn"),
        idle: std::time::Duration::from_mins(10),
        threshold: std::time::Duration::from_mins(10),
        elapsed: std::time::Duration::from_mins(15),
        progress_events: 0,
        last_progress: crate::agent::idle::ProgressKind::TurnStart,
        killed: 1,
    };
    let hung = SubAgentStatus::Failed(task_failure_reason(&anyhow::Error::new(hang)));
    assert!(
        matches!(settled_verdict(&hung), Some(JoinedVerdict::NoResult(_))),
        "the runtime's own hang detector reached no verdict"
    );

    let cancelled = SubAgentStatus::Failed(task_failure_reason(&anyhow::Error::new(
        crate::agent::turn_contracts::ToolLoopCancelled,
    )));
    assert!(
        matches!(settled_verdict(&cancelled), Some(JoinedVerdict::Killed(_))),
        "a cancelled loop is somebody ending the run, not the task's own verdict"
    );

    // The same words, but reached through an ordinary task failure: the type
    // says "the task failed", so the bucket does too.
    let impostor = anyhow::anyhow!(
        "tool `read_file` failed: the log says the previous turn {} and {}",
        crate::agent::idle::HANG_TERMINATION_MARKER,
        KILLED_BY_USER_REASON
    );
    let quoted = SubAgentStatus::Failed(task_failure_reason(&impostor));
    assert!(
        matches!(settled_verdict(&quoted), Some(JoinedVerdict::Failed(_))),
        "a tool error that merely quotes a termination is still a task failure: {quoted:?}"
    );
}

/// The identity holds for any mixture, not just the one the fan-out test
/// happens to produce.
#[test]
fn the_summary_total_always_equals_the_sum_of_its_buckets() {
    let statuses = [
        SubAgentStatus::Completed("done".into()),
        SubAgentStatus::Failed("could not be done".into()),
        SubAgentStatus::Failed(KILLED_BY_USER_REASON.into()),
        SubAgentStatus::Failed(format!("sub-agent run {TERMINATED_BEFORE_RESULT_SUFFIX}")),
        SubAgentStatus::Failed(exited_without_result_reason("killed by signal 9")),
        SubAgentStatus::Failed(format!("sub-agent run {PANICKED_BEFORE_RESULT_SUFFIX}")),
    ];
    let settled = statuses
        .iter()
        .enumerate()
        .map(|(index, status)| JoinedMember {
            run_id: format!("run-{index}"),
            task: format!("task {index}"),
            verdict: settled_verdict(status).expect("every status above is terminal"),
        })
        .collect::<Vec<_>>();
    let summary = join_summary("batch-identity", &settled);
    let count = |name: &str| summary[name].as_array().map_or(0, Vec::len);
    assert_eq!(summary["total"].as_u64(), Some(statuses.len() as u64));
    assert_eq!(
        statuses.len(),
        count("completed") + count("failed") + count("killed") + count("no_result"),
        "{summary}"
    );
    assert_eq!(count("completed"), 1, "{summary}");
    assert_eq!(count("failed"), 1, "{summary}");
    assert_eq!(count("killed"), 2, "{summary}");
    assert_eq!(count("no_result"), 2, "{summary}");
}

// ── b4: routing of the summary ───────────────────────────────

/// Core evidence 3: "do these three things in parallel, then tell me" must
/// put **one** message on the channel — the parent's summary — not four.
///
/// Every member here has a recipient and a live channel, so each of them
/// *could* announce; what stops them is the `announce` default that
/// `batch_id` flips. The parent then reports through the production
/// `message_send` tool on the very same channel object, so the count below
/// is the count a phone would show.
///
/// MUTATION GUARD: make `announce_result` default to `true` inside a batch
/// and this sees 4.
#[tokio::test]
async fn a_joined_fan_out_puts_one_summary_on_the_channel_not_one_message_per_member() {
    let (recorder, sent) = NamedRecordingChannel::new("wacli");
    let channel: Arc<dyn Channel> = recorder;
    let tool = make_tool(
        Arc::clone(&channel),
        Arc::new(EchoProvider {
            response: "raw member output".into(),
        }),
    );
    // A real destination: without one, members would stay silent for the
    // uninteresting reason that they have nowhere to send.
    tool.set_active_recipient("120363000000000000@g.us").await;

    let spawned = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "action": "spawn_batch",
            "tasks": ["check the weather", "check the calendar", "check the inbox"],
        })),
    )
    .await
    .expect("spawn_batch tool call");
    assert!(spawned.success, "{spawned:?}");
    let payload = parse_json(&spawned.output);
    let batch_id = payload["batch_id"].as_str().expect("a batch id").to_string();
    assert_eq!(payload["started"], 3, "{payload}");
    assert!(
        payload["spawned"][0]["run_id"].is_string() && !spawned.output.contains("Will announce result when complete."),
        "a batch member must not promise an announcement it will not make: {payload}"
    );

    let joined = tool
        .execute(json!({"action": "join", "batch_id": batch_id}))
        .await
        .expect("join tool call");
    let summary = parse_json(&joined.output);
    assert_eq!(summary["total"], 3, "{summary}");
    assert_eq!(summary["completed"].as_array().map(Vec::len), Some(3), "{summary}");

    // A member announces *after* it publishes its terminal status, so the
    // join can return first. Waiting here is what makes the mutation
    // visible: with announcing left on, three messages land in this window.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        sent.lock().await.is_empty(),
        "no member of a batch may report on the channel by itself: {:?}",
        sent.lock().await
    );

    // The parent turn now says one thing, in its own words, through the
    // same tool an agent would use.
    let reporter = crate::tools::message_send::MessageSendTool::new(Arc::clone(&channel), test_security());
    let reported = reporter
        .execute(json!({
            "message": format!("All three are done ({} of {}).", summary["completed"].as_array().map_or(0, Vec::len), summary["total"]),
            "target": "120363000000000000@g.us",
        }))
        .await
        .expect("message_send tool call");
    assert!(reported.success, "{reported:?}");

    let messages = sent.lock().await;
    assert_eq!(
        messages.len(),
        1,
        "one request, one message: the summary — not one per sub-task plus a summary: {messages:?}"
    );
    assert!(messages[0].contains("All three are done"), "{messages:?}");
    assert!(
        !messages[0].contains("raw member output"),
        "the channel must carry the parent's summary, not a member's raw output: {messages:?}"
    );
}

/// Core evidence 4: nothing above may change what a plain `spawn` does. It
/// still announces its own result, on its own channel, with the same
/// confirmation text.
#[tokio::test]
async fn a_standalone_spawn_announces_exactly_as_before() {
    let (recorder, sent) = NamedRecordingChannel::new("wacli");
    let channel: Arc<dyn Channel> = recorder;
    let tool = make_tool(
        Arc::clone(&channel),
        Arc::new(EchoProvider {
            response: "solo result".into(),
        }),
    );
    tool.set_active_recipient("120363000000000000@g.us").await;

    let result = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "do the one thing"})))
        .await
        .expect("spawn tool call");
    assert!(result.success, "{result:?}");
    assert!(
        result.output.starts_with("Sub-agent spawned (run_id: "),
        "the confirmation text is unchanged: {}",
        result.output
    );
    assert!(
        result.output.ends_with("). Will announce result when complete."),
        "the confirmation text is unchanged: {}",
        result.output
    );

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let messages = sent.lock().await;
    assert_eq!(messages.len(), 1, "a standalone spawn still announces: {messages:?}");
    assert!(messages[0].contains("solo result"), "{messages:?}");
}

/// The switch is a switch for a standalone `spawn`: it may turn its own
/// announcement off and still say where the result went.
#[tokio::test]
async fn announce_is_an_explicit_argument_for_a_standalone_spawn() {
    let (recorder, sent) = NamedRecordingChannel::new("wacli");
    let channel: Arc<dyn Channel> = recorder;
    let tool = make_tool(
        Arc::clone(&channel),
        Arc::new(EchoProvider {
            response: "quiet result".into(),
        }),
    );
    tool.set_active_recipient("120363000000000000@g.us").await;

    let silenced = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({"task": "do it quietly", "announce": false})),
    )
    .await
    .expect("spawn tool call");
    assert!(silenced.success, "{silenced:?}");
    assert!(
        silenced.output.contains("collect it with the 'join' action"),
        "a run that will not announce must say where its result goes: {}",
        silenced.output
    );

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        sent.lock().await.is_empty(),
        "the standalone spawn that switched announcing off must stay quiet: {:?}",
        sent.lock().await
    );
}

/// "One message, not four" was a *default* before this: `announce` was a
/// per-member parameter, and the whitelist that decides what a member entry
/// may set is derived from that schema — so the model could switch a
/// member's own announcement back on and get the fan-out's raw output on
/// the channel plus the summary after it.
///
/// Both routes are closed here, and they are closed differently on purpose:
/// a member entry naming `announce` is **refused**, visibly, in `rejected`,
/// while a batch-level `announce` simply does not reach members — it is a
/// legal argument of the `spawn` this call is not making.
///
/// MUTATION GUARD: drop the `batch_id.is_none() &&` from `announce_result`
/// and the second half sees two member announcements.
#[tokio::test]
async fn a_batch_member_cannot_turn_its_own_announcement_back_on() {
    let (recorder, sent) = NamedRecordingChannel::new("wacli");
    let channel: Arc<dyn Channel> = recorder;
    let tool = make_tool(
        Arc::clone(&channel),
        Arc::new(EchoProvider {
            response: "raw member output".into(),
        }),
    );
    tool.set_active_recipient("120363000000000000@g.us").await;

    // Route 1: the per-member entry.
    let refused = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "action": "spawn_batch",
            "tasks": [{"task": "report for myself", "announce": true}],
        })),
    )
    .await
    .expect_err("the published member schema must reject an announce override");
    assert!(
        refused.to_string().contains("$.tasks[0]"),
        "the refusal must identify the offending member: {refused}"
    );
    assert!(tool.active_runs_snapshot().await.is_empty());

    // Route 2: the batch-level argument, inherited by every member.
    let batch = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "action": "spawn_batch",
            "announce": true,
            "tasks": ["check the weather", "check the calendar"],
        })),
    )
    .await
    .expect("spawn_batch tool call");
    assert!(batch.success, "{batch:?}");
    let payload = parse_json(&batch.output);
    assert_eq!(payload["started"], 2, "{payload}");
    assert!(
        !batch.output.contains("Will announce result when complete."),
        "a member must not promise an announcement it will not make: {payload}"
    );
    let batch_id = payload["batch_id"].as_str().expect("a batch id").to_string();

    let joined = tool
        .execute(json!({"action": "join", "batch_id": batch_id}))
        .await
        .expect("join tool call");
    assert_eq!(
        parse_json(&joined.output)["completed"].as_array().map(Vec::len),
        Some(2)
    );

    // Members announce after publishing their terminal status, so the join
    // returns first; this window is what makes the mutation visible.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        sent.lock().await.is_empty(),
        "no member of a batch may report on the channel by itself: {:?}",
        sent.lock().await
    );
}

/// The switch has to be discoverable or the model can never use it, and
/// what it does *not* reach has to be stated where the model reads it.
#[test]
fn schema_advertises_the_announce_switch_and_its_batch_default() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let schema = tool.parameters_schema();
    assert_eq!(schema["properties"]["announce"]["type"].as_str(), Some("boolean"));
    let description = schema["properties"]["announce"]["description"]
        .as_str()
        .expect("the announce switch is documented");
    assert!(description.contains("spawn_batch"), "{description}");
    assert!(description.contains("join"), "{description}");
    // But not per-member: the whitelist a member entry is checked against
    // is derived from this object, so advertising the key here would be the
    // same thing as letting a member set it.
    //
    // MUTATION GUARD: put `"announce"` back into
    // `batch_member_entry_schema` and this fails.
    let member = &schema["properties"]["tasks"]["items"]["anyOf"][1]["properties"];
    assert!(
        member.get("announce").is_none(),
        "a batch member has no announcement of its own to switch: {member}"
    );
    assert!(member["task"]["type"].as_str() == Some("string"), "{member}");
}
