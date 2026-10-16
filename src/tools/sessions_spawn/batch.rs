use super::{
    SessionsSpawnTool, SubAgentRun, SubAgentStatus, TerminationCause, batch_journal, boxed_spawn_field,
    parse_spawn_scope, termination_cause,
};
use crate::tools::sessions_read_model;
use crate::tools::traits::ToolResult;
use anyhow::Context as _;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;
use uuid::Uuid;

/// One member of a fan-out, as it was launched.
#[derive(Debug, Clone)]
pub(super) struct BatchMember {
    pub(super) run_id: String,
    /// What the member was asked to do, kept here so a member whose registry
    /// row is gone is still reported as something more useful than an id.
    pub(super) task: String,
}

/// The membership of one fan-out.
#[derive(Debug, Clone)]
pub(super) struct BatchRoster {
    pub(super) batch_id: String,
    /// The owner the batch was launched for (OWNER-VIS): `join` answers a
    /// batch only to a caller whose run scope admits this owner.
    pub(super) owner_id: Option<String>,
    pub(super) members: Vec<BatchMember>,
}

/// How many fan-out rosters one tool instance remembers.
///
/// A roster is small and a batch is normally joined within the turn that
/// launched it, so this is a leak guard rather than a policy. Eviction is
/// oldest-first, and a join on an evicted batch falls back to the registry scan
/// this field replaced — the pre-existing behaviour, not a new failure.
const MAX_TRACKED_BATCH_ROSTERS: usize = 256;

/// Terminal verdict for one member of a joined batch.
///
/// `Killed` is deliberately not folded into `Failed`: a task that concluded
/// "this cannot be done" and a task an operator stopped mid-flight mean opposite
/// things to whoever reads the summary.
pub(super) enum JoinedVerdict {
    Completed(String),
    /// The task looked at the problem and concluded it could not be done. The
    /// string is the sub-agent's own account of why, which is exactly why it
    /// is tagged with [`SUB_AGENT_REPORTED_PREFIX`] on the way in: this is the
    /// one bucket whose contents the run itself gets to write.
    Failed(String),
    Killed(String),
    /// The run produced no conclusion at all: its worker died on a signal, its
    /// monitor panicked, its own hang detector ended it, its deadline expired,
    /// or its registry row is simply gone. Kept apart from `Failed` because a
    /// caller reading a failure reasonably believes a judgement was made, and
    /// here none ever was — see [`failure_gave_no_conclusion`].
    NoResult(String),
}

/// One settled member of a joined batch.
pub(super) struct JoinedMember {
    pub(super) run_id: String,
    pub(super) task: String,
    pub(super) verdict: JoinedVerdict,
}

/// How often the join wait re-reads the batch.
///
/// This is a *sampling* interval, not a deadline. `join_batch_members` has no
/// elapsed-time check, no iteration cap and no `tokio::time::timeout`: shorten
/// this and a finished batch surfaces sooner, lengthen it and the only cost is
/// latency. The tests in this module pin that difference — one of them runs a
/// batch across an hour of (virtual) time and still requires the real result.
const JOIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Render the settled batch as the structured summary `join` returns.
///
/// Structured rather than concatenated prose because the four buckets mean
/// different things to whoever reads them, and a caller deciding what to retry
/// has to be able to tell them apart without parsing English.
///
/// Two things keep `total` honest: the exhaustive match below, which gives
/// every verdict exactly one bucket and makes a new variant a compile error
/// here, and the debug assertion, which catches a bucket that is filled but
/// never rendered. Neither is decoration — a member that quietly fits nowhere
/// would disappear from the caller's account of its own fan-out.
pub(super) fn join_summary(batch_id: &str, settled: &[JoinedMember]) -> serde_json::Value {
    let mut completed = Vec::new();
    let mut failed = Vec::new();
    let mut killed = Vec::new();
    let mut no_result = Vec::new();
    for member in settled {
        match &member.verdict {
            JoinedVerdict::Completed(output) => {
                completed.push(json!({"run_id": member.run_id, "task": member.task, "output": output}));
            }
            JoinedVerdict::Failed(error) => {
                failed.push(json!({"run_id": member.run_id, "task": member.task, "error": error}));
            }
            JoinedVerdict::Killed(reason) => {
                killed.push(json!({"run_id": member.run_id, "task": member.task, "reason": reason}));
            }
            JoinedVerdict::NoResult(reason) => {
                no_result.push(json!({"run_id": member.run_id, "task": member.task, "reason": reason}));
            }
        }
    }
    let total = settled.len();
    debug_assert_eq!(
        total,
        completed.len() + failed.len() + killed.len() + no_result.len(),
        "every joined member must appear in exactly one bucket"
    );
    json!({
        "batch_id": batch_id,
        "total": total,
        "completed": completed,
        "failed": failed,
        "killed": killed,
        "no_result": no_result,
    })
}

