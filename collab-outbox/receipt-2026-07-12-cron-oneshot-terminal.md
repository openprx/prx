# Stage 2 Step 2.1 cron one-shot terminal semantics receipt

Date: 2026-07-12  
Branch/worktree: `fix/cron-oneshot-terminal` / `/opt/worker/wt/prx-cron-oneshot-terminal`  
Baseline: `20172e3e`

## Scope and outcome

- Added the typed persisted `CronJobTerminalState::{Succeeded, Failed}`,
  separate from the attempt-oriented `last_status` field.
- SQLite and PostgreSQL now store nullable `terminal_state`, project it into
  `CronJob`, and exclude terminal rows from claim and due-job queries.
- A final `Schedule::At` success or failure writes its run record, bounded run
  retention, last-run fields, typed terminal state, and terminal lifecycle
  event in one backend transaction. `Schedule::At` is never rescheduled.
- `delete_after_run` removes an already-terminal job only after success. Failed
  `At` jobs remain with `Failed` state, run history, and failure event audit.
- Successful auto-delete is part of the same SQLite/PostgreSQL transaction as
  run, terminal state, and terminal event persistence. The conditional delete
  reads the database's current `delete_after_run` value after the terminal
  update and returns whether deletion occurred; scheduler/tool perform no
  generic follow-up removal. Toggling retention while running therefore changes
  retention without invalidating the terminal snapshot CAS.
- Historical `At` rows are backfilled only when `last_run` exists and
  `last_status` is exactly `ok` or `error`. `running`, NULL, and unknown states
  remain unresolved for Step 2.2. SQLite uses its additive column migration;
  PostgreSQL uses `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` plus equivalent
  explicit mapping and disable behavior.
- Only an explicitly terminal `At` updated to a validated future `At` clears
  terminal/last-run fields and re-enables. Recurring schedule edits preserve
  history, and `enabled=true` alone does not re-arm a terminal job.
- Scheduler claims are fenced by the due timestamp plus the polled job's
  `next_run` and serialized schedule snapshot. Terminal commits repeat that
  snapshot fence and require the claimed `running` state, rolling back the run
  insert if a newer plan replaced the snapshot. Manual force-run uses a
  separate snapshot claim that intentionally allows a future `At`; no lease,
  owner, attempt ID, or recovery behavior was added.
- Job updates use compare-and-swap predicates over the loaded schedule,
  `next_run`, and `last_status`. An in-flight nonterminal `At` rejects schedule
  updates, so update cannot overwrite a claim that won after the load. The
  claimed attempt retains its terminal run/event, and only then can the
  terminal job be explicitly re-armed.
- Force-running a nonterminal `At` consumes the one-shot and writes typed
  terminal state. A retained terminal `At` can still be force-run manually;
  that adds ordinary run/last-run audit without changing its original terminal
  state. Manual snapshot claim intentionally ignores `enabled`, preserving
  force-run compatibility for paused `At` jobs; scheduler claim still requires
  enabled and due.
- CLI list and cron tool list/get/status expose terminal state. The cron tool's
  shell `At` add path now persists its existing `delete_after_run` option,
  matching the agent path.
- CLI/tool alignment covers creation, scheduling terminal behavior, retention,
  and display. Explicit future-`At` re-arm is available through the cron tool
  update/store path; the CLI has no `At` update flag and continues to reject
  expression updates on non-cron schedules.
- `docs/tools.md` documents terminal, retention, and explicit re-arm behavior.
- Existing public add/record APIs remain available. `CronJob.terminal_state`
  has a serde default for compatibility with older serialized values.

Explicit non-goals: no claim lease/recovery (Step 2.2), process execution
parity (Step 2.3), ingress work, commit, push, deploy, service restart, or
runtime configuration mutation.

## Baseline red characterization

Command:

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-cron-oneshot-terminal-target TMPDIR=/opt/worker/tmp \
  cargo test --lib successful_retained_at_job_executes_once_across_ticks_and_restart -- --nocapture
```

Baseline result: exit 101. The retained past `At` job executed on two scheduler
ticks and once more after reopening the same workspace as a restarted config:

```text
assertion failed: a successful retained At job must execute exactly once
left: 3
right: 1
```

## Focused green evidence

All Cargo commands used
`CARGO_TARGET_DIR=/opt/worker/tmp/prx-cron-oneshot-terminal-target` and
`TMPDIR=/opt/worker/tmp`.

```text
cargo test --lib successful_retained_at_job_executes_once_across_ticks_and_restart -- --nocapture
exit 0; 1 passed; one run across two ticks and restart, one completed event

