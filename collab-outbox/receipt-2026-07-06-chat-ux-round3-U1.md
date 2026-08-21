# Receipt: Chat UX Round 3 U1

Status: DONE
Commit: ef9ea6d4 fix(chat): harden copy paths
Push: not pushed

## Scope

U1 copy UX polish after demo:

- harden clipboard handoff for tmux/OSC52
- make copy paths discoverable in fullscreen footer/help surface
- provide a native terminal selection escape hatch without disturbing the core chat loop
- mark interactive clipboard/selection behavior for demo recheck

## External comparison

Claude Code fullscreen docs describe an in-app mouse selection path that auto-copies selected text on release, plus `Ctrl+Shift+C` when auto-copy is disabled. They also document native terminal selection escape hatches by terminal (`Fn`, `Option`, `Shift`, etc.) and a mouse-capture disable environment variable for users who prefer terminal-native selection.

Source: https://code.claude.com/docs/en/fullscreen

## Changes

- `src/chat/terminal_proto.rs:292`
  - `/copy` now attempts `tmux load-buffer -w -` when `TMUX` is present before emitting OSC52.
  - OSC52 sequence construction is factored into `osc52_clipboard_sequence` and tested exactly.
  - If tmux handoff fails, PRX logs debug and still falls back to OSC52.
- `src/chat/commands.rs:290`
  - `/copy` description now says it copies the latest or Nth previous assistant reply.
- `src/chat/tui.rs:6202`
  - fullscreen footer now advertises `/copy latest` and `Shift/Option/Fn-drag select`.
- `src/chat/mod.rs:404`
  - added explicit parser for `PRX_TUI_DISABLE_MOUSE=1|true|yes|on`.
- `src/chat/mod.rs:2463`
  - fullscreen terminal setup skips mouse capture when `PRX_TUI_DISABLE_MOUSE` is set, preserving keyboard/alternate-screen/bracketed-paste setup.

## Deliberate boundary

This does not add full Claude-style in-app drag selection yet. That path is higher risk because PRX already uses mouse clicks for transcript/Thinking interactions. The implemented behavior gives the demo-safe pieces now:

- `/copy [N]` selective assistant-response copy remains command-layer safe and is now visible.
- tmux users get `load-buffer -w` hardening before OSC52 fallback.
- terminal-native selection is discoverable via footer guidance.
- users can opt out of TUI mouse capture with `PRX_TUI_DISABLE_MOUSE=1 prx chat` when native drag selection is preferred.

## Validation

Pre-commit targeted checks:

- PASS `cargo fmt --all`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo test --bin prx copy_ -- --nocapture`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo test --bin prx osc52_clipboard_format -- --nocapture`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo test --bin prx mouse_capture_disable_env_parser_is_explicit -- --nocapture`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo test --bin prx help_text_is_generated_from_command_registry -- --nocapture`
- PASS `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo clippy --workspace --all-targets`

Receipt self-check from clean committed source state:

- PASS `cargo fmt --all -- --check`
- PASS `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo clippy --workspace --all-targets`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo test --workspace`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo check --workspace --no-default-features`

## Demo Recheck Needed

- `/copy` after an assistant reply should place content on the local clipboard.
- Inside tmux, verify both clipboard paste and tmux buffer behavior under the user's real `set-clipboard` / `allow-passthrough` config.
- Try terminal-native drag selection with the documented modifier for the user's terminal.
- If mouse capture still blocks native selection, try `PRX_TUI_DISABLE_MOUSE=1 prx chat` and verify drag selection works while keyboard chat flow remains normal.
