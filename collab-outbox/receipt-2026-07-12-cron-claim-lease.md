# Receipt: Cron claim lease and recovery

Date: 2026-07-12  
Branch: `fix/cron-claim-lease`  
Worktree: `/opt/worker/wt/prx-cron-claim-lease`  
Status: implementation and local verification complete; ready for the local Step 2.2 commit; intentionally not pushed or deployed

## Delivered contract

- Cron jobs persist a complete claim tuple: worker owner, attempt ID, claim time, and expiry.
- SQLite and PostgreSQL claim the current job snapshot atomically, permit takeover at the exact expiry boundary, renew only the complete current tuple, and reject stale completion/failure writes.
- Scheduler identity is stable for the process lifetime. One heartbeat driver spans execution, delivery, and fenced commit. A rejected renewal or confirmed deadline immediately drops the in-flight workflow; a transient renewal error retries only until the last acknowledged expiry and never extends local authority.
- Recurring and one-shot completion insert run history and update job state in the same fenced transaction. Run rows retain worker/attempt identity.
- `cron.job.claimed`, `cron.job.claim_recovered`, and `cron.job.claim_lost` expose the same worker/attempt/claimed-at/expires-at payload fields. A lost event adds detection time and reason.
- CLI `cron list` and tool `list/get/status/runs` expose active claim owner, attempt, expiry, claimed count, and run attempt identity.
- Every manual run, including terminal `Schedule::At` audit reruns, acquires a dedicated claim and uses the same renewable driver. Manual recurring completion preserves its scheduled `next_run`; terminal audit finish fences owner/attempt, clears the lease, and preserves terminal state.
- Schedule changes reject every active lease, not only one-shot leases. At expiry an explicit schedule update may clear the stale tuple. Recurring finish preserves a concurrent pause unless deterministic failure explicitly disables the job.
- `scheduler.claim_lease_secs` defaults to 90 and rejects values below 3. Upgrade documentation states that old scheduler processes must be stopped before lease-aware schedulers start, that caller clocks require NTP/bounded skew, and that fencing does not make external side effects exactly-once.

## Red-first evidence

The focused tests were introduced against the Step 2.1 baseline before the implementation. They were red at compile/contract level because `CronJob` had no claim tuple, claim APIs returned a boolean without owner/TTL, run rows had no attempt identity, and expiry recovery/renew/fenced finish APIs did not exist:

- `cron::store::tests::stale_claim_can_be_recovered_after_expiry`
- `cron::store::tests::recovered_claim_fences_old_success_and_failure_without_run_rows`
- `cron::store::tests::renew_returns_updated_handle_and_old_handle_loses_authority`
- `cron::store::tests::legacy_running_without_lease_is_claimable_but_partial_tuple_fails_closed`
- `config::schema::tests::scheduler_claim_lease_defaults_and_validates`

Follow-up observability tests added during review:

- `cron::store::tests::claim_lifecycle_events_share_attempt_identity_payload`
- `tools::cron::tests::read_surfaces_expose_claim_and_attempt_identity`
- `cron::scheduler::tests::scheduler_runtime_identity_is_stable_across_poll_cycles`
- `cron::scheduler::tests::lease_driver_retries_transient_error_then_cancels_on_rejection`
- `cron::scheduler::tests::lease_driver_renews_while_workflow_is_in_delivery_phase`
- `cron::store::tests::caller_commit_time_is_not_authoritative_before_lock`
- `cron::store::tests::recurring_finish_preserves_pause_and_manual_finish_preserves_next_run`
- `cron::store::tests::renewed_manual_claim_commits_after_original_expiry_without_advancing_schedule`
- `cron::store::tests::recurring_schedule_update_rejects_active_claim_but_clears_expired_claim`

