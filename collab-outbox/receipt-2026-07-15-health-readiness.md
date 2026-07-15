# Receipt: Step 3.4 Health and readiness

Date: 2026-07-15
Branch: `fix/health-readiness`
Worktree: `/opt/worker/wt/prx-health-readiness`
Baseline: `52b402ea3b72290a7d6d71175896e5deb3e4111a`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Replaced free-form health strings with typed lifecycle states: `Starting`,
  `Ready`, `Degraded`, `Failed`, `Disabled`, `Stopping`, and `Stopped`.
- Added explicit owner, required/optional classification, freshness TTL, and
  freshness projection to every component record. Existing `status`, `last_ok`,
  `last_error`, and `restart_count` fields remain as compatibility projections
  for doctor, state-file, fitness, cron, channel, and Xin consumers.
- Added a required-component readiness report and an async startup barrier. A
  process with no required component, a required disabled/starting/failed
  component, or a stale required Ready signal is not ready.
- Registered daemon and gateway as the core required components. Configured
  background capabilities are tracked as optional: their failures remain
  visible and degrade their component record without falsely blocking core
  ingress readiness. Disabled capabilities are represented as `Disabled`,
  never `ok`.
- Moved supervisor startup from `ok` to `Starting`. Merely polling a future that
  has not exited can no longer acknowledge readiness. Gateway acknowledges only
  after its listener is bound and all state/routes are built; the standalone
  webhook receiver acknowledges after its listener binds.
- Delayed systemd `READY=1` until both required components explicitly report
  Ready. Shutdown transitions daemon and gateway through `Stopping`/`Stopped`.
- Changed daemon state-writer heartbeats to refresh freshness without promoting
  lifecycle state. Heartbeat worker now explicitly acknowledges initialization
  and refreshes at its owned interval.
- Changed public `/health` to return HTTP 200 only when required readiness is
  satisfied and HTTP 503 otherwise. The response includes a typed readiness
  summary plus the runtime snapshot.
- Bounded public error summaries to 200 characters, flattened control
  characters, and reused the established secret scrubber before storing or
  exposing errors.
- Preserved restart evidence when an owner re-registers a restarting component.

## Red-first evidence preserved

The baseline produced two expected failures:

1. `supervisor_does_not_treat_task_survival_as_readiness` failed because a
   permanently Pending component was immediately reported as `ok`.
2. `public_error_text_is_bounded_and_secret_scrubbed` failed because the health
   snapshot exposed the full `sk-super-secret-value` text and an unbounded
   message.

Both assertions are now green. Additional tests cover all-required semantics,
disabled-required rejection, TTL-driven degradation, owner/TTL metadata,
restart preservation, legacy field compatibility, and HTTP 503 behavior.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final production tree
  with no reported warnings.
- `cargo test -p openprx --lib 'health::tests::' -- --nocapture` - 9 passed,
  0 failed, 4,325 filtered out.
- `cargo test -p openprx --lib 'daemon::tests::' -- --nocapture` - 17 passed,
  0 failed, 4,317 filtered out.
- `cargo test -p openprx --lib readiness_http_is_non_success_without_ready_required_components -- --nocapture`
  - 1 passed, 0 failed, 4,333 filtered out.
- `cargo test -p openprx --lib process_due_jobs_marks_component_ok_even_when_idle -- --nocapture`
  - 1 passed, 0 failed, 4,333 filtered out.
- `cargo test -p openprx --lib supervised_listener_refreshes_health_while_running -- --nocapture`
  - 1 passed, 0 failed, 4,333 filtered out.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full workspace/binary suite,
architecture guards, dependency/security audits, independent review, and a
release build were not run. They remain GitHub delivery gates, not local Step
3.4 gates.

## Scope and rollback

- Scope: `src/health/mod.rs`, `src/daemon/mod.rs`, the gateway health handler,
  the standalone webhook listener acknowledgement, tests colocated in those
  modules, and this receipt.
- Final implementation/test diff: 532 insertions, 52 deletions.
- Final pre-receipt diff SHA-256:
  `fe213cae9a0301dfd92e92d8a3b5e2c137222cfdf676d7d89f518be1ff2524ca`.
- Rollback: revert the local Step 3.4 commit before Step 4.1 is based on it.
- No GitHub action, push, merge, binary install, service operation, process
  restart, active configuration mutation, database mutation, network listener,
  runtime activation, release, or deployment was performed.
