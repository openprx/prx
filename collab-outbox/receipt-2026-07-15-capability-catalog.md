# Receipt: Step 6.3 capability catalog alignment

Date: 2026-07-15
Branch: `feat/capability-catalog`
Worktree: `/opt/worker/wt/prx-capability-catalog`
Baseline: `e13a09b295e2b848e59735710721f4f7eaabcab4`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Added the shared evidence ladder `Declared -> Configured -> Ready -> Healthy`
  and a mandatory non-empty availability reason. Configuration, executable
  construction, and positive health observation are no longer represented by
  one ambiguous `Active` state.
- Added an immutable `ToolCatalog` descriptor snapshot. It is built from the
  exact finalized native/MCP registry received by an entry point, preserves
  registry/spec ordering, resolves duplicate public names with the same
  first-registration ownership as execution, and projects provider `ToolSpec`s.
- `ToolExecutionService` now resolves and advertises from that same catalog
  snapshot. Every registered native tool and MCP alias is `Ready` with the
  concrete backend reason; `Healthy` is reserved for a future positive runtime
  probe and is never inferred during construction.
- Provider-facing tool-spec assembly now uses `ToolCatalog` in the Agent object,
  XML dispatcher, the shared Agent loop used by Channels/Gateway/workers, and
  Chat Redux. Entry-point-specific tool tiering still selects a subset, but the
  descriptor/availability/projection rules no longer diverge.
- Chat `/tools` now lists public capabilities, including aliases, from the same
  catalog and shows the evidence level plus reason instead of counting raw root
  tool boxes as generically available.
- The configuration-only integrations catalog no longer emits `Active` or
  `Available`. A detected channel/provider configuration is only `Configured`
  with an explicit statement that readiness was not probed. Missing setup and
  catalog-only plans are `Declared`; only compiled built-ins such as Shell,
  File System, and the current native platform are `Ready`.
- Integration list/info output reports all four canonical levels and their
  reasons. No configuration predicate can produce `Ready` or `Healthy`.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final implementation
  tree with no reported warnings.
- Capability availability ordering - 1 passed, 0 failed.
- `cargo test -p openprx --lib tools::execution::tests` - 10 passed, 0 failed;
  includes catalog/execution snapshot identity, MCP alias classification,
  readiness reason, policy, approval, sandbox, cancellation, and audit paths.
- `cargo test -p openprx --lib integrations::registry::tests` - 18 passed,
  0 failed; includes configured-not-ready, planned-without-backend, and built-in
  Ready evidence.
- `cargo test -p openprx --lib agent::dispatcher::tests` - 5 passed, 0 failed.
- Chat dispatcher catalog/spec filters - 2 passed, 0 failed.
- Chat `/tools` catalog rendering - 1 passed, 0 failed.
- One initial Chat `/tools` filter used the wrong test-module path and matched
  zero tests. It is excluded from evidence; the exact `mode_tests` path was
  listed and rerun with one passing test.
- `git diff --check` and staged diff check - passed.
- Source search found no remaining direct `flat_map(tool.specs())` entry-point
  projection and no legacy Active/Available/ComingSoon integration variants.

Per `verification-policy.md`, strict clippy, the full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live TUI
acceptance, and a release build were not run. They remain GitHub delivery or
deployment gates, not local Step 6.3 gates.

## Scope and rollback

- Scope: shared availability vocabulary, canonical tool catalog and provider
  projections, truthful integration catalog/status output, focused tests, and
  this receipt.
- Pre-receipt implementation diff: 532 insertions and 247 deletions across 11
  files.
- Pre-receipt staged diff SHA-256:
  `69e5246b96085da0066a0e0c026fc429db70511b326d030fce7a5b30f1788d6b`.
- Rollback: revert the local Step 6.3 commit before basing Step 6.4 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, or runtime activation was performed.
