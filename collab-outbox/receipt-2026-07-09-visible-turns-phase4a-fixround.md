# P4a Fixround Receipt

Baseline: `68392f11` (12 preexisting failures fixed). P4a fixround keeps visible admission at N=1; `provider_turn_visible_admission` still gates on `active_workers == 0` in `src/chat/mod.rs:8670`.

## Code Changes

- A1 fixed: active-turn local command output now surfaces through the reducer/redraw path instead of raw stdout.
  - Added `surface_active_turn_message` wrapper at `src/chat/mod.rs:1602`.
  - Outer event pump active-turn input arm now calls it with `redraw_tx_for_main.as_ref()` at `src/chat/mod.rs:4280`.
  - Test: `p4a_active_turn_message_uses_reducer_redraw_path` at `src/chat/mod.rs:2131`.

- E1 fixed: `ResumeSavedSession` is reachable during a pending Redux turn through TUI `KeyDispatch::ResumeSavedSession -> control_tx.blocking_send(ChatControlEvent::ResumeSavedSession { id })` (`src/chat/mod.rs:11761`, `src/chat/mod.rs:11767`). P4a now defers these resume requests until no pending Redux turn remains.
  - Pending queue: `deferred_resume_saved_session_ids` at `src/chat/mod.rs:4107`.
  - Deferral helper: `defer_resume_saved_session_if_provider_turn_pending` at `src/chat/mod.rs:1611`.
  - Drain point after pending turn finalization: `src/chat/mod.rs:4229`.
  - Control arm guard/comment: `src/chat/mod.rs:4416`.
  - Test: `p4a_resume_saved_session_is_deferred_while_provider_turn_pending` at `src/chat/mod.rs:2156`.

- A2 fixed: nonblocking draft close was unsafe because `TerminalChannel` UI events are bounded; under redraw/UI backpressure `try_send` could silently drop final text. Redux P4a completed turn finalization now awaits `terminal.finalize_draft(...)`.
  - Awaited finalize path: `src/chat/mod.rs:9253`.
  - Removed the nonblocking `try_finalize_draft` helper; `rg try_finalize_draft` returns no matches.
  - Main transcript regression checked with `s5_release_p0_1_openai_tool_call_turn_via_real_path`.

- Event pump/finalizer structure:
  - `PendingReduxTurn` context stores detached Redux turn finalization state at `src/chat/mod.rs:174`.
  - Pending turn map is keyed by `TurnTaskId` at `src/chat/mod.rs:4102`.
  - Outer completion arm retains keyed completion and publishes worker status at `src/chat/mod.rs:4336`.
  - Ready finalizer drain: `finalize_ready_pending_redux_turns` at `src/chat/mod.rs:9343`.
  - Shutdown cancellation finalizer: `finalize_all_pending_redux_turns_as_cancelled` at `src/chat/mod.rs:9415`.
  - Active-turn input pump preserving old in-turn semantics: `process_active_turn_input_batch` at `src/chat/mod.rs:9945`.

- `/exit` delivery under PTY was kept equivalent: `terminal_input_loop` now sends `/quit`/`/exit` to the main loop before breaking, at `src/channels/terminal.rs:1012` and `src/channels/terminal.rs:1039`.

## Equivalence Coverage

- A1 reducer redraw: `p4a_active_turn_message_uses_reducer_redraw_path`.
- In-turn input/cancel/local command ordering: `p4a_active_turn_event_pump_preserves_cancel_local_and_enqueue_order`.
- Completion retained by outer event pump: `p4a_outer_completion_route_retains_event_for_pending_redux_finalizer`.
- Completion event -> terminal plan -> finalizer gate -> committed worker: `p4a_completion_event_and_finalizer_chain_commits_ready_turn`.
- Control interleaving: `p4a_resume_saved_session_is_deferred_while_provider_turn_pending`.
- Closed input channel while pending: `p4a_input_close_keeps_event_pump_alive_until_pending_turn_finishes`.
- Shutdown cancellation rollback: `p4a_shutdown_cancelled_pending_turn_rolls_back_to_user_boundary`.
- Delayed `/exit` while active turn is running: `p4a_active_turn_quit_is_queued_for_delayed_exit`.
- PTY sequential transcript: `p4a_event_pump_two_sequential_turns_complete_and_exit`.

Existing tests still cover the finalizer three terminal branches:
- `provider_terminal_plan_completed_non_empty_builds_gate_fields`
- `provider_terminal_plan_empty_failed_and_cancelled_keep_boundaries`
- `provider_turn_finalizer_event_commits_completed_plan`
- `provider_turn_finalizer_event_closes_failed_and_cancelled_plans`

## Before/After

- Before fixround: user-provided clean baseline `68392f11`, Claude full bin result `5450 passed, 0 failed`.
- After fixround: full bin result `5458 passed, 0 failed, 7 ignored`.
- Net new passing bin tests: 8 P4a tests.

## Verification

- `cargo fmt --check` -> passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy --all-targets --all-features -- -D warnings` -> passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check --all-features` -> passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features` -> passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features` -> passed, `5458 passed; 0 failed; 7 ignored`.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --test chat_pty_e2e --all-features p4a_event_pump_two_sequential_turns_complete_and_exit -- --nocapture` -> passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --test chat_pty_e2e --all-features s5_release_p0_1_openai_tool_call_turn_via_real_path -- --nocapture` -> passed.

Note: I initially tried to assert default PTY saved-session listing in the P4a PTY test, but that harness path is already documented as unreliable for saved-session observation in the adjacent ignored resume-persistence test. The P4a PTY test now covers the real sequential transcript and clean exit; persisted SaveSession behavior is covered by existing reducer/executor tests plus the P4a finalizer chain test above.

## #11 Deferred Resume Ordering Fix

Audit finding: the first E1 fix drained deferred `ResumeSavedSession` immediately after pending Redux finalization and before visible backlog dequeue. That was a narrow N=1 ordering difference from the baseline: if the user queued a normal input during the active turn and also selected resume, P4a could switch to session B before the queued input ran. Baseline behavior runs queued input against session A first, then processes the resume control.

Fix:
- Added `should_drain_deferred_resume_after_visible_inputs` at `src/chat/mod.rs:1629`.
- Moved deferred resume drain after `pop_next_visible_input_task_with_scheduler` in the outer event pump, at `src/chat/mod.rs:4262` -> `src/chat/mod.rs:4268`.
- Drain now requires `pending_turns == 0`, `input_backlog.is_empty()`, and visible admission open. This keeps resume behind all already queued visible input.
- Admission remains unchanged: `can_start_visible: active_workers == 0` at `src/chat/mod.rs:8682`.

New test:
- `p4a_deferred_resume_waits_for_queued_visible_input_from_current_session` at `src/chat/mod.rs:15689`.
- It asserts deferred resume does not drain while a visible input is queued, then dequeues the input first and checks the scheduler task still carries session A's `history_base_len`, locking the baseline "queued input belongs to the pre-resume session" ordering.

Verification after #11:
- `cargo fmt --check` -> passed.
- `git diff --check` -> passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy --all-targets --all-features -- -D warnings` -> passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features` -> passed, `5459 passed; 0 failed; 7 ignored`.

Low-risk `/exit` note: no further code change was made for idle `/exit` echo. The P4a behavior remains covered by the delayed-exit unit test and PTY clean-exit test; the audit's idle echo note is TUI teardown-only and does not leak to provider execution.