/// Classify a run's current status, or `None` while it is still working.
pub(super) fn settled_verdict(status: &SubAgentStatus) -> Option<JoinedVerdict> {
    match status {
        // `AwaitingInput` is reversible by design (the operator approves,
        // denies, or the request lapses), so it is *not* terminal and the join
        // keeps waiting through it.
        SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => None,
        SubAgentStatus::Completed(output) => Some(JoinedVerdict::Completed(output.clone())),
        // One dispatch on the reason's provenance rather than a chain of
        // guards, so the precedence between the three causes lives in exactly
        // one place — [`termination_cause`] — and cannot be reordered here by
        // accident.
        SubAgentStatus::Failed(reason) => Some(match termination_cause(reason) {
            TerminationCause::Killed => JoinedVerdict::Killed(reason.clone()),
            TerminationCause::NoConclusion => JoinedVerdict::NoResult(reason.clone()),
            TerminationCause::SelfReported => JoinedVerdict::Failed(reason.clone()),
        }),
    }
}

/// Block until every member of the batch has reached a terminal status.
///
/// # Why this has no deadline
///
/// A deadline here would be a wall-clock turn timeout wearing a different hat:
/// it would end a fan-out because it took long, and duration is not a fault in
/// this runtime. What actually bounds the wait is that *every member is bounded
/// on its own terms* — a task-mode member runs under
/// [`crate::agent::idle::run_guarded`], and a process-mode member's worker
/// installs the same thresholds in its own process. Every ending commits a
/// terminal status on the member's row rather than leaving it `Running`. When
/// the last member ends, this returns; there is no third outcome to time out
/// on.
///
/// That bound is real but conditional, and the condition is worth stating: a
/// member's watchdog only fires while the runtime it lives in still schedules.
/// A worker stopped by a signal, wedged in a blocking call, or hung before it
/// reached [`crate::agent::idle::install`] has no watchdog left to end it. What
/// covers *that* is not a deadline here — it is that the join stops vouching for
/// members it can no longer account for, so the caller's own detector reaches a
/// verdict. See [`member_vouches_for_the_waiter`] for which members those are,
/// and for the one class it cannot yet tell apart.
///
/// # Why it stamps the caller's beat, and why not on every poll
///
/// The caller is parked here, so it emits nothing of its own, and its own hang
/// detector would eventually judge it wedged. For task-mode members that never
/// happens while they work — their beats are parented on the caller's, so their
/// progress is the caller's progress — but that link only exists when the same
/// turn both spawned and joined, so the wait re-reads their beats rather than
/// relying on it. A process-mode member has no such link at all: the only signal
/// that crosses the process boundary is the bytes the worker writes, and a
/// healthy `session-worker` writes nothing on either pipe until the single
/// result line at the very end. Waiting on one is indistinguishable from
/// hanging.
///
/// So the wait itself supplies the evidence, as
/// [`crate::agent::idle::ProgressKind::SubtaskAlive`]. What it must *not* do is
/// supply it merely because some member's row has not been stamped terminal
/// yet: that is the absence of an ending, not the presence of work, and a turn
/// whose window is refreshed by it is immortal for as long as one member fails
/// to die — which in a runtime with no wall-clock turn budget means the idle
/// detector, the only automatic recovery there is, is switched off for the whole
/// join. [`member_vouches_for_the_waiter`] is the rule that keeps the evidence
/// honest; when no member satisfies it, this loop keeps polling but records
/// nothing, and the caller's own detector is free to reach its verdict.
pub(super) async fn join_batch_members(
    active_runs: &Arc<RwLock<Vec<SubAgentRun>>>,
    members: &[BatchMember],
) -> Vec<JoinedMember> {
    // Read once per join rather than per poll: this is the same source a
    // task-mode member's own `run_guarded` resolves its window from, so the two
    // cannot disagree about when that member was due to be judged.
    let window = crate::agent::idle::configured().idle;
    loop {
        // The guard is bound and dropped inside this block: the wait below must
        // never hold the registry lock, or a member trying to publish its own
        // terminal status would be blocked by the very join waiting for it.
        let vouched = {
            let runs = active_runs.read().await;
            if let Some(settled) = settled_batch(&runs, members) {
                return settled;
            }
            batch_vouches_for_the_waiter(&runs, members, window)
        };
        if vouched {
            // MUTATION GUARD: this line is the whole reason a turn joining a
            // process-mode fan-out is not killed as hung. Remove it and a batch
            // whose members produce no in-band signal takes the joining turn
            // down with it after `[runtime] idle_hang_secs`.
            crate::agent::idle::beat(crate::agent::idle::ProgressKind::SubtaskAlive);
        }
        tokio::time::sleep(JOIN_POLL_INTERVAL).await;
    }
}

