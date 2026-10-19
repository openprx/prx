/// Reason recorded when an operator ends a run through the tool/chat kill path.
///
/// A shared constant rather than a literal at each site because the *reader* of
/// the status — `join`'s outcome classification, chat's `Failed -> Cancelled`
/// projection — has to tell an operator kill from a task that failed on its own
/// terms, and the only carrier of that distinction is this wording.
pub(crate) const KILLED_BY_USER_REASON: &str = "killed by user";

/// Tail of the reason [`vanished_monitor_reason`] records when a run's monitor
/// was cancelled out from under it — the shape of a cascade kill
/// (`prx tasks kill`, `kill_turn_subtree`), which reaches the run through the
/// registry and never gets to write a result of its own.
pub(crate) const TERMINATED_BEFORE_RESULT_SUFFIX: &str = "was terminated before it could report a result";

/// Whether a `Failed` reason describes a kill rather than a task-level failure.
///
/// A *sub-rule* of [`termination_cause`], which only reaches it for a reason
/// the runtime itself wrote — the suffix match below is deliberately never
/// applied to text a sub-agent could have chosen.
///
/// MUTATION GUARD: collapse this into `false` and a killed member of a joined
/// batch is reported as an ordinary failure, which is the one distinction the
/// caller of `join` needs in order to decide whether to retry it.
pub(super) fn failure_is_kill(reason: &str) -> bool {
    reason == KILLED_BY_USER_REASON || reason.ends_with(TERMINATED_BEFORE_RESULT_SUFFIX)
}

/// Head of the reason recorded when a process-mode worker's pipe closed before
/// it ever wrote a result line — b1's classification of a SIGKILL, an OOM kill
/// or a segfault.
pub(crate) const EXITED_WITHOUT_RESULT_PREFIX: &str = "worker exited without result";

/// Tail of the reason recorded when a run's monitor task panicked.
///
/// The counterpart of [`TERMINATED_BEFORE_RESULT_SUFFIX`]: both describe a run
/// that stopped without a conclusion, but a cancellation was *asked for* by
/// somebody and a panic was not.
pub(crate) const PANICKED_BEFORE_RESULT_SUFFIX: &str = "panicked before it could report a result";

/// Head of the reason recorded when the process-mode owner task itself blew up
/// while holding the OS child.
pub(crate) const PROCESS_OWNER_PANICKED_PREFIX: &str = "process owner panicked";

/// Marker the runtime writes in front of a failure reason that came out of the
/// sub-agent itself.
///
/// Every other constant in this group is wording the *runtime* records about a
/// run. This one quarantines wording the run recorded about itself — a
/// process-mode worker's `WorkerResult.error`, an agent loop error carrying a
/// tool's or a provider's message — none of which the runtime authored and all
/// of which a sub-agent (or the untrusted input it is chewing on) can choose.
///
/// It is the load-bearing half of [`termination_cause`]: because the tag can
/// only ever sit at the *front* of a reason and only the runtime puts it there,
/// a sub-agent that names its own failure `killed by user`, `timeout`, or
/// anything ending in [`TERMINATED_BEFORE_RESULT_SUFFIX`] cannot relabel how
/// its own termination is reported to whoever joins the batch.
pub(crate) const SUB_AGENT_REPORTED_PREFIX: &str = "sub-agent reported:";

/// Wrap a sub-agent's own account of why it failed in the quarantine tag.
///
/// A function rather than a `format!` at each site so the tag and the space
/// after it cannot drift, and so a test can build the exact reason production
/// would from an adversarial worker payload.
pub(super) fn sub_agent_reported_failure(detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        SUB_AGENT_REPORTED_PREFIX.to_string()
    } else {
        format!("{SUB_AGENT_REPORTED_PREFIX} {detail}")
    }
}

/// Which of the three ways a run can stop this reason describes.
///
/// `join`'s buckets are a claim about *who* ended a run, and until this
/// summer that claim was read out of the reason's prose — so a sub-agent that
/// wrote the right words into its own error could pick its own bucket, and a
/// generic one like `timeout` could collide with a runtime wording by
/// accident. The classification now follows the reason's *provenance*, which
/// the writer stamps and the reader only reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminationCause {
    /// Somebody ended the run: an operator kill, or a cascade that reached it.
    Killed,
    /// The run stopped without ever reaching a conclusion of its own.
    NoConclusion,
    /// The account came from the sub-agent, so it is a task-level verdict —
    /// whatever words it happens to contain.
    SelfReported,
}

