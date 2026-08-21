# Receipt: parity E2E fixround

Date: 2026-07-05
Branch: main
Push: not pushed

## Commit

- `490fdd5d fix(chat): use redux snapshot for transcript view`

## Scope

- Fixed Ctrl+O transcript opening to prefer the reducer-owned `UiSnapshot.conversation_lines`; `chat_mirror.conversation_lines` is now only the fallback source.
- The transcript view dispatched back into Redux now carries the same snapshot-backed lines that the fullscreen renderer uses.
- Fixed attached child-session prompt labels to resolve the focused session kind from the current render source (`active_session_view` / `sessions_entries`) before falling back to `agent`.
- Updated prompt documentation from hard-coded `agent #N` to `<kind> #N`.

## Regression Tests

- `open_transcript_view_prefers_redux_snapshot_over_empty_mirror`
  - Models the real Redux-only state shape: mirror transcript is empty, while `UiSnapshot.conversation_lines` contains the conversation and tool output.
  - This test would fail before the fix because `open_transcript_view` built the transcript from `guard.conversation_lines`, producing `"(transcript is empty)"`.
- `s4_a_2_snapshot_prompt_uses_shell_kind_for_attached_session`
  - Builds a Redux `UiSnapshot` whose focused session is a shell and asserts the prompt renders `shell #1`, not `agent #1`.
- `prompt_indicator_main_vs_session`
  - Covers the direct fallback/default prompt labels and the explicit shell kind override.

## Validation

Validation was run after the code commit, before writing this receipt, with tracked worktree clean.

- Validation HEAD: `490fdd5d`
- `git status --short --untracked-files=no`: empty before validation
- `git status --short --untracked-files=no`: empty after validation
- `cargo fmt --check`: passed
- `cargo check --workspace --no-default-features`: passed
- `cargo check --workspace --all-features`: passed
- `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`: passed
- `cargo test --bin prx`: passed (`5253 passed; 0 failed; 7 ignored`)
- `git diff --check`: clean

## Notes

- Existing untracked `collab-inbox/` and older untracked receipt files were left untouched.
- No push performed.
