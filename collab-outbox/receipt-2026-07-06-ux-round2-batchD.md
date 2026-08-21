# Receipt: UX round2 Batch D

Scope: Batch D message_send default recipient/channel race.

Commit:
- `4a80c4d8 fix(tools): scope message_send defaults per turn`

Summary:
- Added `MESSAGE_SEND_EXECUTION_CONTEXT` task-local routing context for `message_send`.
- `message_send` now resolves implicit target/channel from per-turn task-local context before falling back to legacy tool slots.
- Channel/gateway and chat legacy/Redux turn execution now scope `message_send` defaults at the real tool execution site.
- Stopped per-turn global active-recipient/channel updates for `message_send`; legacy fallback remains for non-turn contexts.

Regression tests:
- `tools::message_send::tests::task_local_context_beats_mutated_fallback_defaults`
- `tools::message_send::tests::fallback_default_still_works_without_turn_context`

Validation:
- `git diff --quiet && git diff --cached --quiet` -> PASS
- `cargo fmt --all -- --check` -> PASS
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-uxr2-batchd cargo clippy --workspace --all-targets` -> PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-uxr2-batchd cargo test --bin prx` -> PASS (`5281 passed; 0 failed; 7 ignored`)
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-uxr2-batchd cargo check --workspace --no-default-features` -> PASS

Notes:
- The strengthened four-gate receipt check was run after the mandatory post-D Batch A/B fix-round cleared pre-existing clippy violations.
- Binary/test artifact: `/opt/worker/tmp/prx-uxr2-batchd/debug/deps/prx-ffc3817af57d929f`
- Push: not performed.
