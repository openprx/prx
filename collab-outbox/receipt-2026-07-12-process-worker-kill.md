# PR-1 process worker kill receipt

Date: 2026-07-12  
Branch/worktree: `fix/process-worker-kill` / `/opt/worker/wt/prx-process-worker-kill`

## Scope

- Process monitor remains the only owner of `tokio::process::Child`.
- Kill and shutdown callers send an owner-mediated termination request and wait
  up to five seconds for finalization; they never receive or signal a raw
  PID/PGID. Request timeout is not process finalization.
- Task-mode runs retain their existing Tokio abort semantics.
- No commit, push, deployment, service restart, or `prx init` was performed.

## Baseline red characterization

The first characterization test reproduced the existing orphan behavior by
spawning a long-lived child inside the same monitor shape used by process mode,
aborting that monitor, and probing the child afterward.

Command:

```text
cargo test --lib aborting_process_monitor_does_not_leave_owned_child_alive -- --nocapture
```

The initial red invocation used the worktree-local target directory. Every
subsequent build/test command used the required
`CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp` prefix.

Observed failure:

```text
thread 'tools::sessions_spawn::tests::aborting_process_monitor_does_not_leave_owned_child_alive' panicked
aborting the monitor must not orphan its owned child
test result: FAILED
```

The failed assertion was the intended baseline signal: aborting the monitor
dropped its `Child` handle without killing or reaping the still-live OS child.
The fixture killed the orphan during test cleanup.

## Implementation

Behavior changes:

- `src/tools/sessions_spawn.rs`: adds per-run process control, makes the monitor
  the sole `Child` owner, creates a Unix process group, handles owner-mediated
  kill/timeout, waits/reaps the direct child, publishes terminal state/event
  once, and finalizes before external announcement.
- `src/tools/subagents.rs`: process kill requests owner termination and awaits
  finalization; task-mode abort behavior is unchanged.
- `src/chat/sessions/runtime.rs`: `shutdown_all` requests process termination
  and retains any process run whose owner has not finalized; task-mode abort
  remains unchanged.

Mechanical `process_control: None` struct-literal adaptations required for the
new `SubAgentRun` field:

- `src/chat/mod.rs`
- `src/chat/sessions/approval.rs`
- `src/chat/sessions/model.rs`
- `src/tools/session_status.rs`
- `src/tools/sessions_history.rs`
- `src/tools/sessions_list.rs`
- `src/tools/sessions_send.rs`

`src/chat/mod.rs` is also dirty in the main worktree. This branch changes only
one constructor line there; integration must preserve the independent main
worktree Chat diff and resolve that one-line field addition during merge.

## Green implementation evidence

All commands below used:

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp
```

Focused green results recorded so far:

```text
cargo test --lib owner_mediated_process_kill_reaps_leader_and_terminates_group -- --nocapture
1 passed; leader reaped and background descendant no longer exists

cargo test --lib process_termination_requests_share_one_finalization -- --nocapture
1 passed; concurrent and repeated requests share the owner's finalization

cargo test --lib process_termination_keeps_concurrency_slot_until_owner_finalizes -- --nocapture
1 passed; public status remains Running and the slot stays occupied until finalization

cargo test --lib process_termination_records_killed_terminal_event_once -- --nocapture
1 passed; exactly one task.killed and zero task.failed terminal task events

cargo test --lib process_mode_parent_timeout_kills_stuck_process -- --nocapture
1 passed

cargo test --lib process_kill_waits_for_owner_and_is_idempotent -- --nocapture
1 passed; subagents process kill waits and repeated kill succeeds idempotently

cargo test --lib sessions_spawn_process_kill_waits_for_owner_and_is_idempotent -- --nocapture
1 passed; sessions_spawn process kill waits and repeated kill succeeds idempotently

