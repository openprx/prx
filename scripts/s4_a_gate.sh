#!/usr/bin/env bash
# S4-A Renderer source switchover — grep guard.
#
# Pure 模式下 chat_mirror 应零写入：所有 push_* 调用必须先经过
# `top_redux_mode.is_pure()` / `ReduxMode::from_env().is_pure()` /
# `is_pure` 变量守卫跳过。
#
# 用法: ./scripts/s4_a_gate.sh
# 通过条件:
#   1. mirror push 调用至少 4 处（banner / user echo / slash / reasoning）.
#   2. 每处 push 之前 5 行内出现 is_pure 守卫.
#   3. AWK 块级扫描：每处 push 距离最近 `if !.*is_pure` / `if !\w*is_pure`
#      守卫 <= 10 行（防御性 — 实际项目内 2-3 行内即贴合守卫）.

set -euo pipefail

cd "$(dirname "$0")/.."

FILE=src/chat/mod.rs

echo "[S4-A gate] grep chat_mirror.lock() / tui_mirror.lock() push 调用 ..."
MIRROR_PUSH_COUNT=$(grep -c "chat_mirror\.lock()\.push_\|tui_mirror\.lock()\.push_" "$FILE" || true)
echo "  mirror push 调用数: $MIRROR_PUSH_COUNT"

if [ "$MIRROR_PUSH_COUNT" -lt 4 ]; then
    echo "[FAIL] mirror push 总数 $MIRROR_PUSH_COUNT < 4，预期至少 4 处 (banner/user/slash/reasoning)."
    exit 1
fi

echo "[S4-A gate] grep 启发式 is_pure() / is_pure 守卫（前 5 行内）..."
GUARDED_COUNT=$(grep -B 5 "chat_mirror\.lock()\.push_\|tui_mirror\.lock()\.push_" "$FILE" | grep -c "is_pure" || true)
echo "  is_pure 守卫数 (启发式, 前 5 行内): $GUARDED_COUNT"

if [ "$GUARDED_COUNT" -lt "$MIRROR_PUSH_COUNT" ]; then
    echo "[FAIL] 启发式 Pure 守卫数 $GUARDED_COUNT < mirror push 数 $MIRROR_PUSH_COUNT，存在未守卫的旁路写。"
    echo "       检查 src/chat/mod.rs 中的 chat_mirror.lock().push_* / tui_mirror.lock().push_* 调用."
    exit 1
fi

echo "[S4-A gate] AWK 块级扫描：每处 push 距离最近 'if !...is_pure' 守卫 <= 10 行 ..."
UNGUARDED=$(awk '
    BEGIN { guard_line = 0; unguarded = 0 }
    # 守卫形式: if !is_pure / if !top_redux_mode.is_pure() / if .is_pure() { ... } else { push }
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
    echo "$UNGUARDED"
    echo "[FAIL] AWK 块级扫描发现失守 mirror push（见上方）."
    exit 1
}

echo "[PASS] mirror push 数 = $MIRROR_PUSH_COUNT，启发式守卫数 = $GUARDED_COUNT，AWK 块级扫描通过."
