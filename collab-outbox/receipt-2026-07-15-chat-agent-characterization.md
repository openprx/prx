# Receipt: Step 7.1 Chat versus agent loop characterization

Date: 2026-07-15
Branch: `feat/chat-agent-characterization`
Worktree: `/opt/worker/wt/prx-chat-agent-characterization`
Baseline: `71842ea6eaa6848b560dd14b43ee7f8b1c09de0b`
Status: characterization and local verification complete; local commit pending;
not pushed, merged, deployed, routed, or activated

## Delivered characterization

- Added a test-only shared logical provider fixture that exposes both current
  provider adapters: Chat's `stream_chat_with_history` and Agent's
  `chat_traced`. Each path receives the same initial history, native tool call
  identity and arguments, tool implementation, reported usage, overflow
  error, and blocking request.
- Added a same-fixture success test covering provider API selection, real Chat
  deltas, Agent synthesized deltas, one tool execution, tool notifications,
  exact arguments, accumulated usage, provider-bound history, final history,
  and semantic terminal shape.
- Added a same-fixture context-overflow test. Both loops retry exactly once and
  return `recovered`; Chat additionally projects `HistoryCompacted` into its UI
  action stream while Agent keeps recovery inside its call/return boundary.
- Added a same-fixture blocking-provider cancellation test. Chat emits exactly
  one `StreamCancelled` and no competing success/failure terminal. Agent
  returns `ToolLoopCancelled`. Neither path appends false assistant history.
- Added `docs/chat-agent-loop-characterization.md`, which records the current
  behavior matrix for provider streaming, tool calls, recovery, usage, history
  commit, cancellation, and finalization, plus the exact Step 7.2 preservation
  constraints.
- Kept the production boundary unchanged. The only source-module change is a
  `#[cfg(test)]` child-module declaration; no function visibility, provider
  routing, tool routing, reducer behavior, finalizer behavior, or runtime
  configuration changed.

## Proven parity and characterized gaps

- Proven equal on the success fixture: final text `final answer`, one execution
  of the same tool with `{"value":"x"}`, the same tool output, the same
  provider-bound role sequence before call two, and a 20-token final aggregate.
- Proven equal on overflow recovery: two provider attempts and final text
  `recovered`.
- Proven equal on cancellation safety: prompt termination and no false terminal
  history.
- Characterized difference: Chat consumes the real streaming API and emits
  live deltas; Agent consumes the buffered traced API and only synthesizes
  deltas after the response returns.
- Characterized difference: Chat tool-result history includes a UI-oriented
  `success` projection; Agent's native provider history carries canonical call
  ID/content without that field.
- Characterized difference: Chat has typed reducer actions and an ordered
  provider-turn finalizer; Agent returns `ToolLoopOutcome`/`ToolLoopTrace` and
  leaves commit/finalization to each ingress.
- Characterized difference: Chat owns streaming network retry and UI recovery
  signals, while Agent owns overflow recovery around `chat_traced` and receives
  provider failover attempts in `ChatTrace`.

## Step 7.2 boundary fixed by this step

- Extend the existing `agent::loop_`; do not create a new turn runtime.
- Preserve actual incremental Chat streaming through an event adapter.
- Preserve `ToolExecutionService` as the tool execution contract; routing Chat
  through Agent must not reintroduce Agent's direct tool-execution bypass as
  the shared path.
- Choose one retry owner per failure class and project recovery events to Chat.
- Return/emit a neutral history delta; keep Redux and the ordered commit gate as
  Chat state and persistence owners.
- Map completion, failure, and cancellation to exactly one existing Chat
  terminal action, carrying one usage aggregate and provider attribution.
- Leave the shared terminal transaction to Step 7.3.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed on the final tree.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib step_7_1_same_` - 3 passed, 0 failed,
  5652 filtered out. The nonzero tests cover success/tool/usage/history,
  overflow recovery, and cancellation terminals through both real loops.
- `git diff --check` and staged diff check - passed.

Per `verification-policy.md`, strict clippy, the full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live TUI
acceptance, and a release build were not run. They remain GitHub delivery or
deployment gates, not local Step 7.1 gates.

## Scope and rollback

- Scope: test-only shared characterization fixture, three cross-loop tests,
  stable behavior/migration document, module declaration, and this receipt.
- Pre-receipt implementation diff: 724 insertions across 3 files.
- Pre-receipt staged diff SHA-256:
  `71045c8724bdf7d76430a0176203af3c2674c9ef847140c09c7919144e08cbf7`.
- Rollback: revert the local Step 7.1 commit before basing Step 7.2 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production Chat-to-Agent routing was
  performed.
