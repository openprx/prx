# Receipt: Visible Turns Phase 3c - task-aware usage + final aggregate dedup

- Date: 2026-07-10
- Base: `c70b100e` (P3b)
- Scope: P3c only. Worker row and `/cost` per-task UX remain follow-up work.
- Admission guard: not removed or relaxed. Existing guard remains in `src/chat/mod.rs:8328` and `src/chat/mod.rs:8438-8455`; existing guard tests remain around `src/chat/mod.rs:15024-15135`.

## Changed Files

### `src/chat/action.rs`

- `src/chat/action.rs:237-244`
  - Added `ProviderUsageRecordKind`.
  - `FinalAggregate` means one final per-turn usage aggregate produced from provider completion.
  - `Incremental` means a non-final metering segment and is intentionally not deduped by task id.
- `src/chat/action.rs:517-522`
  - Extended `Action::ProviderUsageRecorded` with:
    - `task_id: Option<TurnTaskId>`
    - `usage_kind: ProviderUsageRecordKind`
    - `record: MainSessionTokenUsageRecord`

### `src/chat/dispatcher.rs`

- `src/chat/dispatcher.rs:422-425`
  - Documented `TurnCompletionSignal::consume_turn_usage(task_id)` as the task-scoped final aggregate source whose session-level dedup is applied later through `ProviderUsageRecordKind::FinalAggregate`.

### `src/chat/mod.rs`

- `src/chat/mod.rs:6778-6785`
- `src/chat/mod.rs:6873-6880`
- `src/chat/mod.rs:7484-7491`
- `src/chat/mod.rs:7662-7669`
  - Updated all production `ProviderUsageRecorded` dispatches to pass `task_id: provider_turn_task_id` and `usage_kind: FinalAggregate`.
- `src/chat/mod.rs:9037-9051`
  - Existing `record_provider_turn_usage` scheduler ledger plumbing remains active and records usage into `TurnScheduler::record_usage(id, record)` when a task id is present.

### `src/chat/state.rs`

- `src/chat/state.rs:51-53`
  - Imported `ProviderUsageRecordKind`.
- `src/chat/state.rs:634-636`
  - Added `ControlState.final_usage_tasks_recorded: HashSet<TurnTaskId>`.
- `src/chat/state.rs:682-690`
  - Added `should_record_provider_usage`.
  - Dedup rule: only `(Some(task_id), FinalAggregate)` is idempotent by `task_id`; all `Incremental` and legacy `None` records pass through.
- `src/chat/state.rs:827-834`
  - Initialized the final aggregate dedup set.
- `src/chat/state.rs:1149-1153`
  - Routed the expanded action fields into the reducer.
- `src/chat/state.rs:2513-2517`
  - Cleared final aggregate dedup state on session load to prevent cross-session pollution.
- `src/chat/state.rs:2900-2915`
  - Applied dedup before appending `session.token_usage_records`, recomputing `ui.token_usage_summary`, and emitting `SaveSession + RequestRedraw`.
- `src/chat/state.rs:9383-9492`
  - Added 4 P3c reducer tests.

### `collab-outbox/receipt-2026-07-09-visible-turns-phase3c.md`

- This receipt.

## Dedup Key

- Final aggregate key: `(task_id, ProviderUsageRecordKind::FinalAggregate)`.
- Implementation storage: `HashSet<TurnTaskId>` because P3c has no lease id yet.
- Scope of the dedup: prevents duplicate completion/finalization paths from double-appending the same task's final provider usage.
- Explicit non-scope: `Incremental` records are never keyed only by task id, so future legal multi-segment metering is not dropped. Legacy `task_id=None` is not deduped to preserve the pre-P3 behavior.

## Test Assertions

1. `p3c_same_task_final_aggregate_is_deduped_once` (`src/chat/state.rs:9429-9440`)
   - Two `FinalAggregate` records with the same task id append only once.
   - First dispatch emits `SaveSession + RequestRedraw`; duplicate dispatch is a reducer no-op.
   - Summary total remains the first record's `10` tokens.

2. `p3c_out_of_order_final_usage_stays_on_own_task` (`src/chat/state.rs:9443-9463`)
   - Task B final usage can arrive before task A.
   - Both task totals are recorded in arrival order and summed to `55`.
   - A later duplicate B final aggregate is ignored.
   - Dedup set contains both task ids.

3. `p3c_incremental_usage_for_same_task_is_not_deduped` (`src/chat/state.rs:9466-9477`)
   - Two `Incremental` records with the same task id both append.
   - Request count is `2`, total tokens are `18`.
   - Final aggregate dedup set remains empty.

4. `p3c_legacy_usage_without_task_id_is_not_deduped` (`src/chat/state.rs:9480-9490`)
   - Two legacy `FinalAggregate` records with `task_id=None` both append.
   - Request count is `2`, total tokens are `25`.
   - Final aggregate dedup set remains empty.

## Self Check

All Cargo commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`.

```text
cargo fmt --check
PASS
```

```text
cargo clippy -p openprx --all-targets --all-features -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 2m 30s
PASS
```

```text
cargo check -p openprx --all-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.00s
PASS
```

```text
cargo check -p openprx --no-default-features
Finished `dev` profile [unoptimized + debuginfo] target(s) in 32.19s
PASS
```

```text
cargo test -p openprx --all-features p3c_task_aware_usage -- --nocapture
4 passed; 0 failed; 5453 filtered out
PASS
```

```text
git diff --check
PASS
```