/// Whether any member of the batch is still evidence that the waiting turn is
/// alive.
///
/// A member whose row has vanished contributes nothing: it is not something the
/// join is waiting on, it is something [`settled_batch`] already reports as
/// having no result.
fn batch_vouches_for_the_waiter(
    runs: &[SubAgentRun],
    members: &[BatchMember],
    window: Option<std::time::Duration>,
) -> bool {
    members.iter().any(|member| {
        runs.iter()
            .find(|run| run.id == member.run_id)
            .is_some_and(|run| member_vouches_for_the_waiter(run, window))
    })
}

/// Whether this member may be recorded as
/// [`crate::agent::idle::ProgressKind::SubtaskAlive`] on the waiting turn's
/// beat.
///
/// The question is deliberately *not* "is this member still running": every row
/// starts `Running` and stays that way until somebody commits an ending, so
/// answering yes to that is answering "nobody has told me it stopped", and a
/// turn kept alive by that is kept alive by silence. The question asked here is
/// "can I still account for this member", and it has three answers:
///
/// * **Blocked on a human.** `AwaitingInput` is a positive statement about
///   where the run is — an operator has been asked something and has not
///   answered — so it vouches. A join that gave up on a pending approval would
///   be terminating a turn for the operator's slowness.
/// * **Observable, and within its own window.** A task-mode member runs in this
///   process under [`crate::agent::idle::run_guarded`] with the same `window`,
///   and every provider chunk, tool call and compaction stamps the beat this
///   reads. While that beat is fresher than the window, the member is visibly
///   working. Once it is not, the member's own watchdog was due and did not
///   fire, so the premise that "every member is bounded on its own terms" is
///   false for this member and the join must stop underwriting it. `window` of
///   `None` means idle detection is switched off process-wide, so there is no
///   verdict to protect and nothing to withhold.
/// * **Not observable at all.** A process-mode member is byte-silent by
///   construction, so its beat's age says nothing and applying the rule above
///   would kill the parents of perfectly healthy fan-outs. It vouches on its
///   non-terminal status — the one place this function still trusts an absence.
///   That blind spot is bounded by the worker's own in-process watchdog and by
///   its in-process idle watchdog, and closing it properly needs a liveness frame
///   the worker's one-line stdout protocol does not carry today.
///
/// MUTATION GUARD: return `true` unconditionally and
/// `a_join_stops_vouching_for_an_observable_member_that_went_silent` goes green
/// on a turn that should have been terminated; return `false` for the
/// process-mode arm and `a_join_survives_a_real_child_process_that_never_writes_anything`
/// kills a turn whose child is working.
fn member_vouches_for_the_waiter(run: &SubAgentRun, window: Option<std::time::Duration>) -> bool {
    if matches!(run.status, SubAgentStatus::AwaitingInput { .. }) {
        return true;
    }
    if run.process_control.is_some() {
        return true;
    }
    window.is_none_or(|window| run.idle_for() < window)
}

