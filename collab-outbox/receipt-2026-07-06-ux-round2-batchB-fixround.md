# Receipt: UX round2 Batch A+B fix-round

Scope: O1 num_ctx sentinel/cap, I2 paste chip integrity, I3 clippy gate.

Commits:
- `227ece0c fix(provider): cap ollama num_ctx fallback`
- `7d1bab32 fix(chat): make paste chips atomic`
- `d26b1819 fix(chat): clear input clippy violations`
- `813f8ef7 fix(chat): preserve slash selection on path refresh`

Summary:
- O1: router default `max_context = 1_000_000` sentinel is excluded from Ollama `num_ctx` fallback; large requested `num_ctx` values are clamped to `65536`.
- I2: folded paste chips now use private internal tokens plus structured expansion/rendering; Backspace/Delete/left/right treat chips atomically and user-visible placeholders are no longer editable payload text.
- I3: removed `indexing_slicing` violations in input cursor rendering and at-path test, plus clippy all-targets violations in session idle helpers.
- I1 addendum: unchanged slash/@path candidate refreshes now preserve mirror `selected`, and the symlink security test keeps the outside tempdir alive so resolved-path policy is actually exercised.

Mutation/semantic tests:
- `providers::ollama::tests::request_num_ctx_ignores_router_default_sentinel`
- `providers::ollama::tests::request_num_ctx_clamps_large_router_context_to_safe_cap`
- `providers::ollama::tests::request_num_ctx_clamps_large_explicit_config_to_safe_cap`
- `chat::tui::tests::folded_paste_backspace_removes_whole_chip_without_placeholder_leak`
- `chat::tui::tests::folded_paste_cursor_inside_chip_cannot_insert_into_chip`
- `chat::tui::tests::small_paste_matching_chip_placeholder_is_not_expanded`
- `chat::tui::tests::slash_menu_refresh_preserves_selected_command_row`
- `chat::tui::tests::at_path_refresh_preserves_selected_candidate_for_enter`
- `chat::file_mention_tests::at_path_candidates_are_relative_sorted_and_security_filtered`

Validation:
- `git diff --quiet && git diff --cached --quiet` -> PASS
- `cargo fmt --all -- --check` -> PASS
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-uxr2-batchd cargo clippy --workspace --all-targets` -> PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-uxr2-batchd cargo test --bin prx` -> PASS (`5281 passed; 0 failed; 7 ignored`)
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-uxr2-batchd cargo check --workspace --no-default-features` -> PASS

Binary/test artifact:
- `/opt/worker/tmp/prx-uxr2-batchd/debug/deps/prx-ffc3817af57d929f`

I1 addendum validation:
- Isolated pre-commit state: `git stash push --keep-index` kept only I1 staged diff before commit.
- `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batche cargo clippy --workspace --all-targets` -> PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batche cargo test --bin prx slash_menu_refresh_preserves_selected_command_row -- --nocapture` -> PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batche cargo test --bin prx at_path_refresh_preserves_selected_candidate_for_enter -- --nocapture` -> PASS
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-awareness-batche cargo test --bin prx at_path_candidates_are_relative_sorted_and_security_filtered -- --nocapture` -> PASS

Push: not performed.
