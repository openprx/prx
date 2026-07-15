# Receipt: Step 4.1 Parent/worker config source propagation

Date: 2026-07-15
Branch: `fix/worker-config-source`
Worktree: `/opt/worker/wt/prx-worker-config-source`
Baseline: `6009cfd2697b35dabc2299beeecec9bc089c15ab`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- The parent resolves and canonicalizes its selected config directory before a
  process-mode spawn records or creates the child run.
- The complete configuration source generation covers `config.toml` and every
  sorted TOML fragment under `config.d`, with file names and byte lengths framed
  into a SHA-256 digest.
- Config directory and generation are carried in the worker manifest and are
  therefore covered by the existing expiring HMAC capability.
- The spawned CLI receives the same config directory as a global
  `--config-dir` argument; `main.rs` forwards it to the worker runner.
- Worker validation requires an absolute sealed config directory, a valid
  generation digest, and an exact CLI/manifest directory match.
- Worker startup hashes the source before and after loading and fails closed if
  it differs from the parent generation or changes while being loaded.
- Worker loading now uses `load_existing_read_only_with_config_dir`; a missing
  source is an error and cannot initialize a default config or workspace.
- The loaded in-memory config remains the signed generation even if disk files
  change after the post-load check.

## Red-first evidence preserved

The Step 3.4 baseline produced two expected failures:

1. `process_mode_task_arg_is_not_json_encoded` failed because the worker argv
   did not contain `--config-dir` or the resolved parent directory.
2. `worker_missing_config_source_fails_without_initializing_defaults` failed
   because the runner called the initializing loader and created the missing
   config directory.

Both assertions are now green. Additional tests prove generation changes for
main config and fragment edits, manifest round-trip preservation, sealed
CLI/config matching, rejection of a changed generation before workspace side
effects, and capability rejection after generation tampering.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` for the final focused tests and all-features check.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final tree with no
  reported warnings.
- `cargo test -p openprx --lib process_mode_task_arg_is_not_json_encoded` - 1
  passed, 0 failed, 4,334 filtered out.
- `cargo test -p openprx --lib config_source_generation_tracks_main_and_fragments`
  - 1 passed, 0 failed, 4,334 filtered out.
- `cargo test -p openprx --lib worker_manifest_roundtrip_json` - 1 passed, 0
  failed, 4,334 filtered out.
- `cargo test -p openprx --bin prx worker_missing_config_source_fails_without_initializing_defaults`
  - 1 passed, 0 failed, 5,643 filtered out.
- `cargo test -p openprx --bin prx worker_rejects_changed_config_generation_before_workspace_side_effects`
  - 1 passed, 0 failed, 5,643 filtered out.
- `cargo test -p openprx --bin prx worker_cli_config_dir_must_match_sealed_manifest`
  - 1 passed, 0 failed, 5,643 filtered out.
- `cargo test -p openprx --bin prx tampered_config_generation_capability_rejected`
  - 1 passed, 0 failed, 5,643 filtered out.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full workspace/binary suite,
architecture guards, dependency/security audits, independent review, and a
release build were not run. They remain GitHub delivery gates, not local Step
4.1 gates.

## Scope and rollback

- Scope: `src/tools/sessions_spawn.rs`, `src/main.rs`,
  `src/session_worker/runner.rs`, the shared signed-manifest protocol helper,
  colocated tests, and this receipt.
- Final implementation/test diff: 246 insertions, 7 deletions.
- Final pre-receipt diff SHA-256:
  `4c9a831a64698fe368bd3938a88115fcc9302e9da63c8ebd796a81acb201a304`.
- Rollback: revert the local Step 4.1 commit before Step 4.2 is based on it.
- No GitHub action, push, merge, deploy, binary install, service operation,
  process restart, active configuration mutation, database mutation, network
  listener, runtime activation, or release was performed.
