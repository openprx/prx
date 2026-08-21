# Task 2026-07-11 — Chat UX 无争议子集（F-U2 子进程条 + F-U3 键位补漏）

**基线**：分支 `feat/chat-ux-fu2-fu3-polish`（off main `92b596f5` v0.8.1，工作树干净）。**别 git commit**（提交我来做）。全程 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`。别自提交、别碰无关代码、别改导航语义（那部分等用户拍板另派）。

背景：这些是审计确认的**客观差距、低风险、不需用户决策**的项。审计报告见 `task/prx/execution-plan-open-followups-2026-07-11.md`（任务 B 节）。有争议的 B1-A 导航语义、主 transcript 滚动、行内展开/卡片布局**不在本单**。

## 改动 1 — B1-C：权限确认对话框加箭头选择
现状 `src/chat/tui.rs:1501-1526` 权限弹窗只认 `y/Y`、`n/N`、Esc，无箭头。Claude Code 的权限弹窗支持箭头选。
- 加 ←/→（或 ↑/↓）在 Yes/No（若有 Always 档则 Yes/Always/No）之间移动高亮，Enter 确认当前高亮项。
- **保留**现有 y/n/Esc 热键不变（只是增量加箭头路径）。
- 需要一个"当前高亮项"状态（默认落在最安全项，如 No/拒绝，或对齐现有默认）。给测试。

## 改动 2 — F-U2(a)：底部子进程行 running 态 spinner 动效
现状底部会话/worker 列表行（`src/chat/tui.rs:4497-4579` `render_sessions_list_entry_line`/`render_main_session_list_line`）running 态用**静态**图标 `⏳`（`src/chat/sessions/focus.rs:201-209` `status_glyph`）；只有顶部主 turn 状态栏有 braille spinner（`tui.rs:5464-5481` `render_generation_activity`，帧表 `⠋⠙⠹⠸`）。
- 让 running 行的 marker 也走同一帧表动画（复用 `render_generation_activity` 的帧表 + tick 逻辑，别重造）。
- ⚠️ **livelock 红线（P4b-2 血泪教训，见 memory）**：底部动画**不得引入无条件固定高频重绘**。要求：仅当**存在 running worker/session 时**才驱动周期重绘（如 100-150ms tick），无 running entry 时**完全停 tick**回到事件驱动 redraw_tx。确认 CPU 在多 worker 并发时不飙升（对标 P4b-2 demo 的 ~0.3%）。
- 给测试：running 行 marker 随 tick 变帧、无 running 时不产生周期重绘。

## 改动 3 — F-U2：running 行 title 追加最近工具调用摘要
现状 title 在会话创建时冻结（`src/chat/session.rs:327`），running 中只有秒数在跳，行内容不反映进度。
- running 态行尾追加"最近一条工具调用"的极简摘要（复用 `provider_worker_io_lines_from_conversation`/attach 视图已有的取最近工具调用逻辑 `tui.rs:1301-1398`，只取最后一条截断成几十字符）。
- 截断复用现成 `truncate_chars_with_ellipsis`/`UnicodeWidthStr`（别重造，防宽字符/UTF-8 边界坑）。
- 非 running 态行保持现状。给测试。

## 改动 4 — B1-D：footer 发现性提示
现状常驻 footer（`src/chat/tui.rs:7001-7011` `render_footer`）只写 `Ctrl+G sessions`，没提方向键导航；`render_fullscreen_footer`（7026-7045）已证明可按状态切文案。
- 当底部 entries 非空时，footer 动态加入类似 `↑↓ sessions · Enter attach` 的提示（对齐当前实际键义，别写错）。
- 纯 UI 文案，给渲染测试（entries 非空时提示出现）。

## 验收（receipt 写 `collab-outbox/receipt-2026-07-11-chat-ux-fu2-fu3-polish.md`，别 commit）
逐条实跑贴结果：
1. `cargo fmt --check`
2. `cargo clippy -p openprx --all-targets --all-features -- -D warnings`
3. `cargo check -p openprx --all-features` + `--no-default-features`
4. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 `N passed; M failed`（预期 ≥5478 + 新增，0 failed）；新增测试逐个列名 + 关键项 mutation 证牙。
5. **真机 PTY demo**（对标 P4b-2/P4c 方法，mock provider + `OPENPRX_MOCK_DELAY_MS_BY_PROMPT` 起并发）：①截录 running 行 spinner 帧在变（多帧截图/截录）②权限弹窗箭头选择+Enter 生效（若能触发工具审批）③footer 提示出现 ④**关键：多 worker 并发时 CPU 不飙升**（贴采样，对标 ~0.3%，证明 spinner tick 无 livelock）。跑不动说清卡点别假装。
6. 明确写：**未 commit**、四项各自状态、CPU 采样、真机结果。

铁律：零 unwrap/expect（生产码）、零 warning、English 代码/commit。**spinner tick 的 livelock 红线是本单最高风险，务必守住"无 running 不 tick"**。别自 commit、别碰导航语义。
