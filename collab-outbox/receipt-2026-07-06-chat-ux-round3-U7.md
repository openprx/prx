# Receipt: chat UX round3 U7 Thinking mouse expand

## Scope

- Task: U7 folded Thinking block should expand by mouse click while default folded behavior remains.
- Commit: `595529b5 fix(chat): toggle thinking cards by mouse`
- Push: not pushed.

## Changes

- `src/chat/mod.rs:8983` routes left mouse down events through the fullscreen transcript hit-test after preserving existing wheel scroll handling.
- `src/chat/tui.rs:3594` adds `toggle_reasoning_at_fullscreen_point`, which maps terminal coordinates through the same transcript/chrome/panel layout and scroll math used by rendering.
- `src/chat/tui.rs:3667` limits hit targets to the rendered `Reasoning` header rows only. Body rows, non-reasoning lines, bottom chrome, and panels do not toggle.
- `src/chat/tui.rs:3661` shares persistent conversation-line rendering between hit-test math and the transcript renderer, while keeping streaming tail rendering local to the caller.
- `src/chat/tui.rs:7691` adds regression coverage for header click expand/collapse and body click no-op.
- `src/chat/tui.rs:7719` adds regression coverage that clicks outside the transcript keep the default folded state.

## Validation

Pre-commit targeted validation:

- `cargo fmt --all` PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u7 cargo test --bin prx reasoning -- --nocapture` PASS: 56 passed, 0 failed, 5246 filtered out
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u7 cargo clippy --workspace --all-targets` PASS

Receipt self-check from committed state:

- `cargo fmt --all -- --check` PASS
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u7 cargo clippy --workspace --all-targets` PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u7 cargo test --workspace` PASS
  - lib/bin/doc/integration suite completed successfully
  - main lib summary: 5293 passed, 0 failed, 7 ignored
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u7 cargo check --workspace --no-default-features` PASS

## Demo Recheck

- Needs demo recheck: in the real TUI, click the folded `Thinking (...)` header and confirm it expands; click the expanded header again and confirm it collapses.
- Also recheck that clicking the reasoning body, normal transcript text, and bottom chrome does not unexpectedly toggle.

## Notes

- Default folded state is unchanged.
- Existing `Tab` fold behavior is unchanged.
- Mouse wheel transcript scroll behavior is unchanged and still has priority over click hit-testing.
- Existing untracked `collab-inbox/`, previous receipts, and `task/` remain untouched.
