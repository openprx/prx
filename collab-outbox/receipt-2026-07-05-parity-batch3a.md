# Receipt: parity batch3a F6/F7/F8

Date: 2026-07-05
Agent: Codex
Scope: Batch 3a from `collab-inbox/task-2026-07-05-parity-batch3-p1.md`
Push: not pushed

## Commit

- `602b0c98 fix(chat): close parity batch3a gaps`

## Binary Path

- Built CLI binary: `/opt/worker/code/prx/target/debug/prx`
- Test binary exercised by `cargo test --bin prx`: `/opt/worker/code/prx/target/debug/deps/prx-ffc3817af57d929f`

## Changes

### F6

- Folded tool cards now show up to 3 result preview lines below the metrics row, with an ellipsis tail such as `... +N lines` when more lines remain.
- Ctrl+O transcript view now retains full transcript content instead of clipping to `TRANSCRIPT_MAX_LINES` or clamping away the scroll range.
- Ctrl+O transcript renders full tool args/results and folded reasoning content, giving verbose/full scroll access. The main inline expanded tool card remains bounded; verbose mode is the full-output path.

### F7

- `TerminalGuard` probes `supports_keyboard_enhancement()` and pushes crossterm keyboard enhancement flags only when supported.
- Terminal teardown and panic restore now pop keyboard enhancement flags symmetrically when active.
- Plain Enter on a current line ending in `\` deletes the backslash and inserts a newline without submitting.
- Footer help now advertises `Shift+Enter newline` and `\+Enter continue`.

### F8

- `/compact`, legacy preflight, legacy overflow, Redux preflight, and Redux overflow now prefer provider-backed configurable summary compaction patches.
- Summary compaction patch application dispatches `HistoryCompactionPatchApplied`; failed/timeout summary compaction falls back to deterministic trim/legacy compaction.
- Automatic preflight/overflow compaction now surfaces `SystemMessageAdded` using `format_compact_feedback`.
- `/compact` and automatic legacy compaction refresh `ContextWindowUpdated` so ctx% can drop immediately.
- Added timeout fallback coverage for the LLM summary compaction path.

## Evidence

- `cargo fmt --check` passed.
- `cargo clippy --all-targets -- -D warnings` passed.
- `cargo test --bin prx -- --nocapture` passed: 5223 passed, 0 failed, 6 ignored.
- `cargo build --bin prx` passed and produced `/opt/worker/code/prx/target/debug/prx`.

## Key Anchors

- F6 implementation/tests: `src/chat/tui.rs`
  - folded preview constants and renderer: `TOOL_FOLDED_RESULT_PREVIEW_LINES`, `push_folded_tool_result_preview`
  - verbose transcript: `transcript_lines_from_conversation`, `build_transcript_view`
  - tests: `render_folded_tool_card_done_shows_hook_summary`, `transcript_view_expands_folded_tool_and_reasoning_content`
- F7 implementation/tests: `src/chat/mod.rs`, `src/chat/tui.rs`
  - terminal probe/push/pop: `supports_keyboard_enhancement`, `push_keyboard_enhancement_flags`, `pop_keyboard_enhancement_flags`
  - input continuation: `consume_backslash_line_continuation`
  - tests: `fullscreen_terminal_lifecycle_pushes_and_pops_keyboard_enhancement_when_supported`, `p2_10_backslash_enter_continues_without_submitting`
- F8 implementation/tests: `src/chat/mod.rs`, `src/chat/dispatcher.rs`
  - summary timeout wrapper: `build_chat_compaction_patch_with_timeout`
  - legacy patch sync: `apply_chat_compaction_patch_and_sync`
  - Redux feedback: `send_redux_compaction_feedback`
  - timeout test: `configurable_summary_compaction_timeout_returns_none_for_fallback`

## Deviations / Notes

- No push performed.
- Main inline expanded tool output remains bounded by existing 40-line/240-char limits; Ctrl+O verbose transcript is now the full-output path required for verbose/full scroll access.
