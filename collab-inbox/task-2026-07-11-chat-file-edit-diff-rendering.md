# Task 2026-07-11 — prx chat 文件编辑/新建 diff 显示（A+B，对标 Claude Code）

**基线**：分支 `feat/chat-file-edit-diff-rendering`（off main `974452b4` v0.8.2，工作树干净）。**别 git commit**（提交我来做）。别自提交。全程 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`。

用户反馈：`prx chat` 编辑/新建文件不像 Claude Code 那样显示彩色 diff / 内容预览。调查已定位：**diff 着色器 `render_diff_block` 已存在但没接到工具卡片**；`file_edit` 出的是退化 diff（无上下文/行号）+ 灰色纯文本 + 默认折叠 3 行；`file_write` 新建无内容预览。用户决策 = **A+B 全做**（内联彩色 diff、编辑/写入卡默认展开、context diff 用 `similar` crate、file_write 新建给带行号预览）。

## A — TUI 层：工具卡片体接彩色 diff 渲染（收益最大）
1. **上色**：工具卡片 result body 现在无论折叠/展开都走 `Color::DarkGray` 纯文本（`src/chat/tui.rs:6156` `push_folded_tool_result_preview` / `:6185` `push_expanded_tool_io`）。对 diff-形状输出（`tool_name in {file_edit, file_write}`，或用现成 `renderer::is_diff_block` 探测 `src/chat/renderer.rs:215`），改走 `render_diff_block(text)`（`renderer.rs:254`，全功能 +/- 着色器）→ `ansi_sgr_to_lines`（`tui.rs:5894` 桥，产出已解析 Span 不含裸 ANSI）→ push 成彩色 `Line`，替代灰色。
2. **默认展开**：`file_edit`/`file_write` 卡默认不折叠（现在建卡即 folded `tui.rs:3339`），或对 diff 卡把 `TOOL_FOLDED_RESULT_PREVIEW_LINES`（`tui.rs:404`，现 3）加大到能看清 diff 主体。取"Claude Code 观感=直接看到 diff"。
3. **可选顺带**：`/diff` 全屏 overlay 现在每行硬编码 `Color::White`（`render_active_session_view tui.rs:4948`），可用 `classify_diff_line`（`renderer.rs:317`）决定 +/- 颜色。低优先，做不完不阻塞。
- 注意：彩色 diff 与卡片现有 DarkGray 边框/`│` 视觉协调；宽度截断对含 Span 的 Line 仍成立（`ansi_sgr_to_lines` 产出已解析 Span，安全）。

## B — 工具层：提升 diff 质量 + 补 file_write 预览
1. **`file_edit` 真 unified diff**（`src/tools/file_edit.rs`）：`build_unified_diff`（:35-51）现在只有整块 -/+、无上下文/行号。升级为**带行号 + 前后 N 行上下文**的规范 unified diff。函数已持有整份 `contents`（:239）+ old/new，可定位命中行号取上下文。**引入 `similar` crate** 做行级 diff（`cargo add similar`；当前只有 `strsim` 无行 diff 库）。处理 `replace_all` 多命中（多个 hunk）。output 成功文案（:320-326 `Applied N replacement(s) in {path}\n\n{diff}`）保持"Applied N replacement"前缀（**15+ 单测断言此**，diff 主体变了要同步更新这些测试）。
2. **`file_write` 区分新建/覆盖**（`src/tools/file_write.rs`）：open 前 `path.exists()` 判断。
   - **新建** → output 附**带行号的内容预览**（截断到 N 行，如 40 行 + `… +M more lines`）。
   - **覆盖** → 读旧内容生成 unified diff（复用 similar；大文件按上限截断）。
   - 现状 output 只有 `Written {N} bytes to {path}`（:181），改为 `Written {N} bytes to {path}` + 预览/diff 块。
3. **output 对 LLM 仍可读**：diff/预览文本对模型也有用，保持纯文本可解析；大文件预览/diff 设上限（沿用现有 DIFF/预览 max 常量或新增合理上限，别无界）。

## 依赖注意
- `similar` 是新依赖 → 过 `cargo deny check bans/licenses/sources`（similar = MIT/Apache-2.0，应 OK）+ `cargo audit`。若 deny 因新 source/license 报错，按 deny.toml 规则处理并在 receipt 说明。

## 验收（receipt 写 `collab-outbox/receipt-2026-07-11-file-edit-diff.md`，别 commit）
逐条实跑贴结果：
1. fmt / clippy `-D warnings` / check（双 feature）
2. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 passed/failed；新增/改的测试列名（含 file_edit.rs 同步更新的断言、file_write 新建/覆盖、TUI diff 上色渲染测试）+ 关键 mutation 证牙。
3. `cargo audit` + `cargo deny check advisories`（含 bans/licenses/sources 确认 `similar` 过）双绿；确认无 RUSTSEC-2026-0189 回归。
4. **真机 PTY demo**（关键，对标 Claude Code 观感）：①`file_edit` 改一个现有文件 → 卡片显示**彩色 unified diff（+绿/-红 + 行号 + 上下文）默认可见**（贴截录）②`file_write` 新建文件 → 卡片显示**带行号内容预览**③`file_write` 覆盖现有文件 → 显示 diff。贴关键彩色截录（说明哪些行绿/红）。跑不动说清卡点别假装。
5. 明确写：**未 commit**、A/B 各改动点、similar 依赖是否过 deny、真机 demo 观感。

铁律：零 unwrap/expect（生产码）、零 warning、零死代码、English 代码/commit。output 上限别无界。别自 commit。
