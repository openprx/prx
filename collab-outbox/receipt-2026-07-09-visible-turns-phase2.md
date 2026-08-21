# Visible Turns Phase 2 Receipt - 2026-07-09

## Result

Phase 2 implemented: worker/session pane now selects the live streaming draft by the focused provider worker's real `sequence`. Main transcript rendering remains safe-serial and continues to use the primary draft only. Runtime admission guard was not relaxed.

Base: `0600f805 feat(chat): keyed reducer stream state for visible turns (Phase 1)`

## Changed Files and Lines

- `src/chat/state.rs:388-390`
  - Added `UiSnapshot.visible_streaming_drafts: Arc<Vec<VisibleStreamingDraftView>>`.
- `src/chat/state.rs:440-441`
  - Initialized empty per-worker draft snapshot in `UiSnapshot::initial`.
- `src/chat/state.rs:462-469`
  - Added `UiSnapshot::streaming_draft_for_worker(sequence)`.
- `src/chat/state.rs:487-491`
  - Added `VisibleStreamingDraftView { sequence, draft }`.
- `src/chat/state.rs:516-532`
  - Added `StreamState::streaming_draft_for_worker` and `visible_streaming_draft_views`.
- `src/chat/state.rs:757-758`
  - `build_ui_snapshot` still sets `streaming` from primary, and now also snapshots all visible drafts.
- `src/chat/state.rs:2486-2505`
  - `reduce_provider_worker_status_updated` now selects IO by focused worker sequence and preserves worker view scroll.
- `src/chat/state.rs:2514-2536`
  - `refresh_provider_worker_view_if_focused` now selects IO by focused worker sequence and preserves worker view scroll.
- `src/chat/tui.rs:94-96`
  - Added `TuiState.visible_streaming_drafts` mirror field.
- `src/chat/tui.rs:1390-1398`
  - Added `provider_worker_io_lines_for_streaming_draft`; missing draft returns empty IO and does not replay transcript history.
- `src/chat/tui.rs:3112-3113`
  - Initialized empty visible draft mirror in `TuiState::new`.
- `src/chat/tui.rs:3135-3141`
  - Added `TuiState::streaming_draft_for_worker(sequence)`.
- `src/chat/action.rs:140-165`
  - Added worker-pane append scroll preservation helper.
- `src/chat/mod.rs:7901-7929`
  - `sync_key_mirror_observation_state` now syncs visible drafts into the key mirror and refreshes focused worker IO by real sequence.
- `src/chat/mod.rs:10210-10224`
  - `open_provider_worker_view` now selects IO from `TuiState::streaming_draft_for_worker(sequence)`.

## Four IO Integration Points

- `state.rs:2493-2500` in `reduce_provider_worker_status_updated`.
- `state.rs:2524-2530` in `refresh_provider_worker_view_if_focused`.
- `mod.rs:7917-7923` in `sync_key_mirror_observation_state`.
- `mod.rs:10214-10219` in `open_provider_worker_view`.

All four use `provider_worker_io_lines_for_streaming_draft(..., streaming_draft_for_worker(sequence), ...)`. If no matching draft exists, IO lines are empty. No path falls back to primary.

## Tests Added

- `chat::state::tests::phase_a_f::phase2_snapshot_exposes_worker_drafts_and_keeps_primary_streaming`
  - Asserts snapshot primary remains `draft-a`.
  - Asserts worker sequence `20` resolves to `B live`.
  - Asserts missing sequence returns `None`.
  - Asserts visible draft snapshot order is `[10, 20]`.

- `chat::state::tests::phase_a_f::phase2_worker_pane_focus_uses_matching_draft_not_primary`
  - Focuses worker `10` and asserts pane contains `A live`, not `B live`.
  - Focuses worker `20` and asserts pane contains `B live B2`, not `A live`.

- `chat::state::tests::phase_a_f::phase2_worker_pane_missing_draft_uses_empty_io_not_history_or_primary`
  - Focuses worker `30` with no matching draft.
  - Asserts no `io: recent provider turn`.
  - Asserts neither transcript history nor primary live stream leaks into worker `30`.

- `chat::state::tests::phase_a_f::phase2_main_transcript_primary_streaming_path_is_unchanged`
  - Starts out-of-order drafts `20` then `10`.
  - Asserts snapshot `streaming` and reducer primary both remain sequence `10` / `draft-a`.

- `chat::tui::tests::phase2_bottom_direction_selects_provider_worker_and_enter_opens_worker_view`
  - Asserts bottom-strip synthetic seq opens real worker sequence `3`.
  - Asserts real sequence `3` resolves the draft.
  - Asserts synthetic seq does not resolve a draft.

- `chat::tui::tests::phase2_provider_worker_io_none_is_empty_not_history_fallback`
  - Calls worker IO helper with transcript history and `None` draft.
  - Asserts result is empty.

- `chat::action::tests::phase2_provider_worker_scroll_preserves_top_on_append_and_follows_tail_at_bottom`
  - Asserts offset `3` becomes `5` after two appended lines.
  - Asserts offset `0` remains tail-follow pinned.

- `chat::s4_a_4::phase2_snapshot_mirror_sync_uses_focused_worker_draft_not_primary`
  - Builds snapshot where primary is A and focused worker is B.
  - Asserts snapshot helper resolves B.
  - Asserts mirror sync renders B and not A.
  - Asserts `open_provider_worker_view` dispatches a worker view containing B and not A.

## Self Check

- `cargo fmt --all -- --check`
  - Passed with no output.

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy --all-targets --all-features -- -D warnings`
  - `Finished dev profile [unoptimized + debuginfo] target(s) in 2m 07s`
  - Zero warnings.

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check --all-features`
  - `Finished dev profile [unoptimized + debuginfo] target(s) in 10.71s`
  - Zero warnings.

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features`
  - `Finished dev profile [unoptimized + debuginfo] target(s) in 0.28s`
  - Zero warnings.

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx phase2_ --all-features`
  - Main binary test target: 8 passed, 0 failed, 5435 filtered out.

- `git diff --check`
  - Passed with no output.

## Guard and Non-Goals

- Admission guard remains in place:
  - `src/chat/mod.rs:4063`
  - `src/chat/mod.rs:4086`
  - `src/chat/mod.rs:8303-8308`
  - `src/chat/mod.rs:8418-8435`
  - guard tests remain at `src/chat/mod.rs:15003-15126`.
- Main transcript primary path remains unchanged at `src/chat/state.rs:757`.
- No worker strip/switcher live glyph, `/queue`, cost, tool-card isolation, scheduler concurrency, deploy, or tmux demo changes were made.
- New `expect` calls are test-only; no new non-test `unwrap`/`expect`, `todo`, or `#[allow(dead_code)]` was introduced.
