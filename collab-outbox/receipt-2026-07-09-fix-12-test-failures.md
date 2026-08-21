# Receipt: fix 12 pre-existing full-suite failures

Base: `cbde09ce` (P3c). P4a not included.

User decision on key contract: **方案 B**. Bare arrows navigate the bottom strip/session/worker view when navigable entries exist. When there are no bottom entries, bare arrows fall through to input history or child-view scrolling. Alt arrows remain UI-only strip selection.

## Contract Conflict Check

Before editing `dispatch_global_key`, I grepped and then ran the current passing tests that encode the old-looking bare-arrow navigation behavior:

- `dispatch_directional_session_switching_obeys_focus_input_matrix` — passed before changes and remains unchanged.
- `phase2_bottom_direction_selects_provider_worker_and_enter_opens_worker_view` — passed before changes and remains unchanged.
- `provider_worker_focus_direction_and_esc_are_read_only_view_controls` — passed before changes and remains unchanged.

Per user decision, these are not conflicts anymore; they define scheme B and were preserved.

Input history remains reachable under B:

- Main focus + empty input + no bottom entries falls through to `state.handle_input_key`, so bare Up/Down recall history.
- Main focus + empty input + bottom entries uses strip navigation.
- Child focus + empty input + no bottom entries falls through to child scroll.
- Child/worker focus + empty input + bottom entries uses session/worker navigation.

Code points:

- `src/chat/tui.rs:1572` only intercepts Main bare arrows when bottom entries are non-empty.
- `src/chat/tui.rs:1689` and `src/chat/tui.rs:1725` only intercept Session/Worker bare arrows when bottom entries are non-empty.

## Fixes

1. `s2_5_p1_b_assistant_turn_carries_tool_calls`
   - Root cause: test was stale.
   - Change: updated expected shell args preview from compact JSON to `command="ls"` in `src/chat/state.rs:8766`, matching the existing shell preview formatter.

2. `provider_worker_status_update_refreshes_open_worker_view_with_io`
   - Root cause: test was stale after P2 worker IO design.
   - Change: updated `src/chat/state.rs:3510` expectations so non-streaming worker views do not replay completed transcript/tool IO.

3. `sessions_strip_one_active_entry_shows_marker_glyph_kind_and_title`
   - Root cause: code bug from substrate parity change.
   - Change: restored unicode active marker branch in `session_active_marker` at `src/chat/tui.rs:4412`.

4. `sessions_strip_active_entry_drives_window_when_no_selection`
   - Root cause: same marker code bug.
   - Change: same `session_active_marker` fix; unicode active marker is now `▸`, ASCII remains `>`.

5. `input_history_up_down_still_work_when_slash_menu_closed`
   - Root cause: mixed; scheme B test needed clarification, and code had a real fallthrough bug when no strip entries existed.
   - Change: `dispatch_global_key` now falls through to input history when no bottom entries exist; test now asserts both no-strip history and strip-present navigation.

6. `input_history_edit_then_up_down_steps_clean_entries`
   - Root cause: same no-strip fallthrough bug.
   - Change: code fix makes edited input history reachable when no strip entries exist; test comment documents B.

7. `saved_session_picker_closed_keeps_up_down_input_history_behavior`
   - Root cause: same no-strip fallthrough bug plus stale ambiguity.
   - Change: test now covers both no-strip input history and strip-present navigation.

8. `fullscreen_scroll_focus_rules_do_not_steal_input_or_child_keys`
   - Root cause: same no-strip fallthrough bug plus stale ambiguity.
   - Change: test now asserts no-strip input history remains reachable and strip-present bare arrows choose strip navigation.

9. `bare_arrows_history_cursor_and_child_scroll_are_not_stolen_by_strip_selection`
   - Root cause: stale under scheme B plus no-strip fallthrough bug.
   - Change: test now asserts strip-present bare arrows navigate the strip, then clears entries and proves input history and child scroll remain reachable.

10. `dispatch_child_view_scroll_keys_only_when_child_focus_and_empty_input`
    - Root cause: code bug for no bottom entries plus stale ambiguity.
    - Change: child focus with no bottom entries scrolls; child focus with entries navigates sessions.

11. `alt_arrows_move_ui_only_strip_selection_without_focus_change`
    - Root cause: code bug.
    - Change: Alt arrows now use `move_strip_selection` at `src/chat/tui.rs:1612`, so the synthetic main row is excluded from Alt strip wrap. Existing test unchanged.

12. `fullscreen_footer_hides_completed_sessions_from_active_bottom_list`
    - Root cause: code bug.
    - Change: `session_footer_has_sessions` at `src/chat/tui.rs:3997` now depends on active bottom entries, so completed/history-only sessions restore the normal footer instead of rendering a main-only session list.

## Validation

Before: task/Claude full-suite diagnosis reported 12 failed tests from `cargo test -p openprx --bin prx --all-features`.

After:

- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features`
  - `test result: ok. 5450 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out`

Targeted checks also passed:

- `input_history` filter: 3 passed.
- `dispatch_child_view_scroll_keys_only_when_child_focus_and_empty_input`: passed.
- `bare_arrows_history_cursor_and_child_scroll_are_not_stolen_by_strip_selection`: passed.
- `sessions_strip_one_active_entry_shows_marker_glyph_kind_and_title`: passed.
- `sessions_strip_active_entry_drives_window_when_no_selection`: passed.
- `s2_5_p1_b_assistant_turn_carries_tool_calls`: passed.
- `provider_worker_status_update_refreshes_open_worker_view_with_io`: passed.
- `alt_arrows_move_ui_only_strip_selection_without_focus_change`: passed.
- `fullscreen_footer_hides_completed_sessions_from_active_bottom_list`: passed.
- `fullscreen_scroll_focus_rules_do_not_steal_input_or_child_keys`: passed.
- Preserved B-contract tests: `dispatch_directional_session_switching_obeys_focus_input_matrix`, `phase2_bottom_direction_selects_provider_worker_and_enter_opens_worker_view`, `provider_worker_focus_direction_and_esc_are_read_only_view_controls`: all passed.

Gates passed with zero warnings:

- `cargo fmt --check`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy --all-targets --all-features -- -D warnings`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check --all-features`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features`

`git diff --check` passed.
