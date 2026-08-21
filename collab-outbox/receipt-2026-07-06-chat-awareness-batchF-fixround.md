# Receipt: chat-awareness Batch F fix-round

Date: 2026-07-06
Agent: Codex
Commit: `3c8c987e fix(chat): always expose conversation profile tool`
Push: not pushed

## Scope

Implemented `collab-inbox/fixround-2026-07-06-chat-awareness-batchF-tool-visibility.md`.

Only the tool visibility / prompt preference gap was changed. The accepted `chat_profiles` schema, exact `(channel, chat_id)` lookup, `_zc_scope` target lock, and memories-table redline were not changed.

## Fixes

- Fix 1: `chat_profile_update` is now a Core tool.
  - `src/tools/chat_profile_update.rs:244-249`
  - `tier()` now returns `ToolTier::Core`, so `select_tools_for_intent` includes it every turn even when the user message has no Memory-category keywords.
- Fix 2: Added regression test for no-keyword visibility.
  - `src/tools/intent.rs:208-230`
  - Test: `tools::intent::tests::chat_profile_update_is_offered_without_memory_keywords`
  - The test builds the real `ChatProfileUpdateTool`, uses a message without memory keywords, asserts `chat_profile_update` is selected, and asserts its tier is `ToolTier::Core`.
  - Mutation expectation: changing the tool back to `ToolTier::Standard` removes it from the selected tools and makes this test fail.
- Fix 3: Strengthened model guidance to prefer the profile tool.
  - `src/tools/chat_profile_update.rs:111-113`
  - Description now says this is the correct tool for current chat/group purpose, notes, and tags, and to prefer it over general memory tools.
  - `src/channels/mod.rs:972-973`
  - Injected prompt now says to use `chat_profile_update` and not `memory_store`.
  - `src/channels/mod.rs:10200-10203`
  - Existing channel prompt test now asserts the preference hint.

## Verification

Tracked source state was clean after commit before receipt creation. Only pre-existing untracked collaboration artifacts were present.

Pre-commit targeted checks:
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf-fixround cargo test --bin prx chat_profile_update_is_offered_without_memory_keywords -- --nocapture` PASS, 1 passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf-fixround cargo test --bin prx channel_prompt_includes_channel_delivery_context -- --nocapture` PASS, 1 passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf-fixround cargo test --bin prx rejects_model_supplied_target_and_truncates_tags -- --nocapture` PASS, 1 passed.
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf-fixround cargo clippy --workspace --all-targets` PASS.

Receipt self-check from clean committed tracked state:
- `cargo fmt --all -- --check` PASS.
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf-fixround cargo clippy --workspace --all-targets` PASS.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf-fixround cargo test --bin prx` PASS: 5293 passed, 0 failed, 7 ignored.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf-fixround cargo check --workspace --no-default-features` PASS.

