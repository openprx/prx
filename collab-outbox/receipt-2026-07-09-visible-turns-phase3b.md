# Receipt: Visible Turns Phase 3b - task-aware cancel tokens

- Date: 2026-07-10
- Base: `d25707ee` (P3a)
- Scope: P3b only. P3c usage/cost plumbing was not changed.
- Admission guard: not removed or relaxed. Existing guard remains in `src/chat/mod.rs:8312` and `src/chat/mod.rs:8422-8439`; existing guard tests remain around `src/chat/mod.rs:15008-15135`.

## Changed Files

### `src/chat/state.rs`

- `src/chat/state.rs:583-599`
  - Added `ToolTaskKey::task_id()` helper for task-aware cleanup/approval matching.
- `src/chat/state.rs:624-683`
  - Added `ControlState.turn_cancels: HashMap<TurnTaskId, CancellationToken>`.
  - Added task-aware cancel helpers:
    - `register_turn_cancel`
    - `take_turn_cancel`
    - `remove_turn_cancel`
    - `has_task_turn_cancels`
    - `drain_turn_cancels`
  - Legacy `active_cancel` is preserved for `ToolTaskKey::Primary`.
- `src/chat/state.rs:824`
  - Initialized `turn_cancels` in `ChatState::new`.
- `src/chat/state.rs:1614-1723`
  - `TurnStarted` still registers into legacy Primary.
  - `StartLLMTurn(Some(task_id), ...)` now registers cancel tokens by `TurnTaskId` instead of overwriting `active_cancel`.
- `src/chat/state.rs:1799-1946`
  - `StreamCompleted`, `StreamFailed`, and `StreamCancelled` now remove only the finished/cancelled task's token and clear global `generating`/legacy slot only when no visible drafts and no task tokens remain.
  - Target task tool buffers/cards are still cleared through the P3a task key.
- `src/chat/state.rs:2337-2410`
  - Reworked `CancelRequested` to select the current primary draft's task.
  - Added `primary_cancel_target`, `clear_target_pending_approval`, and `cancel_task`.
  - `cancel_task` clears only the target draft/token/tool state, emits the target `CancelToken`, and preserves other task buffers and approvals.
  - Global approval and legacy slot are cleared only after all visible drafts and task cancel tokens are gone.
- `src/chat/state.rs:2422-2441`
  - `ShutdownRequested` drains and emits all legacy + per-task cancel tokens before quitting.
- `src/chat/state.rs:2448-2504`
  - `SessionLoaded` clears stale task cancel tokens when replacing session state.
- `src/chat/state.rs:9182-9358`
  - Added 5 P3b reducer tests.

### `collab-outbox/receipt-2026-07-09-visible-turns-phase3b.md`

- This receipt.

## Test Assertions

1. `p3b_two_task_tokens_are_independent` (`src/chat/state.rs:9230-9249`)
   - Starts two task-keyed turns with separate tokens.
   - `CancelRequested` emits exactly one token for the primary task.
   - Cancelling emitted token cancels task A only; task B token remains live and stored.

2. `p3b_cancel_primary_does_not_clear_other_task_tool_buffer` (`src/chat/state.rs:9251-9277`)
   - Starts task A and task B.
   - Adds a running tool card to task B.
   - Cancelling primary task A keeps `generating=true`, preserves B draft, preserves B pending tool card, and preserves B cancel token.

3. `p3b_global_state_clears_only_after_all_visible_tasks_are_cancelled` (`src/chat/state.rs:9279-9312`)
   - Starts task A and task B.
   - Adds pending approval owned by task B.
   - Cancelling task A does not globally clear B approval or B token.
   - Cancelling task B then clears `generating`, visible drafts, task tokens, legacy slot, and pending approval.

4. `p3b_shutdown_cancels_all_task_tokens` (`src/chat/state.rs:9314-9335`)
   - Starts two task-keyed turns.
   - `ShutdownRequested` emits both task cancel tokens.
   - After emitted tokens are cancelled, both original task tokens are cancelled; task token map, generating flag, and visible drafts are cleared.

5. `p3b_single_legacy_turn_ctrl_c_still_uses_primary_active_cancel` (`src/chat/state.rs:9337-9358`)
   - Starts legacy `TurnStarted` without task id.
   - `CancelRequested` still emits the legacy `active_cancel` token.
   - After emitted token cancellation, legacy token is cancelled, `active_cancel` is empty, task token map is empty, generation is false, and primary draft is gone.

## Ctrl+C Semantics

- `CancelRequested` remains no-arg Ctrl+C.
- It cancels the current primary draft by reading `StreamState::primary_draft()`.
- If that draft belongs to a task, only that task's draft/token/tool buffer/pending cards are cleared.
- If it is the legacy primary path, it still uses `active_cancel` and preserves the old single-turn behavior.
- Other visible tasks remain live after Ctrl+C; `generating=false`, global approval clearing, and legacy slot clearing happen only when both `visible_drafts` and `turn_cancels` are empty.
- `TurnScheduler::request_cancel` remains the scheduler-layer cancel API; this batch did not change scheduler/runtime worker registry hard-kill behavior.

## Self Check

All commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp` for Cargo commands.

```text
cargo fmt --check
PASS
```

```text
cargo clippy -p openprx --all-targets --all-features -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 52.70s
PASS
```

```text
cargo check -p openprx --all-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 11.61s
PASS
```

```text
cargo check -p openprx --no-default-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 11.55s
PASS
```

```text
cargo test -p openprx --all-features p3b_task_aware_cancel_tokens -- --nocapture
5 passed; 0 failed; 5448 filtered out
PASS
```

```text
git diff --check
PASS
```