## Green evidence

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --lib cron:: --all-features` — PASS, final fourth-review rerun 132 passed.
- Focused claim lifecycle payload test — PASS, 1 passed.
- Focused tool read-surface claim identity test — PASS, 1 passed.
- Earlier focused store claim/lease set — PASS, 23 passed; scheduler module set — PASS, 115 passed; scheduler identity and config/init focused tests — PASS.
- Lease-driver cancellation/retry/delivery focused set — PASS, 2 passed, no sleeps.
- `RUSTFLAGS='-D warnings' CARGO_TARGET_DIR=/opt/worker/tmp/prx-cron-claim-lease-target TMPDIR=/opt/worker/tmp cargo check --all-targets --all-features` — PASS.
- `RUSTFLAGS='-D warnings' CARGO_TARGET_DIR=/opt/worker/tmp/prx-cron-claim-lease-nodefault-target TMPDIR=/opt/worker/tmp cargo check --no-default-features` — PASS.
- `cargo fmt --all -- --check` — PASS.
- `git diff --check` — PASS.

## Formal main-thread closure

- The first formal workspace clippy run was RED on one eager `map_or`, four redundant test clones, and test-only JSON indexing. These were mechanically corrected without changing lease semantics. The final identical command, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, passed.
- `cargo test --bin prx --all-features` — PASS, 5571 passed, 0 failed, 7 ignored.
- The first `cargo test --locked --test architecture_boundaries` run was RED because the real lock-wait test opened `brain.db` directly. The test was routed through the existing Cron repository connection helper. The helper's reviewed allowlist signature was then updated for its required mutable connection and physical line wrapping. Final architecture guard: 4 passed, 0 failed.
- `cron::store::tests::sqlite_finish_rechecks_expiry_after_waiting_for_write_lock` — PASS after the repository-helper refactor, 1 passed.
- Final `cargo fmt --all -- --check` and `git diff --check` — PASS.
- Two independent fifth-round reviews reported no High or Medium findings.

## Second-review closure

- Heartbeat scope: `run_claimed_job` places execution, configured delivery, and fenced commit inside one workflow future watched by `drive_claimed_workflow`.
- Definite loss cancellation: `Ok(None)` and the last acknowledged expiry return `LeaseDriveResult::Lost`; the caller immediately returns, dropping the workflow. The drop-canary test proves the pending workflow is dropped.
- Transient renewal errors: `Err` maps only to `RenewalAttempt::Retry`; it does not mutate the claim handle or expiry. The next tick retries while `Utc::now() < current.expires_at`; the deadline branch then cancels.
- Commit clock: caller `commit_now` is not authoritative. SQLite begins an `IMMEDIATE` transaction and samples UTC only after acquiring the write reservation; PostgreSQL executes `SELECT ... FOR UPDATE`, retains the post-lock diagnostic clock check, and also places `clock_timestamp() < claim_expires_at` inside every fenced finish `UPDATE`. A real concurrent SQLite lock test holds the writer past expiry and proves the stale finish is rejected.
- Manual execution: every schedule acquires a claim and calls the same heartbeat runner. A renewed manual recurring claim commits after its original expiry and preserves `next_run`. Terminal one-shot reruns use a terminal-only claim plus fenced `record_terminal_manual_run`; rearm is rejected while that lease is active.
- Concurrent mutation: active claims reject every schedule update; expired claims may be cleared by an explicit schedule update. Recurring finish uses `CASE WHEN disable_after ... ELSE enabled`, preserving a pause performed during execution.
- Legacy bypasses: production exports and callers of boolean `claim_job`, unfenced `record_run`, `record_last_run`, and unfenced `reschedule_after_run` were removed. Remaining legacy helpers are confined to their defining module's tests only; `record_last_run` was deleted entirely.
- Recovery events: recovered claims include `previous_worker_id`, `previous_attempt_id`, and `previous_expires_at` in addition to the new tuple.
- PostgreSQL gated parity: the env-gated test uses multiple clients and covers before/at-expiry recovery, renewal, stale-owner success and failure rejection, recurring fenced finish, partial-tuple fail-closed decoding, terminal rerun claim/audit, due-list starvation, and row-lock wait past expiry using the post-lock database clock.

## Third-review red and green evidence

- RED: `due_jobs_limit_skips_unexpired_active_claims_without_starving_ready_work` returned the active claimed first row under `max_tasks=1`, starving the ready second row. GREEN after admitting only all-null tuples or complete expired tuples before `LIMIT`.
- RED: `sqlite_finish_rechecks_expiry_after_waiting_for_write_lock` accepted a finish after waiting behind a real `IMMEDIATE` writer until the lease expired. GREEN after moving authority time inside the acquired write transaction.
- RED: `terminal_manual_rerun_requires_a_dedicated_claim_before_execution` could not acquire any claim for a terminal row. GREEN with a terminal-only claim, rearm exclusion, fenced audit finish, and lease clear.
- `terminal_rerun_claim_conflict_does_not_execute_side_effect` proves a conflicting terminal claim rejects before a `touch` command can run.
- `lost_claim_audit_runs_only_after_workflow_drop` proves the pending workflow's drop canary is set before lost-claim audit begins; the audit database path can no longer prolong the external child lifetime.

## Fourth-review closure

- Pre-fix failure evidence for manual-run ordering came from static review: both shell authorization (including v2 single-use grant consumption and gate audit) and `record_action` occurred before terminal/nonterminal claim acquisition. The first new budget test command was started before implementation, but a shared Cargo target lock delayed compilation until after the fix; no RED runtime output was captured, and this receipt does not claim otherwise.
- Manual run now acquires either the normal or terminal claim before any consuming authorization or action-budget operation. `manual_run_claim_conflict_does_not_consume_action_budget` proves a conflict leaves the only configured action available; `manual_run_claim_conflict_does_not_consume_single_use_grant` proves the same v2 one-shot grant remains usable through the real `SideEffectGate` after a conflict.
- `abandon_job_claim` clears a claim only when worker, attempt, claimed-at, and expiry all match. Authorization denial or exhausted action budget immediately invokes this fenced release. Only a successful release emits `cron.job.claim_abandoned`; stale tuples return false and emit no event. `manual_run_authorization_rejection_releases_claim_immediately` proves an authorization rejection clears the claim and permits immediate reclaim, while `abandon_claim_is_exactly_fenced_and_audited_only_on_success` covers stale-owner fencing and event cardinality.
- All three PostgreSQL fenced finish updates now include their own atomic database-clock predicate (`clock_timestamp() < claim_expires_at`) in addition to the post-lock diagnostic check. The PostgreSQL source fixture asserts the three parameterized predicates; the live gated lifecycle remains unavailable because `OPENPRX_TEST_POSTGRES_URL` is unset.
- SQLite and PostgreSQL due queries now admit only a fully empty claim tuple or a complete expired tuple before ordering and `LIMIT`. Partial tuples are isolated rather than consuming a limited slot or failing the whole due batch. The SQLite regression places a partial tuple first, an active claim second, and a ready job third under `max_tasks=1`; the PostgreSQL gated fixture mirrors that ordering.

## Fifth-review closure

- A manual claim temporarily changes a nonterminal job's `last_status` to `running`. Fenced abandon now accepts the pre-claim `last_status` snapshot and restores it in the same exact-tuple SQLite/PostgreSQL update instead of replacing historical `ok`/`error` with `NULL`. Terminal rerun abandon explicitly restores the terminal row's original status through the same parameterized path.
- `abandon_claim_is_exactly_fenced_and_audited_only_on_success` now seeds `last_run`, `last_status=ok`, and `last_output`, proves a stale abandon changes neither the active claim nor any historical field, and proves the authoritative abandon restores the complete snapshot.
- `manual_run_authorization_rejection_releases_claim_immediately` now creates a real successful run before changing the command to an approval-requiring command. After the post-claim authorization rejection, `last_run`, `last_status`, and `last_output` are byte-for-byte unchanged and the job is immediately reclaimable.
- The terminal rerun claim test now abandons an active terminal claim, verifies terminal state plus all three history fields are preserved, and immediately reclaims before completing the audit rerun. The PostgreSQL source fixture asserts that abandon restores the supplied status parameter.
- No deterministic post-claim action-budget rejection test was added: pre-consuming the existing policy budget would be rejected by the non-consuming `is_rate_limited` check before claim acquisition, while forcing the narrow race between that check and `record_action` would require a test hook or nondeterministic concurrency. The production budget-failure branch passes the same pre-claim snapshot to the already-covered fenced abandon API.

## Explicit verification boundaries

- `OPENPRX_TEST_POSTGRES_URL` is not set in this environment. The PostgreSQL lifecycle test therefore returned early by design; this receipt does **not** claim a live PostgreSQL E2E run. PostgreSQL schema/projection fixture coverage and all-feature compilation passed.
- `cargo check --all-targets --no-default-features` remains red on pre-existing feature-gating defects outside this changeset: `benches/router_decision.rs` imports `router::automix` without `llm-router`, and a `src/chat/state.rs` test references `chat::tui` without `terminal-tui`. The production no-default build above is green.
- No scheduler process was externally killed during this handoff. The deterministic store test models process death by abandoning the first claim, proves no takeover before expiry, takeover at exact expiry, and stale success/failure fencing after recovery. Workflow cancellation itself is verified with a pending future and drop canary without sleeps.

## Files changed

- `docs/tools.md`
- `src/config/init.rs`
- `src/config/schema.rs`
- `src/cron/mod.rs`
- `src/cron/postgres.rs`
- `src/cron/scheduler.rs`
- `src/cron/store.rs`
- `src/cron/types.rs`
- `src/self_system/fitness.rs`
- `src/tools/cron.rs`
- `tests/architecture_boundaries.rs`
- `collab-outbox/receipt-2026-07-12-cron-claim-lease.md`

## Handoff boundary

This receipt is included in the local Step 2.2 commit. No push, deployment, service restart, active configuration mutation, or live runtime acceptance was performed.
