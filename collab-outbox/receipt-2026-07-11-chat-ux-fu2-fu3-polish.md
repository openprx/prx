# Receipt 2026-07-11 - Chat UX FU2/FU3 Polish

Branch: `feat/chat-ux-fu2-fu3-polish`  
Base: `92b596f5` per task sheet  
Commit: not created by Codex, per instruction.

## Changes

- Approval popup arrow selection:
  - Added `PendingToolApprovalView.selected_approval` defaulting to deny in `src/chat/sessions/focus.rs:92`.
  - Wired default deny at approval creation/mirroring in `src/chat/dispatcher.rs:1273`, `src/chat/dispatcher.rs:10059`, `src/chat/state.rs:2279`, and `src/chat/mod.rs:18887`.
  - Added Left/Up = deny, Right/Down = approve, Enter = selected decision while preserving `y`/`n`/Esc in `src/chat/tui.rs:1562` and Redux reducer path `src/chat/state.rs:1337`.
  - Render title now exposes current selection and Arrow/Enter hint even when the panel is vertically constrained; button row remains when space permits in `src/chat/tui.rs:7204`.

- Bottom running spinner:
  - Reused the shared spinner frame helper for status bar and bottom rows, with 150 ms frame cadence in `src/chat/tui.rs:4513`.
  - Running bottom rows now render animated spinner glyphs in strip/list/switcher paths around `src/chat/tui.rs:4536` and `src/chat/tui.rs:4644`.
  - Added `periodic_redraw_active_for_view` and `TuiState::periodic_redraw_active` in `src/chat/tui.rs:3386` and `src/chat/tui.rs:5761`.
  - Changed the TUI loop to draw/tick at 150 ms only while streaming/tool/running bottom entries exist; idle poll is 1000 ms and does not redraw on timeout in `src/chat/mod.rs:11850`.

- Running worker title tool summary:
  - Added `ProviderWorkerStatusRow.recent_tool_call` in `src/chat/action.rs:36`.
  - Added compact latest tool summary extraction in `src/chat/tui.rs:1310`.
  - Enriched running worker rows from current conversation in `src/chat/state.rs:2790`.
  - Appended summary to active provider worker switcher/list title in `src/chat/tui.rs:1059`.

- Footer discovery:
  - Added session navigation hint helper and footer/list rendering in `src/chat/tui.rs:4701`, `src/chat/tui.rs:4758`, and `src/chat/tui.rs:7137`.
  - Hint text is `↑↓ sessions · Enter attach/open`, matching existing semantics; no navigation routing was changed.

## Tests Added / Updated

- `chat::tui::tests::approval_child_arrows_select_and_enter_confirms`
- `chat::tui::tests::approval_child_enter_uses_default_deny_and_left_selects_deny`
- `chat::state::tests::redux_approval_arrows_select_and_enter_resolves`
- `chat::tui::tests::running_status_glyph_animates_with_shared_spinner_frames`
- `chat::tui::tests::periodic_redraw_tracks_running_bottom_entries_only`
- `chat::tui::tests::session_list_footer_includes_navigation_hint_when_entries_exist`
- `chat::tui::tests::provider_worker_switcher_title_appends_latest_tool_summary_for_running_rows`
- `chat::state::tests::provider_worker_status_update_enriches_running_rows_with_recent_tool_summary`
- Updated existing footer/strip/approval render assertions to account for animated spinner and the new hint row.

## Validation

Commands run with `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`:

- `cargo fmt --check`: passed.
- `cargo clippy -p openprx --all-targets --all-features -- -D warnings`: passed.
- `cargo check -p openprx --all-features`: passed.
- `cargo check -p openprx --no-default-features`: passed.
- `cargo test -p openprx --bin prx --all-features`: passed.
  - Result: `5486 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out`.
- `cargo build -p openprx --bin prx --all-features`: passed for tmux PTY validation binary.

## PTY / Tmux Validation

Spinner/footer/CPU demo:

```sh
tmux new-session -d -s ux-polish-demo -x 150 -y 42 \
  "cd /opt/worker/code/prx && env HOME=/opt/worker/tmp/prx-ux-polish-home PRX_TUI=1 \
   OPENPRX_MOCK_RESPONSE='UX_POLISH_DONE' \
   OPENPRX_MOCK_DELAY_MS_BY_PROMPT='A=30000;B=30000' \
   /opt/worker/tmp/prx-target/debug/prx --config-dir /opt/worker/tmp/prx-ux-polish-config chat -p mock --model mock"
```

