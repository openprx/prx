# Receipt: parity F3 fixround2

Date: 2026-07-05
Scope: F3 second fix-round only. F5 `1ba6e3a4` was already accepted.
Push: not pushed.

## Commit

- `2fd36122 fix(chat): clear approval mirror on interrupt`

## Changes

- Added `TuiState::clear_pending_tool_approval()` to clear `pending_tool_approval` and restore `focus` to `Main`.
- `dispatch_global_key` now clears mirror approval state before returning `KeyDispatch::InterruptTurn` for Ctrl+C.
- The TUI loop `InterruptTurn` branch also clears mirror approval state before dispatching `Action::CancelRequested`.
- Removed the dead second Ctrl+C branch in `tui.rs` that was fully shadowed by the top-level Ctrl+C guard.
- Updated `approval_child_ctrl_c_keeps_global_interrupt_semantics` to assert the corrected mirror cleanup contract.
- Added `approval_child_ctrl_c_allows_next_message_submission`, which fails if Ctrl+C leaves the mirror stuck in approval mode.

## Tests

- `cargo test --bin prx approval_child_ctrl_c -- --nocapture`: pass
- `cargo test --bin prx dispatch_ctrl_c_signals_interrupt_turn -- --nocapture`: pass
- `cargo test --bin prx cancel_requested_clears_pending_approval -- --nocapture`: pass

## Self-Check

- `cargo fmt --all -- --check`: pass
- `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo test --bin prx -- --test-threads=1`: pass, `5219 passed; 0 failed; 6 ignored`
- `cargo build --bin prx`: pass
- Target dir: `/opt/worker/tmp/prx-parity-fixround-target`
- Binary: `/opt/worker/tmp/prx-parity-fixround-target/debug/prx`

