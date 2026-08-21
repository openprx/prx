# [派工] Batch 1 审计 fix-round（F3 + F5 必修；F1/F2 已 ACCEPT）

审计裁决：F1 `2c3a879b` ACCEPT、F2 `508e93e9` ACCEPT、**F3 `b8dc36a6` FIX-ROUND、F5 `42e49451` FIX-ROUND**。
优先级：**若 F4 (Batch 2) 正在进行，先收尾 F4 并写 batch2 receipt，然后立即做本 fix-round**；Batch 3 (P1) 继续冻结，等 acceptance。仍然禁止 push。

## F3 必修（一个 commit）

1. **[HIGH] Redux Esc-generating 优先级越位**：`reduce_key_pressed` 的 Esc+generating 分支（state.rs:868 区）位于 approval（:875）/slash（:879）分支**之前**，而 legacy `resolve_esc` 的 generating 判定在 approval/switcher/strip 之后。后果：
   - approval modal 按 Esc：legacy 发 deny，redux 同帧 CancelToken 砍整 turn（审批必然在 generating 中，每次弹窗必中）；
   - 生成中开着 switcher/slash 菜单按 Esc：legacy 关 overlay，redux 误取消 turn，且 redux 侧 `ui.slash_menu` 残留不关。
   修法：redux Esc-generating 下移到 approval/slash/switcher/strip 分支之后（对齐 resolve_esc 既定优先级）。**补双路径 Esc 优先级 parity 测试**：①approval modal 开 + generating 按 Esc → 两路径都是 deny 不取消 turn；②generating + slash 菜单开按 Esc → 两路径都只关菜单。
2. **[必修] elapsed 恒 `0s` 是硬编码假值**（`render_generation_activity` 的 `"generating 0s"` 字面量）：二选一——TurnStarted 时间戳进 snapshot 算真 elapsed，或删掉 elapsed 字段只留 `spinner + (esc to interrupt)`。禁止保留常量假显示。同步改 `status_bar_shows_generation_interrupt_hint`（现在锚死 `"generating 0s"`）。
3. **[MED] approval 场景 Ctrl+C 语义链**：①清理死代码——legacy approval 分支的 Ctrl+C deny 已被顶置 InterruptTurn 拦截不可达、tui.rs:1278 旧 Ctrl+C 分支不可达（铁律 2）；②取消 turn 后 `pending_tool_approval` 无清除点（ToolApprovalCleared 只在 session 切换路径派发）→ 修复 modal 悬挂：CancelRequested/turn 终止时清除 pending approval（并给 approval channel 一个明确 deny/abort），加测试。
4. **[LOW] spinner 驱动偏离**：帧现由 `streaming.version % 4`（delta 驱动）——首 token 等待/长 thinking 帧冻结、工具执行阶段整段消失，与派工"1s tick 驱动 + 工具 Running ● 轮转"不符。改为 tick 驱动（或 tick+delta 混合）并让工具执行阶段活动指示不消失；若认为 delta 驱动是合理折衷，必须在 receipt 明示偏离及理由（这次没报）。

## F5 必修（一个 commit）

1. **[HIGH] ctx% 分子数据源错误**：`render_context_budget_usage` 用 `MainSessionTokenUsageSummary.total_tokens`（会话累计、只增不清、每轮重发全量历史 → 近似平方增长；128k 窗口 13 轮后显示 `ctx:102%!` 而实际 ~8%，compaction 后不回落）。改为**当前上下文实测**：`plan_context_budget` 的 used_tokens（派工原指定），或最近一次请求的 prompt+completion 近似；加 100% 封顶；语义标注清晰（used 或 left 二选一，别误导）。
2. **[HIGH 连带] 处置 `#[cfg(test)]` 死函数**：`context_budget_warning_for_tui` 零生产调用者，`plain_mode_suppresses_context_budget_warning_chrome` 在验证已删除的行为（绿测撒谎）。二选一：基于修正后的 ctx% 恢复准确超限提示（推荐，TUI 用户现在失去了准确预警），或删死函数+误导测试段。
3. **[必修] 补 ctx% chrome 的 plain no-render 测试**（验收门 #2）。

## 非阻塞跟进（可顺手做，不强制，单独 commit）

- F2：`slash_menu_only_triggers_at_first_line_start` 补牙（改用第二行 `/he` 有匹配 filter 才咬得住守卫）；`slash_menu_overlay_rect` 补几何测试（锚定/高度/窄端不越界）；删两个零调用空源 helper（tui.rs:661/830 区）+ 不可达 "No matching" 渲染段；`/save` 后刷新 saved_sessions 缓存；启动双次 `saved_chat_sessions()` 合并 + `if let Ok` 吞 Err 加 warn。
- F1：补 wiring 级测试（surface_turn_elapsed_message 直连 fallback 会红的那种）。

## Receipt

写 `collab-outbox/receipt-2026-07-05-parity-batch1-fixround.md`：commit hash、测试名、自检（fmt/clippy --all-targets/test --bin prx）、二进制路径、偏离说明。