/// Read a `Failed` reason's provenance.
///
/// The order of the checks is the whole design and is not interchangeable:
///
/// 1. The quarantine tag first. A reason the runtime marked as the sub-agent's
///    own account is a task verdict no matter what it says, which is what makes
///    the wordings below unforgeable rather than merely unlikely to collide.
/// 2. The runtime tags that are *anchored at the front* and carry an untrusted
///    tail — [`EXITED_WITHOUT_RESULT_PREFIX`] embeds a preview of the worker's
///    stderr, and [`PROCESS_OWNER_PANICKED_PREFIX`] embeds a cleanup error.
///    They are read before the suffix rule below because a worker whose last
///    stderr bytes are [`TERMINATED_BEFORE_RESULT_SUFFIX`] would otherwise
///    relabel its own silent death as an operator kill.
/// 3. Only then the remaining wordings, none of which an untrusted string can
///    reach any more.
///
/// An untagged reason from somewhere that never went through the writers here
/// falls to `SelfReported`: it is the one bucket that claims nothing about the
/// runtime having acted.
///
/// MUTATION GUARD: delete the first arm and
/// `a_sub_agent_cannot_forge_its_own_termination_verdict` fails — a worker
/// whose reported error ends in [`TERMINATED_BEFORE_RESULT_SUFFIX`] is filed
/// as an operator kill.
pub(super) fn termination_cause(reason: &str) -> TerminationCause {
    if reason.starts_with(SUB_AGENT_REPORTED_PREFIX) {
        return TerminationCause::SelfReported;
    }
    if reason.starts_with(EXITED_WITHOUT_RESULT_PREFIX) || reason.starts_with(PROCESS_OWNER_PANICKED_PREFIX) {
        return TerminationCause::NoConclusion;
    }
    if failure_is_kill(reason) {
        return TerminationCause::Killed;
    }
    if failure_gave_no_conclusion(reason) {
        return TerminationCause::NoConclusion;
    }
    TerminationCause::SelfReported
}

/// Turn an agent-loop error into the reason its run's row will carry.
///
/// The decision is made from the error's *type*, on the task that owns the run,
/// at the moment it ends — the only place where the runtime still knows what
/// actually happened. Rendering to text is the last step, and once rendered the
/// text is only ever an explanation for a human; [`termination_cause`] reads the
/// tag this function chose, not the prose.
///
/// MUTATION GUARD: replace the body with `error.to_string()` and
/// `a_task_mode_failure_is_classified_by_the_error_type_not_its_words` fails —
/// a cancelled loop is filed as the task's own verdict, and a tool error that
/// merely quotes the hang detector is filed as `no_result`.
pub(super) fn task_failure_reason(error: &anyhow::Error) -> String {
    if error
        .chain()
        .any(|source| source.is::<crate::agent::idle::IdleHangTerminated>())
    {
        // The runtime's own hang detector ended this turn: its rendering already
        // carries the markers `failure_gave_no_conclusion` recognises, and no
        // sub-agent can produce this type.
        error.to_string()
    } else if crate::agent::turn_contracts::is_tool_loop_cancelled(error) {
        // Somebody cancelled the loop — `prx tasks kill`, a parent cascade, a
        // shutdown. That is a kill, and it is the same shape
        // `vanished_monitor_reason` records for the cascade it can observe.
        format!("sub-agent run {TERMINATED_BEFORE_RESULT_SUFFIX}")
    } else {
        sub_agent_reported_failure(&error.to_string())
    }
}

/// The reason a run's row carries after its worker died without writing a
/// result.
///
/// A function rather than a `format!` at the single write site so a test can
/// build the exact wording production would, from a real killed child, instead
/// of re-typing it and drifting.
pub(super) fn exited_without_result_reason(detail: &str) -> String {
    format!("{EXITED_WITHOUT_RESULT_PREFIX} ({detail})")
}

/// What a spawn confirmation tells the caller about where the result will show
/// up.
///
/// The two sentences are not decoration: a model that just launched a batch has
/// to know that nothing further will arrive on its own, or it will sit waiting
/// for announcements that were switched off and never call `join`.
pub(super) const fn spawn_completion_note(announce_result: bool) -> &'static str {
    if announce_result {
        "Will announce result when complete."
    } else {
        "Result is not announced; collect it with the 'join' action on this batch_id."
    }
}

/// Whether a `Failed` reason describes a run that never reached a conclusion at
/// all, as opposed to a task that concluded it had failed.
///
/// This is the distinction `join` reports as `no_result` versus `failed`, and it
/// is not cosmetic: a caller reading `failed` learns that the sub-agent looked
/// at the problem and judged it undoable, while `no_result` says the sub-agent
/// was cut down before it could judge anything. Collapsing the two invites the
/// caller to trust a verdict that was never rendered.
///
/// Every arm matches a constant that the *writing* site also uses, so the two
/// halves cannot drift. A string is the only channel available because a run's
/// outcome reaches the registry as `SubAgentStatus::Failed(String)`, but the
/// string is no longer trusted on its own: this predicate is a *sub-rule* of
/// [`termination_cause`], which refuses to consult it at all for a reason the
/// sub-agent authored.
///
/// MUTATION GUARD: collapse this into `false` and every silent death is
/// reported as a considered failure.
pub(super) fn failure_gave_no_conclusion(reason: &str) -> bool {
    reason.starts_with(EXITED_WITHOUT_RESULT_PREFIX)
        || reason.ends_with(PANICKED_BEFORE_RESULT_SUFFIX)
        || reason.starts_with(PROCESS_OWNER_PANICKED_PREFIX)
        || crate::agent::idle::message_describes_hang_termination(reason)
}
