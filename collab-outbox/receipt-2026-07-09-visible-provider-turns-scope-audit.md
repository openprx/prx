# Visible Provider Turns Scope Audit - 2026-07-09

## Summary

Goal under audit: support multiple human-visible provider turns running at the same time in `prx chat`, with each turn streaming independently, cancellable independently, and finalizing without corrupting transcript/history.

Current implementation is intentionally safe-serial for visible turns. It supports detached provider workers, queued/priority input, worker status, keyed completion signals, and guarded finalization, but the visible TUI/reducer path still exposes only one active streaming draft.

Conclusion: this is a separate architecture upgrade, not a small follow-up patch. Releasing the current admission guard is unsafe until stream state, snapshot/rendering, reducer actions, and per-task persistence context are all task-aware.

## Current Evidence

- `src/chat/state.rs:460` defines `StreamState` with a single `draft: Option<StreamingDraft>`.
- `src/chat/state.rs:628` builds `UiSnapshot` with `streaming: self.stream.draft.clone()`.
- `src/chat/tui.rs:50` defines a single `StreamingDraft`; `src/chat/tui.rs:94` keeps `TuiState.streaming: Option<StreamingDraft>`.
- `src/chat/tui.rs:5587` treats activity as `state.streaming().is_some()` plus running tool cards.
- `src/chat/state.rs:1372` starts a turn by overwriting `self.stream.draft`.
- `src/chat/state.rs:1450` accepts chunks only when the one active draft id matches.
- `src/chat/state.rs:1502` completes only when the one active draft id matches, then clears `self.stream.draft`.
- `src/chat/mod.rs:4063` and `src/chat/mod.rs:4086` now use `pop_next_visible_input_task_with_scheduler(...)` so active workers block the next visible turn.
- `src/chat/mod.rs:8294` implements that visible admission helper; `src/chat/mod.rs:8409` counts active provider workers.
- `src/chat/mod.rs:14980` tests that active provider workers preserve the backlog and do not mark queued input as dispatched.
- `src/chat/dispatcher.rs:308` has keyed `TurnCompletionSignal` state, so task-keyed completion is partially present.
- `src/chat/mod.rs:180` and `src/chat/mod.rs:8687` define/build `ProviderTurnTerminalPlan`, so completion finalization already has a task-oriented shell.

## Hard Boundaries

### 1. Stream state is single-draft

Current `StreamState` cannot hold two visible drafts. Starting a second visible `StartLLMTurn` would overwrite the first draft, causing later chunks/completion from the first turn to become stale or no-op.

Required change:

- Replace or extend `StreamState.draft: Option<StreamingDraft>` with a keyed collection such as `IndexMap<DraftId, StreamingDraft>` or `IndexMap<TurnTaskId, StreamingTurnDraft>`.
- Preserve visible order using scheduler sequence, not arrival order.
- Keep a compatibility accessor only while migrating old tests/render code.

### 2. Snapshot/rendering exposes only one streaming slot

`UiSnapshot` and `TuiState` expose `streaming: Option<StreamingDraft>`. The renderer is designed to paint one transient assistant block at transcript tail.

Required change:

- Add `streaming_turns: Vec<StreamingTurnView>` to snapshot/UI.
- Render each visible pending turn as its own transcript block: user prompt, assistant stream, status, elapsed, worker id.
- Update provider worker detail view to read the matching turn stream, not the single global stream.

### 3. Reducer actions are not fully task-aware

Stream terminal actions carry `draft_id`, but persistence actions such as `RecordAssistantTurn(String)` do not carry task id or history boundary. Tool buffers are also global for the current turn.

Required change:

- Add task/draft identity to assistant record and tool actions, or introduce task-aware variants.
- Bucket `pending_tool_cards`, `current_turn_tool_calls`, and `current_turn_tool_args` by task/draft.
- Ensure cross-task chunks, tool events, failures, and cancellations cannot mutate the wrong UI/history state.

### 4. Chat main loop is current-turn awaited

The live Redux driver branch starts a provider turn, then waits for that current turn completion before returning to the outer loop. This is safe for serial visible turns but prevents real visible concurrency.

Required change:

- Convert visible turn orchestration to event-driven scheduling.
- Allow the loop to start additional visible workers up to a configured concurrency limit.
- Process input, lifecycle, completion, and finalizer events from the same scheduler loop.
- Do not keep per-turn finalization data only in the stack frame of the currently awaited turn.

### 5. Persistence/finalization context is incomplete for concurrent visible turns

Completion/finalizer structures exist, but success handling still depends on local variables from the current turn frame: `history`, `user_input`, `route_scope`, `turn_run_id`, `provider_started_at`, model/provider names, and terminal draft id.

Required change:

- Store a per-task context at dispatch time:
  - task id and draft id
  - scheduler sequence
  - user input
  - history base length and completion boundary
  - route decision/scope and turn run id
  - provider/model and start timestamp
  - terminal/channel finalization handles or metadata
- Let finalizer consume this context by task id, independent of which turn is currently active in the loop.

## Required Implementation Phases

### Phase 1 - Data model

Introduce a keyed visible draft model while preserving serial behavior by default.

Acceptance:

- Two drafts can coexist in reducer state.
- Interleaved chunks update the correct draft.
- Completing/cancelling one draft does not clear another.

### Phase 2 - Snapshot and renderer

Expose multiple streaming turns to TUI rendering.

