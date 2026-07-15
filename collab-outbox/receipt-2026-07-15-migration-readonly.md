# Receipt: Step 3.1 Migration commands become read-only

Date: 2026-07-15
Branch: `fix/migration-readonly`
Worktree: `/opt/worker/wt/prx-migration-readonly`
Baseline: `598b7f346f6d697808f15cf7542d9bd25f63b1bc`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Routed schema `status`, `verify`, `dry-run`, `plan`, and the deprecated
  `baseline` compatibility command through an existing-config-only loader.
  These commands no longer initialize a config directory, workspace, config
  file, secret migration, or database.
- Replaced the synthetic `schema_migrations` baseline as the source of truth
  with each configured backend's authoritative `memory_schema_migrations`
  ledger and canonical registry.
- Opened SQLite/Lucid databases with `SQLITE_OPEN_READ_ONLY`. A missing database
  or authoritative ledger is an explicit non-success result and creates no
  state.
- Added PostgreSQL inspection using the configured URL/schema, a bounded
  configured connection timeout, validated schema identifier, and a database
  read-only transaction.
- Made unsupported backends, unknown applied versions, migration-name drift,
  checksum drift, missing PostgreSQL URL, and unknown plan targets explicit
  non-success outcomes.
- Reports applied and pending authoritative versions. The old synthetic
  SQLite ledger is displayed only as labeled compatibility evidence and never
  satisfies or mutates the authoritative ledger.
- Disabled `prx migrate baseline` as a write operation while preserving a clear
  compatibility error for existing callers.
- Exposed the existing SQLite/PostgreSQL canonical registry and checksum
  helpers at crate scope so inspection verifies exactly the descriptors used
  by backend startup, without a duplicated registry.

## Red-first evidence preserved

Before implementation, the focused schema-migration run executed five tests:
three passed and two failed for the expected baseline defects.

1. `status_probe_does_not_create_legacy_ledger` failed because status created
   the synthetic `schema_migrations` table.
2. `verify_missing_authoritative_ledger_is_non_success` failed because verify
   returned success when no authoritative ledger existed.

Both assertions remain in the final suite and are green. Additional coverage
proves no missing workspace/database creation, byte-for-byte SQLite read-only
inspection, unsupported-backend rejection, unknown-version rejection,
checksum rejection, legacy-evidence demotion, PostgreSQL identifier validation,
and the real CLI startup boundary.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed in 20.15s on the final tree
  with no reported warnings.
- `cargo test -p openprx --bin prx --all-features 'schema_migration::tests::' -- --nocapture`
  - 9 passed, 0 failed, 0 ignored, 5,629 filtered out.
- `cargo test -p openprx --bin prx --all-features migration_read_only_config_load_does_not_initialize_missing_directory -- --nocapture`
  - 1 passed, 0 failed, 5,637 filtered out.
- `cargo test -p openprx --test migration_readonly_cli --all-features -- --nocapture`
  - 3 passed, 0 failed, 0 filtered out.
- `git diff --check` - passed.

The CLI integration tests execute the built `prx` binary and prove that
missing-config status, missing-database verify, and deprecated baseline all
exit nonzero without creating their forbidden state.

Per `verification-policy.md`, strict clippy, full binary/workspace suites,
architecture guards, dependency/security audits, independent review, and a
release build were not run. They remain GitHub delivery gates, not local Step
3.1 gates.

## Scope and rollback

- Scope: `src/config/schema.rs`, `src/lib.rs`, `src/main.rs`,
  `src/memory/postgres.rs`, `src/memory/sqlite.rs`, `src/migration.rs`,
  `src/schema_migration/mod.rs`, `tests/migration_readonly_cli.rs`, and this
  receipt.
- Final implementation/test diff: 619 insertions, 288 deletions.
- Final pre-receipt diff SHA-256:
  `d070b0995e69a916433de007796ffafe77abf8b1ae77c78395092fdcc47a7564`.
- Rollback: revert the local Step 3.1 commit before Step 3.2 is based on it.
- No GitHub action, push, merge, binary install, service restart, active
  configuration mutation, database mutation, or runtime activation was
  performed.
