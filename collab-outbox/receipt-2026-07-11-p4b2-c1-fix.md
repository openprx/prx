# Receipt 2026-07-11 — P4b-2 C1 debug assert fix

## Scope

- Baseline: current uncommitted P4b-2 working tree on `bc75fad9`.
- Commit: not made.
- Files changed for this C1 pass:
  - `src/chat/mod.rs`

## C1 Fix

- `src/chat/mod.rs:6741-6759`
  - Removed the contradictory `debug_assert!` from the post-route admission rejection branch.
  - The branch is valid when a turn was popped under detached admission, then routes to `ForegroundAwaited` while another detached worker is active.
  - The remaining `tracing::warn!` records `active_workers`, `foreground_active`, `detached_active`, `effective_max_visible_turns`, and `target_kind`.

- `src/chat/mod.rs:8681-8692`
  - Extracted `requeue_post_route_admission_rejected_input`.
  - It preserves the popped input priority, task id, and message while requeueing at the front, then sets the one-shot event-pump defer flag.
  - Runtime behavior is unchanged from the existing P4b-2 B4 fix; the helper exists so the real post-route requeue/defer transition is directly testable.

## T1 Test

- `src/chat/mod.rs:16853-16919`
  - Added `post_route_admission_rejection_requeues_input_and_defers_next_pop`.
  - Scenario:
    - one detached worker is already active;
    - a second visible input pops under detached N=2 admission before its route is known;
    - the route is then treated as `ForegroundAwaited`, whose post-route admission is rejected while the detached worker is active;
    - the helper requeues the popped input and sets the one-shot defer;
    - the next pump pass consumes the defer instead of immediately re-popping under detached capacity.
  - This covers the actual state transition missed by the earlier manual-flag-only test.

## Mutation Proof

- Temporarily changed `requeue_post_route_admission_rejected_input` from `*defer_visible_input_pop_once = true` to `false`.
- Command:
  - `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features post_route_admission_rejection_requeues_input_and_defers_next_pop -- --nocapture`
- Result:
  - Failed as expected.
  - Failure assertion: `post-route rejection must force one event-pump wait before the same input can pop again`.
- Restored correct code and reran the targeted test successfully.

## Validation

- `cargo fmt --check` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features post_route_admission_rejection_requeues_input_and_defers_next_pop -- --nocapture` — passed: `1 passed; 0 failed; 5475 filtered out`.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy -p openprx --all-targets --all-features -- -D warnings` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --all-features` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features` — passed: `5469 passed; 0 failed; 7 ignored`.

## Notes

- No git commit was made.
- P4b-2 admission/history/ordered-commit logic outside this C1 cleanup was not intentionally changed.
