# Receipt: Stage 0.2 architecture regression guards

Date: 2026-07-12
Branch: `audit/architecture-guards`
Baseline: `34d822b1e10f838e96e0512f81a2bf4b8fad2862`

## Scope

Added an always-green source inventory guard for four accepted architecture
boundaries:

- direct `brain.db` opens outside `src/memory/sqlite.rs`;
- raw child-process spawn sites;
- modules declared by both `src/main.rs` and `src/lib.rs`;
- persisted SQL event/ledger tables.

Existing sites are explicitly baselined. A new site fails the test and requires
either routing through the accepted owner or an explicit architecture review
and allowlist update.

## Changed files

- `tests/architecture_boundaries.rs`
- `collab-outbox/receipt-2026-07-12-architecture-guards.md`

## Non-goals

- No existing bypass was migrated.
- No CI workflow was changed; the integration test is discovered by normal
  `cargo test` execution.
- No Rust parser, new dependency, runtime abstraction, or event table was
  introduced.

## Validation

- `cargo test --locked --test architecture_boundaries`
  - passed: 4 tests, 0 failures
- `cargo fmt --all -- --check`
  - passed
- `git diff --check`
  - passed
- Temporary negative probes for each of the four boundaries turned the focused
  test red as expected and were removed before handoff.

The root-agent verification reran the focused test and observed the same
`4 passed; 0 failed` result.

## Risk and rollback

- Risk: these are conservative source-text guards, not Rust AST analysis.
  Deliberately different syntax may evade a guard, while harmless changes to an
  allowlisted statement may require review.
- Rollback: revert this changeset; runtime behavior and persisted data are not
  changed.

No deployment, service restart, `prx init`, push, or PR operation was performed.
