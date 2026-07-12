# Chat dirty-worktree preservation baseline

Date: 2026-07-12  
Branch/worktree: `fix/chat-persistence-sanitization` / `/opt/worker/wt/prx-chat-persistence-sanitization`

## Preservation boundary

The main worktree already contained an uncommitted five-file Chat UX changeset.
Its tracked binary diff SHA-256 was:

`a00d8b2a3a253c58d6bbebd71e04f9efb87f078041342a78963d4cc1f3c7e6cf`

The diff was copied exactly into this isolated worktree after PR-1. The main
worktree was not reset, stashed, edited, or committed.

Preserved files:

- `src/chat/action.rs`
- `src/chat/commands.rs`
- `src/chat/mod.rs`
- `src/chat/state.rs`
- `src/chat/tui.rs`

The preserved work includes transcript mouse scrolling/drag selection and
clipboard copy, help/footer updates, immediate TUI quit handling, folded tool
result defaults, and last-turn duration display. This baseline commit is kept
separate from the Step 1.2 persistence-sanitization implementation.

## Baseline correction

The first full bin test run produced `5522 passed, 4 failed, 7 ignored`. All
four failures were stale terminal-guard expectations that still treated mouse
capture as opt-in, while the preserved implementation and help text enable it
by default. The tests now cover both the default-enabled lifecycle and the
explicit disabled path, including rollback ordering.

Strict clippy also requested two mechanical `const fn` annotations in the
preserved TUI helpers. No behavior changed for those fixes.

## Validation

All Cargo commands used:

`CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`

- `cargo check --all-features`: passed.
- `cargo check --no-default-features`: passed.
- `cargo clippy --all-targets --all-features -- -D warnings`: passed after the
  test-only architecture guard declared the same local lint exceptions used by
  other integration tests and the two const annotations were applied.
- `cargo test --bin prx --all-features`: `5526 passed, 0 failed, 7 ignored`.
- Focused terminal guard tests: `6 passed`.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed.

No push, PR, deploy, service restart, version bump, or `prx init` was performed.
