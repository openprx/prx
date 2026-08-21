# Receipt: Chat UX Round 3 U3 Proposal

Status: PROPOSAL ONLY
Commit: none; no source changes after U1
Push: not pushed

## Scope

U3 asks for a consistent arrow-key model against Claude Code before implementation. This receipt records the comparison, current PRX behavior, proposed key table, and confirmation points. No source changes were made.

## Claude Code Findings

Sources:

- https://code.claude.com/docs/en/interactive-mode
- https://code.claude.com/docs/en/fullscreen

Confirmed behavior from official docs:

- `Up` / `Down` or `Ctrl+P` / `Ctrl+N` move within wrapped/multiline prompt input first, then navigate command history at the visual first/last input row.
- `Left` / `Right` cycle dialog tabs in permission dialogs and menus.
- Transcript viewer supports `Up` / `Down` scrolling in side-question overlays, and fullscreen transcript uses `PgUp` / `PgDn`, `Ctrl+Home`, `Ctrl+End`, mouse wheel.
- Prompt suggestions accept `Tab` or `Right arrow`.
- Vim mode has its own motion model; in normal mode, arrows can move input cursor and fall through to history at bounds.

Inference:

- Claude Code does not use one global meaning for naked arrows. It routes arrows by active UI context: input, menus/dialogs, transcript/overlays, suggestions, and vim mode.
- PRX should follow this context-first model instead of globally replacing input history with session navigation.

## Current PRX State

- `src/chat/tui.rs:1257` gives global controls and overlays first priority.
- `src/chat/tui.rs:1276` uses `Alt+Left/Up/Right/Down` for bottom strip session selection.
- `src/chat/tui.rs:1330` uses naked `Left` / `Right` for adjacent child-session switching only when a child session is focused and input is empty.
- `src/chat/tui.rs:1344` uses naked `Up` / `Down` to scroll focused child/transcript/diff views only when input is empty.
- `src/chat/tui.rs:9320` tests that child/transcript/diff focus owns naked scroll keys with empty input, while non-empty input keeps edit/history semantics.
- `src/chat/tui.rs:9665` still asserts that bare `Up` recalls input history in the main empty prompt.
- `src/chat/state.rs:944` mirrors Alt-arrow strip navigation in the reducer path.

## Proposed Key Model

Principle: active modal surface wins; non-empty input never loses edit/history behavior; empty main prompt may use naked arrows for navigation because there is no draft to edit.

| Context | Naked Up/Down | Naked Left/Right | History remains reachable |
|---|---|---|---|
| slash menu / picker / approval | move selection or dialog control | dialog-specific tab/selection | no history while modal is open |
| non-empty prompt input | input cursor/wrapped-line/history behavior | cursor movement | `Up`/`Down` at input bounds, plus `Ctrl+P`/`Ctrl+N` |
| empty main prompt, sessions exist | move bottom strip selection previous/next | move bottom strip selection previous/next | `Ctrl+P`/`Ctrl+N` for input history |
| empty main prompt, no sessions | input history on `Up`/`Down` | no-op/input cursor behavior | existing `Up`/`Down` unchanged |
| strip selection present | `Up/Left` previous, `Down/Right` next | same | `Esc` clears strip selection; `Ctrl+P`/`Ctrl+N` history |
| child/transcript/diff focused and empty input | scroll focused view | switch adjacent session where applicable | `Ctrl+P`/`Ctrl+N` history if we choose to reserve it globally |
| child/transcript/diff focused and non-empty input | input edit/history | input cursor movement | existing input behavior |

Recommended implementation for first pass:

1. Add naked-arrow strip navigation only for `FocusTarget::Main` with empty input and non-empty session cache.
2. Preserve current child/transcript/diff naked arrows exactly; they already match context-first navigation.
3. Preserve `Alt+arrows` as aliases for existing users.
4. Keep `Alt+Enter` attach, and consider adding naked `Enter` attach only when `strip_selection.is_some()` and input is empty. This needs confirmation because Enter currently has strong submit semantics.
5. Footer/help should advertise `Arrows sessions` only when this lands, and keep `/attach #N` / `Alt+Enter` visible.

## Why This Avoids Swallowing History

- Non-empty prompt keeps current input handling, including wrapped/multiline behavior.
- Empty main prompt with no sessions keeps existing history behavior.
- Empty main prompt with sessions moves session selection; history is still available via `Ctrl+P` / `Ctrl+N`.
- This matches the user's "裸方向键直达" request for visible navigable session UI without breaking active text editing.

## Tests To Add

- main focus + empty input + sessions: bare `Down` changes `strip_selection`
- main focus + empty input + no sessions: bare `Up` still recalls history
- main focus + non-empty input + sessions: bare `Up` / `Down` do not move strip selection
- slash menu open: bare arrows still move menu selection, not strip selection
- child focus + empty input: existing scroll behavior remains
- child focus + non-empty input: existing input/history behavior remains
- reducer path mirrors the same branches as direct TUI dispatch

## Questions For User Confirmation

1. Should naked `Enter` attach the highlighted strip session, or should attach stay `Alt+Enter` / `/attach #N` to avoid accidental submits?
2. In empty main prompt with sessions, is it acceptable that input history moves to `Ctrl+P` / `Ctrl+N`?
3. Should `Left` / `Right` also navigate the bottom strip in main focus, or should only `Up` / `Down` do so?

## Validation

No source changes were made for this proposal receipt. Current committed source state is U1 commit `ef9ea6d4`; the U1 receipt self-check already passed:

- PASS `cargo fmt --all -- --check`
- PASS `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo clippy --workspace --all-targets`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo test --workspace`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo check --workspace --no-default-features`

## Demo Recheck Needed After Implementation

- With empty prompt and running sessions, bare arrows should visibly move the strip highlight.
- With a typed draft, bare arrows should edit/navigate input rather than move sessions.
- With slash/menu/picker open, arrows should stay captured by that modal.
- Input history should remain reachable and predictable.
