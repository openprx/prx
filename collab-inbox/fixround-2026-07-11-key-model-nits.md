# Fixround 2026-07-11 — 键模型重排两处 nit

**基线**：分支 `feat/chat-ux-fu2-fu3-polish`（B-nav 未提交改动在工作树上，直接在其上继续改）。**别 git commit**（提交我来做）。别自提交。别碰轴分离主逻辑/去抖（已审计 ACCEPT），只做下面两项。全程 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`。

来源：主进程审计 B-nav 的两个非阻塞 nit。

## FIX-1（真回归，必做）— Page/Home/End 不该被输入内容门控
现状：`dispatch_global_key` 的 Main 块（约 `tui.rs:1681-1697`）把 `PageUp/PageDown/Home/End` 也放进了 `input.is_empty()` 门控内 → **输入框有草稿时无法翻页/跳顶底滚 transcript**（旧的最外层 focus-agnostic 行为可以，属回归）。
- **修法**：让 `PageUp/PageDown/Home/End` 滚动 transcript/子视口的行为**不受 input 是否为空影响**——有草稿时照样能翻页看历史（翻页是"看"，与 input 编辑正交）。
  - Main focus：Page/Home/End → 主 transcript（无论 input 空否）。
  - child-view focus：Page/Home/End → 子视口（无论 input 空否）。
- **保持不变**：裸 ↑↓←→ 的轴分离仍**受 input 门控**（有草稿时 ↑↓ 移光标、←→ 移光标，不滚/不切）——只有 Page/Home/End 解除门控。
- 给测试：带非空 input 时 PageUp/PageDown/Home/End 仍产生 transcript/子视口滚动 dispatch（对照现状会 Consumed/落 input）。

## FIX-2（完成设计 §4，删惰性死支）— 移除 strip_selection plumbing
审计确认：`strip_selection` 现在**任何键路由都不再写 `Some`**（只被写 `None`），render 高亮实际由 `focus_active_entry_seq` 驱动，`strip_selection == Some(seq)` 分支恒 false → 一批逻辑死支（编译器视为已用不报 warning，但属死代码，违背零死代码精神；设计 §4 本就要求删）。
- **删除**（确认无活跃读者后逐个清）：
  - `state.rs` 的 `strip_selection: Option<u64>` 字段（约 `:347`）+ `reduce_strip_selection_changed` + `Action::StripSelectionChanged` 变体（若删字段后无引用）。
  - render 侧 `strip_selection == Some(..)` 的高亮分支（恒 false 死支），确认高亮完全走 `focus_active_entry_seq`。
  - `refresh_sessions_cache_and_clear_stale_strip_selection`（`mod.rs:1281`）里读 strip_selection 的 reap 逻辑（恒不触发）+ `mod.rs` 里 `StripSelectionChanged{None}` 的 dispatch 点（约 6056/6132/11384/11422 等）。
  - 相关测试若断言 strip_selection 行为，一并删/改。
- ⚠️ **删前确认**：`grep` 全仓库 `strip_selection`/`StripSelectionChanged`，逐个确认无生产活跃读者（除恒 None 写入与死支读取）。若发现某处**确有活跃语义依赖**（非死支），**保留该处并在 receipt 说明**，别为删而删破坏功能。
- 目标：删完 `cargo check` 零 warning、全量测试仍绿、底部列表高亮行为不变（仍由 focus 正确驱动）。

## 验收（receipt 写 `collab-outbox/receipt-2026-07-11-key-model-nits.md`，别 commit）
1. fmt / clippy `-D warnings` / check（双 feature）
2. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 passed/failed；FIX-1 新测试列名；FIX-2 删/改的测试列名
3. `cargo audit` + `cargo deny check advisories` 双绿
4. 真机 PTY 快验：①带草稿按 PageUp 能滚 transcript（FIX-1）②底部列表高亮仍随 focus 正确（FIX-2 没弄坏高亮）
5. 明确写：**未 commit**、FIX-1 状态、FIX-2 删了哪些/是否有保留项及理由、底部高亮验证。

铁律：零 unwrap/expect（生产码）、零 warning、零死代码、English 代码/commit。别自 commit、别碰轴分离/去抖主逻辑。
