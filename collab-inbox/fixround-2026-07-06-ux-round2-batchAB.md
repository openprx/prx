# [派工] UX round2 Batch A+B fix-round（O1 sentinel + I2 红线 + I3 破门；I1 顺带清 lint）

审计裁决（两份 Batch B 审计合并，采信更严格结论）：O2 `c0c1ef67` ACCEPT、**O1 `d09a7124` FIX-ROUND**、**I1 `9e2c6251` FIX-ROUND（第二份审计降级：F-2 HIGH 选中重置回归 + F-4 测试没牙）**、**I2 `db9362ba` FIX-ROUND**、**I3 `ac15c973` FIX-ROUND（clippy 破门）**。**排在 Batch D 之后做。** 禁止 push。

## I1 追加必修（第二份审计 F-2/F-4，与 I3 lint 同批）

1. **[HIGH] F-2 每键 refresh 清零 mirror 菜单选中态 → 提交项≠高亮项，且回归既有 slash 菜单导航**：主循环对每个非 Esc 键（含 Up/Down）调 `refresh_at_path_candidates_for_tui`（mod.rs:8518 区）→ `update_at_path_candidates`（无 dedup）→ `sync_slash_menu_for_sources` → `refresh` → **`selected=0`**。Redux 侧有相等 early-return 保住选中、渲染走 Redux snapshot，但**提交权威是 mirror**（`KeyDispatch::Submitted`，mod.rs:8538）→ Down 高亮第 2 项、Enter 插入第 1 项；且 slash 菜单 `/`+Down 后 selected 也被弹回 0（**回归了 parity 战役的 slash 导航**）。修法：mirror 侧 refresh 前对相等候选集 dedup（filter 未变则跳过 sync），或 `refresh_with_entries` 在候选集不变时保留 selected。**测试**：`@f`+Down+refresh+Enter → 得第 2 项；`/`+Down+refresh → selected 仍为 1（修复前应红，可采用审计的复现测试）。
2. **[MED] F-4 symlink 安全测试没牙**：`at_path_candidates_are_relative_sorted_and_security_filtered` 里 `outside` tempdir 在 `#[cfg(unix)]` 块结束即 drop → symlink 悬空 → canonicalize 失败滤掉候选（与 policy 无关）；mutation 证实禁用 `is_resolved_path_allowed` 测试照过。**实现本身安全**（adversarial 8 变体全挡），但测试要把 `outside` 提到外层作用域，让 symlink 活着走到 policy。
3. 顺带 backlog（非本轮）：at_path 每键 read_dir+逐候选 canonicalize 无预算上限（超大目录卡输入线程）；缺 legacy-vs-redux at_path 联合 parity 测试；workspace_only=false 时 forbidden_paths 词法匹配可被工作区内 symlink 绕过（file_read 同源，另立条目）。

## S1 必修（Batch C，一处 clippy）

- `src/chat/sessions/pty.rs:444` `SinkInner` 的手写 `impl Debug` 加了 `last_output_at` 字段却没列进去 → `missing_fields_in_debug` 破 `-D warnings`。修：`.field("last_output_at", &self.last_output_at)` 或 `.finish_non_exhaustive()`。S1 逻辑/测试本身审计判正确（mutation 验牙过），只清这一处 lint。C1 `/copy` 已 ACCEPT 不动。

## O1 必修（一个 commit，独立）

**根因**：`ollama_model_num_ctx_from_router` 只过滤 `>0`，但 `RouterModelConfig.max_context` 的 serde 默认是 `default_router_max_context()=1_000_000`（config/schema.rs:568-570, 709-710）。任何 provider="ollama" 但未显式写 max_context 的 `[[router.models]]` 条目 → 请求发 `num_ctx=1_000_000` → llama.cpp KV cache 爆炸/OOM/加载失败——**把"能用"改成"不能用"的回归**。

**修法**：①router fallback 排除 sentinel（`max_context > 0 && max_context < 1_000_000` 才采用，或直接判 `!= default_router_max_context()`）；②加保守 cap（建议 65536，或 `[providers.ollama] num_ctx_cap` 可配）——即便显式配 128k 也 clamp 到 cap 防 OOM；③文档/receipt 明示"num_ctx 无条件随请求发送，会覆盖 Modelfile/set parameter 的服务端值"这一行为变化。**测试**：未配 max_context 的 ollama 路由条目 → num_ctx=DEFAULT 8192（不是 1M）；显式配 128k → clamp 到 cap。修复前应红。

## ⚠️ receipt 自检规范强化

本批 receipt 漏跑 `clippy --workspace --all-targets -D warnings`（只跑了 check + 专项测试），放过了 3 处 `indexing_slicing` deny 违规。**今后每个 receipt 自检必须包含**：`cargo fmt --all -- --check` + `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` + `cargo test --bin prx`（全量，非专项）+ `cargo check --workspace --no-default-features`。缺一不可，receipt 里逐条列结果。

## I2 必修（一个 commit，最高优先）

**根因**：粘贴 chip 以「可逐字符编辑的明文占位符」存进 `lines`，`text()` 用朴素 `String::replace(placeholder, content)` 展开，无边界保护无哨兵。3 个 mutation 实证红线破裂：
- chip 后按一次 Backspace 删占位符尾字符 → 映射失配 → **整段粘贴内容丢失 + 破损占位符泄漏进发给模型的 payload**（高频操作）
- 光标移进占位符中间输入 → 同样破坏映射
- 用户粘贴字面等于占位符文本的小内容（未达阈值）→ replace 误替换 → 内容污染/注入

**修法**（择一，倾向前者）：
1. **原子不可编辑 chip**：chip 作为单个不可键入单元存储（内部结构记 chip id → 原文映射，渲染时展开为占位显示，`lines` 里不放可编辑的明文占位符）；光标不能进入 chip 内部，Backspace/Delete 在 chip 边界整体删除该 chip；`text()` 按结构展开而非字符串 replace。
2. 或：唯一不可键入哨兵（如私有区 Unicode）包裹 + 按位移映射记录每个 chip 的 [start,end)，编辑时检测哨兵完整性，破损则整体移除该 chip。

**测试（修复前应红）**：把审计的三个探针语义固化为正式测试——①chip 后 Backspace 后提交，原文完整或 chip 整体消失（绝不泄漏破损占位符）；②chip 内部无法插入字符；③粘贴等于占位符字面的小内容不被误替换。

## I3 必修（一个 commit）

- `tui.rs:5912`/`5914` 生产码 `wrapped_rows[cursor_visual_row].start/.end` → 改 `.get(cursor_visual_row)` + 兜底（运行时虽不 panic 但破 `indexing_slicing=deny` 门 + 铁律 #2）。
- 顺带清 I1 的 `tui.rs:8055 menu.entries[0].label`（测试行）→ `.get(0)`/`.first()` 或 `#[allow]`（倾向改 .first()）。
- 功能本身审计判正确（CJK 宽度换行/光标映射/chrome 高度/窄终端 saturating 全过），只清 lint。

## Receipt

`collab-outbox/receipt-2026-07-06-ux-round2-batchB-fixround.md`：commit（I2/I3 各一）、mutation 语义测试名、**完整四门自检结果**、二进制路径。
