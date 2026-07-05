# Receipt: parity Batch 3b (F9/F10/F11/F12)

Date: 2026-07-05
Repo: `/opt/worker/code/prx`
Push: not pushed
Binary: `/opt/worker/code/prx/target/debug/prx`

## Scope Completed

- F9 Anthropic cache tokens now flow from Anthropic responses/SSE into `TokenUsage`, `MeteredTokenUsageRecord`, session summaries, `/cost`, and export metadata.
- F9 `ModelPricing` now includes `cache_write`/`cache_read`; Anthropic default prices account for cache write/read separately while keeping cache tokens included in prompt-side totals.
- F10 strip selection now clears on session reap and dispatches `StripSelectionChanged { selected: None }`; stale Alt+Enter is consumed with `session gone` in both TUI and Redux reducer paths.
- F11 `src/tools/shell.rs` now runs shell commands as managed children with `kill_on_drop(true)`, piped output, Unix process groups, and drop-time process-group kill so abort/timeout tears down background children.
- F11 spawn-point review: chat shell already had pgid kill; `sessions_spawn` worker lifecycle already has explicit parent timeout control; MCP/message-send/git helper spawns are not the interactive shell tool path and were left unchanged.
- F12 fullscreen transcript scrolling now records a top-row content anchor while scrolled up; new output sets the footer hint without pushing the reader to newer rows. End/jump-bottom restores follow mode.

## Validation

- `cargo fmt --check` passed.
- `cargo clippy --all-targets -- -D warnings` passed.
- `cargo test --bin prx cache -- --nocapture` passed: 58 passed.
- `cargo test --bin prx alt_enter -- --nocapture` passed: 6 passed.
- `cargo test --bin prx shell_abort_kills_process_group -- --ignored --nocapture` passed: 1 passed.
- `cargo test --bin prx fullscreen_scrolled_transcript_keeps_content_anchor_when_output_arrives -- --nocapture` passed: 1 passed.
- `cargo test --bin prx -- --nocapture` passed: 5229 passed, 0 failed, 7 ignored.
- `cargo build --bin prx` passed.
- `git diff --check` passed.

## Deviations / Notes

- No push performed.
- F11 uses SIGKILL in the drop path because drop cannot await the chat shell's SIGTERM grace sequence; explicit timeout/abort coverage is provided by the ignored real-process test.
