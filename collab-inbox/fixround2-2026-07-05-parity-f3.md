# [派工] F3 第二轮 fix-round（窄范围，2 点）；F5 已 ACCEPT

复审裁决：F5 `1ba6e3a4` ACCEPT ✅；F3 `09d9eb10` 其余 4 点达标（Esc 优先级/parity 测试/假 elapsed/ResolveApproval 安全语义均过），仅剩 2 个窄修点。仍禁止 push。

## 必修（一个 commit）

1. **[HIGH] Ctrl+C 取消审批后 mirror 悬挂 → 静默丢用户消息**：
   - 现状：CancelRequested 清了 reducer 侧 pending approval 并 ResolveApproval(deny)，但 **legacy mirror(TuiState) 的 `pending_tool_approval` + `focus=Approval` 无人清除**（生产里只有 dispatcher.rs:1020 置 Some、tui.rs approval 分支 y/n/Esc 清、mod.rs:8326/8505 session 切换清）。mirror 是唯一提交通路（`KeyDispatch::Submitted` → input_tx，mod.rs:7629），Ctrl+C 后续按键全被 mirror approval 分支 `Consumed` / paste 被 approval_active guard（mod.rs:7560/7904）吞掉——**用户下一条消息回车后静默丢失**（snapshot 输入框还清空，看似已发送），直到误按 y/n/Esc 或切 session。
   - 修法：Ctrl+C/CancelRequested 派发处同步清 mirror（如 mod.rs:7665 InterruptTurn 分支加 mirror.lock() 清 `pending_tool_approval`+归位 focus）。
   - 测试：①修正 `approval_child_ctrl_c_keeps_global_interrupt_semantics`——它现在把「mirror 保持 Some」固化成契约，是错的；②新增"Ctrl+C 取消审批后，下一条消息能正常 Submitted 到 input_tx"的测试（有牙：不清 mirror 必红）。
2. **[LOW] 删死分支**：tui.rs:1272-1274 `Char('c')+CONTROL → InterruptTurn` 被 1153 行顶置同款 guard 全遮蔽，纯死代码（上轮派工点名项，漏删）。

## 可选顺手（单独 commit，不强制）

- `/compact` 分支（mod.rs:3728 区）补发 `ContextWindowUpdated`（约 4 行），让手动压缩后状态栏 ctx% 立刻回落（现在滞后到下一 turn）。
- spinner wall-clock tick（用已有 50ms idle poll 捎带 tick 字段，让等待期/工具期 glyph 真转起来）——也可留到 P1。

## Receipt

写 `collab-outbox/receipt-2026-07-05-parity-f3-fixround2.md`：commit、测试名、自检、二进制路径。写完 receipt 即可，无需 tmux 通知。
