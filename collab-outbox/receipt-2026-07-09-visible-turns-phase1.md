# Visible Turns Phase 1 Receipt - 2026-07-09

## Result

Phase 1 implemented: reducer stream state now supports multiple keyed visible drafts while keeping the live visible admission guard in place. This is structural only; it does not claim user-visible concurrent transcript rendering yet.

Base: `75c7823d wip(chat): worker/scheduler substrate baseline`

## Changed Files and Lines

- `src/chat/action.rs:274-282`
  - Added `provider_turn_sequence: Option<u64>` to `Action::StartLLMTurn`.

- `src/chat/mod.rs:6488-6493`
  - Live Redux-driver dispatch now passes the scheduler sequence from `TurnScheduler` into `StartLLMTurn`.
  - The visible admission guard remains active at `src/chat/mod.rs:4063`, `src/chat/mod.rs:4086`, `src/chat/mod.rs:8297-8302`, and `src/chat/mod.rs:8412-8429`.

- `src/chat/state.rs:458-535`
  - Added `StreamingTurnDraft { task_id, sequence, prompt_preview, draft }`.
  - Replaced the old single `StreamState.draft` source with `StreamState.visible_drafts: Vec<StreamingTurnDraft>`.
  - Added `primary_draft()`, `primary_streaming_draft()`, keyed insert/mutate/remove, clear, non-empty, and version fingerprint helpers.

- `src/chat/state.rs:577-582`, `src/chat/state.rs:776-780`
  - Snapshot dirty fingerprint now uses all visible draft `(draft_id, version)` pairs, so non-primary stream changes invalidate snapshots.

- `src/chat/state.rs:643-645`, `src/chat/state.rs:697-718`
  - `ChatState::new` initializes `visible_drafts`.
  - `UiSnapshot.streaming` remains compatibility-only and is computed from `primary_streaming_draft()`.

- `src/chat/state.rs:870-882`, `src/chat/state.rs:1385-1415`
  - `StartLLMTurn` reducer path accepts task sequence and builds the keyed draft with prompt preview.

- `src/chat/state.rs:1420-1540`
  - `TurnStarted` and both TUI/non-TUI `StartLLMTurn` paths insert keyed visible drafts instead of writing an old draft slot.

- `src/chat/state.rs:1549-1577`
  - Stream chunks route by `draft_id`; stale/missing draft chunks are ignored.

- `src/chat/state.rs:1595-1676`
  - Completion removes only the matching draft and clears global generating/tool buffers only when no visible drafts remain.

- `src/chat/state.rs:1684-1748`
  - Failed/cancelled terminal paths remove only the matching draft and leave other visible drafts active.

- `src/chat/state.rs:6537-6727`
  - Added seven Phase 1 reducer tests.

- `src/chat/dispatcher.rs:7102-7140`, `src/chat/dispatcher.rs:7149-7155`
  - Updated dispatcher tests for the new `StartLLMTurn` field and primary-draft compatibility accessor.

- `src/chat/tui.rs:12903`, `src/chat/tui.rs:12933`, `src/chat/tui.rs:12992`, `src/chat/tui.rs:13206`
  - Updated TUI parity fixtures to read compatibility streaming from `primary_streaming_draft()`.

- `collab-outbox/receipt-2026-07-09-visible-provider-turns-scope-audit.md:177-210`
  - Added product gate, Phase 2 scroll-anchor acceptance, Phase 3 per-task cost acceptance, and Phase 3 cancel/tool-card isolation acceptance.

## Test Assertions Added

- `phase1_two_visible_drafts_start_without_overwriting`
  - Starts `draft-b` then `draft-a`.
  - Asserts visible order is scheduler sequence order: `["draft-a", "draft-b"]`.
  - Asserts `primary_draft()` is sequence `10` with prompt preview `first prompt`.
  - Asserts structural `generating` remains true.

- `phase1_stream_chunks_route_by_draft_id`
  - Starts two drafts.
  - Sends interleaved chunks to `draft-b` and `draft-a`.
  - Asserts both emit redraw.
  - Asserts `draft-a == "A1"`, `draft-b == "B1"`, and visible order is unchanged.

- `phase1_stream_completed_removes_only_matching_draft`
  - Starts two drafts.
  - Completes `draft-a`.
  - Asserts SaveSession effect is emitted.
  - Asserts only `draft-b` remains.
  - Asserts `generating` remains true and primary stream is `draft-b`.

- `phase1_stale_chunk_for_completed_draft_is_ignored`
  - Starts two drafts, completes `draft-a`, then sends a late chunk for `draft-a`.
  - Asserts the late chunk has no effects.
  - Asserts only `draft-b` remains and `draft-b` text stays empty.

- `phase1_stream_cancelled_removes_only_matching_draft`
  - Starts two drafts, cancels `draft-a`.
  - Asserts redraw is emitted.
  - Asserts only `draft-b` remains.
  - Asserts cancelling one structural draft does not stop the other.

- `phase1_stream_failed_removes_only_matching_draft`
  - Starts two drafts, fails `draft-a`.
  - Asserts Error hook effect is emitted.
  - Asserts only `draft-b` remains.
  - Asserts failing one structural draft does not stop the other.

- `phase1_snapshot_dirty_changes_when_non_primary_draft_version_changes`
  - Starts `draft-a` primary and `draft-b` non-primary.
  - Sends a chunk to non-primary `draft-b`.
  - Asserts redraw is emitted.
  - Asserts snapshot dirty fingerprint changes.
  - Asserts `draft-b == "B1"` and primary remains `draft-a`.

## Self Check

- `cargo fmt --all -- --check`
  - Passed with no output.

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy --all-targets --all-features -- -D warnings`
  - `Finished dev profile [unoptimized + debuginfo] target(s) in 2m 06s`
  - Zero warnings.

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check --all-features`
  - `Finished dev profile [unoptimized + debuginfo] target(s) in 12.55s`
  - Zero warnings.

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features`
  - `Finished dev profile [unoptimized + debuginfo] target(s) in 8.13s`
  - Zero warnings.

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx phase1_ --all-features`
  - Main binary test target: 8 passed, 0 failed, 5428 filtered out.
  - The 8 includes the seven new `chat::state::tests::phase_a_f::phase1_*` tests plus one pre-existing `memory::sqlite::tests::conversation_turns_phase1_owner_columns_exist` name match.

## Admission Guard Evidence

`rg -n "pop_next_visible_input_task_with_scheduler|provider_turn_visible_admission|can_start_visible|workers\\.active_count" src/chat/mod.rs`

- `src/chat/mod.rs:4063` calls `pop_next_visible_input_task_with_scheduler(...)`.
- `src/chat/mod.rs:4086` calls `pop_next_visible_input_task_with_scheduler(...)`.
- `src/chat/mod.rs:8297-8302` keeps the pop helper gated by `provider_turn_visible_admission(workers).can_start_visible`.
- `src/chat/mod.rs:8412-8429` keeps `provider_turn_visible_admission(...)` blocking visible starts while active provider workers exist.
- Guard tests still present at `src/chat/mod.rs:14879-15002`.

## Notes

- No deploy or tmux demo was performed. Phase 1 is reducer/data-model only; the guarded runtime still remains safe-serial for visible provider turns.
- The old single `StreamState.draft` field is not retained. Compatibility uses computed `primary_draft()` / `primary_streaming_draft()` accessors from `visible_drafts`.
