# Chat and agent loop characterization

Status: Step 7.3 shared-terminal baseline

Step 7.1 baseline commit: `e0740a468961536185c8c1ffc2a2a6411ad35152`

Chat joins the existing `agent::loop_::run_tool_call_loop_outcome` turn owner.
Redux remains the TUI state and persistence projection, while all production
entry points now close through `agent::terminal::finalize_turn`. See
`docs/shared-turn-terminal-commit.md` for the durable commit contract.

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

The fixture remains test-only and now guards the routed production path.

## Current behavior matrix

| Dimension | Shared invariant proven now | Routed owner | Entry-specific projection | Remaining Step 7.3 boundary |
| --- | --- | --- | --- | --- |
| Provider execution | Same logical two-call script returns `final answer` | `run_tool_call_loop_outcome` owns both real Chat streaming and buffered Agent calls | `ToolLoopRuntimeAdapter` maps live text, reasoning, retry, and compaction events to Redux actions | Terminal commit only; provider execution is already shared |
| Tool calls | Same call ID/name/arguments executes exactly once and produces the same output | Every production path executes through `ToolExecutionService`; priority, bounded read-only concurrency, timeouts, serial stateful barriers, and rollback signals remain in the owner | Chat injects its TUI approval/sandbox service; other entries assemble the same service contract from their registry and policy | Do not introduce a terminal-time execution bypass |
| Recovery | Both retry one identical context-overflow fixture and return `recovered` on call two | The shared owner classifies streaming network errors, owns backoff and overflow recovery, and emits neutral recovery events | Redux maps exact configured compaction patches and UI feedback; buffered callers retain `ChatTrace` failover attribution | Finalizer consumes the resulting terminal state once |
| Usage | Both aggregate the fixture's two calls to 20 tokens | One `ProviderUsageAccumulator` in the shared owner produces the aggregate and attribution trace | Redux receives one `StreamUsageMetered`; other entries consume `ToolLoopTrace.tokens_used` | Settle that aggregate exactly once across entry points |
| Provider-bound history | Before call two, both have roles `system,user,assistant,tool` | The shared owner writes one canonical native assistant/tool payload with call ID and content | UI success/status stays in `ToolFinished`, not provider history | Commit one canonical history projection |
| Durable history commit | Final text is the same and one assistant result is produced | The shared owner mutates the turn-local history and returns the terminal outcome | Redux still emits `RecordAssistantTurn`; ordered Chat commit/finalizer still owns durable persistence | Replace ingress-specific terminal commit with one shared finalizer |
| Cancellation | The same blocked call stops promptly and appends no false success | The shared owner returns `ToolLoopCancelled` | Redux maps it to exactly one `StreamCancelled`; no competing completion/failure action is emitted | Settle lease/attempt/telemetry once on cancellation |
| Finalization | One semantic terminal result per invocation | `ToolLoopOutcome` plus `ToolLoopTrace` is the shared execution result; `agent::terminal::finalize_turn` owns the durable cross-entry close | Chat still uses task-scoped `ProviderTurnReadyForCommit` and `ProviderTurnFinalizerEvent` for ordering/domain state | Delivered: one idempotent `turn.finalized` marker and one settlement ID per turn |

## Routed source ownership snapshot

- Shared provider/tool owner: `run_tool_call_loop_outcome` in
  `src/agent/loop_.rs`.
- Shared live event boundary: `ToolLoopEvent`, `ToolLoopEventSink`, and
  `ToolLoopRuntimeAdapter` in `src/agent/loop_.rs`.
- Shared streaming boundary: `run_streaming_provider_turn`; it aggregates
  partial tool-call chunks, live text/reasoning, usage, and network retries.
- Shared execution boundary: `execute_tools_with_service`; the former
  `execute_tools_with_policy` path is compiled only for historical tests.
- Chat adapter: production `drive_start_turn_stream` in
  `src/chat/dispatcher.rs` calls the shared owner and only maps neutral events,
  preflight state, and the result into existing Redux actions.
- Historical duplicate Chat driver: `drive_start_turn_stream_legacy` is
  `#[cfg(test)]`; production code cannot call it.
- Chat terminal projection: `StreamUsageMetered`, `RecordAssistantTurn`, and
  exactly one `StreamCompleted`, `StreamFailed`, `StreamCancelled`, or
  task-scoped `ProviderTurnReadyForCommit`.
- Chat durable reducer boundary: `reduce_stream_completed` in
  `src/chat/state.rs`; task-scoped ordering/finalization is owned by
  `ProviderTurnTerminalPlan` and `ProviderTurnFinalizerEvent` in
  `src/chat/mod.rs`.
- Buffered Agent/provider boundary: `Provider::chat_traced` inside the same
  owner; Chat selects `Provider::stream_chat_with_history` through its adapter.
- Shared cancellation boundary: `ToolLoopCancelled`.

## Step 7.2 delivered boundary

- Extended the existing Agent owner; no replacement runtime was introduced.
- Preserved real Chat streaming through a neutral event sink and kept buffered
  Agent behavior through the same owner.
- Routed all production tool execution through `ToolExecutionService`, with
  Chat's existing approval/sandbox service injected unchanged.
- Preserved task IDs, cancellation, runtime envelopes, tool call IDs, exact
  configured compaction patches, usage, and provider attribution.
- Standardized provider-bound tool history on the Agent canonical payload;
  Redux tool cards retain UI-only success state.
- Kept Redux preflight, visual projection, command handling, reducer state,
  ordered commit, and task-scoped finalization intact.
- Left cross-entry terminal transaction ownership untouched for Step 7.3.

## Step 7.2 acceptance evidence

The final routed tree passes:

- `step_7_1_same_`: 3 passed; same-fixture success, overflow, cancellation.
- `run_tool_call_loop_`: 9 passed; multimodal, fallback aggregation, native
  persistence, bounded read-only concurrency, serial stateful tools, large
  output persistence.
- `supervised_`: 29 passed; shared-service allow/deny/allowlist behavior plus
  existing supervised policy coverage.
- `driver_`: 27 passed; real streaming/tool protocol, registry handling,
  approval, cancellation, compaction, network retry, usage/final actions, and
  Redux preflight/history projections.
- `cargo fmt --all`, `cargo fmt --all -- --check`,
  `cargo check -p openprx --all-features`, and `git diff --check`: passed.

Per the local verification policy, strict clippy, full suites, security audits,
release builds, and live deployment validation remain GitHub delivery or
release gates. They were not run for this local implementation step.
