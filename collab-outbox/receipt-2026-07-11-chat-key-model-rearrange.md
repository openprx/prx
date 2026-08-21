# Chat Key Model Rearrange Receipt

Date: 2026-07-11
Branch: `feat/chat-ux-fu2-fu3-polish`
Base HEAD: `3eb7380f`
Commit: not committed

## Scope

Implemented the axis-separated chat key model from `collab-inbox/task-2026-07-11-chat-key-model-rearrange.md` and `/opt/worker/task/prx/design-chat-key-model-rearrange-2026-07-11.md`.

Changed files:

- `src/chat/tui.rs`
- `src/chat/mod.rs`
- `src/chat/state.rs`

Diff stat:

```text
src/chat/mod.rs   | 155 +++++++++---
src/chat/state.rs |  75 +-----
src/chat/tui.rs   | 712 ++++++++++++++++++++++++------------------------------
3 files changed, 442 insertions(+), 500 deletions(-)
```

## Implementation

### TUI key dispatch

- `src/chat/tui.rs:623` updated `KeyDispatch`:
  - removed direct key-path use of `StripSelectionChanged`;
  - added transcript-specific actions: `ScrollTranscriptUp`, `ScrollTranscriptDown`, `PageTranscriptUp`, `PageTranscriptDown`, `TranscriptHome`, `TranscriptEnd`.
- `src/chat/tui.rs:1224` added bottom-entry activation helpers for immediate directional switching.
- `src/chat/tui.rs:1565` rewired `dispatch_global_key`:
  - main empty input: `Up/Down` scroll main transcript, `Left/Right` switch bottom entries, `PageUp/PageDown/Home/End` target transcript;
  - child/worker empty input: `Up/Down/Page*` scroll the child detail, `Left/Right` switch bottom entries, landing main closes/detaches child focus;
  - read-only transcript/diff focus keeps scroll semantics;
  - approval flow remains before the new global routing.
- `src/chat/tui.rs:1692` and `src/chat/tui.rs:1740` consume crossterm repeat `Left/Right` at the pure dispatch layer.
- `src/chat/tui.rs:920`, `src/chat/tui.rs:1815`, and `src/chat/tui.rs:1862` moved input-history recall to `Ctrl+P/Ctrl+N`; single-line bare `Up/Down` no longer recall history.
- `src/chat/tui.rs:3961` added one-line transcript scroll helpers for main transcript `Up/Down`.
- `src/chat/tui.rs:4636` and `src/chat/tui.rs:7077` fixed footer hints:
  - main: `↑/↓ scroll`, `←/→ session` when bottom entries exist, `Ctrl+P/N history`;
  - child: `↑/↓ scroll · ←/→ switch · Esc back`.

### Runtime loop

- `src/chat/mod.rs:1571` added a 100 ms runtime debounce for directional bottom switching.
- `src/chat/mod.rs:1594` suppresses rapid `Left/Right` attach/switch/open/close/detach dispatches to avoid key-repeat attach storms.
- `src/chat/mod.rs:12043` applies the debounce after `dispatch_global_key`.
- `src/chat/mod.rs:12347` handles the new transcript scroll/page/home/end dispatches in the TUI loop.
- Removed the old focus-agnostic pre-dispatch `PageUp/PageDown/Home/End` handling so page keys are focus-aware.

### Strip selection

- `src/chat/state.rs:341` now documents `strip_selection` as a legacy highlight/cache only.
- `src/chat/state.rs:1204` removed `Esc` clearing the strip cache as a key-model side effect.
- Removed reducer-side `Alt+Left/Right/Enter` strip navigation. `Alt+Enter` now falls through to newline behavior when appropriate.
- Kept `Action::StripSelectionChanged` and `strip_selection` storage because stale-session reaping/render compatibility still references them; no current key route writes strip selection from `Alt` navigation.

## Tests

Updated or added tests for the new contract, including:

- `src/chat/tui.rs:8844` `p2_10_history_ctrl_p_recalls_last_submission`
- `src/chat/tui.rs:9300` `input_history_moves_to_ctrl_p_n_when_slash_menu_closed`
- `src/chat/tui.rs:10415` `dispatch_child_view_scroll_keys_only_when_child_focus_and_empty_input`
- `src/chat/tui.rs:10523` `dispatch_directional_session_switching_obeys_focus_input_matrix`
- `src/chat/tui.rs:10701` `alt_enter_no_longer_attaches_strip_selection_and_still_inserts_newline`
- `src/chat/tui.rs:10730` `stale_strip_selection_no_longer_affects_alt_enter`
- `src/chat/tui.rs:11662` `provider_worker_focus_direction_and_esc_are_read_only_view_controls`
- `src/chat/mod.rs:15643` `directional_switch_debounce_suppresses_rapid_attach_dispatches`
- `src/chat/state.rs:3926` / `3958` / `3989` updated reducer tests for removed `Alt+strip_selection` behavior.

Focused test runs:

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features chat::tui -- --nocapture
result: ok. 274 passed; 0 failed; 5217 filtered out

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features chat::state -- --nocapture
result: ok. 207 passed; 0 failed; 5284 filtered out

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features directional_switch_debounce_suppresses_rapid_attach_dispatches
result: ok. 1 passed; 0 failed
```

Full test:

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features
test result: ok. 5485 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out; finished in 12.68s
```

## Gates

```text
cargo fmt --check
result: passed

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy -p openprx --all-targets --all-features -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 56.21s

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --all-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 16.43s

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.06s

cargo audit
Scanning Cargo.lock for vulnerabilities (914 crate dependencies)
result: passed

cargo deny check advisories
advisories ok
```

## PTY Demo

Built the current binary:

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo build -p openprx --bin prx --all-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 42.24s
```

Started a real tmux PTY with:

```text
PRX_TUI=1 OPENPRX_MOCK_RESPONSE='DEMO_REPLY line1 line2 line3 line4 line5 line6 line7 line8 line9 line10' \
/opt/worker/tmp/prx-target/debug/prx chat -p mock -m mock --config-dir /opt/worker/tmp/prx-key-demo-config
```

Observed:

- initial main footer: `↑/↓ scroll · Ctrl+P/N history · Ctrl+G sessions ...`;
- after ten mock turns, `Up x5` moved transcript tail from `> msg 10` to `> msg 9`;
- `Down x5` returned transcript tail to `> msg 10`;
- `Ctrl+P/Ctrl+N` changed draft input history text in the PTY; exact direction is locked by unit tests because tmux capture showed a one-frame redraw lag;
- after starting two background shell sessions, bottom strip showed main + `#1` + `#2` and main footer included `←/→ session`;
- `Right` immediately attached shell `#1`;
- child footer showed `↑/↓ scroll · ←/→ switch · Esc back`;
- rapid `Right x10` advanced from `#1` to `#2` without repeated attach storm;
- `Left` switched back to `#1`.

The tmux session was killed after capture.

## Notes

- No P4b/P4c visible-turn scheduling logic was changed.
- No commit was created.
