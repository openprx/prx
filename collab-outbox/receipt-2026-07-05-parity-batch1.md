# Parity Batch 1 Receipt - 2026-07-05

## Scope

Completed Batch 1 from `collab-inbox/task-2026-07-05-parity-gap-fix-campaign.md`: F1, F2, F3, F5.

Push status: no push performed.

## Commits

- F1: `2c3a879b` - `fix(chat): suppress turn elapsed in plain mode`
- F2: `508e93e9` - `fix(chat): align slash menu parity`
- F3: `b8dc36a6` - `fix(chat): make generation interruptible`
- F5: `42e49451` - `fix(chat): show context budget in status`

## Added / Updated Tests

- `plain_mode_suppresses_turn_elapsed_chrome`
- `slash_menu_filters_command_descriptions`
- `slash_menu_closes_when_filter_has_no_matches`
- `slash_menu_only_triggers_at_first_line_start`
- `slash_menu_enter_submits_no_arg_command`
- `slash_menu_sources_match_legacy_and_redux_for_same_keys`
- `resolve_esc_generating_interrupts_before_input_or_focus_cleanup`
- `overlay_open_ctrl_c_and_empty_ctrl_d_keep_global_semantics`
- `esc_during_generation_interrupts_before_clearing_input`
- `status_bar_shows_generation_interrupt_hint`
- `esc_key_during_generation_cancels_active_turn`
- `test_redux_tool_progress_returns_log_redraw_and_visible_generation`
- `s4_a_1_tool_progress_dirty_but_retry_trace_only_is_clean`
- `s4_a_3_dispatcher_skips_unrelated_action`
- `status_bar_renders_context_budget_percent_with_estimate_marker`
- `plain_mode_suppresses_context_budget_warning_chrome`

## Validation

- `cargo fmt --all`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo fmt --all -- --check` - passed
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo clippy --workspace --all-targets -- -D warnings` - passed
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx` - passed: 5208 passed, 0 failed, 6 ignored
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo build --bin prx` - passed

Narrow checks run during the batch:

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx plain_mode_suppresses_turn_elapsed_chrome -- --nocapture`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx slash_menu -- --nocapture`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx generation -- --nocapture`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx overlay_open_ctrl -- --nocapture`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx tool_progress_dirty -- --nocapture`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx resolve_esc_generating -- --nocapture`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx status_bar_renders_context_budget_percent_with_estimate_marker -- --nocapture`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx plain_mode_suppresses_context_budget_warning_chrome -- --nocapture`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-parity-batch1-target cargo test --bin prx s4_a_3_dispatcher_skips_unrelated_action -- --nocapture`

Binary path for verification:

- `/opt/worker/tmp/prx-parity-batch1-target/debug/prx`

## Notes / Deviations

- F1 CDX-1 plain startup banner was evaluated as existing startup convention, not the leaking per-turn chrome, so it was left unchanged.
- F3 status-line generation activity now shows spinner plus interrupt affordance and is driven by the existing tick path. Precise elapsed turn start time is not present in the TUI snapshot, so the new visible hint currently renders elapsed as `0s`.
- F3 changed `ToolProgress` from trace-only to visible UI state. The dispatcher no-snapshot test was updated to use `StreamRetryAttempt`, which remains trace-only and non-dirty.
- F5 removed the live per-turn TUI context budget warning dispatch; the context budget is now persistent status-bar chrome, and the plain-mode no-render guard remains covered by test.
- The F5 commit also contains validation cleanup discovered by clippy/full-test reruns: a const qualifier for the F2 slash menu source helper and one dispatcher test expectation update. No behavior was changed by these cleanup edits.
