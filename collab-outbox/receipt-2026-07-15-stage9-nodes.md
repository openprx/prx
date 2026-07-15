# Receipt: Stage 9 batch 1 - Nodes

Date: 2026-07-15
Branch: `feat/stage9-nodes`
Worktree: `/opt/worker/wt/prx-stage9-nodes`
Base: `fcb41420b0275808ceabe4fcb2800d2adbdb7680`
Status: implemented and locally verified; local commit pending; not pushed,
merged, deployed, installed, or activated

## Delivered

- Added server-side single-flight mutation replay keyed by the stable JSON-RPC
  request ID, including payload-fingerprint conflict rejection, TTL, and a
  bounded table.
- Replaced full-buffer command output with bounded concurrent pipe draining;
  timeout and cancellation terminate and reap the child.
- Bounded client RPC responses, server RPC requests, file reads, and file
  writes before untrusted content can grow without limit.
- Replaced check-then-open file operations with descriptor-relative Unix
  traversal using no-follow opens for every component and regular-file checks;
  unsupported platforms fail closed for node file RPC.
- Restricted callbacks to public HTTPS, validated every DNS answer immediately
  before delivery, pinned the answers into a no-proxy client, and disabled
  redirects.
- Added a long-lived `NodeManager` that reuses HTTP/2 clients and
  circuit-breaker state, replacing a client only when effective node settings
  change.
- Corrected the generated Nodes example and rewrote `docs/remote-nodes.md` to
  match the live protocol and security boundaries.

## Local verification

Commands use `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed during implementation.
- `cargo fmt --all` and `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib nodes::` - 93 passed, 0 failed.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full suite, security audit,
release build, and GitHub delivery checks are not part of this local batch gate.

## Scope and rollback

- Scope: Nodes protocol transport/client/server/tool ownership, node config
  example/schema documentation, focused tests, runtime documentation, and this
  receipt.
- Skills, plugins/hooks, media, and provider/router/cost Stage 9 batches were
  not started here.
- Rollback: revert the local Nodes batch commit.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.
