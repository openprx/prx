# Chat and agent loop characterization

Status: Step 7.1 characterization baseline

Baseline commit: `71842ea6eaa6848b560dd14b43ee7f8b1c09de0b`

This document fixes the behavior boundary that Step 7.2 must preserve while
Chat joins the existing `agent::loop_` turn owner. It does not authorize a new
turn runtime and does not move production control flow by itself.

## Executable fixture

`src/chat/dispatcher/turn_characterization_tests.rs` defines one logical
provider script with both provider adapters:

- Chat consumes it through `stream_chat_with_history`.
- Agent consumes the same script through `chat_traced`.
- Both receive the same initial history, native tool call ID/name/arguments,
  tool implementation, reported usage, overflow error, and blocking request.

Three tests execute both real loops:

1. `step_7_1_same_success_fixture_characterizes_stream_tool_usage_history_and_terminal`
2. `step_7_1_same_overflow_fixture_characterizes_recovery_signals`
3. `step_7_1_same_blocking_fixture_characterizes_cancellation_terminals`

The fixture is test-only. No production function was made public and no Chat
or Agent routing changed in Step 7.1.

## Current behavior matrix

| Dimension | Shared invariant proven now | Chat Redux driver | `agent::loop_` | Step 7.2 preservation rule |
| --- | --- | --- | --- | --- |
| Provider execution | Same logical two-call script returns `final answer` | Calls `stream_chat_with_history` and forwards real deltas while the stream is open | Calls `chat_traced`; `on_delta` is synthesized only after the buffered final response exists | The shared owner must expose live delta and reasoning events for Chat without creating a second provider loop |
| Tool calls | Same call ID/name/arguments executes exactly once and produces the same tool output | Executes sequentially through `ToolExecutionService`, emitting `ToolStarted`/`ToolFinished` actions | Parses native or text calls, then uses `execute_tools_with_policy`; read-only calls may run in bounded parallel batches and stateful calls remain serial | Keep `ToolExecutionService` as the execution contract and preserve Agent scheduling only behind that contract; do not restore Chat direct execution |
| Recovery | Both retry one identical context-overflow fixture and return `recovered` on call two | Owns stream error classification, network backoff, overflow compaction, and UI retry/compaction actions | Owns overflow compaction around `chat_traced`; provider-level retry/failover is represented in `ChatTrace` | Choose one retry owner per failure class and project retry/compaction events to the reducer; never stack Chat and Agent retry loops |
| Usage | Both aggregate the fixture's two reported calls to 20 total tokens | Emits one final `StreamUsageMetered` aggregate, later consumed by Chat finalization | Returns the aggregate in `ToolLoopTrace.tokens_used`, together with final provider/model and attempts | Carry one aggregate and one attribution trace through the shared result; reducer/finalizer may project it but must not re-sum it |
| Provider-bound history | Before call two, both have roles `system,user,assistant,tool` | Assistant/tool payload is JSON and the tool result includes the Chat-only `success` projection | Native history preserves the provider call ID but its role-tool payload contains only canonical call ID/content | Select one provider-compatible wire projection. UI-only success/status fields belong in events/cards, not a second provider-history dialect |
| Durable history commit | Final text is the same and one assistant result is produced | Sends `RecordAssistantTurn` before one terminal action; reducer and ordered commit/finalizer own persistence | Mutates the caller-owned history before returning; each ingress decides when/how to persist it | The shared loop must return or emit a neutral history delta; Chat's ordered commit remains the only Chat persistence boundary |
| Cancellation | The same blocked provider call stops promptly and appends no false success | Emits exactly one `StreamCancelled` and no `StreamCompleted`/`StreamFailed` | Returns `ToolLoopCancelled` as an error; caller owns terminal projection | Map the shared cancelled outcome to exactly one Chat terminal action and do not append assistant/tool history after cancellation |
| Finalization | One semantic terminal result per invocation | Has `StreamCompleted`/`StreamFailed`/`StreamCancelled`, task-scoped `ProviderTurnReadyForCommit`, and an ordered `ProviderTurnFinalizerEvent` queue | Returns `ToolLoopOutcome` plus `ToolLoopTrace`; terminal commit remains ingress-specific | Step 7.2 adapts the shared result to existing Chat finalization. Step 7.3, not 7.2, creates the cross-entry terminal commit owner |

## Source ownership snapshot

- Chat provider/tool loop: `drive_start_turn_stream` in
  `src/chat/dispatcher.rs`.
- Chat stream and retry boundary: `run_one_stream_pass_with_retry` and
  `run_one_stream_pass` in `src/chat/dispatcher.rs`.
- Chat terminal projection: `StreamUsageMetered`, `RecordAssistantTurn`, and
  `StreamCompleted` or `ProviderTurnReadyForCommit` at the end of
  `drive_start_turn_stream`.
- Chat durable reducer boundary: `reduce_stream_completed` in
  `src/chat/state.rs`; task-scoped ordering/finalization is owned by
  `ProviderTurnTerminalPlan` and `ProviderTurnFinalizerEvent` in
  `src/chat/mod.rs`.
- Existing shared Agent owner: `run_tool_call_loop_outcome` in
  `src/agent/loop_.rs`.
- Agent provider boundary: `Provider::chat_traced` inside that function.
- Agent tool scheduling/execution boundary: `execute_tools_with_policy`.
- Agent cancellation boundary: `ToolLoopCancelled`.

## Step 7.2 implementation constraints

1. Extend the existing Agent owner; do not extract a replacement runtime.
2. Introduce an event/output adapter for visible text deltas, reasoning,
   tool-start/tool-finish/progress, recovery, usage, and one terminal result.
3. Preserve actual streaming for Chat. Falling back to post-response synthetic
   chunks is a user-visible regression even if final text is equal.
4. Thread Chat's `ToolExecutionService`/approval/sandbox strategy through the
   Agent owner, or migrate Agent execution to that service first. There must
   not be one service path for Chat and a bypass path for Agent after routing.
5. Preserve task IDs, operation IDs, cancellation tokens, runtime envelope,
   scope, tool call IDs, and provider attribution without reconstructing them
   from display strings.
6. Emit a neutral history delta and keep the reducer/ordered gate as Chat's
   state and persistence owner.
7. Convert shared cancellation/failure/completion to exactly one existing Chat
   terminal action. Do not finalize or settle usage inside both layers.
8. Do not implement the Step 7.3 shared terminal transaction early.

## Acceptance boundary for Step 7.2

The three Step 7.1 fixtures must continue to pass after routing, with these
intentional changes only:

- Chat and Agent provider execution are owned by the same loop.
- Their provider-bound tool history uses one canonical payload.
- Chat still observes real incremental deltas and exactly one terminal action.
- Tool execution still goes through `ToolExecutionService`.
- Usage remains 20 tokens for the success fixture and is settled once.
- Overflow retries exactly once, cancellation appends no false terminal
  history, and the fixture tool executes exactly once.
