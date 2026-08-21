# Receipt: UX Round2 Batch B

Date: 2026-07-06
Scope: Batch B (I1/I2/I3)
Push: not pushed

## Commits

- I1 `9e2c6251 fix(chat): add at-path completion menu`
- I2 `db9362ba fix(chat): fold large pastes in input`
- I3 `ac15c973 fix(chat): soft-wrap input rendering`

## Changes

### I1 @path completion menu

- Added shared `AtPathCandidate` source type and `Action::AtPathCandidatesUpdated`.
- Added word-start `@path` cursor context alongside the existing SlashMenu context.
- Reused SlashMenu overlay/navigation for `@path` candidates:
  - Tab/Enter inserts the selected path.
  - File candidates append a trailing space.
  - Directory candidates keep the trailing `/` and do not append a space, so drilldown can continue.
  - Esc closes the menu without immediately reopening.
- TUI loop now enumerates relative workspace candidates through `SecurityPolicy::is_path_allowed` and `SecurityPolicy::is_resolved_path_allowed` before dispatching sources.
- Redux path consumes candidates only via `AtPathCandidatesUpdated`, avoiding empty reducer sources.
- Candidate filtering supports substring and fuzzy subsequence matching, caps at 50, and sorts directories first.

### I2 paste chip and input scroll

- Large paste threshold: folds when paste is `>5` lines or `>1024` bytes.
- Visible draft inserts `[Pasted text #N: M lines]` chips while retaining original paste payload in `TuiInput`.
- `TuiInput::text()` expands chips, so submit/history uses original content.
- Multiple paste chips increment per draft.
- 32KB cap still applies to expanded input payload; truncation remains visible through the input title.
- Draft snapshots for history navigation and reverse search now preserve paste chip metadata.
- Input rendering now scrolls the visible logical-line window to keep the cursor visible once input exceeds `INPUT_MAX_VISIBLE_ROWS`.

### I3 input soft wrap

- Added Unicode display-width aware soft wrap for input rendering.
- Fullscreen bottom chrome now reserves height based on wrapped visual rows at the current terminal width, capped by `INPUT_MAX_VISIBLE_ROWS`.
- Cursor placement maps to the wrapped visual row and wrapped column, including CJK/wide characters.
- Wrapped input scrolls by visual rows, so a long single-line draft follows the cursor tail.

## Validation

Receipt self-check was run after all Batch B code commits with clean staged/tracked code state:

```text
git diff --quiet && git diff --cached --quiet
cargo test --bin prx at_path -- --nocapture
cargo test --bin prx paste -- --nocapture
cargo test --bin prx input_wrap_ranges_respect_unicode_display_width -- --nocapture
cargo test --bin prx long_single_input_line_uses_wrapped_chrome_height -- --nocapture
cargo test --bin prx render_input_soft_wrap_scrolls_to_cursor_tail -- --nocapture
cargo test --bin prx render_input_scrolls_visible_window_to_cursor -- --nocapture
cargo test --bin prx slash_menu_sources_match_legacy_and_redux_for_same_keys -- --nocapture
cargo check --workspace --no-default-features
```

Result: all passed.

## Notes

- Plain mode has no render surface for these UI overlays; submission content remains expanded through `TuiInput::text()` on the TUI path.
- Receipt file is intentionally uncommitted.