cargo test --bin prx shutdown_all_waits_for_process_owner_before_clearing_registry -- --nocapture
1 passed; Chat shutdown keeps the process run registered until owner finalization
```

The final Unix characterization uses the production owner-control path and
checks both the process-group leader and a background descendant.

## Review-fix round: single group owner and post-leader safety

Independent review identified additional owner-protocol races:

1. separate group-signal and `OwnedProcessGroup` Drop paths could signal the
   same numeric PGID again after direct-child reap;
2. registry terminal status and control finalization were not one locked commit;
3. inherited stdout/stderr pipes could hang after direct leader exit;
4. signalling a PGID after leader reap could target a reused unrelated group;
5. an unwind inside the child-owning phase had to be caught while the outer
   function still held `&mut Child` and the explicit group capability.

New red characterization:

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test --lib termination_after_leader_exit_does_not_wait_forever_on_inherited_pipe -- --nocapture

thread 'tools::sessions_spawn::tests::termination_after_leader_exit_does_not_wait_forever_on_inherited_pipe' panicked:
kill request must still reach the owner after leader exit
test result: FAILED. 0 passed; 1 failed
```

The fixture used a naturally exiting process-group leader plus a live
background descendant that inherited stdout. The baseline owner waited forever
on the pipe and never finalized the kill request.

Final safety contract:

- `OwnedProcessGroup` is the only process-group signal authority.
- Live kill and timeout signal through that owner exactly once and disarm
  immediately on success/ESRCH. The requester returns the distinct `Pending`
  request result after five seconds without writing finalization, while the
  owner continues holding `Child` until reap is confirmed.
- A `start_kill` fallback error does not skip the bounded initial wait; errors
  are combined. If that bounded wait expires, the owner continues waiting and
  does not release the run slot.
- A direct `child.wait()` error, or panic-cleanup `try_wait()` error, first
  relinquishes PGID authority and never signals the old numeric group.
- Once the direct leader is reaped, PGID authority is permanently relinquished.
  No reader scheduling or `killpg(..., 0)` probe is used to infer ownership.
- Post-leader output drain is bounded to one second. A kill request in this
  phase returns `TerminationFailed`; it never claims that an escaped descendant
  was killed.
- The child-owning async phase is caught inside `run_sub_agent_process`; after
  unwind the outer function still owns `Child` and the group capability and
  explicitly signals once and retains ownership until reap before returning
  failure.
- Terminal registry mutation and control finalization commit under one registry
  write lock.

```text
cargo test --lib termination_after_leader_exit_does_not_wait_forever_on_inherited_pipe -- --nocapture
1 passed; post-reap kill returned TerminationFailed promptly without signalling PGID

cargo test --lib live_termination_disarms_single_group_owner_before_reap -- --nocapture
1 passed; live group signal count exactly one and owner disarmed before reap returned

cargo test --lib process_mode_parent_timeout_kills_stuck_process -- --nocapture
1 passed; timeout used the same single group owner

cargo test --lib stdin_error_cleanup_signals_group_once -- --nocapture
1 passed; shared explicit-cleanup primitive signalled once (this fixture does
not induce a real production stdin write failure)

cargo test --lib pipe_setup_error_cleanup_signals_group_once -- --nocapture
1 passed; shared explicit-cleanup primitive signalled once (this fixture does
not induce a real production pipe-setup failure)

cargo test --lib owned_child_panic_cleanup_signals_once_and_reaps_before_return -- --nocapture
1 passed; an injected panic inside the child-owning lifecycle helper was caught,
then cleanup signalled once and reaped the direct child before return

cargo test --lib terminal_commit_does_not_release_slot_before_control_finalization -- --nocapture
1 passed; barrier held the registry write lock and blocked readers until status and finalization committed together

cargo test --lib owner_mediated_process_kill_reaps_leader_and_terminates_group -- --nocapture
1 passed

cargo test --lib process_termination_keeps_concurrency_slot_until_owner_finalizes -- --nocapture
1 passed

cargo test --lib process_termination_records_killed_terminal_event_once -- --nocapture
1 passed

cargo test --lib sessions_spawn_process_kill_waits_for_owner_and_is_idempotent -- --nocapture
1 passed

cargo test --lib process_kill_waits_for_owner_and_is_idempotent -- --nocapture
2 passed (the filter covers both sessions_spawn and subagents process-kill tests)

cargo test --bin prx shutdown_all_waits_for_process_owner_before_clearing_registry -- --nocapture
1 passed after the review fixes; non-test library and Chat binary paths rebuilt successfully

cargo check --lib
passed; non-test library configuration compiles with the single-owner path

cargo fmt --all -- --check
passed

git diff --check
passed
```

## Review-fix round: bounded requester, continuing OS owner

