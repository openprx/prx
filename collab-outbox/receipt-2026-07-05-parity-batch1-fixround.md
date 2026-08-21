# Receipt: parity batch1 fix-round

Date: 2026-07-05
Scope: Batch1 fix-round F3/F5 only. F1/F2 accepted by audit. F4 was already closed before this fix-round.
Push: not pushed.

## Commits

- F3: `09d9eb10 fix(chat): align generation cancel priority`
- F5: `1ba6e3a4 fix(chat): use current context budget in status`

## F3

- Moved Redux Esc+generating handling below saved picker, switcher, strip selection, approval, and slash-menu handling.
- Added `Effect::ResolveApproval` so reducer-owned approval key handling can deny/approve through the dispatcher approval router without cancelling the active turn.
- `CancelRequested` now clears pending foreground approval and resolves it as deny before clearing the active draft.
- Removed the unreachable approval Ctrl+C deny branch from legacy TUI dispatch; Ctrl+C keeps global interrupt semantics.
- Removed the hardcoded `generating 0s` display. Status now shows `generating (esc to interrupt)`.
- Kept generation activity visible while a tool card is `Running`, even if no streaming draft is active.

Tests:

- `cargo test --bin prx redux_esc_ -- --nocapture`
- `cargo test --bin prx approval_child_ctrl_c_keeps_global_interrupt_semantics -- --nocapture`
- `cargo test --bin prx status_bar_shows_generation -- --nocapture`
- `cargo test --bin prx cancel_requested_clears_pending_approval -- --nocapture`
- `cargo test --bin prx resolve_esc_approval_focus_takes_priority_over_generating -- --nocapture`

Deviation:

- Spinner is no longer fake elapsed and no longer disappears during tool-running stages, but no new wall-clock 1s tick field was added in this fix-round. Frame selection still uses stream version when available and a stable frame for tool-only running state.

## F5

- `ContextWindowUpdated` now carries both `used_context_tokens` and `max_context_tokens`.
- TUI state, Redux UI state, snapshots, and `BottomChromeView` now expose current context usage separately from cumulative session token usage.
- Status bar context chrome now renders from `plan_context_budget().used_tokens / max_context_tokens`, caps at 100%, and labels the metric as `ctx:N% used`.
- Removed the dead test-only `context_budget_warning_for_tui` behavior and replaced it with production-used `context_budget_status_for_tui`.
- Added plain-mode no-render coverage for context-budget chrome.

Tests:

- `cargo test --bin prx context_budget -- --nocapture`
- `cargo test --bin prx context_window_updated_writes_snapshot_and_dedups -- --nocapture`
- `cargo test --bin prx status_bar_renders_context_budget -- --nocapture`
- `cargo test --bin prx bottom_chrome -- --nocapture`

## Self-Check

- `cargo fmt --all -- --check`: pass
- `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo test --bin prx`: default parallel run reached `5214 passed; 4 failed; 6 ignored`; the four failures were outside touched paths:
  - `providers::tests::summarize_provider_availability_marks_degraded_when_only_primary_has_credentials`
  - `providers::tests::summarize_provider_availability_marks_openai_codex_available_with_auth_profile`
  - `providers::tests::summarize_provider_availability_marks_openai_codex_unavailable_without_auth_profile`
  - `security::landlock::tests::landlock_extra_exec_dir_is_executable_only_when_granted`
- Each of those four failed tests passed when rerun individually.
- `cargo test --bin prx -- --test-threads=1`: pass, `5218 passed; 0 failed; 6 ignored`
- `cargo build --bin prx`: pass
- Target dir: `/opt/worker/tmp/prx-parity-fixround-target`
- Binary: `/opt/worker/tmp/prx-parity-fixround-target/debug/prx`

