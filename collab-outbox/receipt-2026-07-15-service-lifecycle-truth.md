# Receipt: Step 3.3 Service lifecycle truth

Date: 2026-07-15
Branch: `fix/service-lifecycle-truth`
Worktree: `/opt/worker/wt/prx-service-lifecycle-truth`
Baseline: `ac0c35ef48013c0cd7ae38932c40b23ec43524ef`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Replaced the invalid launchd raw XML containing literal `\"` sequences with
  a pure, valid-shape plist generator and XML-escaped arguments/paths.
- Preserved the effective config directory in launchd, systemd, OpenRC, and
  Windows definitions through an explicit `--config-dir` argument. Paths are
  escaped for XML, systemd argument syntax, shell/OpenRC, or Windows command
  syntax as appropriate.
- Changed systemd install so `daemon-reload` and `enable` failures propagate.
- Changed macOS/Linux/Windows stop so manager failures propagate before any
  success message.
- Changed systemd/OpenRC/Windows uninstall so stop, runlevel/task deletion,
  file removal, and daemon-reload failures propagate instead of being reduced
  to warnings or ignored results.
- Preserved the existing strict start/restart propagation.
- Added typed `ServiceState` (`Running`, `Stopped`, `NotInstalled`, `Unknown`)
  and `ServiceStatus` with manager, unit path, and bounded command detail.
- Status now prints the structured result and succeeds only for `Running`.
  Stopped, missing, unknown, spawn failure, and manager failure all produce a
  meaningful nonzero CLI result.
- Strengthened command errors with command, exit status, stdout, and stderr;
  nonzero capture can no longer masquerade as successful status text.

## Red-first evidence preserved

The baseline Service filter executed 18 tests with 16 passing and two failing
for the expected defects:

1. `systemd_unit_preserves_selected_config_directory` proved the unit omitted
   the selected config directory.
2. `run_capture_rejects_non_zero_status` proved a command that printed
   `inactive` and exited 3 was returned as success.

Both assertions remain and are green. Additional tests cover launchd XML
shape/escaping, config propagation for all definition formats, typed status
classification, non-Running exit semantics, and OS-manager failure propagation.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed in 1m55s on the final
  production tree with no reported warnings.
- `cargo test -p openprx --bin prx --all-features 'service::tests::' -- --nocapture`
  - 22 passed, 0 failed, 0 ignored, 5,626 filtered out.
- `cargo test -p openprx --test service_lifecycle_cli --all-features -- --nocapture`
  - 4 passed, 0 failed, 0 filtered out.
- `git diff --check` - passed.

The CLI integration suite places a fake `systemctl` first in `PATH`; it never
touches the host service manager. It proves stopped status is structured and
nonzero, stop failure cannot print success, enable failure propagates after
unit generation, uninstall reload failure propagates, and the generated unit
contains the selected config directory.

Per `verification-policy.md`, strict clippy, full binary/workspace suites,
architecture guards, dependency/security audits, independent review, and a
release build were not run. They remain GitHub delivery gates, not local Step
3.3 gates.

## Scope and rollback

- Scope: `src/service/mod.rs`, `tests/service_lifecycle_cli.rs`, and this
  receipt.
- Final implementation/test diff: 463 insertions, 99 deletions.
- Final pre-receipt diff SHA-256:
  `a583a338d662a8f38b1071274a63187e0b748fef536ecfdbd2b29125b39d8676`.
- Rollback: revert the local Step 3.3 commit before Step 3.4 is based on it.
- No host service command, GitHub action, push, merge, binary install, service
  restart, active configuration mutation, database mutation, or runtime
  activation was performed.
