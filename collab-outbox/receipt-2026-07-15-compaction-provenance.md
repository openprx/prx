# Receipt: Step 5.3 compaction provenance

Date: 2026-07-15
Branch: `feat/compaction-provenance`
Worktree: `/opt/worker/wt/prx-compaction-provenance`
Baseline: `b7b942c7ff3d8c3c44f233c0309d4bc26f29c378`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Replaced compaction's synthetic `{index, role, content_hash}` source
  references with actual `MessageEvent.event_id` strings resolved from the
  current session's canonical plus legacy event scope.
- Added a typed covered-event range containing the first and last event IDs,
  first and last database row IDs, and source-event count. Event IDs and their
  range are an inseparable pair and are validated before SQLite or Postgres
  persistence.
- Resolution requires one unique, ordered, contiguous match of all compacted
  user/assistant messages inside the bounded session event window. Missing,
  truncated, or ambiguous history records no event provenance instead of
  selecting arbitrary or invented identifiers.
- Preserved document references as a separate compaction field; document IDs
  are never represented as MessageEvent IDs.
- Applied the same provenance behavior to configurable Agent compaction and
  legacy Chat overflow compaction.
- Both compaction paths now append a `compaction.summary.created` MessageEvent
  when exact provenance is available. Its causation points at the final covered
  source event and its payload contains the complete source ID list and typed
  covered range. The compaction-run payload links back to this summary event.
- Added the covered-range column and registered migration metadata for SQLite
  and Postgres. Existing SQLite databases upgrade idempotently; both backend
  append/read mappings remain aligned.

## Red-first and negative evidence

The schema test was first extended to require
`compaction_runs.source_event_range_json`; before implementation it failed with
`missing compaction_runs.source_event_range_json`.

Contract tests reject object-shaped content-hash references and require event
IDs to agree with range endpoints and count. A runtime negative test records
two identical event sequences and proves ambiguity produces no provenance.
Positive SQLite-backed tests prove both compaction entrypoints persist the
actual source event IDs/range and emit a causally linked summary event.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final tree with no
  reported warnings.
- `cargo test -p openprx --lib compaction_provenance` - 3 passed, 0 failed.
- `cargo test -p openprx --lib configurable_compaction_records_run_and_summary_memory`
  - 1 passed, 0 failed.
- `cargo test -p openprx --lib legacy_chat_compaction_persists_run_and_summary_memory`
  - 1 passed, 0 failed.
- `cargo test -p openprx --lib legacy_compaction_runs_schema_adds_source_event_range`
  - 1 passed, 0 failed.
- `cargo test -p openprx --lib compaction_run_persists_summary_audit` - 1
  passed, 0 failed.
- `cargo test -p openprx --lib message_events_schema_is_created` - 1 passed, 0
  failed.
- `cargo test -p openprx --lib sqlite_and_postgres_compaction_paths_persist_ids_and_covered_range`
  - 1 passed, 0 failed.
- `cargo test -p openprx --lib g1_schema_migration` - 3 passed, 0 failed.
- `cargo test -p openprx --lib postgres` - 19 passed, 0 failed. Environment
  conformance bodies safely skipped because `OPENPRX_TEST_POSTGRES_URL` was not
  configured; no live Postgres claim is made.
- `git diff --check` - passed.

Three initially attempted aliases matched zero tests and are intentionally not
counted above; their exact nonzero test names were found and rerun.

Per `verification-policy.md`, strict clippy, the full workspace/integration
suite, architecture guards, dependency/security audits, independent review,
live Postgres conformance, and a release build were not run. They remain GitHub
delivery gates, not local Step 5.3 gates.

## Scope and rollback

- Scope: Agent and legacy Chat compaction audit/provenance, shared compaction
  DTO validation, SQLite/Postgres schema and mappings, focused tests, and this
  receipt.
- Final pre-receipt implementation/test diff: 747 insertions, 93 deletions.
- Final pre-receipt diff SHA-256:
  `f353d16e7f17da0b8400ac7178987cf680077b5516b9fc819ea85cb85d84ef05`.
- Rollback: revert the local Step 5.3 commit before Step 5.4 is based on it.
- No push, merge, deploy, binary install, service operation, process restart,
  network listener, active configuration mutation, external database mutation,
  GitHub action, release, or runtime activation was performed.
