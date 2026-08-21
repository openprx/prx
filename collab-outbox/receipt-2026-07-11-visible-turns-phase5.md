# Receipt 2026-07-11 - Visible Turns Phase 5 Ordered Verify

Baseline: `d99e92ee` clean. No commit made.

## Scope

P5 was validation hardening only. I did not change P4b-2/P4c production logic, admission, ordered commit, finalizer, or daemon/systemd behavior.

## Code Changes

- Added test-only helpers in `src/chat/history_commit.rs` tests for started task setup, registration, terminal outcome recording, and decision sequence extraction.
- Added coordinator N>=3/N=4 ordered-release tests:
  - `three_task_out_of_order_completions_release_only_contiguous_prefix`: `src/chat/history_commit.rs:397`
  - `three_task_middle_cancel_unblocks_later_commit_in_sequence`: `src/chat/history_commit.rs:465`
  - `three_task_first_failure_skips_then_releases_later_commits`: `src/chat/history_commit.rs:524`
  - `four_task_mixed_outcomes_release_only_ordered_ready_prefixes`: `src/chat/history_commit.rs:579`
- Added finalizer-level N=3 ordered persistence test:
  - `provider_turn_finalizer_n3_out_of_order_releases_ordered_persistence_actions`: `src/chat/mod.rs:17954`

## Assertions Covered

- Coordinator pure out-of-order: completion order 3 -> 1 -> 2 releases only the minimum contiguous ready prefix at each drain.
- Coordinator middle cancel: completion order 3 -> 2(cancel) -> 1 releases `[Commit1, Skip2(Cancelled), Commit3]`, preserving rollback and commit lengths.
- Coordinator first fail: later completed outcomes remain held until `[Skip1(Failed), Commit2, Commit3]` can be released in sequence.
- Coordinator N=4 mixed: complete/cancel/fail/complete outcomes arrive out of order, release only ordered prefixes, and end with `pending_tasks()==0` / `pending_outcomes()==0`.
- Finalizer N=3: task 3 and task 2 complete before task 1, stay held as `finalized=false`, then task 1 releases ordered results `[1,2,3]`. Ordered dispatch emits exactly:
  - `RecordUserTurn(alpha)`, `RecordAssistantTurn(alpha)`, `StreamCompleted(alpha)`
  - `RecordUserTurn(bravo)`, `RecordAssistantTurn(bravo)`, `StreamCompleted(bravo)`
  - `RecordUserTurn(charlie)`, `RecordAssistantTurn(charlie)`, `StreamCompleted(charlie)`

## Mutation Proof

Temporary mutation:

```rust
while let Some(sequence) = self.outcomes_by_sequence.keys().next().copied() {
```

instead of the real ordered gate:

```rust
while let Some(sequence) = self.pending_order.iter().next().copied() {
```

Results:

- `three_task_out_of_order_completions_release_only_contiguous_prefix` failed with:
  - `third cannot release before first and second`
- `provider_turn_finalizer_n3_out_of_order_releases_ordered_persistence_actions` failed with:
  - left: task 3 `terminal_status: "completed", finalized: true`
  - right: task 3 `terminal_status: "unknown", finalized: false`

Mutation was reverted before final validation.

## Validation

All commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`.

- `cargo fmt --check`: passed.
- `cargo clippy -p openprx --all-targets --all-features -- -D warnings`: passed, zero warnings.
- `cargo check -p openprx --all-features`: passed.
- `cargo check -p openprx --no-default-features`: passed.
- `cargo test -p openprx --bin prx --all-features`: passed.
  - Result: `5478 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out`.
- `cargo build -p openprx --bin prx --all-features`: passed for the demo binary.

## PTY / Tmux Demo

No production daemon or systemd state was touched. Demo used:

- temporary config dir: `/opt/worker/tmp/prx-p5-config`
- temporary home: `/opt/worker/tmp/prx-p5-home`
- debug binary: `/opt/worker/tmp/prx-target/debug/prx`
- temp config override:

```toml
[chat]
max_concurrent_visible_turns = 3
```

Command:

```sh
tmux new-session -d -s p5-demo -x 160 -y 46 \
  "cd /opt/worker/code/prx && env HOME=/opt/worker/tmp/prx-p5-home PRX_TUI=1 \
   OPENPRX_MOCK_RESPONSE='P5_ORDERED_REPLY' \
   OPENPRX_MOCK_DELAY_MS_BY_PROMPT='alpha=15000;bravo=3000;charlie=8000' \
   /opt/worker/tmp/prx-target/debug/prx --config-dir /opt/worker/tmp/prx-p5-config chat -p mock --model mock"
```

Sent prompts in dispatch order:

1. `alpha P5 dispatch first`
2. `bravo P5 dispatch second`
3. `charlie P5 dispatch third`

The delay rule makes provider completion order differ from dispatch order: bravo -> charlie -> alpha.

Concurrent worker evidence:

```text
PRX | mode:edit auth:supervised | workers:3 welapsed:4s w#1:detached:run:4s w#2:detached:run:4s +1w
  w#1 worker provider running - main provider w#1 detached task=1
  w#2 worker provider running - main provider w#2 detached task=2
  w#3 worker provider running - main provider w#3 detached task=3
```

After completion:

```text
Main provider workers: 0 running, 0 cancelling, 0 awaiting commit, 3 finalized payloads, 12 finalized tokens.
No active worker rows are currently retained.
```

Exported JSON transcript from `/export json` and checked:

```text
turns=6 roles=user,assistant,user,assistant,user,assistant
user:alpha P5 dispatch first
assistant:P5_ORDERED_REPLY
user:bravo P5 dispatch second
assistant:P5_ORDERED_REPLY
user:charlie P5 dispatch third
assistant:P5_ORDERED_REPLY
```

The exported file was removed after extracting evidence. The tmux session `p5-demo` was stopped.

## Worktree

Tracked modified files:

- `src/chat/history_commit.rs`
- `src/chat/mod.rs`
- `collab-outbox/receipt-2026-07-11-visible-turns-phase5.md`

Existing untracked `collab-inbox/`, older receipts, and `task/` entries were left untouched.
