# Receipt: chat UX round3 U5 slash command filter

## Scope

- Task: U5 slash filtering should match command names, not description text.
- Commit: `cbc249c0 fix(chat): filter slash commands by name`
- Push: not pushed.

## Changes

- `src/chat/tui.rs:377` now matches slash command filters against canonical command names and aliases only.
- Removed description-text matching from top-level slash command filtering so description words such as "mode", "most", or "conversation" cannot surface unrelated commands.
- `src/chat/tui.rs:7957` strengthens `/mo` regression coverage: every visible command label must contain `mo` in the command name, and `/model` remains present.
- `src/chat/tui.rs:7979` replaces the old description-match expectation with a redline test: `/conversation` leaves the input intact but closes the slash menu because it is description-only.

## Validation

Pre-commit targeted validation:

- `cargo fmt --all` PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u5 cargo test --bin prx slash_menu_ -- --nocapture` PASS: 23 passed, 0 failed, 5277 filtered out
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u5 cargo clippy --workspace --all-targets` PASS

Receipt self-check from committed state:

- `cargo fmt --all -- --check` PASS
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u5 cargo clippy --workspace --all-targets` PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u5 cargo test --workspace` PASS
  - lib/bin/doc/integration suite completed successfully
  - main lib summary: 5293 passed, 0 failed, 7 ignored
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u5 cargo check --workspace --no-default-features` PASS

## Demo Recheck

- Needs demo recheck: type `/mo` in the TUI slash menu and confirm unrelated description-only matches such as `/copy`, `/plan`, `/edit`, and `/auto` do not appear.

## Notes

- Argument candidate filtering was not changed; U5 only targeted the top-level command list.
- Existing untracked `collab-inbox/`, previous receipts, and `task/` remain untouched.
