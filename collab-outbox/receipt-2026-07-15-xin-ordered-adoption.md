# Receipt: Step 8.3 Xin ordered prerequisites and adoption

Date: 2026-07-15
Branch: `feat/xin-ordered-adoption`
Worktree: `/opt/worker/wt/prx-xin-ordered-adoption`
Base: `4dbb7f78c167a2e283f0ad4b4911e2b3f46fe4e8`
Status: implementation and local verification complete; to be recorded by the
local Step 8.3 commit; not pushed, merged, deployed, installed, or activated

## Delivered

- Required all prior ordered steps to be completed in both
  `next_runnable_step` and the atomic lease claim, closing direct-ID and
  stale-read bypasses.
- Added `xin_task_adoptions` as a unique durable legacy-task-to-goal link.
- Made adoption one immediate transaction covering eligibility, goal and both
  steps, link insertion, source-task disable, lineage events, and outbox rows.
- Made adoption replay return the existing linked goal and startup replay count
  zero new work; recurring and non-stale tasks remain ineligible.
- Preserved owner/topic/parent/source, task semantics, priority, approval grant,
  and retry policy on the adopted goal and migrated work step.
- Passed goal lineage through the step execution bridge into a caller-owned
  shared `RuntimeEnvelope`, so Agent memory/tool/audit/event scope retains
  owner, topic, task, and source-message identity.
- Kept the public Agent CLI entrypoint behavior-compatible by delegating to the
  scoped entry with no override.
- Added ordered-selection, direct-claim, prior-failure, adoption rollback,
  adoption replay, startup replay, schema-upgrade, and runtime-envelope tests.
- Added `docs/xin-ordered-adoption.md` as the Step 8.3 architecture baseline.

## Local verification

Commands use `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed during implementation.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib agent_turn_envelope_preserves_caller_scope_and_cli_defaults` - 1 passed, 0 failed.
- `cargo test -p openprx --lib xin::store::tests::` - 47 passed, 0 failed.
- `cargo test -p openprx --lib xin::runner::tests::` - 17 passed, 0 failed.
- Focused acceptance total: 65 passed, 0 failed.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live
runtime acceptance, and release builds are not part of this local step gate.
They remain GitHub delivery or release/deployment gates.

## Scope and rollback

- Scope: Xin ordered selection/claim, legacy adoption schema and transaction,
  goal runtime lineage, scoped Agent envelope entry, focused tests, architecture
  doc, and this receipt.
- Step 8.4 Heartbeat retirement was not implemented.
- Rollback: revert the local Step 8.3 commit before basing Step 8.4 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.
