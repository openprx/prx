# Receipt: Step 4.2 Transactional configuration mutation

Date: 2026-07-15
Branch: `fix/transactional-config`
Worktree: `/opt/worker/wt/prx-transactional-config`
Baseline: `19271af0514aa126bb96a91db70d44c6a7ddc2b1`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Kept initialization and inspection distinct: `load_or_init` remains the
  explicit initializer, while `load_existing_read_only` and its config-dir
  variant require an existing tree and do not create or migrate state.
- Added explicit `plan_mutation` and `commit_mutation_atomically` APIs for the
  complete effective configuration tree.
- Mutation planning renders all desired managed files, copies current unknown
  user-owned fragments into an isolated staging directory, and validates the
  fully merged configuration before target mutation.
- Commit repeats staging validation under the cross-process writer lock,
  snapshots every affected file, and publishes an odd generation before any
  file change. Configuration readers retry on a changing generation and only
  accept a matching even generation.
- Any ordinary write/delete/publish error restores the previous main and
  managed files before republishing the prior stable generation. If a process
  dies while an odd generation is present, readers fail closed instead of
  accepting a mixed tree.
- The generation marker is a hot-reload event, so a completed multi-file
  transaction always produces a stable post-commit reload opportunity.
- Routed all production configuration writers through the transaction:
  `Config::save`, split, merge, legacy-secret migration, `prx init`, onboarding
  consumers, structured Gateway config updates, and Gateway raw-file updates.
- Raw-file editing retains peer managed fragments byte-for-byte. Explicit
  unknown-file edits are allowed, but unknown names never enter the managed
  deletion set.
- `prx init --force` now removes managed fragments obsolete for the selected
  preset. A full-to-minimal regeneration retains only `memory.toml` and
  `agent.toml` while preserving every unknown operator-owned fragment.
- `Spec::generate` stages and commits configuration before creating/scaffolding
  workspace state. The CLI now awaits that transactional operation.
- Documented the tree transaction, generation barrier, rollback, and
  interrupted-generation recovery boundary in `docs/configuration.md`.

## Red-first and review evidence

On the Step 4.1 baseline,
`force_regeneration_removes_stale_managed_fragments_but_preserves_unknown`
failed on surviving `channels.toml` after full-to-minimal `--force`. The same
assertion is now green and checks all 11 obsolete managed names plus the
operator-owned file.

An injected mid-commit failure test proves complete managed-tree rollback and
stable even-generation restoration. Additional tests prove staging failure
does not touch the target, generation changes force a reread even when the
first read errors, raw managed edits validate before commit, explicit unknown
edits remain user-owned, and flat configs with unknown-only `config.d` stay
flat.

The final regression pass also caught an empty-`config.d` compatibility issue
introduced during implementation. The unnecessary directory removal was
removed, and both the merge guard and flat/unknown save tests pass on the final
tree.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final tree with no
  reported warnings.
- `cargo test -p openprx --lib 'config::files::tests::'` - 11 passed, 0 failed,
  4,332 filtered out.
- `cargo test -p openprx --lib 'config::init::tests::'` - 22 passed, 0 failed,
  4,321 filtered out.
- `cargo test -p openprx --lib config_split_ -- --nocapture` - 2 passed, 0
  failed, 4,341 filtered out.
- `cargo test -p openprx --lib config_merge_refuses_unmanaged_fragments -- --nocapture`
  - 1 passed, 0 failed, 4,342 filtered out on the final tree.
- `cargo test -p openprx --lib config_save_ -- --nocapture` - 4 passed, 0
  failed, 4,339 filtered out.
- `cargo test -p openprx --lib migration_read_only_config_load_does_not_initialize_missing_directory -- --nocapture`
  - 1 passed, 0 failed, 4,342 filtered out.
- `cargo test -p openprx --lib config_source_generation_tracks_main_and_fragments -- --nocapture`
  - 1 passed, 0 failed, 4,342 filtered out.
- `cargo test -p openprx --test config_persistence -- --nocapture` - 19 passed,
  0 failed, 0 filtered out.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full workspace/binary suite,
architecture guards, dependency/security audits, independent review, and a
release build were not run. They remain GitHub delivery gates, not local Step
4.2 gates.

## Scope and rollback

- Scope: config file planning/commit/loading, initialization, Gateway config
  mutation, worker config generation consistency, configuration documentation,
  colocated tests, and this receipt.
- Final implementation/test/docs diff: 862 insertions, 123 deletions.
- Final pre-receipt diff SHA-256:
  `2c6949e14d10b6c06e76f9f3f32c3cc5b91d6546d14eea8f7b4ea2a65e8f736a`.
- Rollback: revert the local Step 4.2 commit before Step 4.3 is based on it.
- No `prx init` command was executed. Tests invoked `Spec::generate` only
  against isolated temporary directories.
- No GitHub action, push, merge, deploy, binary install, service operation,
  process restart, active configuration mutation, database mutation, network
  listener, runtime activation, or release was performed.
