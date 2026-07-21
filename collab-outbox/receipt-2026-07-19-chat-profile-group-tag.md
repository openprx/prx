# Receipt: chat profile group tag fix (2026-07-19)

## Scope

Executed the authorized P0→P1→P2 fix-round without pushing to GitHub.

## Changes

- P0: removed model-visible `trusted runtime` wording from
  `src/tools/chat_profile_update.rs`; retained `_zc_scope_trusted` validation
  and model-supplied target rejection.
- P1: added registered-tool-only fallback parsing for compatible-provider
  `<function=NAME>{json}</function>` content when structured calls are absent.
- P1: clarified that the current conversation target is automatic and only
  purpose/notes/tags should be supplied.
- P2: preserved and revalidated the direct execution path; the full suite
  covers scope injection and profile persistence paths, while live group
  acceptance remains the main-session deployment gate.
- Updated existing architecture/security regression expectations to match the
  already-authorized unrestricted shell/http posture.

## Verification before commit

- `cargo fmt --all -- --check`: pass.
- `cargo check --workspace --no-default-features`: pass.
- `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`: pass.
- `cargo test --workspace`: pass; 5703 library tests passed, 6 ignored, and
  all integration/doc tests passed.

## Delivery

- Version bumped to `0.8.19`.
- Commit is local only; GitHub push is intentionally prohibited by the
  work-order rules.
