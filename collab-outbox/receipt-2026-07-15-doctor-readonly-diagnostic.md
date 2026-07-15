# Receipt: Step 3.2 Doctor becomes a diagnostic

Date: 2026-07-15
Branch: `fix/doctor-readonly-diagnostic`
Worktree: `/opt/worker/wt/prx-doctor-readonly-diagnostic`
Baseline: `9641a38fe42475b31c6feaba3a1f6c38fb38336b`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Routed every `doctor` subcommand through the existing-config-only loader.
  Doctor no longer initializes or migrates config, workspace, or secrets.
- Added a typed `DoctorReport` with typed findings and explicit `Declared`,
  `Configured`, `Ready`, `Healthy`, `Disabled`, and `Unknown` states. Printed
  findings include the state instead of collapsing disabled/unknown into
  healthy.
- Main, memory, and runtime Doctor reports now return an error when any finding
  has ERROR severity, producing a nonzero CLI result.
- Removed the workspace write probe. Doctor now checks directory readability
  and declared permission bits without creating and deleting a probe file.
- Removed full Memory backend construction from runtime memory and console
  checks. SQLite/Lucid/PostgreSQL health uses the read-only authoritative
  migration-ledger inspection; SQLite session visibility uses a read-only
  database open; unsupported or unobservable paths report Disabled or Unknown.
- Eliminated Doctor-triggered SQLite creation, schema/ACL bootstrap,
  auto-hydration, memory hygiene, and other Memory factory side effects.
- Added a read-only model-catalog probe. It may read fresh/stale cache evidence
  or perform a live fetch, but never creates or updates the workspace cache.
  Provider probe ERRORs now also produce a nonzero result.
- Disabled network/channel/memory components are reported as `Disabled` and
  skipped instead of being probed and counted as healthy.

## Red-first evidence preserved

Two focused baseline regressions each executed one test and failed for the
expected reasons:

1. `doctor_run_returns_error_when_report_contains_errors` showed a report with
   three ERROR findings still returned `Ok(())`.
2. `runtime_memory_probe_does_not_create_missing_sqlite_database` showed the
   runtime memory check created `memory/brain.db` through the full Memory
   factory.

Both tests remain in the final Doctor suite and are green. Additional coverage
proves all six typed states are emitted by real checks, disabled scheduler state
is not Healthy, workspace checks leave no probe files, model-cache bytes remain
unchanged, and real CLI paths create no config/workspace/database state.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed in 1m00s on the final
  production tree with no reported warnings.
- `cargo test -p openprx --bin prx --all-features 'doctor::tests::' -- --nocapture`
  - 36 passed, 0 failed, 0 ignored, 5,606 filtered out.
- `cargo test -p openprx --bin prx --all-features run_models_probe_read_only_uses_cache_without_mutation -- --nocapture`
  - 1 passed, 0 failed, 5,641 filtered out.
- `cargo test -p openprx --test doctor_readonly_cli --all-features -- --nocapture`
  - 3 passed, 0 failed, 0 filtered out.
- `git diff --check` - passed.

The CLI integration tests execute the built `prx` binary and prove missing
config is not initialized, ERROR findings exit nonzero without creating a
workspace, and runtime diagnosis does not create `brain.db`.

Per `verification-policy.md`, strict clippy, full binary/workspace suites,
architecture guards, dependency/security audits, independent review, and a
release build were not run. They remain GitHub delivery gates, not local Step
3.2 gates.

## Scope and rollback

- Scope: `src/doctor/mod.rs`, `src/main.rs`, `src/onboard/mod.rs`,
  `src/onboard/wizard.rs`, `tests/doctor_readonly_cli.rs`, and this receipt.
- Final implementation/test diff: 535 insertions, 171 deletions.
- Final pre-receipt diff SHA-256:
  `0ab860e6527f4a8971f128a9f4dc59a1dfc181b65c075949dd86a7f9c0ec8f2e`.
- Rollback: revert the local Step 3.2 commit before Step 3.3 is based on it.
- No GitHub action, push, merge, binary install, service restart, active
  configuration mutation, database mutation, or runtime activation was
  performed.
