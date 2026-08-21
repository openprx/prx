# Receipt: Chat UX Round 3 U6

## Scope
- Task: U6 input "ghost text" and Esc residue.
- Commit: `e1b38795 fix(chat): clear slash command ghost input`
- Push: not pushed.

## Changes
- `src/chat/tui.rs:939` now treats Esc in a slash-command completion context as "close menu and clear the slash command draft".
  - Example: typing `/mo` and pressing Esc leaves the input empty instead of leaving `/mo` or `/`.
  - Slash argument completion and `@path` completion are not cleared by this branch, so an in-progress non-command draft is not destroyed.
- Added regression coverage:
  - `src/chat/tui.rs:8263` verifies Esc clears the slash-command draft while keeping Tab-selected `/export ` argument completion behavior intact.
  - `src/chat/tui.rs:8300` verifies a submitted slash command leaves input state/display empty and does not render the old command text in the input area.

## Verification
- Pre-commit targeted:
  - `cargo fmt --all` PASS
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u6 cargo test --bin prx slash_ -- --nocapture` PASS
    - `44 passed; 0 failed; 5260 filtered out`
  - `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u6 cargo clippy --workspace --all-targets` PASS
- Receipt self-check from committed clean source state:
  - `cargo fmt --all -- --check` PASS
  - `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u6 cargo clippy --workspace --all-targets` PASS
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u6 cargo test --workspace` PASS
    - command exit code 0; output was large and tool-truncated, final visible suites and doctests passed
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u6 cargo check --workspace --no-default-features` PASS

## Demo Recheck
- Needs demo recheck:
  - Type `/`, `/mo`, or another partial slash command, press Esc once, confirm the input box is empty.
  - Execute `/help` or another no-arg slash command, confirm the input box returns to a clean empty state and does not show the old command as bright text.
