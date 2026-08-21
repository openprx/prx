# Task 2026-07-11 — Chat 键模型重排实现（轴分离）

**基线**：分支 `feat/chat-ux-fu2-fu3-polish` HEAD `3eb7380f`（UX 子集已提交，叠在其上，别 rebase）。**别 git commit**（提交我来做）。全程 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`。别自提交。

## 必读设计（权威规范，逐条实现）
`/opt/worker/task/prx/design-chat-key-model-rearrange-2026-07-11.md` —— 完整键映射表 S1-S7、方案B契约 diff、实现影响、file:line 锚点全在里面。**两处用户已拍板**：§3.4=(A) 保持竖直列表 + Left/Right（不改渲染）；§4=删除 Alt。

## 核心（轴分离，对标 Claude Code）
**Up/Down = 滚当前主视口/子视口 transcript；Left/Right = 沿底部会话条立即切换。全焦点态统一。** 输入历史迁 Ctrl+P/N。

## 实现要点（详见设计 §6）
1. **`dispatch_global_key`（tui.rs:1461）重排**：
   - 删 Main-empty 选中块（1557-1599）→ 改：Left/Right 复用 Session/Worker 的"立即切换"逻辑（`move_bottom_list_selection(…,None,…)`）；Up/Down → 新 `ScrollTranscriptUp/Down`。
   - **删 Alt 分支（1600-1641）** + 退役 `strip_selection`：连带删 `StripSelectionChanged` dispatch 变体、Esc 先清 strip_selection 分支、`move_strip_selection`/`strip_selection_index`（若无其他调用者）、`strip_selection` 字段。列表高亮/窗口锚改用 `focus_active_entry_seq(focus)`（`render_sessions_list_lines` 已有 fallback，去掉 strip_selection 优先级）。
   - Session/Worker focus（1674-1743）：方向匹配从"全箭头立即切"改为**仅 Left/Right 立即切**；Up/Down 交给滚子视口（Session→`ScrollSessionUp/Down` 已有；Worker→新增 `ScrollWorkerUp/Down`，视口 `build_provider_worker_active_view_with_io_preserving_scroll` mod.rs:8305/11328 已有，接线）。
   - child-view（1744-1752）：Up/Down 滚子视口保留；补 Left/Right 立即切条（entries 非空时）。
2. **新增滚动**：`FullscreenTranscriptScroll::line_up/line_down`（tui.rs:3949，用 saturating 防溢出，到顶/底 no-op）+ loop 内 `ScrollTranscriptUp/Down` 映射。
3. **input 历史迁移**（handle_key tui.rs:3011-3037）：删 ↑↓ 的 history_prev/next（改：多行移光标、单行 no-op/Unhandled）；新增 `Ctrl+P`/`Ctrl+N` → history_prev/next（标准 readline，存草稿）。
4. **PageUp/Down/Home/End 改 focus-aware**（mod.rs:11981-12006）：Main→主 transcript、child-view→子视口，消除现存"child-view 下 PageUp 被主 transcript 抢"的潜伏不一致。
5. **⚠️ key-repeat 去抖（立即切换核心风险）**：按住/auto-repeat Left/Right（`KeyEventKind::Repeat` mod.rs:11941 被当权威输入）会连发 SwitchSession/AttachSession → 真实子会话 attach/detach churn + 多个 synthetic `/detach`。**真实会话 attach 必须去抖**（每 ~80-120ms 单步，或中途仅移焦点游标、settle 后才提交实际 attach effect）；worker 视图切换/游标移动只读廉价，可不去抖。给去抖专测（连发 Repeat 不产生 N 次 attach）。

## ⚠️ 与子集协调：改正 footer 文案（必做）
子集（`3eb7380f`）加的 footer 提示是 `↑↓ sessions · Enter attach/open`——**在新模型下语义错了**（↑↓ 现在是滚动、←→ 才切会话、Enter 不再 attach）。**必须更新**为反映新键义，用设计 §7 的 token：
- Main 态：`↑/↓ scroll · ←/→ session · Ctrl+P/N history · Ctrl+G sessions · Esc cancel`（按实际空间裁剪，别写错语义）
- child-view 态：`↑/↓ scroll · ←/→ switch · Esc back`
改子集加的那个 hint helper（tui.rs:4701/4758/7137 区），别新开第二套。

## 测试（设计 §6，强制全量）
- **重写**：~21 处 `StripSelectionChanged` + 7 处 `SwitchSession` 断言；`alt_arrows_*`/`alt_enter_*`（tui.rs:10460-10570）、`esc_clears_strip_selection`（10572）、`bare_arrows_history_cursor_and_child_scroll_*`（10591）、`dispatch_child_view_scroll_keys_only_when_child_focus`（10177）全按新语义改写。
- **新增**：S1 Up/Down=ScrollTranscript + Left/Right=立即 Switch；S3/S5 单行 Up/Down no-op；Ctrl+P/N=历史；S4 Up/Down=ScrollSession + Left/Right 切换；Worker 滚动；去抖专测。给关键项 mutation 证牙。
- **强制全量** `cargo test -p openprx --bin prx --all-features`（铁律：过滤子集曾漏 12 个既有失败）。

## 验收（receipt 写 `collab-outbox/receipt-2026-07-11-chat-key-model-rearrange.md`，别 commit）
逐条实跑贴结果：
1. fmt / clippy `-D warnings` / check（双 feature）
2. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 passed/failed；新增/改写测试列名 + 关键 mutation 证牙
3. `cargo audit` + `cargo deny check advisories` 双绿
4. **真机 PTY demo**（对标方法）：①Up/Down 滚主 transcript（贴滚动前后截录）②Left/Right 立即切会话/worker（无需 Enter）③Ctrl+P/N 走输入历史④**按住 Left/Right 连发不产生 attach 风暴**（去抖证据）⑤footer 文案已改正为新语义。跑不动说清卡点别假装。
5. 明确写：**未 commit**、各改动点状态、去抖实现方式、footer 是否改正、真机结果。

铁律：零 unwrap/expect（生产码）、零 warning、English 代码/commit。**去抖是本单最高风险，务必守住"连按不产生 attach 风暴"**。别自 commit。