Acceptance:

- UI can show at least two active visible turns simultaneously.
- Worker detail view can show live IO for the selected worker.
- Bottom chrome activity reflects all active streams/workers.

### Phase 3 - Task-aware reducer events

Make assistant persistence and tool buffers task-aware.

Acceptance:

- Interleaved `ToolStarted`/`ToolFinished` events do not cross-contaminate tool cards or `tool_calls`.
- `RecordAssistantTurn` cannot write an answer to the wrong user turn.
- Existing single-turn tests remain green through compatibility paths.

### Phase 4 - Event-driven visible scheduler

Replace current-turn waiting with a scheduler loop that can run multiple visible workers.

Acceptance:

- Backlog can dispatch more than one visible provider worker up to a concurrency limit.
- `/workers cancel w#N` cancels only that worker.
- `/queue` reflects queued vs running work accurately.
- Priority still affects queued dispatch order, not already running turns.

### Phase 5 - History commit and deployment validation

Finalize out-of-order completions safely.

Acceptance:

- If task #3 finishes before task #2, #3 does not corrupt transcript/history.
- Commit/skip decision is visible through worker status.
- SaveSession snapshots are ordered and consistent.
- Real `/home/ck/.cargo/bin/prx chat` in `tmux demo` proves:
  - two long visible turns streaming simultaneously,
  - one cancelled while the other continues,
  - priority queued turn dispatches before normal queued turn when a slot opens,
  - no sentinel output leaks after cancellation,
  - transcript/export/cost remain coherent after finalization.

## Risk Assessment

High-risk areas:

- History ordering when completions arrive out of order.
- Global tool buffers crossing task boundaries.
- SaveSession snapshots emitted before the correct assistant turn is attached.
- Renderer layout instability if multiple streaming blocks grow at once.
- Compatibility with non-TUI or legacy driver paths.

Lower-risk areas:

- Keyed terminal outcome signaling is already partially present in `TurnCompletionSignal`.
- Worker registry and finalizer concepts already support detached task identity.
- Queue and priority semantics are already validated in the current safe-serial model.

## Recommended Next Task

Start with Phase 1 only: keyed reducer stream state plus tests for interleaved chunks/completion. Do not remove the visible admission guard until Phase 4 and Phase 5 are complete.

Proposed first implementation target:

- Add `StreamingTurnDraft { task_id: Option<TurnTaskId>, draft: StreamingDraft, sequence: u64, prompt_preview: String }`.
- Add `StreamState.visible_drafts`.
- Keep `StreamState.draft` or a `primary_draft()` accessor temporarily for snapshot compatibility.
- Add tests:
  - two drafts start without overwriting,
  - chunks route by draft id,
  - completion of one draft leaves the other active,
  - stale chunk for completed draft is ignored.

This first phase is intentionally structural. It should not claim user-visible concurrent streaming until renderer and main-loop phases are also complete.

## Implementation Gate Addendum

### Product gate before Phase 2+

Claude Code parity does not itself require multiple visible provider turns streaming inside one TUI transcript at the same time. That makes concurrent visible turns a product decision, not a parity bug fix. Before Phase 2 starts, decide whether PRX needs this for PRX-specific workflows such as background agents, multi-session operator views, or visible worker orchestration in one chat. If the answer is no, keep the safe-serial transcript model and expose concurrency through worker/session surfaces instead.

**DECIDED 2026-07-09 (owner):** Take the worker/session-pane direction. The main chat transcript stays **safe-serial** (single visible streaming turn, admission guard retained); concurrency is surfaced through worker/session panes, not by streaming multiple visible turns inside one transcript. Phase 2 renderer work therefore targets a worker/session detail surface, NOT multi-block concurrent streaming in the main transcript. The scroll-anchoring acceptance below still applies to the worker/session surface. Phase 1 keyed reducer stream state remains the correct shared foundation for this direction (a worker pane still needs per-task/per-draft stream identity).

### Phase 2 scroll anchoring acceptance

Multiple streaming blocks can grow at the same time, so Phase 2 must specify viewport stability before renderer work is accepted:

- If the user is at bottom, follow the latest growth without jitter.
- If the user scrolled away from bottom, preserve the top visible anchor across updates.
- Each streaming block needs a stable render identity derived from task/draft id.
- Prompt preview/status height must be bounded so late metadata does not reflow large regions.
- Worker detail updates must not steal focus or jump the transcript viewport.

### Phase 3 per-task cost acceptance

Cost/token coherence belongs in Phase 3 because task-aware reducer context is the first point where usage can be bucketed safely. Add per-task usage aggregation to the Phase 3 acceptance criteria:

- Usage events carry task/draft identity or resolve through a task-owned completion context.
- Provider usage is accumulated exactly once per task, even if completion/finalizer events arrive out of order.
- Worker rows can show per-task tokens/cost while global `/cost` only reflects finalized committed usage.
- Cancellation/failure paths do not bill another task or duplicate usage into the session total.

### Phase 3 cancel/tool-card isolation acceptance

Phase 3 must explicitly cover cancellation isolation:

- Start visible workers A and B; both own running tool cards.
- Cancel A.
- B's tool cards remain visible and continue receiving matching updates.
- A's cards finish/cancel only under A's block and do not clear B's `tool_calls`, partial args, or pending card indexes.
- A late A tool event after cancellation is ignored or routed only to A's closed context; it must not mutate B.
