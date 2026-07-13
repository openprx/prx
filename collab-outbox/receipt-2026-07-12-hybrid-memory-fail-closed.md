# Step 1.3 hybrid process-memory fail-closed receipt

Date: 2026-07-12  
Branch/worktree: `fix/hybrid-memory-fail-closed` / `/opt/worker/wt/prx-hybrid-memory-fail-closed`  
Baseline: `48cd8eb0`

## Scope and result

- `sessions_spawn.process_memory_strategy = "hybrid"` is rejected during
  `Config::validate` with a precise explanation: no production merge consumer
  or merge/reject/ack/cleanup protocol exists.
- `shared_fabric` and `isolated_private` remain accepted.
- Process spawn validates the strategy before recording the request event or
  inserting a `Running` registry entry, preventing ghost events/runs.
- A legacy serialized and correctly resealed hybrid `WorkerManifest` remains
  parseable, but the binary execution validation boundary rejects it with the
  same precise error.
- Existing hybrid backend/API/history primitives and their tests remain in the
  tree. No Option B merge protocol was implemented and no compatibility data
  was deleted.
- Init/schema support declarations now advertise only the two executable
  strategies.
- No commit, push, deploy, service restart, or runtime configuration mutation
  was performed.

## Red characterization

Command (isolated target directory):

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-hybrid-failclosed-target TMPDIR=/opt/worker/tmp cargo test --lib process_memory_strategy_is_explicitly_validated -- --nocapture
```

Observed baseline failure (exit 101):

```text
called `Result::unwrap_err()` on an `Ok` value: "hybrid"
```

This proved the public spawn normalization still accepted hybrid before the
fail-closed implementation.

## Focused green evidence

All commands below used
`CARGO_TARGET_DIR=/opt/worker/tmp/prx-hybrid-failclosed-target TMPDIR=/opt/worker/tmp`.

```text
cargo test --lib config_rejects_hybrid_process_memory_without_merge_consumer -- --nocapture
exit 0; 1 passed

cargo test --lib hybrid_process_spawn_is_rejected_before_events_or_registry_side_effects -- --nocapture
exit 0; 1 passed; no memory event and no active registry run

cargo test --lib process_memory_strategy_is_explicitly_validated -- --nocapture
exit 0; 1 passed

cargo test --bin prx worker_manifest_rejects_hybrid_memory_without_merge_consumer -- --nocapture
exit 0; 1 passed; legacy serde parse succeeded, resealed execution validation rejected hybrid

cargo test --bin prx hybrid_worker_shared_context_reads_parent_fabric -- --nocapture
exit 0; 1 passed

cargo test --bin prx hybrid_worker_result_creates_private_draft_and_parent_merge_request -- --nocapture
exit 0; 1 passed

cargo test --bin prx hybrid_worker_draft_obeys_readonly_resource_gate -- --nocapture
exit 0; 1 passed
```

The final three tests intentionally exercise retained historical hybrid backend
primitives directly; they do not make hybrid executable through configuration
or a sealed worker manifest.

One over-broad exploratory command, `cargo test <worker-filter>`, was stopped
with Ctrl-C (exit 130) after Cargo began linking unrelated integration test
targets. It produced no test failure. The corrected binary-only command above
is the authoritative worker result.

## Compile and hygiene gates

```text
cargo check --all-targets --all-features
exit 0

cargo check --no-default-features
exit 0

cargo fmt --all -- --check
exit 0

git diff --check
exit 0
```

## Formal gate closure

The main thread reran the strict affected gates with the same isolated target:

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
exit 0

cargo test --bin prx --all-features
5535 passed; 0 failed; 7 ignored

cargo test --locked --test architecture_boundaries
4 passed; 0 failed
```

Two independent final reviews reported no High or Medium blockers. The final
review specifically rechecked that hybrid rejection precedes approval-grant
reconstruction, audit, grant consumption, request events, registry insertion,
and worker workspace creation.

## Review fix: reject before approval gate

Review found that the first implementation normalized the process memory
strategy after `SideEffectGate`. That ordering could emit an approval audit,
consume a valid single-use v2 grant, or return an approval error before the
more fundamental hybrid configuration error.

The process strategy is now normalized immediately after mode parsing, before
approval-grant reconstruction, `SideEffectGate`, its audit, and grant
consumption. The no-ghost handler test intentionally supplies no grant and
still receives the exact hybrid-unavailable error, proving approval is not the
earlier failure. It continues to assert no memory event and no registry run.

The public configuration guide now lists only `shared_fabric` and
`isolated_private`, and explains why `hybrid` is fail-closed.

Review-fix verification:

```text
cargo test --lib hybrid_process_spawn_is_rejected_before_events_or_registry_side_effects -- --nocapture
exit 0; 1 passed; exact hybrid error without an approval grant, zero memory events, zero registry runs

cargo test --lib process_memory_strategy_is_explicitly_validated -- --nocapture
exit 0; 1 passed

cargo fmt --all -- --check
exit 0

git diff --check
exit 0
```
