#!/usr/bin/env bash
# S4-A / S4-B Renderer source switchover — grep guard.
#
# After S4-B: every chat_mirror.lock().push_* / tui_mirror.lock().push_* call
# must be gone, so the whole mirror write path no longer exists (Pure is the
# only runtime path).
#
# During S4-A: each of those pushes must be wrapped by an is_pure guard (the
# nearest is_pure guard is at most 10 lines above each mirror push).
#
# Usage: ./scripts/s4_a_gate.sh
# Pass when the mirror-push count is 0 (S4-B complete) or every push is guarded
# (S4-A).

set -euo pipefail

cd "$(dirname "$0")/.."

# chat/mod.rs was split into these files; the gate covers all of them.
FILES=(src/chat/mod.rs src/chat/active_turn.rs src/chat/bootstrap.rs src/chat/chat_events.rs src/chat/compaction.rs src/chat/copy_select.rs src/chat/external_editor.rs src/chat/file_mention.rs src/chat/input_queue.rs src/chat/output.rs src/chat/provider_turns.rs src/chat/pty_attach.rs src/chat/run.rs src/chat/run/*.rs src/chat/session_glue.rs src/chat/session_store.rs src/chat/session_switch.rs src/chat/terminal.rs src/chat/tui_loop.rs src/chat/views.rs)

for file in "${FILES[@]}"; do
    if [[ ! -s "$file" ]]; then
        echo "[FAIL] missing or empty chat source: $file"
        exit 1
    fi
done

echo "[S4-A/B gate] grep chat_mirror.lock() / tui_mirror.lock() push calls ..."
MIRROR_PUSH_COUNT=$(cat "${FILES[@]}" | grep -c "chat_mirror\.lock()\.push_\|tui_mirror\.lock()\.push_" || true)
echo "  mirror push call count: $MIRROR_PUSH_COUNT"

if [ "$MIRROR_PUSH_COUNT" -eq 0 ]; then
    echo "[PASS] S4-B complete: every mirror push call is gone (Pure is the only runtime path)."
    exit 0
fi

# S4-A: check guards
echo "[S4-A gate] grep heuristic is_pure() / is_pure guards (within the previous 5 lines)..."
GUARDED_COUNT=$(cat "${FILES[@]}" | grep -B 5 "chat_mirror\.lock()\.push_\|tui_mirror\.lock()\.push_" | grep -c "is_pure" || true)
echo "  is_pure guard count (heuristic, within previous 5 lines): $GUARDED_COUNT"

if [ "$GUARDED_COUNT" -lt "$MIRROR_PUSH_COUNT" ]; then
    echo "[FAIL] heuristic Pure guard count $GUARDED_COUNT < mirror push count ${MIRROR_PUSH_COUNT}; an unguarded bypass write remains."
    exit 1
fi

echo "[S4-A gate] AWK block scan: each push is at most 10 lines from the nearest 'if !...is_pure' guard ..."
UNGUARDED_OUT=$(awk '
    BEGIN { guard_line = 0; unguarded = 0 }
    FNR == 1 { guard_line = 0 }
    /if \!.*is_pure|if .*is_pure\(\)/ {
        guard_line = NR
    }
    /chat_mirror\.lock\(\)\.push_|tui_mirror\.lock\(\)\.push_/ {
        dist = NR - guard_line
        if (guard_line == 0) {
            print "  UNGUARDED push at " FILENAME ":" FNR " (no prior is_pure guard)"
            unguarded++
        } else if (dist > 10) {
            print "  push at " FILENAME ":" FNR " too far from latest is_pure guard at line " guard_line " (" dist " lines apart)"
            unguarded++
        }
    }
    END { exit (unguarded > 0 ? 1 : 0) }
' "${FILES[@]}") || {
    echo "$UNGUARDED_OUT"
    echo "[FAIL] AWK block scan found an unguarded mirror push (see above)."
    exit 1
}

echo "[PASS] S4-A: mirror push count = ${MIRROR_PUSH_COUNT}, heuristic guard count = ${GUARDED_COUNT}, AWK block scan passed."
