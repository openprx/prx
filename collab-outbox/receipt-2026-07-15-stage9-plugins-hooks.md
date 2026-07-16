# Receipt: Stage 9 batch 3 - Plugins and hooks

Date: 2026-07-15
Branch: `feat/stage9-plugins`
Worktree: `/opt/worker/wt/prx-stage9-plugins`
Base: `2faffe109ce03a739bf32e1ecab9365d8c0b9792`
Status: implemented and locally verified; local commit pending; not pushed,
merged, deployed, installed, or activated

## Delivered

- Added one process-level `PluginRuntime` per canonical workspace. Gateway,
  Channels, HookManager, the tool registry, cron scheduling, and the event bus
  now share this owner instead of constructing independent plugin worlds.
- Added immutable plugin generations containing the registry and every derived
  tool, middleware, hook, and cron adapter. The stable multi-spec tool router
  resolves specs and calls against a single current generation.
- Made runtime reload serialized and atomic: the complete candidate generation
  is built off-path, the requested plugin is verified, and one `ArcSwap`
  publishes it. Candidate failure leaves the previous generation active.
- Removed the registry unload gap from direct `PluginManager::reload_plugin` by
  preparing and compiling before one registry replacement.
- Replaced unbounded event-bus channels with bounded subscriber queues, added
  global subscription/topic limits, and implemented `*` wildcard truthfully.
- Added real subscriber pumps. Hook-capable plugin instances continuously
  consume subscription receivers and deliver events to their guest `on-event`
  export; other capability worlds return an explicit unsupported error instead
  of registering and discarding a receiver.
- Made cron scheduling process-owned and generation-aware so reloads reuse one
  trigger history and cannot leave an old detached scheduler as the live owner.
- Hardened native hook execution: bounded configuration/payload/stderr/action
  surfaces, secure RAII payload tempfiles, whole-lifecycle timeout, child kill
  and reap, and content-addressed atomic `hooks.json` refresh that preserves the
  old generation on invalid input.
- Bounded plugin manifests and WASM components, rejected manifest/WASM symlinks
  and escaping WASM paths, made discovery deterministic, and prevented plugins
  with denied required permissions from receiving live adapters.
- Added `docs/plugin-runtime-lifecycle.md` and aligned the hook and event-bus
  references with actual runtime behavior.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib hooks::` - 26 passed, 0 failed.
- `cargo test -p openprx --lib plugins::` - 114 passed, 0 failed.
- Focused acceptance total - 140 passed, 0 failed.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full suite, security audit,
release build, and GitHub delivery checks are not part of this local batch gate.

## Scope and rollback

- Scope: plugin generation ownership, dynamic adapters, event subscription
  delivery, cron ownership, hook process/config lifecycle, plugin admission
  bounds, affected Gateway/Channels call sites, tests, documentation, and this
  receipt.
- Media/multimodal and provider/router/cost Stage 9 batches were not started in
  this worktree.
- Rollback: revert the local Plugins/hooks batch commit.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.
