# Receipt: Chat awareness Batch E

Scope: ChatKind unification, Telegram group prefix, and TUI/channel prompt split.

Commits:
- `813f8ef7 fix(chat): preserve slash selection on path refresh` (late UX round2 AB/I1 addendum prerequisite)
- `22de7ba6 feat(channels): track structured chat kind metadata`

Summary:
- Added structured `ChatKind` plus optional `chat_title` and `sender_display` metadata to `ChannelMessage`.
- Populated authoritative chat metadata for Telegram, Discord, Signal, and wacli; other channel fixtures/literals default to direct-message semantics.
- Central channel consumers now prefer `ChatKind` for chat type, memory visibility, autosave suppression, smart group decisions, and tool scope; legacy `reply_target` inference remains only as a default-DM fallback.
- Telegram group turns now prefix content as `[Telegram Group] {sender}: ...` and carry title/display metadata.
- Base runtime/TUI system prompts no longer include messaging-bot delivery wording; channel prompts append channel delivery/capability instructions only on channel paths.

Regression tests:
- `channels::tests::structured_chat_kind_drives_group_scope_for_bare_telegram_targets`
- `channels::tests::process_channel_message_telegram_group_does_not_autosave`
- `channels::telegram::tests::parse_update_message_mention_only_group_strips_mention_and_drops_empty`
- `channels::telegram::tests::smart_mode_passes_through_unmentioned_group_message_with_hints`
- `channels::telegram::tests::smart_mode_sets_mentioned_hint_when_addressed`
- `channels::telegram::tests::explicit_off_mode_never_drops_unmentioned_group_message`
- `channels::tests::runtime_prompt_excludes_channel_delivery_context`
- `channels::tests::channel_prompt_includes_channel_delivery_context`

Self-check from clean committed tracked state:
- `git diff --quiet && git diff --cached --quiet` -> PASS
- `cargo fmt --all -- --check` -> PASS
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batche cargo clippy --workspace --all-targets` -> PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batche cargo test --bin prx` -> PASS (`5286 passed; 0 failed; 7 ignored`)
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batche cargo check --workspace --no-default-features` -> PASS

Binary/test artifact:
- `/opt/worker/tmp/prx-chat-awareness-batche/debug/deps/prx-ffc3817af57d929f`

Push: not performed.
