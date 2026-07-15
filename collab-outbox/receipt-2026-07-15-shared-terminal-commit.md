# Receipt: Step 7.3 shared terminal commit

Date: 2026-07-15
Branch: `feat/shared-terminal-commit`
Worktree: `/opt/worker/wt/prx-shared-terminal-commit`
Base: `adf6e87b22ed28c38d760a07a37437d4ec8af952`
Status: implementation and local verification complete; to be recorded by the
local Step 7.3 commit; not pushed, merged, deployed, installed, or activated

## Delivered

- Added `agent::terminal::finalize_turn` as the shared durable close for
  provider/tool turns. It owns idempotent assistant history projection,
  provider outcome telemetry, one usage/cost settlement, terminal telemetry,
  attempt/lease metadata, delivery intent, and the final `turn.finalized`
  marker.
- Made the terminal marker the last write. Stable assistant, provider-attempt,
  provider-final, and terminal idempotency keys make partial commit replay
  converge without duplicate history, outcomes, or terminal markers.
- Added settlement IDs to metered usage records and made Chat session usage
  projection deduplicate replays by settlement ID.
- Routed Chat Redux/legacy/ordered/detached completion, silence, failure, and
  cancellation through the finalizer while retaining Chat ordering and domain
  state ownership.
- Routed Agent CLI single-shot/interactive/CTE/cancellation and
  `process_message` paths through the finalizer. Direct assistant writes remain
  only as persistence-failure fallbacks.
- Routed Channels, Gateway webhook, Gateway console, Session Worker,
  `sessions_spawn`, and Delegate success/failure/silent/timeout/cancellation
  paths through the same terminal owner as applicable.
- Preserved domain ledgers and projections: provider outcomes remain telemetry;
  Chat/session worker/spawn/delegate process or task events remain their domain
  authority; delivery adapters still perform actual delivery.
- Added the architecture baseline in
  `docs/shared-turn-terminal-commit.md` and updated the Chat/Agent
  characterization status.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- Shared terminal replay tests - 3 passed, 0 failed:
  completed replay; non-reply terminal states; eight-entrypoint contract.
- Chat settlement replay - 1 passed, 0 failed.
- CTE approval shared-close test - 1 passed, 0 failed.
- Channel full-turn shared terminal test - 1 passed, 0 failed.
- Gateway webhook durable event test - 1 passed, 0 failed.
- Gateway console shared-history test - 1 passed, 0 failed.
- Delegate and `sessions_spawn` request/result/terminal tests - 2 passed,
  0 failed.
- `provider_turn_finalizer_` - 7 passed, 0 failed.
- `step_7_1_same_` - 3 passed, 0 failed.
- `driver_` - 27 passed, 0 failed.
- Total focused acceptance executions above: 47 passed, 0 failed.
- `git diff --check` - passed.

The first test compile exposed old Delegate assertions that still addressed the
pre-trace return shape; they were updated to inspect the wrapped tool result.
The first Delegate/spawn and Gateway event tests correctly exposed their old
two-event assumptions; they now assert the request, assistant projection,
provider outcome, single terminal marker, and preserved domain result. The
first Channel provenance rerun exposed principal-scope filtering in the test;
the test now queries the outbound canonical scope and proves the shared run ID.
All final focused reruns passed.

Per `verification-policy.md`, strict clippy, full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live TUI
acceptance, and release builds were not run. They remain GitHub delivery or
release/deployment gates.

## Scope and rollback

- Scope: shared terminal finalizer, event/usage idempotency support, production
  entry-point adapters, focused contract/integration tests, architecture docs,
  and this receipt.
- No Stage 8 Xin/Heartbeat work was implemented.
- Rollback: revert the local Step 7.3 commit before basing Step 8.1 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.
