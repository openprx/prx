# Receipt: Step 6.2 Chat Redux ToolExecutionService migration

Date: 2026-07-15
Branch: `feat/chat-tool-execution`
Worktree: `/opt/worker/wt/prx-chat-tool-execution`
Baseline: `dc18c77198bdc7edf8f74f410dc42b3a10cf3079`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Chat Redux no longer owns a private `needs_approval` decision branch or calls
  `execute_named_with_cancellation` directly. Its tool-call driver submits one
  typed command/context to `ToolExecutionService` and projects the typed result
  into provider history and reducer actions.
- `EffectDeps` now receives the authoritative runtime `SecurityPolicy`, not a
  separate Chat approval manager. Scope ACL and read-only/supervised/full
  autonomy decisions therefore run through the same policy adapter as the
  common service.
- Wrapped the existing `ApprovalRouter` and reducer actions as
  `ChatTuiApprovalStrategy`. Existing Y/N, arrow/Enter, Esc, concurrent-request
  fail-closed, dropped-channel fail-closed, and active-turn cancellation
  behavior is preserved.
- Approved calls mint the same command/resource-bound runtime grant helper used
  by the Agent path. Caller-supplied scope/grant markers are still stripped by
  the service; only the trusted TUI strategy can inject approval material.
- `ChatToolSandboxStrategy` keeps `ToolStarted` ordered after policy/approval
  and before raw invocation. A closed reducer action channel aborts before the
  tool future starts.
- Added a shared boxed-registry adapter so Chat can reuse stateful native tools
  and MCP aliases without consuming, cloning, or rebuilding tool instances.
  MCP public alias identity and named invocation are covered by the service
  regression test.
- Chat execution context now carries the active workspace, stable session key,
  turn run/task lineage, owner/topic/source-event fields, terminal sender and
  channel, and the existing `terminal:user` chat identity. The turn spawn
  context is enriched at its existing construction boundary so tool grants and
  child execution see the same owner lineage.
- Plan-mode interception, provider-history payloads, error visibility,
  unrecoverable retry suppression, output compaction, tool finish actions, and
  stream cancellation remain Chat projections around the canonical service.
- Legacy Agent execution is intentionally unchanged in this step; its existing
  grant helper was only made crate-visible for reuse. Registry alignment is the
  explicit Step 6.3 boundary.

## Red evidence and local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- The first focused test command matched zero tests because the approval tests
  live under `chat::dispatcher::real_mode_tests`; exact nonzero filters were
  listed and rerun. The zero-match command is not counted below.
- The first test compilation exposed test helper functions scoped inside a
  sibling module. They were moved to the dispatcher test support boundary; all
  exact tests then compiled and passed.
- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final implementation
  tree with no reported warnings.
- `cargo test -p openprx --lib tools::execution::tests` - 9 passed, 0 failed,
  including the shared boxed-registry MCP alias path.
- Redux TUI approval allow/reject filter - 2 passed, 0 failed. The allow test
  additionally proves trusted approval boolean and command-bound grant
  injection before execution.
- Direct approval adapter fail-closed/normal filter - 2 passed, 0 failed.
- Read-only policy denial - 1 passed, proving no prompt, no `ToolStarted`, and
  no execution.
- Cancellation while TUI approval is pending - 1 passed, proving typed
  cancellation, router cleanup, no `ToolStarted`, and no execution.
- Repeated unrecoverable failure suppression - 1 passed.
- Chat Redux result/plan/history regression module - 13 passed, covering plan
  interception, recoverable/permanent errors, result compaction, and reducer
  snapshot behavior.
- ApprovalRouter concurrent fail-closed regression - 1 passed.
- Redux approval selection, Esc denial, and cancellation cleanup state tests -
  1 passed each.
- `git diff --check` and staged diff check - passed.
- Source inspection found no `execute_named_with_cancellation` call in
  `src/chat/dispatcher.rs`; remaining `needs_approval` text is confined to old
  ApprovalManager unit assertions and a historical test comment.

Per `verification-policy.md`, strict clippy, the full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live TUI
acceptance, and a release build were not run. They remain GitHub delivery or
deployment gates, not local Step 6.2 gates.

## Scope and rollback

- Scope: Chat Redux execution/approval adapters and dependency wiring, shared
  registry compatibility, trusted turn context/grant reuse, focused tests, and
  this receipt.
- Pre-receipt implementation diff: 675 insertions and 283 deletions across 4
  files.
- Pre-receipt implementation diff SHA-256:
  `a6b455d124b3d748934c3290dc5dce79b54fd5c7087d8943f879a31405dc5c76`.
- Rollback: revert the local Step 6.2 commit before basing Step 6.3 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, or runtime activation was performed.
