# P4b-2 Receipt — visible scheduler concurrency

Baseline: `bc75fad9` (P4b-1 amend, user reported 5462 passed / 0 failed).

Status: implemented locally, not committed.

## Code changes

- `src/config/schema.rs:5187-5204`
  - Added `[chat].max_concurrent_visible_turns`.
  - Default is `2`.
  - Runtime clamps effective value with `.max(1)`.
  - Updated config tests at `src/config/schema.rs:6274-6288`.

- `src/chat/mod.rs:175-197`
  - Extended `PerTurnContext` with `history_user_message`.
  - Converted per-turn senders to `Option` so finalizer can close streams while retaining the context for delayed ordered commit.
  - Added `PendingOrderedProviderTurnCommit`.

- `src/chat/mod.rs:3714-3724`, `src/chat/mod.rs:8776-8815`
  - Added unified admission:
    - `ForegroundAwaited` starts only when `active_workers == 0`.
    - Any foreground/Legacy active blocks detached Redux turns.
    - Detached Redux starts only when `foreground_active == 0 && detached_active < max_concurrent_visible_turns`.
    - Effective max is at least 1.
  - `AwaitingCommit` still counts active, so a completed but not ordered-committed turn holds capacity.

- `src/chat/mod.rs:6723-6752`
  - Added post-route admission guard.
  - If a turn routes to Legacy while admission is blocked, the original input is requeued at the front with its original priority and no provider worker is started.

- `src/chat/mod.rs:6409-6411`, `src/chat/mod.rs:6753-6775`, `src/chat/mod.rs:6810-6816`
  - Redux detached sends provider a per-task `history_for_provider = canonical history clone + this user`.
  - Canonical `history` is not mutated at Redux start.
  - Legacy/non-TUI still pushes user into canonical history before running the legacy tool loop.

- `src/chat/mod.rs:9273-9665`
  - Ordered commit gate now stores completed terminal payloads in `pending_ordered_provider_turn_commits`.
  - `dispatch_ordered_provider_turn_commit` emits `RecordUserTurn -> RecordAssistantTurn -> StreamCompleted`, so reducer `SaveSession` sees ordered user/assistant pairs.
  - Canonical `history` and local `chat_session` are updated only inside ordered commit release.
  - Failed/cancelled turns remain ordered skips and do not write assistant/session.

- `src/chat/mod.rs:4280-4310`, `src/chat/mod.rs:9552-9665`
  - Fixed P4b-1 Finding3: if a later task completes before an earlier task, its draft/payload stays pending. When the earlier task releases coordinator readiness, `apply_ready_ordered_provider_turn_commits` applies all ready completed commits in sequence.

- `src/providers/router.rs:282-300`, `src/providers/router.rs:371-398`, `src/providers/router.rs:471-482`, `src/providers/router.rs:551-635`
  - Added `OPENPRX_MOCK_DELAY_MS_BY_PROMPT`.
  - Format: `substring=milliseconds;substring=milliseconds`.
  - Matching uses the last user message in the provider history and applies a one-time initial stream delay.

## Tests added/updated

- `src/chat/mod.rs:16540-16755`
  - Admission N=2 allows one detached turn and blocks the third slot.
  - Legacy foreground worker blocks both detached and foreground admission.

- `src/chat/mod.rs:16387-16463`
  - P4b-1 ordered-save test updated for new ordered `RecordUserTurn -> RecordAssistantTurn -> StreamCompleted` sequence.
  - Asserts `ProviderTurnReadyForCommit` still emits no `SaveSession`; `SaveSession` appears only after ordered `StreamCompleted`.

- `src/chat/mod.rs:17371-17470`
  - Coordinator out-of-order completion test:
    - later task completes first -> result is `finalized=false`, worker remains `AwaitingCommit`;
    - earlier task completes -> ready results release `[first, second]` in sequence;
    - both workers become `Committed`.

- `src/providers/router.rs:873-889`
  - Mock prompt-delay parser and last-user-message matching test.

## Demo steps prepared

Use mock provider with two submitted prompts whose contents contain distinct delay keys:

```bash
OPENPRX_MOCK_RESPONSE='MOCK_DONE' \
OPENPRX_MOCK_DELAY_MS_BY_PROMPT='A=5000;B=1000' \
PRX_TUI=1 \
prx chat
```

Then submit a prompt containing `A`, immediately submit a prompt containing `B`, and observe that both visible Redux turns are admitted with default `chat.max_concurrent_visible_turns = 2`; B may complete first, but session/history commit remains ordered by task sequence.

## Validation

- `cargo fmt --check` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --all-features` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy -p openprx --all-targets --all-features -- -D warnings` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features` — passed: `5466 passed; 0 failed; 7 ignored`.

## Fixround: B4 post-route requeue livelock

- `src/chat/mod.rs:1656-1660`
  - Added `consume_deferred_visible_input_pop`, a one-shot helper for skipping exactly one visible-input pop after post-route admission requeues the same task.

- `src/chat/mod.rs:4125-4127`
  - Added `defer_visible_input_pop_once` beside the visible input backlog and scheduler state.

- `src/chat/mod.rs:4319-4333`
  - The event pump now consumes the one-shot defer before calling `pop_next_visible_input_task_with_scheduler`.
  - This preserves the requeued input at the front, but lets the loop reach `select!` so in-flight provider completion/lifecycle/control events can be consumed and stricter Legacy admission can be freed.

- `src/chat/mod.rs:6757-6764`
  - Post-route admission rejection still requeues the original input at the front, but now sets `defer_visible_input_pop_once = true` before continuing.
  - This prevents the previous tight loop: pop as Detached-capacity-eligible, route as ForegroundAwaited, reject because another detached worker is active, requeue, immediately pop the same task again without polling completions.

## Fixround Tests

- `src/chat/mod.rs:16787-16836`
  - Added `post_route_requeue_defers_next_visible_pop_once_under_detached_capacity`.
  - Covers default N=2 shape where Detached admission would otherwise allow another pop while a detached worker is active.
  - Asserts the requeued task remains queued during the one-shot defer, is not repeatedly marked dispatched, and can pop after the event-pump wait.

- `src/chat/mod.rs:17543-17645`
  - Added C7a coverage: `provider_turn_finalizer_releases_later_completed_turn_when_earlier_cancelled`.
  - A later completed turn stays held while an earlier turn is pending cancellation; finalizing the earlier cancellation emits ordered results `[cancelled, completed]`, marks the first worker `Cancelled`, and commits the later worker.

## Fixround Validation

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features post_route_requeue_defers_next_visible_pop_once_under_detached_capacity -- --nocapture` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features provider_turn_finalizer_releases_later_completed_turn_when_earlier_cancelled -- --nocapture` — passed.
- `cargo fmt --check` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --all-features` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy -p openprx --all-targets --all-features -- -D warnings` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features` — passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features` — passed: `5468 passed; 0 failed; 7 ignored`.

## Notes

- No git commit was made.
- Main transcript remains a single primary path: ordered persistence is still emitted through `RecordUserTurn`, `RecordAssistantTurn`, and `StreamCompleted`; task identity is carried via task id for assistant/tool buckets.
