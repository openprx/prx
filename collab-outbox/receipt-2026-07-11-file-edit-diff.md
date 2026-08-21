# Receipt: chat file edit/write diff rendering

Task: `collab-inbox/task-2026-07-11-chat-file-edit-diff-rendering.md`

Baseline: `974452b4` on branch `feat/chat-file-edit-diff-rendering`

Commit: not created, per instruction.

## Changes

### Tool-layer diff output

- Added `similar = "2.7"` dependency in `Cargo.toml:85`; `Cargo.lock` updated.
- Added shared tool diff helpers in `src/tools/tool_diff.rs:8` and exported module at `src/tools/mod.rs:57`.
- `build_unified_diff` now emits `--- a/...`, `+++ b/...`, and `@@ -old,+new @@` hunk ranges with context via `similar`.
- `build_new_file_preview` emits a bounded line-numbered preview for newly created files.
- `file_edit` now uses shared unified diff generation at `src/tools/file_edit.rs:266`.
- `file_write` now reads existing contents before write at `src/tools/file_write.rs:155`, emits overwrite diffs at `src/tools/file_write.rs:195`, and emits new-file previews at `src/tools/file_write.rs:197`.
- `file_write` keeps symlink refusal behavior while distinguishing new vs overwrite through `read_existing_file_for_diff` at `src/tools/file_write.rs:227`.

### TUI rendering

- Made `renderer::is_diff_block` crate-visible at `src/chat/renderer.rs:215` so tool cards can reuse the existing diff detector.
- `file_edit` and `file_write` tool cards default expanded through `tool_result_defaults_expanded` at `src/chat/tui.rs:3493`.
- Fixed both mirror path and Redux reducer path:
  - TUI state start path uses the default-expanded policy at `src/chat/tui.rs:3354`.
  - Redux `ToolStarted` reducer uses the same policy at `src/chat/state.rs:2040` and `src/chat/state.rs:2043`.
- Folded and expanded tool bodies route file diffs through `render_diff_block` + `ansi_sgr_to_lines`:
  - Folded preview hook: `src/chat/tui.rs:6181`.
  - Expanded body hook: `src/chat/tui.rs:6272`.
  - Diff-body conversion helper: `src/chat/tui.rs:6333`.
  - Diff start finder: `src/chat/tui.rs:6356`.
  - Prefix-preserving line stitcher: `src/chat/tui.rs:6372`.

## Tests Added/Updated

- `src/tools/tool_diff.rs:80`: unified diff includes file headers, hunk header, context, minus, and plus lines.
- `src/tools/tool_diff.rs:92`: new file preview includes line numbers and caps rows.
- `src/tools/file_edit.rs:407`, `src/tools/file_edit.rs:480`, `src/tools/file_edit.rs:631`: updated `Applied N replacement` tests for new line-numbered/context diff shape.
- `src/tools/file_write.rs:323`: new-file write includes `/dev/null`, target header, and numbered preview.
- `src/tools/file_write.rs:362`: overwrite emits unified diff.
- `src/tools/file_write.rs:464`: new-file preview is bounded.
- `src/chat/tui.rs:7949`: file edit/write cards default expanded.
- `src/chat/tui.rs:8217`: rendered file_edit diff contains colored spans for hunk/minus/plus.
- `src/chat/tui.rs:8242`: folded file_write diff preview preserves colored spans.
- `src/chat/state.rs:5582`: Redux reducer path default-expands `file_edit`/`file_write` while keeping `shell` folded.

## PTY Demo

Binary: `/opt/worker/tmp/prx-target/debug/prx`

Config/workspace: `/opt/worker/tmp/prx-file-diff-demo-config`

All three demos used `PRX_TUI=1`, `--provider mock`, and mock native tool calls.

1. `file_edit`
   - Capture: `/opt/worker/tmp/prx-file-diff-edit.capture`
   - ANSI capture: `/opt/worker/tmp/prx-file-diff-edit.ansi`
   - Excerpt:
     - `✓ run file_edit(path=edit.txt, old_bytes=3B, new_bytes=3B)`
     - `⎿ input  path=edit.txt, old_bytes=3B, new_bytes=3B`
     - `⎿ output ✓ 9 lines · 103B`
     - `--- a/edit.txt`
     - `+++ b/edit.txt`
     - `@@ -1,3 +1,3 @@`
     - `-old`
     - `+new`
   - ANSI evidence:
     - hunk header captured with `38;5;6m`
     - minus line captured with `38;5;1m`
     - plus line captured with `38;5;2m`

2. `file_write` new file
   - Capture: `/opt/worker/tmp/prx-file-diff-write-new.capture`
   - ANSI capture: `/opt/worker/tmp/prx-file-diff-write-new.ansi`
   - Excerpt:
     - `✓ run file_write(path=new-demo.txt, bytes=11B)`
     - `Written 11 bytes to new-demo.txt`
     - `--- /dev/null`
     - `+++ b/new-demo.txt`
     - `@@ new file preview: first 40 lines @@`
     - `   1 | alpha`
     - `   2 | beta`

3. `file_write` overwrite
   - Capture: `/opt/worker/tmp/prx-file-diff-write-overwrite.capture`
   - ANSI capture: `/opt/worker/tmp/prx-file-diff-write-overwrite.ansi`
   - Excerpt:
     - `✓ run file_write(path=overwrite.txt, bytes=9B)`
     - `Written 9 bytes to overwrite.txt`
     - `--- a/overwrite.txt`
     - `+++ b/overwrite.txt`
     - `@@ -1,2 +1,2 @@`
     - `-old`
     - `+new`
   - ANSI evidence:
     - hunk header captured with `38;5;6m`
     - minus line captured with `38;5;1m`
     - plus line captured with `38;5;2m`

## Gates

- `cargo fmt --check`: passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy -p openprx --all-targets --all-features -- -D warnings`: passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --all-features`: passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features`: passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features`: passed, `5490 passed; 0 failed; 7 ignored`.
- `cargo audit`: passed, 0 exit code.
- `cargo deny check advisories`: passed, `advisories ok`.
- `cargo deny check bans licenses sources`: passed, `bans ok, licenses ok, sources ok`; existing duplicate warnings remain unrelated.

## Notes

- `similar` passed cargo-deny bans/licenses/sources in the final lockfile.
- The TUI default-expanded requirement needed one extra fix beyond the initial mirror path: the Redux reducer had its own `folded: true` initialization. That is now routed through the same `tool_result_defaults_expanded` policy.
- Existing non-file tools remain default folded.