/// The batch's verdicts, or `None` while any member is still working.
///
/// Deliberately two passes under one lock rather than one pass that builds as
/// it goes: a finished member's output can be large, and a join that polls for
/// an hour would otherwise clone all of them on every poll only to throw them
/// away. Building only once the answer is final also means the verdicts come
/// from a single consistent snapshot, so a member cannot be seen `Completed`
/// on the deciding pass and be gone by the reporting one.
fn settled_batch(runs: &[SubAgentRun], members: &[BatchMember]) -> Option<Vec<JoinedMember>> {
    let all_settled = members.iter().all(|member| {
        runs.iter()
            .find(|run| run.id == member.run_id)
            // A row that is gone is settled in the only sense that matters
            // here: nothing about it will ever change again.
            .is_none_or(|run| !matches!(run.status, SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. }))
    });
    if !all_settled {
        return None;
    }
    Some(
        members
            .iter()
            .map(|member| {
                runs.iter().find(|run| run.id == member.run_id).map_or_else(
                    // The roster names this member but the registry no longer
                    // holds it — retired by the chat session reaper, or dropped
                    // by a session shutdown. Reporting it as a member without a
                    // result is the honest verdict, and it is the whole reason
                    // the roster is the membership: derived from the registry,
                    // this member would not have been in the batch at all.
                    || JoinedMember {
                        run_id: member.run_id.clone(),
                        task: member.task.clone(),
                        verdict: JoinedVerdict::NoResult(
                            "no confirmed registry result is available for this recorded batch member".to_string(),
                        ),
                    },
                    |run| JoinedMember {
                        run_id: member.run_id.clone(),
                        task: run.task.clone(),
                        // Non-terminal statuses were ruled out above; a status
                        // that slipped through is reported as the absence it is
                        // rather than panicking inside a tool call.
                        verdict: settled_verdict(&run.status).unwrap_or_else(|| {
                            JoinedVerdict::NoResult("the run was still working when the batch settled".to_string())
                        }),
                    },
                )
            })
            .collect(),
    )
}

