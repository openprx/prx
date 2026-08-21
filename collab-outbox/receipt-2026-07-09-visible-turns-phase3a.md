# Receipt: Visible Turns Phase 3a — task-aware tool buffers

- Date: 2026-07-10
- Base: `f58bb2bb`
- Scope: P3a only. Tool buffers are task-aware; P3b cancel-token map and P3c usage/cost dedup were not implemented.
- Admission guard: preserved. `provider_turn_visible_admission` still gates visible provider turns in `src/chat/mod.rs:8312` and is defined at `src/chat/mod.rs:8422` with `can_start_visible: active_workers == 0` at `src/chat/mod.rs:8439`.

## Changed Files

- `src/chat/action.rs`
  - `ToolStarted` now carries `task_id`, `sequence`, `tool_call_id`: lines 359-365.
  - `ToolFinished` now carries `task_id`, `sequence`, `tool_call_id`: lines 367-375.
  - `ToolApprovalRequested` now carries `task_id`: lines 383-388.
  - `RecordAssistantTurn` is now `{ task_id, content }`: lines 416-419.
- `src/chat/session.rs`
  - `ToolCallSummary` persists audit-only `task_id` and `sequence` with backward-compatible serde defaults: lines 33-40.
- `src/chat/sessions/focus.rs`
  - `PendingToolApprovalView` now retains `task_id`: lines 92-96.
- `src/chat/state.rs`
  - Added `ToolTaskKey`, `ToolInvocationKey`, `TaskToolBuffer`, and `ControlState.tool_buffers`: lines 578-627.
  - Removed tool pending state from `StreamState`; per-task buffers own pending card indices.
  - Added keyed buffer helpers: lines 630-654.
  - `StartLLMTurn`/legacy `TurnStarted` clear only the matching task/Primary tool buffer: lines 1618-1619, 1663-1664.
  - `StreamCompleted`/`StreamFailed`/`StreamCancelled` derive the tool key from the removed draft and clear/finalize only that key: lines 1742-1783, 1787-1817, 1826-1842, 1870-1882.
  - `remove_pending_tool_cards` and `finalize_pending_tool_cards` are keyed: lines 1911-1983.
  - `ToolStarted`/`ToolFinished` write args, pending cards, summaries, `task_id`, and `sequence` into the matching task bucket: lines 1988-2155.
  - `ToolApprovalRequested` stores and effects `task_id`: lines 2188-2206.
  - `CancelRequested` remains Primary/legacy in this batch; it does not implement P3b task cancel tokens: lines 2280-2291.
  - `RecordAssistantTurn` drains only the matching task bucket: lines 2510-2521.
  - Added 5 P3a reducer tests: lines 8881-9079.
- `src/chat/dispatcher.rs`
  - `Effect::RequestApproval` mirrors `task_id` into the TUI pending approval view: lines 1243-1263.
  - `drive_start_turn_stream` receives `provider_turn_task_id`: lines 1806-1820.
  - Provider driver passes task id into `execute_single_tool_call`: lines 2143-2152.
  - Driver sends task-aware `RecordAssistantTurn`: lines 2244-2247.
  - Tool executor sends task-aware `ToolStarted`, `ToolFinished`, and `ToolApprovalRequested`: lines 2380-2417, 2435-2441, 2528-2536.
- `src/chat/mod.rs`
  - Non-provider approval paths use `task_id: None`: lines 5319-5324, 11721-11725.
  - Legacy chat loop `RecordAssistantTurn` passes current `provider_turn_task_id`: lines 7544-7549.
- `src/chat/tui.rs`
  - TUI pending-approval fixtures and pure tool-card test updated for the new fields: lines 9885-9890, 12288-12293, 13143-13148, 13356-13361.

## P3a Tests

Command:

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --all-features p3a_task_aware_tool_buffers -- --nocapture
```

Result: 5 passed, 0 failed.

- `p3a_cancelled_task_finalizes_only_its_tool_buffer`: cancels task A by `StreamCancelled`; asserts A pending count is 0, B pending count remains 1, and B's running card is still visible.
- `p3a_completed_task_clears_only_matching_buffer`: completes task A without `RecordAssistantTurn`; asserts A tool calls are cleared while task B args and pending card remain.
- `p3a_record_assistant_turn_drains_only_requested_task_calls`: records assistant turn for task B; asserts persisted tool call is B only with `task_id=task_b` and `sequence=seq_b`, while A's call stays buffered.
- `p3a_same_tool_name_args_are_isolated_by_task`: starts same `shell` tool name in A and B; finishes B first; asserts B gets `echo b`, A pending/args remain, then A gets `echo a`.
- `p3a_legacy_primary_tool_buffer_path_still_records_tool_calls`: legacy `task_id=None` path still records one tool call and drains `ToolTaskKey::Primary`; `task_id` and `sequence` audit fields remain `None`.

## Required Invariants

- `RecordAssistantTurn` is task-aware. Runtime driver sends `provider_turn_task_id`; reducer drains only `ToolTaskKey::Task(id)` or legacy `Primary`.
- Pending approval has task ownership. `Action::ToolApprovalRequested`, `Effect::RequestApproval`, and `PendingToolApprovalView` all carry `task_id`. Non-provider/manual approval paths explicitly use `None`.
- Cross-task same-name tool calls are isolated by the task bucket. P3a still uses name/tool_call_id inside a task bucket; same-task same-name concurrency remains out of scope.
- P3b not done: `active_cancel` remains single-slot and `CancelRequested` remains Primary/current-turn semantics.
- P3c not done: usage/cost events were not changed.

## Self-Check

```text
cargo fmt --check
```

Passed.

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy -p openprx --all-targets --all-features -- -D warnings
```

Passed: `Finished dev profile ... target(s) in 47.04s`.

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --all-features
```

Passed: `Finished dev profile ... target(s) in 10.68s`.

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features
```

Passed with zero warnings: `Finished dev profile ... target(s) in 8.95s`.