cargo test --lib failed_retained_at_job_is_terminal_and_not_due_again -- --nocapture
exit 0; 1 passed; Failed terminal state and one cron.job.failed event

cargo test --lib schedule_update_rearms_terminal_job_but_enable_alone_does_not -- --nocapture
exit 0; 1 passed

cargo test --lib historical_completed_at_job_is_backfilled_terminal_and_disabled -- --nocapture
exit 0; 1 passed; ok/error map explicitly, running/NULL/unknown remain unresolved

cargo test --lib recurring_schedule_update_preserves_run_history -- --nocapture
exit 0; 1 passed

cargo test --lib at_snapshot_fences_claim_update_and_terminal_commit -- --nocapture
exit 0; 1 passed; update-before-claim invalidates old snapshot, in-flight update fails, claimed attempt terminals, then explicit re-arm succeeds

cargo test --lib postgres_terminal_schema_and_projection_fixture_are_aligned -- --nocapture
exit 0; 1 passed

cargo test --lib add_shell_schedule_at_persists_explicit_delete_after_run -- --nocapture
exit 0; 1 passed

cargo test --lib persist_job_result_success_deletes_one_shot -- --nocapture
exit 0; 1 passed

cargo test --lib persist_job_result_failure_retains_auto_delete_one_shot_audit -- --nocapture
exit 0; 1 passed; Failed row and run history retained despite delete_after_run=true

cargo test --lib force_run_consumes_nonterminal_at_but_allows_terminal_manual_rerun -- --nocapture
exit 0; 1 passed; first run terminals, second manual run preserves terminal and appends audit

cargo test --lib failed_force_run_retains_auto_delete_at_audit -- --nocapture
exit 0; 1 passed

cargo test --lib force_run_paused_at_preserves_manual_run_compatibility -- --nocapture
exit 0; 1 passed

cargo test --lib one_shot_terminal_delete_uses_current_retention_flag_atomically -- --nocapture
exit 0; 1 passed; in-flight true-to-false toggle retains the successful terminal row

cargo test --lib rearm_and_terminal_auto_delete_are_serialized -- --nocapture
exit 0; 1 passed; re-arm invalidates the old snapshot, while terminal-first atomically deletes and makes later re-arm not-found
```

Module regression results:

```text
cargo test --lib cron::store::tests -- --nocapture
exit 0; 19 passed

cargo test --lib cron::scheduler::tests -- --nocapture
exit 0; 23 passed

cargo test --lib tools::cron::tests -- --nocapture
exit 0; 26 passed

cargo test --lib cron::postgres::tests -- --nocapture
exit 0; 2 passed
```

`OPENPRX_TEST_POSTGRES_URL` was not set, so the existing live PostgreSQL
lifecycle test followed its documented early-return path. PostgreSQL coverage
in this environment is therefore repository-level: schema migration/projection
fixture plus compilation of the parameterized transactional implementation.
The live gated test now constructs a legacy PostgreSQL table without the new
column, inserts historical ok/error/running/NULL/unknown `At` rows, calls
`init_schema`, and checks migration results. It also covers future scheduler
claim rejection, manual claim, terminal due exclusion, stale snapshot claim,
in-flight update rejection, claimed terminal commit, and post-terminal re-arm
when a PostgreSQL test URL is supplied. It also covers running retention toggle
and the same re-arm-versus-atomic-delete winner contract.

## Compile and hygiene gates

```text
cargo check --lib
exit 0

cargo check --all-targets --all-features
exit 0 (final rerun after all changes)

cargo check --no-default-features
exit 0 (final rerun after all changes)

cargo fmt --all -- --check
exit 0

git diff --check
exit 0
```

## Formal gate closure

The main thread reran the strict gates with the same isolated target:

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
exit 0

cargo test --bin prx --all-features
5548 passed; 0 failed; 7 ignored

cargo test --locked --test architecture_boundaries
4 passed; 0 failed
```

Five review rounds closed the initial and follow-up correctness findings. Two
independent final reviews reported no High or Medium blockers. PostgreSQL live
execution remains explicitly unverified on this host because
`OPENPRX_TEST_POSTGRES_URL` was not configured.

Rollback is file-local: revert the typed field/schema projection, transactional
one-shot terminal writer, scheduler branch, and UI projections together. The
nullable additive column is backward-compatible with the prior binary.
