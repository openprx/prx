# Receipt: Step 2.4 Gateway idempotency lifecycle

Date: 2026-07-15
Branch: `fix/gateway-idempotency-lifecycle`
Worktree: `/opt/worker/wt/prx-gateway-idempotency-lifecycle`
Baseline: `1e1a8cbb2e23b48b2d07298a84061d66f79f8933`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Replaced timestamp-only idempotency entries with explicit `Processing`,
  `Succeeded`, and `Failed` states.
- Added monotonically increasing attempt generations and a non-clone RAII
  claim. Only the exact owner may commit success/failure; cancellation or future
  drop marks the exact attempt retryable without corrupting a newer generation.
- Only successful requests replay. Concurrent work and same-key/different-body
  requests return `409`; failed/cancelled requests may retry; exhausted key or
  replay capacity returns `503`.
- Successful replay preserves response, model, and stable response identity
  without a second provider call. Responses over 1 MiB retain a success
  tombstone and result hash, return `replay_unavailable`, and are never
  re-executed.
- Terminal TTL starts at completion/failure. Live processing and unexpired
  terminal entries are never evicted. In-flight/replay payload is bounded by a
  32 MiB process budget.
- Idempotency keys are limited to 256 bytes, scoped to the authenticated webhook
  identity, and SHA-256 hashed before storage, event propagation, or autosave.
  Raw keys are not logged or persisted.
- Request fingerprints bind each key to the exact request body. Idempotent
  retries reuse the same autosave and MessageEvent identities.
- The contract remains in-memory and at-least-once after an execution error;
  Step 2.5 still owns durable standalone-webhook ingestion transactions.

## Red-first evidence preserved

Before handler integration, the four recorded tests failed for the expected
baseline defects:

1. provider failure poisoned the key and blocked retry;
2. task cancellation poisoned the key and blocked retry;
3. concurrent work returned a false `200 duplicate` instead of Processing;
4. successful duplicate handling had no stable response identity or replay.

Those assertions were retained and are green after the implementation.

## Local verification

Commands used the shared isolated target directory
`/opt/worker/tmp/prx-process-parity-target` and `TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` — passed.
- `cargo fmt --all -- --check` — passed.
- `cargo check -p openprx --all-features` — passed in 19.15s on the final tree.
- `cargo test -p openprx --bin prx --all-features idempotency -- --nocapture`
  — 21 passed, 0 failed, 0 ignored, 5,603 filtered out. This includes the four
  red-first regressions plus lifecycle, TTL, ABA, conflict, capacity, payload
  budget, oversize tombstone, key hashing/limit, and autosave retry coverage.
- `cargo test -p openprx --bin prx --all-features webhook_records_gateway_message_events -- --nocapture`
  — 1 passed, 0 failed, 5,623 filtered out; hashed event idempotency identity is
  verified and the raw external key is absent.
- `cargo test -p openprx --bin prx --all-features webhook_memory_key -- --nocapture`
  — 2 passed, 0 failed, 5,622 filtered out; non-idempotent keys remain unique
  while retries use a stable digest key.
- `git diff --check` — passed.

An initial focused run executed 21 tests with 20 passing and one new autosave
test failing because its fixture was shorter than the existing 30-character
autosave threshold. The production implementation was unchanged; the fixture
was corrected to exercise the real autosave branch, and the same 21-test filter
then passed completely.

Per `verification-policy.md`, strict clippy, the full binary/workspace suites,
architecture guards, dependency/security audits, independent review, and
release build were not run. They are GitHub delivery gates, not local Step 2.4
gates.

## Scope and rollback

- Source scope: `src/gateway/mod.rs` only.
- Final pre-commit diff: 997 insertions, 89 deletions.
- Final pre-commit diff SHA-256:
  `334ec96a65669e16d2fb531ec91ace2060edf8155307e87470946c47de4e8f24`.
- Rollback: revert the local Step 2.4 commit before Step 2.5 is based on it.
- No GitHub action, binary install, service restart, active configuration
  mutation, database mutation, or live runtime acceptance was performed.
