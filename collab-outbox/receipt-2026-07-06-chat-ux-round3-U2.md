# Receipt: Chat UX Round 3 U2 Proposal

Status: PROPOSAL ONLY
Commit: none; no source changes after U1
Push: not pushed

## Scope

U2 asks for a Claude Code comparison and proposal before broad implementation. This receipt records the investigation, a gap list, low-risk implementation slice, and questions that should be confirmed before changing the bottom child-session layout.

## Claude Code Findings

Sources:

- https://code.claude.com/docs/en/interactive-mode
- https://code.claude.com/docs/en/fullscreen
- https://code.claude.com/docs/en/terminal-config

Confirmed behavior from official docs:

- `Ctrl+B` backgrounds running Bash commands and agents; tmux users press it twice because of the tmux prefix.
- `/tasks` is the documented way to see running shells and subagents; `Ctrl+T` is the checklist, not the background-task view.
- Shell mode with `!` shows real-time progress and output, and supports the same backgrounding flow.
- Fullscreen mode supports clicking collapsed tool results to expand them.
- Fullscreen mode keeps the input fixed at the bottom and handles mouse scrolling/clicking inside the app.

Not found in official docs:

- a precise visual spec for Claude Code's bottom running-task strip
- exact row/chip/card dimensions for background tasks
- whether the user specifically expects a persistent bottom strip, a `/tasks` overlay, or transcript-integrated task cards

## Current PRX State

- `src/chat/tui.rs:3945` renders the always-visible bottom session strip.
- `src/chat/tui.rs:4019` formats each session chip as glyph, `#N`, kind, elapsed, optional idle marker, optional token usage, and title.
- `src/chat/tui.rs:4071` / `src/chat/tui.rs:4096` build a one-row strip with overflow handling.
- `src/chat/state.rs:2232` updates structured child-session entries from `Action::SessionsEntriesUpdated`.
- `src/chat/sessions/runtime.rs:825` has `tail(seq, last_n)` for child-session output.
- `/logs #N` and `/attach #N` already expose session logs/tails at command level.

## Gap List

1. Real-time output preview:
   - Current strip mostly echoes title/command, elapsed, status, and usage.
   - It does not put the latest child output line into the always-visible strip.

2. Running-state motion:
   - Current non-ASCII running glyph is status-based but not visibly animated.
   - It does not share the main generation spinner cadence.

3. Expand affordance:
   - `/attach #N` and `/logs #N` work, and Alt+Enter attaches selected strip item.
   - The strip itself does not visibly advertise "expand/live output" on the row.

4. Layout:
   - Current strip is a dense one-line chip row.
   - Claude Code docs confirm `/tasks` for running shells/subagents, but do not specify that the persistent bottom surface is a card/list. A card-style rewrite should wait for user confirmation.

## Proposed Low-Risk Implementation Slice

Implement only these first:

1. Add `latest_preview: Option<String>` to the UI-facing session entry shape or a parallel preview map keyed by `seq`.
   - Source for agents: latest ring tail line via the existing session event bridge.
   - Source for shells/PTY: existing tail/ring path where available.
   - Sanitize to a single line, strip control sequences, cap width before rendering.

2. Render running entries as:
   - compact: `⠋ #3 shell 12s test… · cargo test passed 12/40`
   - ASCII: `* #3 shell 12s test... | cargo test passed 12/40`
   - completed/failed entries keep the current stable glyphs.

3. Reuse the main spinner frame source instead of adding a new timer.
   - The 1s session poll is likely too slow for spinner animation; use the render tick or existing streaming animation cadence if available.
   - If no render tick exists while idle, spinner can advance only when session snapshots arrive; receipt must call that out.

4. Make expansion discoverable without layout rewrite:
   - footer/help: `Alt+arrows sessions · Alt+Enter attach · /logs #N`
   - selected strip chip may append `Enter attach` only when there is enough width.
   - Keep `/logs #N` as the stable textual fallback.

5. Tests:
   - strip row includes latest preview for running session and not for empty preview
   - preview truncates by display width and does not overflow row
   - running spinner frame changes under supplied frame index
   - `/logs #N` fallback remains in slash/help surface

## Questions For User Confirmation

1. Should the persistent bottom strip remain one row, or should PRX add a multi-row `/tasks`-style overlay/card list?
2. For multiple running sessions, should the bottom strip prioritize the active/selected session preview over showing many compact chips?
3. Should clicking a strip chip attach/expand, or should mouse click remain conservative and use Alt+Enter only?
4. Is the desired "Claude-like" part primarily live output preview, visual spinner motion, or a larger task-list surface?

## Validation

No source changes were made for this proposal receipt. Current committed source state is U1 commit `ef9ea6d4`; the U1 receipt self-check already passed:

- PASS `cargo fmt --all -- --check`
- PASS `RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo clippy --workspace --all-targets`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo test --workspace`
- PASS `CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-ux-round3-u1 cargo check --workspace --no-default-features`

## Demo Recheck Needed After Implementation

- Start a long-running `/shell` or model-spawned session and confirm the strip shows live latest output, not just the command.
- Confirm spinner motion is visible while no main assistant response is streaming.
- Confirm `/logs #N` and attach remain reachable and do not interfere with the main input.
