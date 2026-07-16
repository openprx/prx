# Receipt: Stage 9 batch 5 - Providers, router, and cost

Date: 2026-07-15
Branch: `feat/stage9-providers`
Worktree: `/opt/worker/wt/prx-stage9-providers`
Base: `def818244fa4f58cb82778b1a0607f574479d28b`
Status: implemented and locally verified; local commit pending; not pushed,
merged, deployed, installed, or activated

## Delivered

- Added per-model, per-mode capability truth and changed Router to report its
  resolved route while Reliable reports the safe intersection of compatible
  failover candidates.
- Stopped Gemini from advertising native tool support until its request path
  actually transmits tool schemas. Agent and outer preflight paths now consume
  the same mode-aware capability answer for tools and vision.
- Preserved complete ordered provider/model attempt attribution for streaming
  fallback, including failures before content, and made the successful attempt
  the final provider/model source.
- Unified provider construction and availability inspection behind one
  credential resolver. Fixed routed-provider isolation so a different route
  cannot inherit the primary provider's credential.
- Added an explicit idempotent `usage.settled` event to the shared terminal
  boundary used by all runtime entrypoint kinds.
- Added one process-level `CostTracker` authority per canonical workspace,
  durable settlement-id deduplication, actual metered-cost projection, and
  atomic settlement-time budget status.
- Represented disabled tracking, unknown pricing, replay, and recorded budget
  outcomes as typed `CostSettlement` values. Unknown pricing is never recorded
  as zero cost.
- Added `docs/provider-routing-cost-lifecycle.md` and updated provider/router
  runtime-boundary documentation.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed.
- `cargo test -p openprx --lib providers::` - 587 passed, 0 failed.
- `cargo test -p openprx --lib cost::` - 13 passed, 0 failed.
- `cargo test -p openprx --lib agent::terminal::` - 3 passed, 0 failed.
- Focused acceptance total - 603 passed, 0 failed.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full suite, security audit,
release build, and GitHub delivery checks are not part of this local batch gate.

## Scope and rollback

- Scope: provider capability truth, credential resolution and route isolation,
  streaming attempt trace, canonical usage events, cost settlement authority,
  affected runtime call sites, tests, docs, and this receipt.
- This is the fifth and final Stage 9 implementation batch.
- Rollback: revert the local Providers/router/cost batch commit.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.
