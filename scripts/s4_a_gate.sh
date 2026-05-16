#!/usr/bin/env bash
# S4-A Renderer source switchover — grep guard.
#
# 在 Pure 模式下 chat_mirror 应零写入：所有 push_* 调用必须先经过
# `ReduxMode::from_env().is_pure()` 守卫跳过。本脚本用 grep 验证守卫存在。
#
# 用法: ./scripts/s4_a_gate.sh
# 通过条件: chat_mirror.lock().push_* / tui_mirror.lock().push_* 调用至少
# 有 4 处被 `is_pure()` 守卫包裹（banner / user echo / slash / reasoning）.

set -euo pipefail

cd "$(dirname "$0")/.."

echo "[S4-A gate] grep chat_mirror.lock() / tui_mirror.lock() push 调用 ..."
MIRROR_PUSH_COUNT=$(grep -c "chat_mirror\.lock()\.push_\|tui_mirror\.lock()\.push_" src/chat/mod.rs || true)
echo "  mirror push 调用数: $MIRROR_PUSH_COUNT"

echo "[S4-A gate] grep is_pure() 守卫 ..."
GUARDED_COUNT=$(grep -B 3 "chat_mirror\.lock()\.push_\|tui_mirror\.lock()\.push_" src/chat/mod.rs | grep -c "is_pure" || true)
echo "  is_pure() 守卫数 (前 3 行内): $GUARDED_COUNT"

if [ "$GUARDED_COUNT" -lt 4 ]; then
    echo "[FAIL] Pure 守卫数 $GUARDED_COUNT < 4，存在未守卫的 mirror push 旁路写。"
    echo "       检查 src/chat/mod.rs 中的 chat_mirror.lock().push_* / tui_mirror.lock().push_* 调用."
    exit 1
fi

echo "[PASS] Pure 守卫数 = $GUARDED_COUNT >= 4，所有已知 mirror push 路径已加 Pure 守卫."
