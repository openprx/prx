# Receipt: chat history scroll / mouse copy A+B+C

Baseline: `feat/chat-history-copy-guidance` off `c998ad79`, package version left at `0.8.4`.

No commit was made.

## Changes

### B. PTY handoff mouse restore gate

- `src/chat/sessions/pty.rs:212,237,290,298,364` threads `mouse_capture_active` through `PtyHandoffGuard` and `write_handoff_terminal_restore`.
- `src/chat/sessions/pty.rs:371` now emits `CHAT_MOUSE_CAPTURE_ENABLE` only when chat had mouse capture active before the handoff.
- `src/chat/sessions/pty.rs:298` re-runs `EnableMouseCapture` only when `mouse_capture_active` is true.
- `src/chat/mod.rs:10793,11592` passes live `CHAT_MOUSE_CAPTURE_ACTIVE` into external-editor and PTY restore paths.
- Regression: `src/chat/sessions/pty.rs:1634` asserts mouse-off chat restores alt/bracketed modes without mouse enable escapes.

### A. Footer/help/placeholder guidance

- `src/chat/commands.rs:460` adds a `History view & copy` help section covering `Home/PageUp`, `Ctrl+O`, `--plain`/`PRX_TUI=0`, `/copy latest|N`, and `/export md|json`.
- `src/chat/tui.rs:7156` updates footer text with `Home/PageUp history`, `Ctrl+O transcript`, `/copy latest`, and `--plain drag copy`.
- `src/chat/tui.rs:1508,4139,4324` updates empty transcript placeholders to mention `PageUp/Home` and `/help for copy/export`.
- Tests: `src/chat/commands.rs:1174`, `src/chat/tui.rs:11411`, `src/chat/tui.rs:11628`, `src/chat/tui.rs:12261`, `src/chat/tui.rs:12546`.

### C. Mouse release / restore toggle

- Key choice: `Ctrl+Space`; design table checked in `/opt/worker/task/prx/design-chat-key-model-rearrange-2026-07-11.md` and current code grep found no conflict.
- `src/chat/tui.rs:1815` accepts both `KeyCode::Char(' ') + CONTROL` and `KeyCode::Null + CONTROL` for terminal/tmux encoding differences.
- `src/chat/tui.rs:711,1578` dispatches `ToggleMouseSelectionMode`.
- `src/chat/mod.rs:1615` toggles mouse capture off/on and dispatches UI state.
- `src/chat/mod.rs:12490` ignores mouse events while selection mode is active, so terminal drag selection can work.
- `src/chat/action.rs:280` and `src/chat/state.rs:1027,2991,3326` carry the UI-only `MouseSelectionModeChanged` state into snapshots.
- `src/chat/tui.rs:7162` shows `selection mode: drag select/copy · Ctrl+Space restore mouse`.
- Tests: `src/chat/mod.rs:14584,14596`, `src/chat/tui.rs:9859,9869`, `src/chat/state.rs:9704`.

## Validation

Environment used for cargo gates:

`CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`

- `cargo fmt --check` - pass
- `cargo clippy -p openprx --all-targets --all-features -- -D warnings` - pass
- `cargo check -p openprx --all-features` - pass
- `cargo check -p openprx --no-default-features` - pass
- `cargo test -p openprx --bin prx --all-features` - pass: `5501 passed; 0 failed; 7 ignored`
- `cargo audit` - pass, exit 0
- `cargo deny check advisories` - pass: `advisories ok`
- `cargo deny check bans licenses sources` - pass: `bans ok, licenses ok, sources ok`
- `git diff --check` - pass

Focused tests run before full gate:

- `mouse_selection` - 5 passed
- `handoff_restore` - 2 passed
- `footer_exposes_history_and_plain_copy_guidance` - passed
- `help_text_is_generated_from_command_registry` - passed
- `fullscreen_empty_chat_draws_transcript_pane_and_pinned_chrome` - passed
- Four full-test regressions from updated contracts were fixed and rechecked:
  `external_editor_fullscreen_suspend_leaves_alt_and_restore_reenters`,
  `fullscreen_footer_advertises_copy_paths`,
  `transcript_view_is_full_and_handles_empty_history`,
  `slash_command_submit_leaves_input_render_empty`.

## PTY demo evidence

Binary: `/opt/worker/tmp/prx-target/debug/prx` built from this worktree.

- A footer guidance: tmux capture saved at `/opt/worker/tmp/prx-demo-a-help.txt`; line 14 shows `Home/PageUp history`, `Ctrl+O transcript`, `/copy latest`, `--plain drag copy`.
- A help/placeholder: `/help` and placeholder copy are locked by render tests. In live TUI, `/clear` adds a system line (`Conversation cleared`) so the empty placeholder is not visible after that action.
- B PTY handoff: tmux raw pipe saved at `/opt/worker/tmp/prx-demo-b.raw`; real `/pty sh -lc "printf PTY_READY; sleep 30"` then `/attach #1`, `Ctrl-]` detach. Raw scan:
  - `PTY_READY` present
  - `\x1b[?1000h`: 0
  - `\x1b[?1002h`: 0
  - `\x1b[?1003h`: 0
  - `\x1b[?1006h`: 0
  - `\x1b[?1015h`: 0
- C toggle: `PRX_TUI_ENABLE_MOUSE=1` tmux demo accepted `C-@` as Ctrl+Space equivalent. First press showed `selection mode: drag select/copy · Ctrl+Space restore mouse`; second press restored the normal footer. Final capture saved at `/opt/worker/tmp/prx-demo-c-final.txt`.

## Notes

- One `/pty` first-attach attempt timed out with `the chat renderer did not pause in time`; the running PTY was then attached successfully with `/attach #1`. This was demo timing, not the restore gate; the successful attach/detach raw log is the B evidence.
- No version bump, no commit.
