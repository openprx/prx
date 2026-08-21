# Receipt: parity no-default-features fixround

Date: 2026-07-05
Branch: main
Push: not pushed

## Commit

- `7dbf5f80 fix(chat): move slash catalog types out of tui`

## Scope

- Moved `SlashProviderModelCatalog` and `SlashModelCandidate` into non-gated `chat::slash_types`.
- Added `chat::tui` re-export so existing TUI-side paths remain available when `terminal-tui` is enabled.
- Updated non-gated `Action::SlashMenuSourcesUpdated` and `UiState` slash catalog references to use `chat::slash_types`.
- Left TUI-only and terminal-TUI-gated references on the re-exported `chat::tui` path.

## Validation

Validation was run after the code commit, before writing this receipt, with tracked worktree clean.

- Validation HEAD: `7dbf5f80`
- `git status --short --untracked-files=no`: empty before validation
- `git status --short --untracked-files=no`: empty after validation
- `cargo fmt --check`: passed
- `cargo check --workspace --no-default-features`: passed
- `cargo check --workspace --all-features`: passed
- `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`: passed
- `cargo test --bin prx`: passed (`5251 passed; 0 failed; 7 ignored`)
- `git diff --check`: clean

## Notes

- Existing untracked `collab-inbox/` and older untracked receipt files were left untouched.
- No push performed.
