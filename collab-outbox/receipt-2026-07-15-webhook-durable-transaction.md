# Receipt: Step 2.5 Standalone webhook durable transaction

Date: 2026-07-15
Branch: `fix/webhook-durable-transaction`
Worktree: `/opt/worker/wt/prx-webhook-durable-transaction`
Baseline: `677d98eb80b721101b2c1a703131c3a332c04b28`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Replaced the standalone webhook's process-local timestamp idempotency cache
  with durable `pending`, `committed`, and `failed` ingestion records.
- Added generation-fenced claims with a 30-second lease. Failed attempts and
  expired pending claims may be retried; a live claim reports `processing`.
- Bound each claim to the scoped idempotency identity, normalized external
  event identity, and exact request hash. Same-key/different-body requests
  return `409 request_conflict`; committed retries replay the durable topic ID.
- Limited raw idempotency keys to 256 bytes and stored only a token-scoped
  SHA-256 digest. Raw keys are neither persisted nor logged.
- Committed topic creation/update, participant membership, eligible memory,
  memory-fabric outbox event, and final ingestion state in one SQLite
  transaction. A failed transaction rolls back all domain writes before the
  exact claim is marked failed.
- Added an injected `WebhookRepository` boundary. The request handler no longer
  opens a database or constructs its own memory backend.
- Wired daemon startup through the authoritative configuration, including the
  optional HMAC-SHA256 signing secret.
- Standalone durable ingestion accepts configured `sqlite` and `lucid`
  backends. Unsupported backends fail closed instead of silently writing a
  separate local `brain.db`.
- Documented authentication, backend support, durable state, and atomic write
  behavior in `docs/configuration.md`.

## Red-first evidence preserved

Before implementation,
`failed_ingestion_rolls_back_all_state_and_same_key_retries` failed after the
first injected memory-write error: the test expected zero topics but observed
one. This proved that the old flow stranded its topic/participant writes and
also consumed the in-memory idempotency key. The same test now proves rollback,
durable failed state, same-key retry, and a single final topic/memory/outbox.

Focused coverage also proves restart replay without duplicate rows,
same-key/different-body conflict, pending lease behavior and reclamation,
configured-backend fail-closed behavior, and configured HMAC enforcement.

## Local verification

Commands used the shared isolated target directory
`/opt/worker/tmp/prx-process-parity-target` and `TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed in 16.25s on the final tree.
- `cargo test -p openprx --bin prx --all-features 'webhook::tests' -- --nocapture`
  - 16 passed, 0 failed, 0 ignored, 5,615 filtered out.
- `cargo test -p openprx --bin prx --all-features memory_webhook_config_signing_secret_roundtrip -- --nocapture`
  - 1 passed, 0 failed, 5,630 filtered out.
- `git diff --check` - passed.

One intermediate compile failed because this test module aliases `#[test]` to
Tokio's async test attribute and the new configuration round-trip test was
initially synchronous. The test was changed to `async fn`; production code was
unchanged, and the final focused run passed.

Per `verification-policy.md`, strict clippy, full binary/workspace suites,
architecture guards, dependency/security audits, independent review, and a
release build were not run. They remain GitHub delivery gates, not local Step
2.5 gates.

## Scope and rollback

- Source and documentation scope: `src/webhook/mod.rs`,
  `src/config/schema.rs`, `src/daemon/mod.rs`, and `docs/configuration.md`.
- Final source/documentation diff: 992 insertions, 239 deletions.
- Final pre-receipt diff SHA-256:
  `9580fe6b424673e563c83d5c8cd24c375cfeae958e913eaff110088d78ccce42`.
- Rollback: revert the local Step 2.5 commit before Step 3.1 is based on it.
- No GitHub action, push, merge, binary install, service restart, active
  configuration mutation, live database mutation, or runtime activation was
  performed.
