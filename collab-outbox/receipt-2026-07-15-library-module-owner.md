# Receipt: Step 4.3 library/binary module graph

Date: 2026-07-15
Branch: `refactor/library-module-owner`
Worktree: `/opt/worker/wt/prx-library-module-owner`
Baseline: `f4f639da93b9a71ac61c489bc4f7165a374dfc1e`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Made `src/lib.rs` the single owner of every runtime subsystem source module.
  `src/main.rs` no longer recompiles 44 shared modules and retains only the
  binary-only CLI dispatch module at `src/runtime/mode.rs`.
- Replaced binary-local module declarations with imports from the `openprx`
  library. The CLI bootstrap and dispatch behavior were not refactored.
- Removed binary-local copies of `ServiceCommands`, `ChannelCommands`,
  `SkillCommands`, `MigrateCommands`, `CronCommands`, `EvolutionCommands`,
  `EvolutionLayerArg`, and `IntegrationCommands`; lib and bin now use the same
  DTO definitions.
- Reconciled two pre-existing DTO drifts while selecting the binary behavior as
  authoritative: `MigrateCommands::Plan` and `IntegrationCommands::List` now
  exist in the shared library definitions. Existing command help text and clap
  attributes were preserved from the former binary definitions.
- Moved `ChatSubscriber`, `ChatWriter`, `ChatFmtLayer`, and
  `CHAT_TRACING_RELOAD` to the library root, so binary tracing initialization
  and Chat redirection use the same process-global registry.
- Exposed only the four existing lib-to-bin boundary operations required by
  the ownership split: stored-config rendering, approval schema initialization,
  the explicitly never-cancelled shutdown token, and the shutdown module path.
- Added a structural regression test covering every former shared `mod`
  declaration, every shared command DTO, and the tracing registry location.
- Added CLI characterization for the formerly drifted `migrate plan` DTO.

## Red-first evidence

Before the ownership migration,
`module_ownership_tests::binary_imports_the_library_module_graph` failed on
`src/main.rs` declaring `mod agent;`. The final test checks all former shared
module declarations, all eight cross-boundary DTO names, and the process-global
tracing registry, and is green.

The initial shared-graph compile also proved the actual visibility boundary:
only four existing operations required public lib-to-bin access. No runtime
control flow, handler ordering, signal ownership, or subsystem implementation
was moved or changed.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final tree with no
  reported warnings.
- `cargo test -p openprx --lib module_ownership_tests::binary_imports_the_library_module_graph -- --exact --nocapture`
  - 1 passed, 0 failed, 5,625 filtered out.
- `cargo test -p openprx --bin prx -- --nocapture` - 28 passed, 0 failed, 0
  filtered out. This includes CLI parsing, completion/help definition, dispatch
  signal-policy tests, the shared integration-list DTO, and the shared
  migrate-plan DTO.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full workspace/integration
suite, architecture guards, dependency/security audits, independent review,
and a release build were not run. They remain GitHub delivery gates, not local
Step 4.3 gates.

## Scope and rollback

- Scope: lib/bin module declarations, shared cross-boundary DTOs, the existing
  Chat tracing registry, minimal visibility changes needed by the new crate
  boundary, colocated characterization tests, and this receipt.
- Final pre-receipt implementation/test diff: 195 insertions, 387 deletions.
- Final pre-receipt diff SHA-256:
  `20e79e99f258275e14eff3b6dd7a6bd0c59f24ad7b7a728367364e4391b5dacd`.
- Rollback: revert the local Step 4.3 commit before Step 5.1 is based on it.
- No push, merge, deploy, binary install, service operation, process restart,
  active configuration mutation, database mutation, network listener, runtime
  activation, GitHub action, or release was performed.
