# Receipt: Step 8.4 Heartbeat retirement

Date: 2026-07-15
Branch: `feat/heartbeat-xin-retirement`
Worktree: `/opt/worker/wt/prx-heartbeat-xin-retirement`
Base: `463c621d4c018ab32aa51ab8c6c5be7ee0c8ef13`
Status: implemented and locally verified; to be recorded by the local Step 8.4
commit; not pushed, merged, deployed, installed, or activated

## Delivered

- Preserved HEARTBEAT.md creation, dash-bullet parsing, configured prompt,
  interval floor, and active-hour semantics.
- Materialized each normalized bullet as a stable SHA-256-named recurring Xin
  AgentSession task, due immediately on first creation.
- Reconciled prompt/interval updates in place, preserved IDs across restart and
  reordering, disabled removed tasks, and re-enabled reintroduced tasks.
- Prevented unchanged reconciliation from generating update/event churn.
- Removed the daemon Heartbeat supervisor and direct per-bullet `agent::run`
  loop; Xin is now the only scheduling and execution owner.
- Made Heartbeat enable the shared Xin runtime even if Xin itself is disabled,
  while heartbeat-only mode uses a database-scoped `heartbeat:` due/stale path
  and does not admit or mutate ordinary Xin tasks/goals/built-ins/adoption.
- Preserved Heartbeat health visibility under Xin ownership and used the faster
  enabled poll interval when Heartbeat and Xin coexist.
- Added stable reconciliation, active-hours, heartbeat-only isolation,
  shared-interval, daemon activation, and existing Xin regression coverage.
- Updated `docs/configuration.md` and added
  `docs/heartbeat-xin-materialization.md`.

## Local verification

Commands use `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed during implementation.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib heartbeat::` - 21 passed, 0 failed.
- `cargo test -p openprx --lib xin::runner::tests::` - 20 passed, 0 failed.
- `cargo test -p openprx --lib xin::store::tests::` - 48 passed, 0 failed.
- `cargo test -p openprx --lib daemon::tests::` - 18 passed, 0 failed.
- Focused acceptance: 107 passed, 0 failed.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live
runtime acceptance, and release builds are not part of this local step gate.
They remain GitHub delivery or release/deployment gates.

## Scope and rollback

- Scope: Heartbeat parser/materializer, Xin namespace reconciliation and
  heartbeat-only admission, daemon supervisor ownership, focused tests,
  configuration/architecture docs, and this receipt.
- Stage 9 capability-domain batches were not started in this numbered step.
- Rollback: revert the local Step 8.4 commit before starting Stage 9.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.
