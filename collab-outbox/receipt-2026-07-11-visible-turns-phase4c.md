# Receipt 2026-07-11 - Visible Turns Phase 4c UX

Baseline: `f88ec4d6` clean. No commit made.

## Scope

Implemented only the three P4c UX items. I did not change P4b-2 admission, ordered commit, history commit, or livelock logic.

## Changes

1. `/queue` now reports queued, priority queued, and running turns from `TurnScheduler::status()`.
   - Main-loop `/queue` call passes the scheduler: `src/chat/mod.rs:4965`.
   - Active-turn local `/queue` also passes the scheduler: `src/chat/mod.rs:10233`.
   - Report header is now `Main queue: {queued} queued ({priority} priority), {running} running.`: `src/chat/mod.rs:10610`.
   - Existing queue preview behavior is unchanged.

2. `/workers cancel w#N` can target any retained running/cancelling provider worker, including detached workers.
   - Added targeted reducer action `Action::CancelProviderTurn { task_id }`: `src/chat/action.rs:573`, `src/chat/action.rs:650`.
   - Added reducer path that cancels only the visible draft/token for the requested `TurnTaskId`: `src/chat/state.rs:1167`, `src/chat/state.rs:2369`, `src/chat/state.rs:3398`.
   - Removed the old "current foreground only" guard in the shared cancel helper and kept the not-found/terminal-state guards: `src/chat/mod.rs:10421`.
   - Added outer-loop `/workers` handling so bare `/workers cancel w#N` works while detached turns are running instead of falling through to Unknown command: `src/chat/mod.rs:4970`.
   - Foreground awaited active-turn cancel still maps to `CancelRequested`; detached/global cancel maps to `CancelProviderTurn`: `src/chat/mod.rs:10479`, `src/chat/mod.rs:10489`.

3. Priority dispatch was already aligned; added a regression test for the N=2 detached-slot case.
   - Test proves one detached worker running plus backlog `[normal, priority]` pops the priority turn first: `src/chat/mod.rs:16976`.

## Tests Added/Updated

- Updated queue report assertions:
  - `queue_report_summarizes_backlog_with_preview`
  - `active_turn_queue_command_is_handled_without_enqueue`
  - `p4a_active_turn_event_pump_preserves_cancel_local_and_enqueue_order`
- Updated current foreground worker cancel signal assertion:
  - `active_turn_workers_cancel_command_marks_current_worker_cancelling_without_enqueue`
- Added detached/global cancel coverage:
  - `workers_cancel_command_targets_detached_worker_without_cancelling_peer`: `src/chat/mod.rs:15951`
  - `workers_cancel_command_targets_detached_worker_from_outer_loop`: `src/chat/mod.rs:16009`
  - `p4c_cancel_provider_turn_targets_requested_task_token_only`: `src/chat/state.rs:9397`
- Added priority dispatch coverage:
  - `visible_input_pop_prefers_priority_when_detached_slot_is_available`: `src/chat/mod.rs:16976`

## Mutation Proof

- Change 2 mutation: changed detached worker cancel to emit `CancelRequested` instead of `CancelProviderTurn { task_id }`.
  - `workers_cancel_command_targets_detached_worker_without_cancelling_peer` failed:
    - left: `Some(CancelRequested)`
    - right: `Some(CancelProviderTurn { task_id: TurnTaskId(2) })`
- Change 3 mutation: changed queued-input pop selection to ignore priority once a normal item had been seen.
  - `visible_input_pop_prefers_priority_when_detached_slot_is_available` failed:
    - left: `"normal queued first"`
    - right: `"urgent queued second"`

## Validation

All commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`.

- `cargo fmt --check`: passed.
- `cargo clippy -p openprx --all-targets --all-features -- -D warnings`: passed, zero warnings.
- `cargo check -p openprx --all-features`: passed.
- `cargo check -p openprx --no-default-features`: passed.
- `cargo test -p openprx --bin prx --all-features`: passed.
  - Result: `5473 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out`.
- `cargo build -p openprx --bin prx --all-features`: passed for tmux validation binary.

## PTY / Tmux Validation

Ran current debug binary in tmux:

```sh
tmux new-session -d -s p4c-demo -x 150 -y 42 \
  "cd /opt/worker/code/prx && env PRX_TUI=1 OPENPRX_MOCK_RESPONSE='P4C_DONE' \
   OPENPRX_MOCK_DELAY_MS_BY_PROMPT='A=30000;B=30000' \
   /opt/worker/tmp/prx-target/debug/prx chat -p mock --model mock"
```

Before cancel, two detached workers were running:

```text
Main provider workers: 2 running, 0 cancelling, 0 awaiting commit, 0 finalized payloads, 0 finalized tokens.
- w#1 task=1 kind=detached state=running completion=pending elapsed=1s
- w#2 task=2 kind=detached state=running completion=pending elapsed=0s
```

`/queue` while both were running:

```text
Main queue: 0 queued (0 priority), 2 running.
Queue is empty.
```

`/workers cancel w#2`:

```text
Requested cancellation for provider worker w#2 task=2 kind=detached state=cancelling.

(cancelled)
```

Follow-up `/workers` showed w#1 still running and w#2 no longer running:

```text
Main provider workers: 1 running, 0 cancelling, 1 awaiting commit, 0 finalized payloads, 0 finalized tokens.
- w#1 task=1 kind=detached state=running completion=pending elapsed=10s
- w#2 task=2 kind=detached state=awaiting_commit completion=ready elapsed=9s
```

The tmux session `p4c-demo` was stopped after validation.

## Worktree

Tracked modified files:

- `src/chat/action.rs`
- `src/chat/mod.rs`
- `src/chat/state.rs`
- `collab-outbox/receipt-2026-07-11-visible-turns-phase4c.md`

Existing untracked `collab-inbox/`, older receipts, and `task/` entries were left untouched.
