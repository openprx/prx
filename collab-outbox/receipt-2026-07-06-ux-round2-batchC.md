# Receipt: UX Round2 Batch C

Date: 2026-07-06
Scope: Batch C (S1/C1)
Push: not pushed

## Commits

- S1 `6b38e69d fix(chat): base idle warning on last activity`
- C1 `c7c72ef1 feat(chat): add copy command for assistant replies`

## Changes

### S1 idle warning last-activity

- `SessionRing` now records `last_pushed_at`; tests can inject timestamps with `push_at`.
- `idle_warning_seqs` now uses last output activity:
  - agent sessions: `SessionRing::last_pushed_at`, fallback to `started_at`.
  - shell sessions: `SessionRing::last_pushed_at`, fallback to `started_at`.
  - PTY sessions: PTY drain sink `last_output_at`, fallback to `started_at`.
- Main chat session tick passes the live `session_rings` map into the idle-warning calculation.
- Continuous output from an old long-running agent no longer marks idle; no output for the warning window still marks idle.

### C1 /copy

- Added `/copy [N]` to `COMMAND_SPECS`, so slash menu/help registry sees it.
- `/copy` selects the latest assistant turn; `/copy N` selects the Nth latest assistant turn.
- TUI/non-plain path writes OSC 52 clipboard content via existing `terminal_proto::copy_to_clipboard`.
- Plain mode prints the selected raw assistant markdown directly, without OSC.
- OSC 52 payload source is truncated at a UTF-8 boundary to 74 KiB with a visible truncation message.

## Validation

Receipt self-check was run after all Batch C code commits with clean staged/tracked code state:

```text
git diff --quiet && git diff --cached --quiet
cargo test --bin prx idle_warning -- --nocapture
cargo test --bin prx copy_command -- --nocapture
cargo test --bin prx command_registry_covers_all_known_parser_commands -- --nocapture
cargo check --workspace --no-default-features
```

Result: all passed.

## Notes

- Receipt file is intentionally uncommitted.
