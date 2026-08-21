# Receipt: visible turns Phase 4a event scheduler

Base: `cbde09ce` (P3c).

Scope: P4a only. This changes who consumes turn events, not visible-turn admission. No P4b/P4c work was done: no concurrency config, no `can_start_visible` relaxation, no ordered commit gate, no history shape change, and no Legacy path concurrency.

## Code changes

- `src/chat/mod.rs:174` adds `PendingReduxTurn`, a Redux-driver-only pending context for the fields the old inner wait kept on the stack: draft/tool channels, join handles, user input, run id, route scope/decision, provider/model names, start time, and pre-turn history length.
- `src/chat/mod.rs:4027` adds `pending_redux_turns: HashMap<TurnTaskId, PendingReduxTurn>`.
- `src/chat/mod.rs:4078` makes the outer run loop drain ready pending Redux completions before accepting another visible input.
- `src/chat/mod.rs:4133` keeps input handling event-driven and preserves the old active-turn semantics by routing input through the shared active-turn input pump while a Redux turn is pending.
- `src/chat/mod.rs:4209` routes `provider_completion_rx` through the shared completion router and immediately finalizes any ready pending Redux turn.
- `src/chat/mod.rs:4250` finalizes pending Redux turns as cancelled on shutdown.
- `src/chat/mod.rs:6704` changes the Redux driver branch after `StartLLMTurn` dispatch to register the turn into `pending_redux_turns` and return to the outer event pump. The old inner wait path remains as fallback for missing task id.
- `src/chat/mod.rs:8679` adds `route_provider_completion_event_and_publish`, preserving the existing route semantics plus status publication.
- `src/chat/mod.rs:9022` moves the old Redux completion/finalization body into `finalize_pending_redux_turn`.
- `src/chat/mod.rs:9252` adds `finalize_ready_pending_redux_turns`; `src/chat/mod.rs:9324` adds shutdown cancellation finalization.
- `src/chat/mod.rs:9854` adds the shared active-turn input pump, preserving `/workers cancel`, `/queue`, `/cost`, local session commands, and enqueue-for-next-turn behavior while a provider turn is active.
- `src/channels/terminal.rs:828` adds non-blocking `TerminalChannel::try_finalize_draft` so the event pump never blocks on UI queue capacity during completion finalization.
- `src/channels/terminal.rs:1041` sends `/quit` and `/exit` into the normal input channel before closing the fallback input loop, so the outer run loop owns the existing quit branch even with the event pump.
- `tests/chat_pty_e2e.rs:1125` adds a P4a PTY test covering two sequential visible turns, clean `/exit`, and saved-session persistence.

## N=1 equivalence

- Admission remains unchanged at `src/chat/mod.rs:8576`: `can_start_visible: active_workers == 0`.
- P4a registers detached Redux turns into `pending_redux_turns`, but the outer loop calls `finalize_ready_pending_redux_turns` before popping the next visible input, and `pop_next_visible_input_task_with_scheduler` still sees the active provider worker. This keeps visible turn execution serial.
- Active-turn input handling preserves the old inner wait behavior: cancel/local commands are handled immediately; normal input, including `/exit`, is enqueued for after the active turn. No second visible turn starts until the active worker reaches terminal state.
- History/session persistence remains on the same finalizer path moved from the old Redux inner wait. P4a does not introduce a per-task history clone, commit gate, or concurrent history writes.
- Legacy `ForegroundAwaited` behavior is not made concurrent.

## Tests

Focused assertions:

- `chat::p3_directional_switch_tests::p4a_active_turn_event_pump_preserves_cancel_local_and_enqueue_order`: active pump preserves `/workers cancel`, local `/queue`, and normal input enqueue order.
- `chat::p3_directional_switch_tests::p4a_outer_completion_route_retains_event_for_pending_redux_finalizer`: completion routing with `current_task_id=None` stores the keyed completion for the pending Redux finalizer and marks worker completion-ready.
- `p4a_event_pump_two_sequential_turns_persist_and_exit`: real PTY two-turn N=1 flow completes both turns, exits, and sees the saved session via `--list-sessions`.

Regression coverage run:

- `cargo test -p openprx p4a_ --all-features -- --nocapture` — passed.
- `cargo test -p openprx --test chat_pty_e2e test_chat_exit_command_clean --all-features -- --nocapture` — passed.
- `cargo test -p openprx --test chat_pty_e2e s5_release_p0_1_anthropic_full_turn_via_real_path --all-features -- --nocapture` — passed.
- `cargo test -p openprx --test chat_pty_e2e s5_release_p0_1_openai_tool_call_turn_via_real_path --all-features -- --nocapture` — passed.
- `cargo test -p openprx --test chat_pty_e2e test_chat_pure_mock_response_works --all-features -- --nocapture` — passed.
- `cargo test -p openprx --test chat_pty_e2e test_chat_pure_tool_call_completes --all-features -- --nocapture` — passed.
- `cargo test -p openprx --test chat_pty_e2e s4_a_p1_pure_exit_after_turn_chat_run_level --all-features -- --nocapture` — passed.
- `cargo test -p openprx --test chat_pty_e2e s5_release_p0_1_gemini_cancel_midstream_via_real_path --all-features -- --nocapture` — passed.
- `cargo test -p openprx phase2_main_transcript_primary_streaming_path_is_unchanged --all-features -- --nocapture` — passed.

## Gates

All required gates passed with zero warnings:

- `cargo fmt --check`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy --all-targets --all-features -- -D warnings`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check --all-features`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features`

`git diff --check` also passed.
