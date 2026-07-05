# Receipt: parity Batch 3c follow-up

Date: 2026-07-05
Repo: `/opt/worker/code/prx`
Push: not pushed
Binary: `/opt/worker/code/prx/target/debug/prx`

## Scope Completed

- Inline markdown now renders `**bold**`, `*italic*`, `_italic_`, and inline code in `src/chat/renderer.rs`.
- Generation spinner now advances from a 50ms wall-clock tick, so waiting/tool phases animate even when streaming draft version is unchanged.
- Slash menu follow-ups:
  - second-line `/he` with valid command matches no longer opens the slash menu;
  - overlay geometry is covered by a test;
  - unused empty-source slash-menu wrappers were removed;
  - unreachable `No matching slash commands` render branch no longer paints dead text.
- Saved-session startup cache is loaded once for TUI mirror + Redux shadow, with `tracing::warn!` on load failure.
- Main-turn elapsed surface now has wiring coverage proving `surface_turn_elapsed_message` dispatches `SystemMessageAdded` and redraw.
- F4 follow-ups:
  - markdown render cache capacity raised from 128 to 512;
  - cache tests using global cache are serialized;
  - streaming markdown over 32 KiB uses a plain-text threshold path with cursor instead of expensive highlight;
  - ANSI bridge supports `38;5`/`48;5` indexed color and skips non-SGR CSI sequences.
- External signal Ctrl+C now clears the TUI approval mirror and requests redraw, matching the keyboard Ctrl+C path.

## Validation

- `cargo test --bin prx inline_bold_and_italic_formatting -- --nocapture` passed.
- `cargo test --bin prx slash_menu_only_triggers_at_first_line_start -- --nocapture` passed.
- `cargo test --bin prx slash_menu_overlay_rect_stays_above_bottom_chrome -- --nocapture` passed.
- `cargo test --bin prx ansi_bridge_supports_indexed_color_and_skips_non_sgr_csi -- --nocapture` passed.
- `cargo test --bin prx large_streaming_markdown_uses_plain_threshold_path_with_cursor -- --nocapture` passed.
- `cargo test --bin prx finalized_assistant_markdown_uses_render_cache -- --nocapture` passed.
- `cargo test --bin prx surface_turn_elapsed_message_dispatches_system_message_and_redraw -- --nocapture` passed.
- `cargo fmt --check` passed.
- `cargo clippy --all-targets -- -D warnings` passed.
- `cargo test --bin prx -- --nocapture` passed: 5234 passed, 0 failed, 7 ignored.
- `cargo build --bin prx` passed.
- `git diff --check` passed.

## Deviations / Notes

- No push performed.
- The `/save` cache-refresh bullet was not separately changed because this codebase has no explicit `/save` slash command in `commands.rs`; the saved-session cache work performed here addresses the documented startup double-read/warn gap.
- The larger F4 seq-keyed render-cache redesign was not attempted in 3c; this follow-up uses a larger bounded cache plus serialized global-cache tests and streaming-size thresholding.
