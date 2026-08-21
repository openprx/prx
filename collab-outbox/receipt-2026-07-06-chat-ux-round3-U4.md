# Receipt: Chat UX Round 3 U4

## Scope
- Task: U4 `/cost` and status line showed `cost unknown` for the demo provider/model even though token accounting was correct.
- Commit: `5710e0d2 fix(chat): price kimi code usage`
- Push: not pushed.

## Changes
- Added default Kimi pricing entries in `src/config/schema.rs:1723` for:
  - `kimi-code/kimi-k2.7-code`
  - `kimi-code/kimi-k2.7-code-highspeed`
  - `kimi-code/kimi2.6`
  - `kimi-code/kimi-k2.6`
  - `kimi-code/kimi-k2.5`
  - `moonshot/kimi-k2.6`
  - `moonshot/kimi-k2.5`
- Added a usage-record regression test in `src/chat/session.rs:590` for `kimi-code/kimi-k2.7-code` so the demo model computes cost instead of returning `None`.
- Cache-hit prices use Kimi's published cache-hit input tier; cache-write prices intentionally fall back to normal input price because the public Kimi pricing table does not expose a separate cache-write tier.

## Pricing Sources
- Kimi API Platform pricing overview: https://platform.kimi.ai/
- Kimi K2.7 Code pricing page: https://platform.kimi.ai/docs/pricing/chat-k27-code
- Kimi K2.6 pricing page: https://platform.kimi.ai/docs/pricing/chat-k26
- Kimi K2.5 pricing page: https://platform.kimi.ai/docs/pricing/chat-k25

## Verification
- Pre-commit targeted:
  - `cargo fmt --all` PASS
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u4 cargo test --bin prx provider_usage_record_computes_kimi_code_cost -- --nocapture` PASS
    - `1 passed; 0 failed; 5302 filtered out`
  - `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u4 cargo clippy --workspace --all-targets` PASS
- Receipt self-check from committed clean source state:
  - `cargo fmt --all -- --check` PASS
  - `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u4 cargo clippy --workspace --all-targets` PASS
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u4 cargo test --workspace` PASS
    - command exit code 0; output was large and tool-truncated, final visible suites and doctests passed
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u4 cargo check --workspace --no-default-features` PASS

## Demo Recheck
- Needs demo recheck after deploying the new binary:
  - Start chat with `provider=kimi-code`, `model=kimi-k2.7-code`.
  - Generate a response with reported token usage.
  - Run `/cost` and confirm the row/status no longer says `cost unknown`.
