# Receipt: Step 8.2 Xin transactional transitions

Date: 2026-07-15
Branch: `feat/xin-transactional-transitions`
Worktree: `/opt/worker/wt/prx-xin-transactional-transitions`
Base: `469d3f55b8652c240a609f187f959d32b5d89129`
Status: implementation and local verification complete; to be recorded by the
local Step 8.2 commit; not pushed, merged, deployed, installed, or activated

## Delivered

- Replaced the legacy task runner's separate result, run-history, and recurring
  reschedule writes with `commit_task_execution`, one immediate local
  transaction covering all three states and their lifecycle events.
- Restricted the superseded separate result/run/reschedule helpers to tests so
  production code cannot silently rebuild the fragmented commit sequence.
- Made runner success depend on successful persistence, preventing summaries
  from reporting completed work whose result transaction did not commit.
- Added a durable `xin_event_outbox`. Each local lifecycle event and outbox row
  are inserted as an atomic savepoint pair and participate in any surrounding
  result/goal transaction.
- Added idempotent shared-memory mirroring with a caller-owned stable event ID.
  Pending rows retry after every Xin repository operation; delivery attempts and
  bounded errors remain inspectable in the local database.
- Preserved the Step 8.1 lease fence: step result, goal-progress recomputation,
  terminal events, and outbox rows remain in one lease-authorized immediate
  transaction.
- Added success, rollback fault-injection, local event/outbox pairing,
  cross-database recovery, and idempotent replay coverage.
- Added `docs/xin-transactional-transitions.md` as the Step 8.2 architecture
  baseline.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib xin::store::tests::` - 43 passed, 0 failed.
- `cargo test -p openprx --lib xin::runner::tests::` - 15 passed, 0 failed.
- Focused acceptance total: 58 passed, 0 failed.
- `git diff --check` - passed.

The first store sweep exposed one inaccurate test assumption: current goal
status remains pending while a step runs and is recomputed at a terminal
transition. The rollback assertion now compares against the actual
pre-completion goal state. No production behavior changed for that correction.

Per `verification-policy.md`, strict clippy, full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live
runtime acceptance, and release builds were not run. They remain GitHub
delivery or release/deployment gates.

## Scope and rollback

- Scope: Xin task execution persistence, lifecycle-event outbox, idempotent
  SQLite memory mirror, runner commit handling, focused tests, architecture doc,
  and this receipt.
- Step 8.3 ordered prerequisites/adoption and Step 8.4 Heartbeat retirement were
  not implemented.
- Rollback: revert the local Step 8.2 commit before basing Step 8.3 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.
