# Key Model Nits Fixround Receipt

Date: 2026-07-11
Branch: `feat/chat-ux-fu2-fu3-polish`
Base: current uncommitted B-nav working tree
Commit: not committed

## Summary

Implemented `collab-inbox/fixround-2026-07-11-key-model-nits.md` on top of the existing uncommitted key-model rearrange work.

Changed files:

- `src/chat/action.rs`
- `src/chat/mod.rs`
- `src/chat/state.rs`
- `src/chat/tui.rs`

## FIX-1

Status: fixed.

What changed:

- `src/chat/tui.rs:1672` routes `PageUp/PageDown/Home/End` for main focus before input editing, so transcript paging/jump works even when a draft is present.
- `src/chat/tui.rs:1681` routes `PageUp/PageDown/Home/End` for child-view focus before input editing.
- `src/chat/tui.rs:684` added `SessionHome` / `SessionEnd` dispatch variants.
- `src/chat/mod.rs:12357` and `src/chat/mod.rs:12360` handle child Home/End by jumping the active child viewport to oldest retained output or tail.
- Bare `Up/Down/Left/Right` axis behavior was not changed; those paths still respect the existing input-empty gate.

Tests:

- `src/chat/tui.rs:10344` `dispatch_page_home_end_scroll_main_transcript_even_with_input`
- `src/chat/tui.rs:10385` `dispatch_child_view_scroll_keys_only_when_child_focus_and_empty_input`

## FIX-2

Status: fixed.

Confirmed with `rg` that no active production key route wrote `strip_selection = Some(..)`. The remaining readers/writers were stale cache plumbing, render selected-highlight branches, reducer action support, and tests that manufactured the old state.

Removed:

- `Action::StripSelectionChanged` from `src/chat/action.rs`.
- `UiState::strip_selection` and `UiSnapshot::strip_selection` from `src/chat/state.rs`.
- `BottomChromeView::strip_selection` from `src/chat/tui.rs`.
- stale-selection reducer and snapshot dirty handling from `src/chat/state.rs`.
- stale-selection reaping helper/dispatch path from `src/chat/mod.rs`; `src/chat/mod.rs:1281` is now plain `refresh_sessions_cache`.
- render selected-highlight branches from the bottom list/strip renderer.

Retained behavior:

- bottom-list windowing and active marker now rely only on `focus_active_entry_seq` / `focus_entry_index` (`src/chat/tui.rs:1135`, `src/chat/tui.rs:4712`);
- main/session/worker high-lighting remains focus-driven, not selected-cache-driven.

Tests changed:

- removed old reducer test for `StripSelectionChanged`;
- rewrote stale-strip Alt tests to assert newline / modal behavior without the dead field;
- `src/chat/tui.rs:11029` `sessions_strip_active_entry_beyond_initial_window_is_visible_and_marked`;
- `src/chat/tui.rs:11245` `sessions_strip_focus_drives_active_marker_without_selection_highlight`;
- `src/chat/mod.rs:1812` `sessions_tick_helper_refreshes_session_cache`.

Post-cleanup grep:

```text
rg -n "strip_selection|StripSelectionChanged|render_sessions_strip_line_with_selection|strip_selection_index" src/chat/tui.rs src/chat/mod.rs src/chat/state.rs src/chat/action.rs
result: no matches
```

## Validation

Focused tests:

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features chat::tui -- --nocapture
test result: ok. 274 passed; 0 failed; 0 ignored; 0 measured; 5216 filtered out

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features chat::state -- --nocapture
test result: ok. 206 passed; 0 failed; 0 ignored; 0 measured; 5284 filtered out

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features sessions_tick_helper_refreshes_session_cache -- --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 5489 filtered out
```

Required gates:

```text
cargo fmt --check
result: passed

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy -p openprx --all-targets --all-features -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 58.88s

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --all-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 11.04s

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.74s

CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features
test result: ok. 5483 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out; finished in 8.68s

cargo audit
Scanning Cargo.lock for vulnerabilities (914 crate dependencies)
result: passed

cargo deny check advisories
advisories ok
```

## PTY

Built current binary:

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo build -p openprx --bin prx --all-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 02s
```

Real tmux PTY checks:

- With draft text `draft-visible` in the input box, `PageUp` scrolled the transcript from `> msg 12` to `> msg 11` while preserving the draft text.
- Fresh bottom-list run showed focus marker on main: `▸ main chat active`.
- `Right` attached `#1`; bottom list showed `▸... #1 ...` and child footer `↑/↓ scroll · ←/→ switch · Esc back`.
- Another `Right` attached `#2`; bottom list showed `▸... #2 ...`.

The demo tmux sessions were killed after capture.

## Notes

- Did not change the axis-separation or directional debounce logic beyond FIX-1's Page/Home/End gate.
- Did not commit.
