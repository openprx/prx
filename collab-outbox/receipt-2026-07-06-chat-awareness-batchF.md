# Receipt: chat-awareness Batch F

Date: 2026-07-06
Agent: Codex
Commit: `3c0095f7 feat(chat): persist scoped conversation profiles`
Push: not pushed

## Scope

Implemented Batch F items 6-10 from `collab-inbox/task-2026-07-06-chat-awareness.md`, released by `collab-inbox/fixround-2026-07-06-chat-awareness-batchE-accept-batchF-go.md`.

## Changes

- Added `chat_profiles` persistence to the memory trait plus SQLite and Postgres implementations.
  - Schema matches required columns and `UNIQUE(channel, chat_id)`.
  - Exact lookup by `(channel, chat_id)`.
  - Auto metadata upsert updates only `chat_kind`, `title` when supplied, and `updated_at`; it preserves `purpose`, `notes`, `tags`, and `updated_by`.
- Added `chat_profile_update` tool.
  - Public parameters are only `purpose`, `notes`, and `tags`.
  - Rejects model-supplied `channel` / `chat_id`.
  - Target is locked to trusted `_zc_scope.channel` / `_zc_scope.chat_id`.
  - Writes `updated_by='agent'`.
  - Uses `SideEffectGate` with `ResourceRiskLevel::Medium`.
  - Truncates `purpose` to 300 chars, `notes` to 1024 chars, and `tags` to 10 items with tool output notices.
- Added channel inbound metadata backfill in `process_channel_message`.
- Added channel-only prompt injection for current conversation profile.
  - Uses exact `(channel, reply_target)` profile lookup.
  - No semantic retrieval and no cross-chat lookup.
  - Injects Platform/You/Type/Chat rows even when no profile content is present.
  - TUI runtime prompt path is unchanged and does not use this channel wrapper.
  - `ChatKind::Thread` renders as `thread`.
- Added `Channel::bot_identity()` default plus wacli and Telegram overrides.
  - wacli reads `bot_jid` / `bot_number`.
  - Telegram reads the cached `bot_username` only; prompt construction does not make network calls.

## Tests Added / Strengthened

- SQLite profile metadata upsert preserves self-maintained fields and title overwrite rules.
- Tool test: group turn update writes `updated_by='agent'` only to the current group row.
- Tool test: `chat_profile_update` does not write the `memories` table.
- Tool test: model-supplied `channel` / `chat_id` target is rejected.
- Tool test: tag overflow truncates to 10 with a notice.
- Prompt tests:
  - cross-chat isolation: A group purpose does not appear in B group or DM prompt.
  - no-profile snapshot still contains Platform/You/Type/Chat.
  - with-profile snapshot contains purpose/notes/tags.
  - long purpose/notes are truncated for prompt display.
  - Thread renders self-consistently as `thread`.
  - existing channel prompt test now also asserts the Current Conversation block.

## Verification

Tracked source state was clean after commit before receipt creation. Only pre-existing untracked collaboration artifacts were present.

Pre-commit:
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf cargo clippy --workspace --all-targets` PASS.
- Targeted tests:
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf cargo test --bin prx chat_profile -- --nocapture` PASS, 3 passed.
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf cargo test --bin prx current_conversation_prompt -- --nocapture` PASS, 3 passed.
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf cargo test --bin prx channel_prompt_includes_channel_delivery_context -- --nocapture` PASS, 1 passed.

Receipt self-check from clean committed tracked state:
- `cargo fmt --all -- --check` PASS.
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf cargo clippy --workspace --all-targets` PASS.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf cargo test --bin prx` PASS: 5292 passed, 0 failed, 7 ignored.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batchf cargo check --workspace --no-default-features` PASS.

## Real E2E

Not executed.

Reason:
- A live user daemon exists: `systemctl --user status prx` reports active `/home/ck/.cargo/bin/prx daemon`.
- The installed daemon binary does not contain this Batch F tool yet: `grep -a "chat_profile_update" /home/ck/.cargo/bin/prx` returned `installed_binary_missing_chat_profile_update`.
- The repo worktree commit was not installed into the live daemon, and I did not replace/restart the user's live daemon without explicit deployment approval.
- wacli config exists (`/home/ck/.openprx/config.d/channels.toml`) and points at `/opt/worker/code/wacli-official/dist/wacli`, but no safe real target chat was provided for sending a mark-profile test message. I did not send unsolicited messages to real WhatsApp/Telegram contacts or groups.

Local persistence/prompt behavior is covered by the unit tests above at the storage and prompt-builder boundaries. Full daemon restart E2E remains unexecuted for the reasons listed here.
