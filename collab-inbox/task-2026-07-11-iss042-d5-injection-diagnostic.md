# Task 2026-07-11 — ISS-042 D5：注入超预算用户可见诊断

**基线**：分支 `fix/iss042-d5-injection-overbudget-diagnostic`（off main `974452b4` v0.8.2，工作树干净）。**别 git commit**（提交我来做）。别自提交。全程 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`。

用户决策：ISS-042 **只做 D5**，**不做**全量 Option A、**不做** C3。**隐私红线不动**——持久化仍只落 original，@path 文件正文/memory 内容绝不落盘。本单只加一条诊断，**不改**任何持久化/裁剪行为/隐私边界。

## 背景（审计 #6）
PRX 有两份历史：**enriched**（喂 provider，含 memory preamble + @path 文件正文注入）与 **original**（持久化/用户可见，不含注入）。预算触发按 enriched 判断（`src/chat/agent/loop_.rs` compaction 路径）。当 **注入本身**撑爆 provider hard limit（而耐久 turns 本身放得下）时，现状退到**静默钝裁剪**（`src/chat/dispatcher.rs:1705` `budget.over_hard_limit` → `trim_history_to_context_budget_preserving_compaction_replacement_with_floor`），用户无感知 → 以为模型变笨/回复缺上下文。

## 目标（D5，唯一改动）
当检测到"**注入驱动的超预算**"（original 放得下但 enriched 超 hard limit）时，除了照常保守裁剪外，**额外发一条用户可见诊断**（`Action::SystemMessageAdded`），让用户知道"你的 @path/记忆注入太大（约 N tok），已裁剪，本轮回复可能缺部分注入上下文"。

## 实现要点
1. **检测"注入驱动超预算"**：在触发 `over_hard_limit` 保守裁剪的路径（dispatcher.rs:1705 一带 + 若 loop_ 里也有对称路径一并覆盖），比较：
   - `estimate_history_tokens(original / compaction_guard_history)` 是否 **≤ budget**（耐久本身放得下）
   - 且 `estimate_history_tokens(enriched)` **> hard limit**（加注入后超了）
   - 二者同时成立 = 注入驱动。用现成的 token 估算函数（grep `estimate_history_tokens`/`ContextBudget`，别新造估算）。
   - 注入 delta ≈ enriched - original 的 token 估算，用于诊断文案里的"约 N tok"。
2. **发用户可见诊断**：复用 `send_redux_compaction_feedback`（dispatcher.rs:1738）的模式或直接 `Action::SystemMessageAdded { text }`（:1759）。文案英文，示例：`Note: your @path/memory context (~N tokens) exceeded the model's context budget and was trimmed for this turn; the reply may miss some injected context.`（按实际 token 数填 N）。
   - **去重/节流**：别每轮刷屏——参考现有 `last_feedback` 节流（send_redux_compaction_feedback 的 dedup 参数 dispatcher.rs:1933/2060），只在状态变化/首次时发一次。
3. **不改**：裁剪逻辑本身（裁什么、floor 保护）保持现状（这是 D1 保守默认，不动）；持久化仍只写 original（隐私红线）；不引入 C3 引用清单；不落盘 enriched。
4. **注意**：只对"注入驱动"发诊断——正常对话历史累积超预算（非注入导致）**不**发这条（那是正常 compaction，已有反馈通道），别误报。

## 验收（receipt 写 `collab-outbox/receipt-2026-07-11-iss042-d5.md`，别 commit）
逐条实跑贴结果：
1. fmt / clippy `-D warnings` / check（双 feature）
2. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 passed/failed；新增测试列名 + mutation 证牙（检测逻辑改回 bug → 诊断该发不发 / 误发变红）。至少覆盖：①注入驱动超预算→发诊断 ②正常历史超预算(非注入)→不发这条 ③节流不刷屏。
3. `cargo audit` + `cargo deny check advisories` 双绿
4. **隐私回归确认（红线）**：确认本改动**未**把 enriched/注入正文写进任何持久化路径——若加了任何落盘，明确否决。可跑现有 SaveSession 隐私测试确认落盘仍只含 original。
5. **真机 PTY demo**：构造一轮大 @path/memory 注入撑爆预算（mock 小 context 或大文件），确认用户看到诊断消息、且回复照常产出（裁剪生效）。贴截录。
6. 明确写：**未 commit**、检测逻辑、诊断文案、节流方式、隐私未破证据、真机结果。

铁律：零 unwrap/expect（生产码）、零 warning、零死代码、English 代码/commit/诊断文案。**隐私红线（只落 original）是硬门**。别自 commit。
