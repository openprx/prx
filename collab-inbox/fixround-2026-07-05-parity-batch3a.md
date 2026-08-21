# [派工] Batch 3a fix-round（F6/F7/F8 各窄返工）

审计裁决：F6/F7/F8 均 FIX-ROUND（各自范围很小）。**顺序：先做完 Batch 3b 并写 receipt，再做本 fix-round，然后才 3c**。仍禁止 push。

## ⚠️ 流程规范（本轮起执行）

3a receipt 的测试证据被 3b 未提交 WIP 污染（测试二进制晚于脏文件 mtime，且当时脏树编译不过）。**以后每个 receipt 的自检必须在已 commit 的干净状态下跑**（先 commit 再跑 fmt/clippy/test，或在干净 worktree 跑），receipt 里写明验证时的 HEAD 与 `git status` 干净与否。

## F6 必修（一个 commit）

1. **预览截断换宽度感知**：`push_folded_tool_result_preview`（tui.rs:4851 区）现用 `clamp_one_line`（chars 计数 + split_whitespace 折叠缩进）——180 字符在 80 列下 wrap 成 ~9 视觉行、CJK 翻倍、代码缩进失真。换 `truncate_chars_with_ellipsis`（UnicodeWidth，同文件现成）按**列宽**截，且保留原始空白（代码预览不折叠缩进）。
2. **展开卡加 verbose 线索**：截断尾注（tui.rs:4937-4944）现只有 "… truncated: N lines · X hidden"——加 "(Ctrl+O for full transcript)" 之类提示，否则用户不知道完整内容在哪。
3. **补测试**：`+N lines` 尾注分支（现有测试 result 恰 3 行 hidden=0 不触发）；error 卡预览断言（现零断言，且注意 error 首行与统计行重复展示——顺手去重或接受并注明）。

## F7 必修（一个 commit）

1. **两条 handoff 路径补 keyboard-enhancement 对称**：
   - 外部编辑器：`suspend_for_editor`（mod.rs:6741-6747）补 PopKeyboardEnhancementFlags、`restore_after_editor`（6749-6755）补 re-push（仅当 `keyboard_enhancement_active`）。
   - PTY attach：`write_chat_alt_screen_leave_for_handoff`（pty.rs:331）/`write_handoff_terminal_restore`（pty.rs:341）/`PtyHandoffGuard::drop` re-assert（pty.rs:284-286）同样补 pop/re-push（`CSI < u` 序列）。
   - 后果背景：不补的话 vim/nano/less 等进 alt-screen 的子程序落在推过 flags 的栈上，键盘全乱码。
2. **补 `fail_push` 回滚测试**：`fail_push_keyboard_enhancement` 旋钮（mod.rs:10054）存在但无测试置 true——push 失败事务回滚现在无牙。

## F8 必修（一个 commit）

1. **[MED·头号] Legacy 三条路径 guard/audit 源改 persisted**：`/compact`（mod.rs:3944）、legacy preflight（~5757）、legacy overflow（~5940）现调 `build_configurable_compaction_patch(&history,…)`——guard 源是 enriched legacy history（记忆前置+工具消息），reducer 校验的是 persisted 镜像 → **任何用过工具/记忆的会话 guard 必 mismatch**，镜像不同步、摘要不持久化（ISS-043 收益丢失）、双历史分歧加剧。对齐 Redux 的做法：改调 `build_configurable_compaction_patch_with_source_history`（budget 用 enriched、guard/audit 用 persisted 源——overflow 作用域 `persisted_history_for_turn` 现成）。overflow 摘要成功分支恢复 persisted 源审计（ISS-042 约定）。**补一个"legacy 会话带 tool 消息压缩后 reducer guard 命中"的有牙测试**（现在这个场景 guard 必 miss，测试应在修复前红）。

## F8 应修（SHOULD，可同 commit 或并入 3c）

2. `/compact` await 摘要前发一条 "Compacting conversation…"（消除最长 300s 无反馈）。
3. 补 feedback 发射测试（Redux preflight/overflow SystemMessageAdded + legacy preflight 块）。
4. Redux 驱动 mid-turn 压缩后补发 ContextWindowUpdated（对齐 legacy 立即回落）。
5. 低于阈值的 `/compact` fallback 改 noop（防连续 /compact 把摘要 char-trim 截毁）；`ui_dirty_for` 的 HistoryCompactionPatchApplied 过时注释修正。

## 追加必修：F10 跟进（Batch 3b 审计结论，可并入本 fix-round 或单独 commit）

- **[MED] reducer ALT 块顺序对齐**：reducer 的 Alt 方向键/Alt+Enter 块（state.rs:904 区）位于 slash_menu 分支（:941）之前，且 saved_session_picker 在 reducer 侧只拦 Esc（:884）；mirror 是 picker/slash_menu 全键捕获在 ALT 块之前（tui.rs:1152 区 vs 1224）。分歧：slash 菜单开着按 Alt+Up，mirror 走菜单导航、reducer 却动 strip_selection；菜单/picker 开着按 Alt+Enter，mirror Consumed、reducer 可能 push "session gone"（Snapshot 渲染下用户可见）。修法：reducer ALT 块挪到 slash_menu 分支之后 + 补 picker 全键 guard，加 overlay-open 场景的双路径 parity 测试。

## 追加进 3c 跟进包（记录，不在本轮）

- transcript view 滚动全量 clone（每滚动事件 view clone ×3，无界后数 MB 会话卡顿）→ `Arc<[String]>` 或 offset-only action。
- footer 文案挤掉 Ctrl+R/Shift+Tab 提示的发现性问题；keyboard lifecycle 测试与 panic 测试的窄 flake 窗口。
- Redux preflight 卡超限时每 turn 重发 "nothing to drop" 的去重。
- （3b 审计新增）F9 补"旧 JSON 缺 cache 字段反序列化"回归测试；F10 补 reap 清除测试 + reducer 侧无选中 Alt+Enter fall-through 换行测试；F12 resize/折叠时行索引锚漂移（如追求内容级锚记消息 id+行内偏移，属增强）。

## Receipt

`collab-outbox/receipt-2026-07-05-parity-batch3a-fixround.md`：commit（F6/F7/F8 各一个）、测试名、干净状态自检、二进制路径。
