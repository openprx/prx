# Receipt: visible turns P4b-1 ordered commit gate

Date: 2026-07-10
Base: e8ee9e75 (P4a independently verified baseline)
Scope: P4b-1 only. Admission remains N=1; no P4b-2 concurrency unlock.

## Summary

Implemented PerTurnContext plus an ordered persistence gate for task-scoped Redux visible provider turns.

Chosen implementation: do not add a public reducer `CommitProviderTurn` action. Instead, task-scoped dispatcher completion now emits an internal `ProviderTurnReadyForCommit` action that records the `TurnCompletionSignal` outcome but is a reducer no-op. `chat::run` finalizer records the terminal outcome into `HistoryCommitCoordinator`, waits for `drain_ready()`, and only then dispatches the existing reducer persistence actions:

- `RecordAssistantTurn` for non-empty completed turns.
- `StreamCompleted` for completed turns, including empty turns, so reducer `SaveSession` is emitted only after the commit gate.
- Failed/cancelled turns remain ordered skips and do not dispatch `RecordAssistantTurn` or `StreamCompleted`.

## Precise Changes

- `src/chat/mod.rs:174` introduced `PerTurnContext`, keyed by `TurnTaskId` in the `chat::run` stack-local map at `src/chat/mod.rs:4112`.
- `src/chat/mod.rs:175-188` PerTurnContext fields after fixround:
  - identity/history: `user_input`, `history_len_before_user_turn`.
  - runtime IDs/routing: `turn_run_id`, `route_scope`, `route_decision`, `provider_started_at`, `provider_name`, `model_name`.
  - control/finalizer: `draft_id`, `delta_tx`, `tool_event_tx`, `draft_updater`, `tool_event_forwarder`.
- `src/chat/mod.rs:6757-6758` passes per-turn spawn/send context directly into `StartLLMTurn`; the fixround removed the temporary clones that only existed to also stash those contexts in PerTurnContext.
- `src/chat/mod.rs:6808-6827` registers Redux provider turns into `per_turn_contexts`.
- `src/chat/mod.rs:8959-9009` carries `reasoning` through `ProviderTurnTerminalPlan::Completed` so ordered `StreamCompleted` preserves reasoning cards.
- `src/chat/mod.rs:9128-9153` added `dispatch_ordered_provider_turn_commit`, which emits `RecordAssistantTurn` then `StreamCompleted`.
- `src/chat/mod.rs:9157-9365` changed the pending Redux finalizer into `finalize_per_turn_context`; it builds the terminal payload, drains `HistoryCommitCoordinator`, awaits stream forwarders, and dispatches reducer persistence only if the completed decision is finalized/ready.
- `src/chat/mod.rs:9412-9480` changed ready completion drain to remove entries from `per_turn_contexts` and finalize them through the gate.
- `src/chat/mod.rs:9484-9525` changed shutdown cancellation drain to operate on `per_turn_contexts`.
- `src/chat/action.rs:360-367` added `ProviderTurnReadyForCommit`.
- `src/chat/dispatcher.rs:280-284` extended `TurnOutcomeKind::Completed` with `reasoning`.
- `src/chat/dispatcher.rs:493-562` treats `ProviderTurnReadyForCommit` as a terminal completion signal with keyed draft routing.
- `src/chat/dispatcher.rs:2232-2285` task-scoped driver completions now emit `ProviderTurnReadyForCommit`; non-task-scoped legacy/test paths keep `RecordAssistantTurn -> StreamCompleted`.
- `src/chat/state.rs:1078-1083` reducer handles `ProviderTurnReadyForCommit` as no-op.
- `src/chat/state.rs:3318-3323` marks `ProviderTurnReadyForCommit` non-dirty.
- Admission verified unchanged at `src/chat/mod.rs:8679-8681`: `can_start_visible: active_workers == 0`.

## Fixround

- Finding 1 fixed: removed PerTurnContext fields that were only consumed by the debug log:
  - removed `enriched_input`, `system_prompt`, `history_base_len`, `history_snapshot`, `history_len_before_assistant`, `cancel`, `turn_spawn_ctx`, `turn_message_send_ctx`.
  - removed the per-turn `history.clone()` deep copy and the `enriched.clone()` / `system_prompt.clone()` copies from context registration.
  - changed the finalizer debug log to only record retained fields: `task_id`, `draft_id`, `turn_run_id`, `provider_name`, `model_name`, `history_len_before_user_turn`.
  - restored `StartLLMTurn` spawn/send context passing to move the context into the action instead of cloning it for PerTurnContext.
- Finding 2 fixed conservatively: removed the PerTurnContext `final_payload` field and the clone/take round trip. The finalizer still clones `terminal_plan` once when enqueuing the coordinator event, then matches the local `terminal_plan`.
- Finding 3 noted for P4b-2: if `finalized == false` under future N>1, completion must remain pending and dispatch ordered reducer terminal actions after the earlier sequence unlocks. P4b-1 keeps N=1, so this is not changed here.

## N=1 Equivalence

- Admission is unchanged, so only one visible provider worker can run.
- In N=1, `HistoryCommitCoordinator.drain_ready()` has no earlier pending task to wait for; completed outcomes release immediately.
- The reducer-visible successful terminal sequence remains `RecordAssistantTurn -> StreamCompleted`; only its source moved from the dispatcher driver to the ordered finalizer.
- Failed/cancelled semantics remain non-persistent: reducer still sees `StreamFailed`/`StreamCancelled` from the driver for UI cleanup, and the ordered gate skips assistant/session persistence.
- Empty assistant responses preserve the existing system-message path, and `StreamCompleted` is now dispatched after the gate to release reducer draft state and `SaveSession` in order.

## Tests Added/Updated

- `src/chat/dispatcher.rs:5169` `task_scoped_start_turn_emits_ready_for_ordered_commit`
  - Drives a real task-scoped `StartTurn`.
  - Asserts the driver emits streaming chunks, then `ProviderTurnReadyForCommit`, not early `RecordAssistantTurn`/`StreamCompleted`.
- `src/chat/mod.rs:16156` `p4b1_ready_signal_is_noop_until_ordered_commit_dispatches_save`
  - Asserts `ProviderTurnReadyForCommit` emits no effects and leaves the reducer draft open.
  - Asserts ordered commit emits `RecordAssistantTurn` first with no `SaveSession`.
  - Asserts ordered `StreamCompleted` emits `SaveSession`.
- `src/chat/mod.rs:16236` `p4b1_failed_and_cancelled_skip_do_not_emit_persistence_actions`
  - Runs failed/cancelled outcomes through the finalizer/coordinator path.
  - Asserts finalized skips do not emit `RecordAssistantTurn` or `StreamCompleted`.
- Existing terminal-plan tests were updated to carry/verify reasoning preservation.

## Verification

Before:

- Baseline e8ee9e75 was reported green by Claude: 5459 passed, 0 failed.

After:

- `cargo fmt --check`
  - passed
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy -p openprx --all-targets --all-features -- -D warnings`
  - passed
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check --all-features`
  - passed
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features`
  - passed
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features p4b1_ -- --nocapture`
  - 2 passed, 0 failed
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features task_scoped_start_turn_emits_ready_for_ordered_commit -- --nocapture`
  - 1 passed, 0 failed
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features`
  - 5462 passed, 0 failed, 7 ignored, 0 measured, 0 filtered out

Main transcript primary: no regressions observed in the full `prx` bin test suite; no separate production deploy/tmux run was performed in this P4b-1 local gate.
