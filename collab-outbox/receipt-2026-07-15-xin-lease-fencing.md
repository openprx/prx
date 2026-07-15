# Receipt: Step 8.1 Xin lease fencing

Date: 2026-07-15
Branch: `feat/xin-lease-fencing`
Worktree: `/opt/worker/wt/prx-xin-lease-fencing`
Base: `0b40549281e1e36484ab8fb1536240cb821c89a5`
Status: implementation and local verification complete; to be recorded by the
local Step 8.1 commit; not pushed, merged, deployed, installed, or activated

## Delivered

- Added persisted `xin_steps.lease_epoch INTEGER NOT NULL DEFAULT 0` and exposed
  it on `XinStep` with backward-compatible serde defaulting.
- Made every successful claim atomically increment the epoch. Reclaims by the
  same worker ID receive a new generation; renewal extends expiry while keeping
  the epoch unchanged.
- Required owner plus epoch and a live expiry for claimed-to-running, renew,
  checkpoint, complete, retry/fail, and their terminal lifecycle append.
- Kept completion/failure state mutation and lifecycle event append inside one
  immediate SQLite transaction. Terminal/retry event payloads now record the
  authorizing step ID, lease owner, and lease epoch.
- Removed unfenced checkpoint/terminal helpers from production builds. Legacy
  complete/fail helpers remain test-only for state-machine fixtures that do not
  execute through the runner.
- Preserved and verified the runner cancellation boundary: renewal rejection or
  deadline loss cancels and drops the execution body, joins the heartbeat, and
  skips stale checkpoint/terminal persistence.
- Added old-database column migration, old-serialized-step compatibility, epoch
  reclaim/fencing, terminal event authority, renewal, checkpoint, and execution
  cancellation coverage.
- Added `docs/xin-lease-fencing.md` as the Step 8.1 architecture baseline.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib xin::store::tests::` - 38 passed, 0 failed.
- `cargo test -p openprx --lib xin::runner::tests::` - 15 passed, 0 failed.
- Focused acceptance total: 53 passed, 0 failed.
- `git diff --check` - passed.

The first broad runner sweep found one stale permission fixture: it expected a
medium-risk command without a grant to fail while using the Step 6.4 Full
autonomy default. The no-grant and persisted-grant tests now explicitly use
Supervised mode, restoring the semantics named by the tests; the final runner
sweep passed 15/15. No production permission behavior changed.

Per `verification-policy.md`, strict clippy, full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live
runtime acceptance, and release builds were not run. They remain GitHub
delivery or release/deployment gates.

## Scope and rollback

- Scope: Xin step schema/type, claim and lease-fenced store mutations, runner
  lease tests/checkpoint metadata, compatibility tests, architecture doc, and
  this receipt.
- Step 8.2 transactional transition redesign and Step 8.3 Heartbeat migration
  were not implemented.
- Rollback: revert the local Step 8.1 commit before basing Step 8.2 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.
