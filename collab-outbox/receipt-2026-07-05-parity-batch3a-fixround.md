# Receipt: parity batch3a fix-round

Date: 2026-07-05
Branch: main
Push: not pushed

## Commits

- F6: `aad2e96d fix(chat): tighten batch3a f6 tool previews`
- F7: `67cf5362 fix(chat): restore keyboard flags across handoffs`
- F8: `1b19b44d fix(chat): align compaction guards with persisted history`
- F10 follow-up from fix-round doc: `a91ac169 fix(chat): align overlay key priority`

## Scope

- F6: folded tool result preview now uses width-aware truncation that preserves whitespace; expanded truncation hint names `Ctrl+O`; added folded-card/error-preview coverage.
- F7: external editor and PTY handoff now pop/re-push keyboard enhancement flags when active; added push-failure rollback and handoff sequence coverage.
- F8: legacy `/compact`, preflight, and overflow compaction now use enriched history for budget decisions and persisted history for guard/audit source; overflow success restores persisted-source legacy audit; `/compact` emits `Compacting conversation...` before awaiting summary; added a reducer guard-hit regression with enriched tool/internal messages.
- F10: reducer key priority now matches mirror overlay routing for slash menu and saved-session picker before ALT strip handling; added overlay-open parity regressions.

## Targeted tests run before commits

- F6: `cargo fmt && cargo test --bin prx folded_tool_card -- --nocapture && cargo test --bin prx expanded_tool_output_truncates_long_body -- --nocapture`
- F7: `cargo fmt && cargo test --bin prx keyboard_enhancement -- --nocapture && cargo test --bin prx external_editor_handoff -- --nocapture && cargo test --bin prx fullscreen_attach_leave_disables_chat_mouse_before_alt_leave -- --nocapture && cargo test --bin prx fullscreen_handoff_restore_resets_child_then_reenters_chat_alt_screen_and_mouse -- --nocapture && cargo test --bin prx handoff_keyboard_sequences_are_conditional -- --nocapture`
- F8: `cargo fmt && cargo test --bin prx legacy_compaction_with_tool_message_uses_persisted_guard_for_reducer -- --nocapture && cargo test --bin prx configurable_summary_compaction_timeout_returns_none_for_fallback -- --nocapture && cargo test --bin prx redux_compaction_patch -- --nocapture`
- F10: `cargo fmt && cargo test --bin prx slash_menu_captures_alt_arrows_before_strip_selection -- --nocapture && cargo test --bin prx saved_session_picker_captures_alt_enter_before_stale_strip_selection -- --nocapture && cargo test --bin prx saved_session_picker_open_move_close_lifecycle -- --nocapture`

## Receipt self-check

Self-check was run after all code commits, before writing this receipt, with tracked worktree clean.

- Validation HEAD: `a91ac169`
- `git status --short --untracked-files=no`: empty
- `git diff --check`: clean
- `cargo fmt --check`: passed
- `cargo clippy --all-targets -- -D warnings`: passed
- `cargo test --bin prx -- --nocapture`: passed (`5241 passed; 0 failed; 7 ignored`)
- `cargo build --bin prx`: passed
- Binary: `/opt/worker/code/prx/target/debug/prx`

## Notes

- Untracked `collab-inbox/` and older untracked receipt files were intentionally left untouched.
- No push performed.
