# Receipt: Step 5.4 backend bypass elimination

Date: 2026-07-15
Branch: `fix/backend-bypasses`
Worktree: `/opt/worker/wt/prx-backend-bypasses`
Baseline: `1eed0f62ef3c7b064552c940c3636d366dd5f7d3`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- `memory_get` and `memory_search` now read through their injected
  `Arc<dyn Memory>` backend. Backend-neutral keyed/context reads and explicit
  ACL `Enforce`/`Observe` modes are implemented by SQLite and PostgreSQL.
  Observe mode preserves compatibility results while recording would-deny
  audit decisions; enforce mode returns only principal-visible rows.
- Removed the old production `memory_search` SQLite SQL reader and markdown
  directory scan. `memory_get` retains only its explicit, path-confined
  markdown fallback when ACL enforcement is disabled and the configured
  backend reports no keyed entry.
- PostgreSQL Cron lifecycle events now append to the configured
  `{schema}.{table}_memory_events` table in the same transaction as
  `cron_job_events`. The PostgreSQL path no longer opens or mirrors into a
  workspace-local `brain.db`; SQLite retains its colocated event mirror.
- The standalone webhook server receives a sealed repository handle from the
  daemon assembly boundary instead of selecting storage inside the HTTP
  service. SQLite/Lucid use the SQLite repository; PostgreSQL uses configured
  schema/table-derived topic, participant, ingestion, memory, and memory-event
  tables. Claim, projection, event append, and fenced commit are one PostgreSQL
  transaction. Markdown/none remain fail-closed.
- Process worker manifests record the effective parent memory backend. A
  worker rejects backend drift against its sealed config generation, and
  `shared_fabric` constructs the actual configured parent backend through the
  unified memory factory. Shared-context hydration reuses that injected
  backend rather than reopening SQLite. Explicit `isolated_private` remains a
  worker-local SQLite contract.
- Memory evolution no longer constructs `SqliteMemory`. Daemon scheduling and
  the manual evolution CLI inject the configured memory backend into
  `MemoryEvolutionEngine`; proposal and trash operations use the trait object.
- Added structural guards for each production assembly boundary and extended
  the shared SQLite/PostgreSQL ACL conformance contract to cover keyed and
  search observe/enforce behavior.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final tree with no
  reported warnings.
- `cargo test -p openprx --lib tools::memory_get::tests` - 13 passed, 0 failed.
- `cargo test -p openprx --lib tools::memory_search::tests` - 14 passed, 0
  failed.
- SQLite scoped memory ACL conformance - 1 passed, 0 failed, including the new
  observe/enforce keyed and search assertions.
- PostgreSQL memory-fabric env conformance - 1 test executed successfully; its
  live body safely skipped because `OPENPRX_TEST_POSTGRES_URL` was unset.
- Webhook tests - 18 passed, 0 failed, including SQLite transaction/retry,
  repository injection, PostgreSQL env-conformance registration, and
  unsupported-backend fail-closed behavior. The PostgreSQL live body safely
  skipped because the test URL was unset.
- Cron SQLite event projection, PostgreSQL structural projection, and
  PostgreSQL env lifecycle filters - 1 passed each. The PostgreSQL lifecycle
  live body safely skipped because the test URL was unset.
- Session-worker backend-drift and shared-factory guards - 1 passed each;
  worker protocol roundtrip tests - 3 passed; process-manifest compaction and
  parent-memory tests - 1 passed each.
- Memory-evolution tests - 3 passed, including configured-backend injection.
- `git diff --check` - passed.

One initially attempted process-manifest alias matched zero tests and is not
counted above; its exact two nonzero test names were found and rerun.

Per `verification-policy.md`, strict clippy, the full workspace/integration
suite, architecture guards, dependency/security audits, independent review,
live PostgreSQL conformance, and a release build were not run. They remain
GitHub delivery gates, not local Step 5.4 gates.

## Scope and rollback

- Scope: configured memory read APIs, Cron event projection, standalone webhook
  repository assembly and PostgreSQL support, session-worker shared-memory
  backend identity, self-system memory injection, focused tests, and this
  receipt.
- Final pre-receipt implementation/test diff: 1,540 insertions, 788 deletions
  across 17 files.
- Final pre-receipt diff SHA-256:
  `f4b7365536629091e69e255af4f316f617583e52a514b9108fbb51ce1c3ba68a`.
- Rollback: revert the local Step 5.4 commit before Step 6.1 is based on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, or runtime activation was performed.
