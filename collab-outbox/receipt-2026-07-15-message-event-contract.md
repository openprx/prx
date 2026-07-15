# Receipt: Step 5.1 extend MessageEvent

Date: 2026-07-15
Branch: `feat/message-event-contract`
Worktree: `/opt/worker/wt/prx-message-event-contract`
Baseline: `1ee58b968e52258fa48b65a9e108e63103b6c192`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Extended the existing `MessageEventInput` and `MessageEvent`; no
  `RuntimeEvent` struct, enum, table, or parallel ledger was added.
- Replaced the unstructured Rust `source: String` API with
  `MessageEventSource`, including stable string-compatible serde for known and
  custom adapters.
- Added typed `MessageEventSubject` variants and optional goal, causation,
  correlation, attempt, and lease-epoch lineage.
- Made `event_type` an explicit required append input. Both backends validate
  it before opening a transaction; content is no longer parsed to infer type.
- Preserved the existing role/content and outbox behavior by explicitly using
  `message.created`, `worker.result.created`, or the caller-supplied runtime
  event type at each fabric entrypoint.
- Added equivalent SQLite and Postgres columns:
  `source_ref_json`, `subject_ref_json`, `goal_id`,
  `causation_event_id`, `correlation_id`, `attempt_id`, and `lease_epoch`, plus
  the already-present but now explicit `event_type` projection.
- SQLite and Postgres serialize, insert, select, map, append the memory-event
  outbox row, and commit within their existing single transaction boundaries.
- Kept the legacy `source` text column and string JSON representation for
  compatibility. Existing SQLite rows gain the new nullable columns in place;
  missing historical event type reads as `message.legacy`.
- RuntimeEnvelope now projects task/topic into typed subject lineage and its
  source message event into causation lineage. Goal/attempt/lease fields remain
  optional until their owning domains supply them in later planned steps.
- Updated all direct test/compatibility append fixtures to state an event type
  explicitly.

## Red-first and parity evidence

On the Step 4.3 baseline,
`memory::sqlite::tests::message_events_schema_is_created` failed with
`missing message_events.source_ref_json`. It is green on the final tree and
checks every new column.

The explicit-type test stores content that does not contain an event-type
prefix and proves the supplied `router.route_decision` is returned and mirrored
to the outbox. A separate test rejects an empty event type before any row is
written. The legacy-schema test opens a pre-Step-5.1 table, upgrades it in
place, and reads its event through the compatibility projection.

A shared contract test checks both backend append implementations for every
lineage field, explicit `input.event_type`, transaction creation, and commit.
The environment did not provide `OPENPRX_TEST_POSTGRES_URL`, so the live
Postgres conformance branch was not exercised and is not claimed; Postgres code
was compiled by the all-features check and its nine module tests passed.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final tree with no
  reported warnings.
- `cargo test -p openprx --lib message_event -- --nocapture` - 23 passed, 0
  failed, 5,607 filtered out.
- `cargo test -p openprx --lib memory::fabric::tests:: -- --nocapture` - 6
  passed, 0 failed, 5,624 filtered out.
- `cargo test -p openprx --lib runtime::envelope::tests:: -- --nocapture` - 19
  passed, 0 failed, 5,611 filtered out.
- `cargo test -p openprx --lib memory::postgres::tests:: -- --nocapture` - 9
  passed, 0 failed, 5,621 filtered out; environment-gated live database bodies
  returned early because no Postgres URL was configured.
- `git diff --check` - passed.
- Source scan for a `RuntimeEvent` type/table - no matches.

Per `verification-policy.md`, strict clippy, the full workspace/integration
suite, architecture guards, dependency/security audits, independent review,
live Postgres conformance, and a release build were not run. They remain
GitHub delivery gates, not local Step 5.1 gates.

## Scope and rollback

- Scope: MessageEvent DTO/source/subject types, MemoryFabric event typing,
  SQLite/Postgres schema and append/read mapping, RuntimeEnvelope lineage
  projection, compatibility fixtures, colocated tests, and this receipt.
- Final pre-receipt implementation/test diff: 750 insertions, 120 deletions.
- Final pre-receipt diff SHA-256:
  `4f140cd8464b697ffb3a474a5340e87aadfff6760d62844894144ba7f1bc508f`.
- Rollback: revert the local Step 5.1 commit before Step 5.2 is based on it.
- No push, merge, deploy, binary install, service operation, process restart,
  active configuration mutation, database mutation outside isolated temporary
  SQLite files, network listener, runtime activation, GitHub action, or release
  was performed.