Submitted:

1. `A ux polish spinner first`
2. `B ux polish spinner second`

Captured spinner frames changed on running rows and status bar:

```text
PRX | mode:edit auth:supervised | workers:2 welapsed:3s | ⠹ generating (esc to interrupt)
 ⠹ w#1 worker provider running · 3s · 0 tok | $0.0000 · main provider w#1 detached task=1
 ⠹ w#2 worker provider running · 2s · 0 tok | $0.0000 · main provider w#2 detached task=2
 ↑↓ sessions · Enter attach/open

PRX | mode:edit auth:supervised | workers:2 welapsed:3s | ⠙ generating (esc to interrupt)
 ⠙ w#1 worker provider running · 3s · 0 tok | $0.0000 · main provider w#1 detached task=1
 ⠙ w#2 worker provider running · 3s · 0 tok | $0.0000 · main provider w#2 detached task=2
 ↑↓ sessions · Enter attach/open

PRX | mode:edit auth:supervised | workers:2 welapsed:3s | ⠸ generating (esc to interrupt)
 ⠸ w#1 worker provider running · 3s · 0 tok | $0.0000 · main provider w#1 detached task=1
 ⠸ w#2 worker provider running · 3s · 0 tok | $0.0000 · main provider w#2 detached task=2
 ↑↓ sessions · Enter attach/open
```

CPU sampling with `pidstat`:

```text
Idle/no running:
Average: UID PID     %usr %system %CPU Command
         1000 1397184 0.00 0.00   0.00 prx

Two running workers, debug binary, 150 ms animation cadence:
Average: UID PID     %usr %system %CPU Command
         1000 1397184 2.20 0.00   2.20 prx
```

Interpretation: no-running idle tick is stopped (`0.00%` on the actual `prx` PID). Running animation still redraws, but at the bounded 150 ms cadence; the observed `2.20%` is debug-binary full-screen render cost during two active workers, not an idle livelock.

Approval demo:

```sh
tmux new-session -d -s ux-approval-demo -x 150 -y 60 \
  "cd /opt/worker/code/prx && env HOME=/opt/worker/tmp/prx-ux-approval-home PRX_TUI=1 \
   OPENPRX_MOCK_RESPONSE='APPROVAL_DONE' \
   OPENPRX_MOCK_TOOL_CALL='shell:{\"command\":\"echo APPROVAL_OK\"}' \
   /opt/worker/tmp/prx-target/debug/prx --config-dir /opt/worker/tmp/prx-ux-approval-config chat -p mock --model mock"
```

Default safe selection:

```text
┌ Tool Approval - selected: deny - ←/→ select - Enter confirm ──────────────────────────────────────┐
│▸ Tool: shell                                                                                      │
│Args: {"command":"echo APPROVAL_OK"}                                                               │
└───────────────────────────────────────────────────────────────────────────────────────────────────┘
```

After Right arrow:

```text
┌ Tool Approval - selected: approve - ←/→ select - Enter confirm ───────────────────────────────────┐
│▸ Tool: shell                                                                                      │
│Args: {"command":"echo APPROVAL_OK"}                                                               │
└───────────────────────────────────────────────────────────────────────────────────────────────────┘
```

After Enter:

```text
✓ run shell(command="echo APPROVAL_OK")
  ⎿ output ✓ 22ms · 1 line · 12B
    │ APPROVAL_OK
○ APPROVAL_DONE

turn completed 3s
```

Temporary tmux sessions `ux-polish-demo` and `ux-approval-demo` were stopped after validation.

## Worktree

Tracked modified files:

- `src/chat/action.rs`
- `src/chat/dispatcher.rs`
- `src/chat/mod.rs`
- `src/chat/sessions/focus.rs`
- `src/chat/state.rs`
- `src/chat/tui.rs`
- `collab-outbox/receipt-2026-07-11-chat-ux-fu2-fu3-polish.md`

Existing untracked `collab-inbox/`, older receipts, and `task/` entries were left untouched.