impl SessionsSpawnTool {
    /// What one `spawn_batch` member entry may carry — declared exactly once.
    ///
    /// This is both what the model is told (it is inlined into `tasks.items`
    /// by [`SessionsSpawnTool::parameters_schema`]) and what the merge in
    /// [`Self::batch_member_args`] enforces, via the key set derived from it
    /// in [`Self::batch_member_overridable_keys`]. One declaration, so the
    /// advertised contract and the enforced one cannot drift apart.
    ///
    /// `announce` is deliberately **absent**. A fan-out's account of itself is
    /// the single summary its caller sends after `join`; a member that could
    /// switch its own announcement on would turn that into N+1 messages, and
    /// because the whitelist is derived from this object, listing the key here
    /// is the same thing as letting the model set it. A run that should report
    /// for itself is a plain `spawn`.
    pub(super) fn batch_member_entry_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task": {"type": "string", "minLength": 1},
                "agent": {"type": "string"},
                "model": {"type": "string"},
                "provider": {"type": "string"},
                "mode": {"type": "string", "enum": ["task", "process"]},
                "recipient": {"type": "string"},
            },
            "required": ["task"]
        })
    }

    /// The keys a member entry may set, read off [`Self::batch_member_entry_schema`].
    ///
    /// Derived rather than restated: a parameter added to the entry schema is
    /// accepted here the same day, and — the point of it — a *runtime* field
    /// added elsewhere is refused here without anyone remembering to extend a
    /// denylist. `_zc_scope`, `_zc_scope_trusted`, `_prx_scope_trusted` and
    /// the two approval-grant keys are not in the schema, so they are not in
    /// this set, so a member entry cannot set them.
    ///
    /// A schema that stopped exposing `properties` would yield the empty set
    /// and refuse every entry: the failure direction is refusal, not silent
    /// acceptance.
    pub(super) fn batch_member_overridable_keys() -> &'static BTreeSet<String> {
        static KEYS: OnceLock<BTreeSet<String>> = OnceLock::new();
        KEYS.get_or_init(|| {
            Self::batch_member_entry_schema()
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .map(|properties| properties.keys().cloned().collect())
                .unwrap_or_default()
        })
    }

    /// Merge one batch entry over the batch-level arguments.
    ///
    /// The batch-level object carries the trusted per-turn scope and the
    /// approval grant for this tool call, so members inherit them by
    /// construction. Inheritance is only half the story, and the half that
    /// was reasoned about here before: the entry is written by the model, so
    /// any key it names *overwrites* what was inherited. `_zc_scope_trusted`
    /// is the single boolean [`parse_spawn_scope`] consults, and `_zc_scope`
    /// beside it names the sender, channel and chat id the sub-agent then
    /// speaks as — an entry allowed to set those picks its own identity and
    /// its own recipient, on a channel this turn never came from.
    ///
    /// So the merge is a whitelist, not a denylist: an entry may set only the
    /// per-member parameters the `spawn` schema declares, and any other key is
    /// an **error** rather than a silent drop. Dropping it would let a forged
    /// scope fail quietly and look like an ordinary spawn; refusing it puts
    /// the attempt in the caller's `rejected` list where it can be seen. The
    /// message names the offending key and never its value, which is
    /// model-authored content.
    ///
    /// `action`/`tasks` are dropped because they describe the batch, not a
    /// member, and every remaining batch-level key acts as a default that the
    /// member entry may override.
    pub(super) fn batch_member_args(
        batch_args: &serde_json::Value,
        entry: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let mut member = batch_args
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("sessions_spawn arguments must be a JSON object"))?;
        member.remove("action");
        member.remove("tasks");
        match entry {
            // A bare string is the common case ("run these three tasks") and
            // means exactly `{"task": "..."}`.
            serde_json::Value::String(task) => {
                member.insert("task".to_string(), json!(task));
            }
            serde_json::Value::Object(fields) => {
                let overridable = Self::batch_member_overridable_keys();
                for (key, value) in fields {
                    // MUTATION GUARD: delete this refusal and a member entry
                    // carrying `_zc_scope_trusted`/`_zc_scope` overwrites the
                    // batch's real scope, choosing its own sender, channel and
                    // recipient.
                    if !overridable.contains(key) {
                        anyhow::bail!(
                            "entry key '{key}' is not a per-member parameter of 'spawn'; a batch entry may only set: {}",
                            overridable.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
                        );
                    }
                    member.insert(key.clone(), value.clone());
                }
            }
            other => anyhow::bail!("each entry of 'tasks' must be a string or an object, got: {other}"),
        }
        Ok(serde_json::Value::Object(member))
    }

    /// Fan out: start every task in `tasks` and label them with one batch id.
    ///
    /// Every member starts immediately — there is no concurrency cap, no
    /// staging, and no queue. Each member goes through [`Self::execute_spawn`],
    /// so it is registered and gated exactly as a standalone `spawn` would be;
    /// the only differences are the `batch_id` on its row and that it does not
    /// announce its own result (see `announce_result` there) — the fan-out's
    /// account of itself is the one summary its caller sends after `join`.
    ///
    /// A member that cannot be started is reported in `rejected` rather than
    /// aborting the fan-out: the caller asked for N independent tasks, and one
    /// unusable agent name is not a reason to withhold the other N-1.
    pub(super) async fn execute_spawn_batch(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let Some(entries) = args.get("tasks").and_then(|value| value.as_array()) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing 'tasks' array for spawn_batch action".into()),
            });
        };
        if entries.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'tasks' must contain at least one task for spawn_batch action".into()),
            });
        }

        let batch_caller = crate::tools::execution::current_child_call_context();
        if let Some(caller) = &batch_caller {
            caller.verify_tool("sessions_spawn")?;
        }
        let batch_owner = batch_caller
            .as_ref()
            .map(|caller| caller.context().envelope.resolved_owner_id())
            .or_else(|| parse_spawn_scope(args).and_then(|scope| scope.owner_id));
        let batch_id = format!("batch-{}", Uuid::new_v4());
        let mut member_args = Vec::with_capacity(entries.len());
        let mut rejected = Vec::new();
        for entry in entries {
            match Self::batch_member_args(args, entry) {
                Ok(built) => member_args.push((Uuid::new_v4().to_string(), built)),
                Err(error) => rejected.push(json!({"task": entry, "error": error.to_string()})),
            }
        }

        let prepared = if let Some(grant) = crate::runtime::child_source::outgoing_grant() {
            let fabric = grant.source_fabric().clone();
            Some(batch_journal::PreparedBatch::prepare(fabric, &grant, &batch_id, &member_args).await?)
        } else if let Some(caller) = crate::tools::execution::current_child_call_context() {
            caller.verify_tool("sessions_spawn")?;
            if let Some(publication) = caller.context().ordinary_child_publication.as_ref() {
                batch_journal::PreparedBatch::prepare_ordinary(publication, caller.context(), &batch_id, &member_args)
                    .await?
            } else {
                None
            }
        } else {
            None
        };

        // Launched together rather than one after another: `execute_spawn`
        // awaits (memory events, config hashing for process mode) before it
        // returns, and a caller that asked for a fan-out should not pay for
        // those serially. All of these futures are polled on *this* task, which
        // is what keeps `child_beat`'s task-local parent link intact.
        let outcomes = futures_util::future::join_all(member_args.iter().map(|(run_id, member)| {
            boxed_spawn_field(|| self.execute_spawn_with_id(member, Some(&batch_id), Some(run_id)))
        }))
        .await;

        let mut dispatched = Vec::with_capacity(outcomes.len());
        for ((reserved, _), outcome) in member_args.iter().zip(&outcomes) {
            dispatched.push(match outcome {
                Ok(outcome) => match &outcome.run_id {
                    Some(actual) => {
                        anyhow::ensure!(actual == reserved, "batch member run differs from reserved identity");
                        batch_journal::DispatchState::Started
                    }
                    None => batch_journal::DispatchState::Rejected,
                },
                // `execute_spawn_with_id` registers the run immediately before
                // its infallible driver launch. Every returned error therefore
                // means no member was dispatched and can be recorded as a
                // visible rejection instead of an unresolvable unknown.
                Err(_) => batch_journal::DispatchState::Rejected,
            });
        }
        let journal_required = prepared.is_some();
        let journal_failed = if let Some(prepared) = prepared {
            prepared.finish(dispatched).await.is_err()
        } else {
            false
        };
        let mut spawned = Vec::new();
        let mut roster = Vec::new();
        for ((_, member), outcome) in member_args.iter().zip(outcomes) {
            let task = member.get("task").cloned().unwrap_or(serde_json::Value::Null);
            match outcome {
                Ok(outcome) => match outcome.run_id {
                    Some(run_id) => {
                        // The roster is written from the same value the caller
                        // is shown, before anything can retire the row: this is
                        // the batch's membership, fixed at launch.
                        roster.push(BatchMember {
                            run_id: run_id.clone(),
                            task: task.as_str().map_or_else(|| task.to_string(), str::to_string),
                        });
                        spawned.push(json!({"run_id": run_id, "task": task}));
                    }
                    None => rejected.push(json!({
                        "task": task,
                        "error": outcome.result.error.unwrap_or_else(|| "sub-agent was not started".to_string()),
                    })),
                },
                Err(error) => rejected.push(json!({"task": task, "error": error.to_string()})),
            }
        }
        if !roster.is_empty() {
            let mut rosters = self.batch_rosters.write().await;
            while rosters.len() >= MAX_TRACKED_BATCH_ROSTERS {
                rosters.remove(0);
            }
            rosters.push(BatchRoster {
                batch_id: batch_id.clone(),
                owner_id: batch_owner,
                members: roster,
            });
        }

        let started = spawned.len();
        let mut payload = json!({
            "batch_id": batch_id,
            "requested": entries.len(),
            "started": started,
            "spawned": spawned,
            "rejected": rejected,
        });
        if journal_required {
            payload
                .as_object_mut()
                .context("batch result must be an object")?
                .insert("recovery_journal_confirmed".to_owned(), json!(!journal_failed));
        }
        Ok(ToolResult {
            success: started > 0 && !journal_failed,
            output: serde_json::to_string_pretty(&payload)?,
            error: if journal_failed {
                Some(format!(
                    "batch {batch_id} dispatched, but durable dispatch confirmation is unavailable; use this batch ID to reconcile before retrying"
                ))
            } else {
                (started == 0).then(|| format!("no sub-agent in batch {batch_id} was confirmed started"))
            },
        })
    }

    /// Converge: block until every member of `batch_id` has finished, then
    /// report all of them.
    ///
    /// Partial success is data, not a tool failure — a batch where one member
    /// was killed still returns `success: true` with that member in `killed`,
    /// because the caller's next decision depends on seeing all four buckets at
    /// once. Nothing is hidden: [`join_summary`] accounts for every member
    /// exactly once and `total` is the sum of the buckets.
    pub(super) async fn execute_join(
        &self,
        batch_id: &str,
        owner_scope: &sessions_read_model::RunOwnerScope,
    ) -> anyhow::Result<ToolResult> {
        let caller = crate::tools::execution::current_child_call_context();
        if let Some(caller) = &caller {
            caller.verify_tool("sessions_spawn")?;
        }
        let publication = caller
            .as_ref()
            .and_then(|caller| caller.context().ordinary_child_publication.as_ref());
        if let (Some(publication), Some(caller)) = (publication, caller.as_ref()) {
            publication.verify_caller(caller.context())?;
        }
        let recording_enabled = publication.map_or(self.event_recording.enabled, |publication| {
            publication
                .generation()
                .effective
                .memory
                .event_recording_config()
                .enabled
        });
        let source_fabric = publication
            .map(|publication| publication.source_fabric())
            .or_else(|| self.memory_fabric());
        let recorded = if recording_enabled {
            if let Some(fabric) = source_fabric {
                batch_journal::read(
                    &fabric,
                    std::path::Path::new(fabric.workspace_id()),
                    batch_id,
                    owner_scope,
                    None,
                )
                .await?
            } else {
                None
            }
        } else {
            None
        };
        let members = if let Some(roster) = recorded {
            roster.members
        } else {
            self.batch_members(batch_id, owner_scope).await
        };
        if members.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown batch '{batch_id}': no sub-agent run in this session carries that batch id."
                )),
            });
        }

        let settled = join_batch_members(&self.active_runs, &members).await;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&join_summary(batch_id, &settled))?,
            error: None,
        })
    }

    /// Who this batch launched.
    ///
    /// The roster is preferred over the registry because it is the only source
    /// that cannot lose a member: `active_runs` is pruned by the chat session
    /// reaper, so scanning it answers "which members are still on file", and a
    /// join that took that for the membership silently shrank its own `total`.
    ///
    /// The scan remains as the fallback for the two cases the roster cannot
    /// cover — a batch launched before this tool instance existed, and one
    /// evicted by [`MAX_TRACKED_BATCH_ROSTERS`] — where it is exactly as good,
    /// and exactly as lossy, as it was before.
    async fn batch_members(
        &self,
        batch_id: &str,
        owner_scope: &sessions_read_model::RunOwnerScope,
    ) -> Vec<BatchMember> {
        let recorded = {
            let rosters = self.batch_rosters.read().await;
            rosters
                .iter()
                .find(|roster| roster.batch_id == batch_id)
                .map(|roster| (owner_scope.admits(roster.owner_id.as_deref()), roster.members.clone()))
        };
        if let Some((admitted, members)) = recorded {
            // Another owner's batch is answered as an unknown one.
            return if admitted { members } else { Vec::new() };
        }
        let runs = self.active_runs.read().await;
        runs.iter()
            .filter(|run| run.batch_id.as_deref() == Some(batch_id) && owner_scope.admits(run.owner_id.as_deref()))
            .map(|run| BatchMember {
                run_id: run.id.clone(),
                task: run.task.clone(),
            })
            .collect()
    }
}
