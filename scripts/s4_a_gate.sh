#!/usr/bin/env bash
# S4-A / S4-B Renderer source switchover — grep guard.
#
# S4-B 完成后: chat_mirror.lock().push_* / tui_mirror.lock().push_* 应全部
# 删除，整条 mirror 写路径不复存在（Pure 是唯一运行路径）.
#
# S4-A 阶段: 上述 push 必须被 is_pure 守卫包裹（每处 mirror push 距离最近
# is_pure 守卫 <= 10 行）.
#
# 用法: ./scripts/s4_a_gate.sh
# 通过条件: mirror push 总数 == 0 (S4-B 完成) 或者守卫完整 (S4-A 阶段).

set -euo pipefail

cd "$(dirname "$0")/.."

FILE=src/chat/mod.rs

echo "[S4-A/B gate] grep chat_mirror.lock() / tui_mirror.lock() push 调用 ..."
MIRROR_PUSH_COUNT=$(grep -c "chat_mirror\.lock()\.push_\|tui_mirror\.lock()\.push_" "$FILE" || true)
echo "  mirror push 调用数: $MIRROR_PUSH_COUNT"

if [ "$MIRROR_PUSH_COUNT" -eq 0 ]; then
    echo "[PASS] S4-B 完成态: mirror push 调用已全部删除 (Pure 是唯一运行路径)."
    exit 0
fi

# S4-A 阶段：检查守卫
echo "[S4-A gate] grep 启发式 is_pure() / is_pure 守卫（前 5 行内）..."
GUARDED_COUNT=$(grep -B 5 "chat_mirror\.lock()\.push_\|tui_mirror\.lock()\.push_" "$FILE" | grep -c "is_pure" || true)
echo "  is_pure 守卫数 (启发式, 前 5 行内): $GUARDED_COUNT"

if [ "$GUARDED_COUNT" -lt "$MIRROR_PUSH_COUNT" ]; then
    echo "[FAIL] 启发式 Pure 守卫数 $GUARDED_COUNT < mirror push 数 $MIRROR_PUSH_COUNT，存在未守卫的旁路写。"
    exit 1
fi

echo "[S4-A gate] AWK 块级扫描：每处 push 距离最近 'if !...is_pure' 守卫 <= 10 行 ..."
UNGUARDED_OUT=$(awk '
    BEGIN { guard_line = 0; unguarded = 0 }
    /if \!.*is_pure|if .*is_pure\(\)/ {
        guard_line = NR
    }
    /chat_mirror\.lock\(\)\.push_|tui_mirror\.lock\(\)\.push_/ {
        dist = NR - guard_line
        if (guard_line == 0) {
            print "  UNGUARDED push at line " NR " (no prior is_pure guard)"
            unguarded++
        } else if (dist > 10) {
            print "  push at line " NR " too far from latest is_pure guard at line " guard_line " (" dist " lines apart)"
            unguarded++
        }
    }
    END { exit (unguarded > 0 ? 1 : 0) }
' "$FILE") || {
    echo "$UNGUARDED_OUT"
    echo "[FAIL] AWK 块级扫描发现失守 mirror push（见上方）."
    exit 1
}

echo "[PASS] S4-A 阶段: mirror push 数 = $MIRROR_PUSH_COUNT，启发式守卫数 = $GUARDED_COUNT，AWK 块级扫描通过."