```text
cargo test --lib termination_request_timeout_does_not_finalize_or_release_slot -- --nocapture
1 passed; requester returned Pending while finalization stayed None
and the Running slot remained occupied

cargo test --lib owner_keeps_child_after_requester_timeout_until_reap -- --nocapture
1 passed; owner retained Child after requester timeout and finalized only after reap

cargo test --bin prx shutdown_all_retains_unfinalized_process_run -- --nocapture
1 passed; Chat shutdown retained the unfinalized process run
```

## Final review: truthful Pending result and unresolved ownership

- Termination requests now return either `Finalized(ProcessFinalization)` or
  `Pending`. Handler timeout no longer impersonates an owner-reported failure.
- `sessions_spawn` and `subagents` report that reap is still pending and the run
  remains active. Chat shutdown likewise retains that run.
- Natural `child.wait()` error, post-group-kill `child.wait()` error,
  direct-fallback wait error, and panic-cleanup `try_wait()` error all
  relinquish any old PGID authority and enter unresolved ownership: the owner
  future retains `Child`, finalization stays unset, and the slot stays live.
- Chat shutdown derives its initial agent IDs from the original
  `ManagedSessionView` snapshot, acts only on active entries in that cutoff,
  removes terminal/finalized entries in it, and preserves later insertions.

```text
cargo test --lib process_kill_reports_pending_without_claiming_owner_failure -- --nocapture
2 passed; both process kill handlers report Pending without claiming kill failure

cargo test --lib injected_ -- --nocapture
3 relevant process-owner tests passed for natural wait, termination wait, and panic try_wait errors

cargo test --bin prx shutdown_all_clears_terminal_snapshot_but_preserves_new_run -- --nocapture
1 passed; terminal snapshot entries cleared and concurrent new run preserved
```

## Final race closure: timeout boundary and unified shutdown cutoff

- After the finalization wait times out, the requester performs one final
  `control.finalization()` read. A finalization published at that boundary is
  returned as `Finalized(...)`; only an observed `None` becomes `Pending`.
- Shutdown derives separate agent, shell, and PTY ID cutoffs from the original
  `ManagedSessionView` snapshot. Summaries, ignored IDs, termination actions,
  and registry removal therefore describe the same initial population.
- Runs, shells, or PTYs inserted after that snapshot are not aborted, killed,
  summarized, ignored, or removed by the in-flight shutdown.

```text
cargo test --lib termination_request_timeout_observes_boundary_finalization -- --nocapture
1 passed; barrier-published boundary finalization returned Finalized(Terminated)

cargo test --bin prx shutdown_all_preserves_shell_inserted_after_snapshot -- --nocapture
1 passed; initial shell killed while post-snapshot shell remained live and unsummarized

cargo clippy --all-targets --all-features -- -D warnings
passed after mechanical lint cleanup

cargo check --all-features
passed

cargo check --no-default-features
passed

cargo test --lib process_termination_ / owner_mediated_process_kill_reaps_leader_and_terminates_group / injected_ / process_kill_
3 + 1 + 5 + 5 passed across the four focused invocations

cargo test --bin prx shutdown_all_
7 passed
```

Runtime-destruction boundary: there is no synchronous Drop watchdog and no
`running_run_count` status override. If Tokio destroys the entire runtime, that
in-memory registry is also being destroyed; an OS child or descendant may
survive runtime destruction, and this receipt makes no observable registry or
reap guarantee across that boundary.

## Remaining risks and non-goals

- The pre-existing admission-check/registry-insert TOCTOU is not changed by
  this PR; it should be handled as a separate concurrency changeset.
- Non-Unix platforms can terminate and retain ownership while waiting for the
  direct child, but have no portable process-group equivalent for descendant
  cleanup.
- A descendant that calls `setsid` escapes the original process group. It is not
  guaranteed to be killed. After leader reap, such a descendant cannot be
  safely addressed through the old numeric PGID; the owner reports failure.
- Any descendant that remains alive after the direct leader is reaped can keep
  running. The owner permanently relinquishes the old PGID and will not trade
  unrelated-process safety for a claim of descendant cleanup.
- The full test suite was not run in this focused changeset handoff. The formal
  all-targets/all-features clippy gate and the listed focused tests were run.
