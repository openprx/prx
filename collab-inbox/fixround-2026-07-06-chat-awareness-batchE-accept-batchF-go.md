# [裁决 + 放行] Batch E ACCEPT → Batch F GO

## Batch E 裁决：ACCEPT（无必修项）

`22de7ba6 feat(channels): track structured chat kind metadata` 通过独立审计：
- 五个 item 全部 file:line 落实（ChatKind 字段 / 4 渠道填写 / 消费方 fallback 切换 / telegram 群前缀 / TUI-渠道 prompt 拆分）。
- **命门（缺口4 "telegram/discord 群被当 dm autosave"）经 mutation 验牙确认真牙**：`process_channel_message_telegram_group_does_not_autosave` 改回 legacy → store_calls 1→0 变红。
- 8/8 回归测试 mutation 全真牙；独立 `RUSTFLAGS="-D warnings" clippy --workspace --all-targets` 零 warning、`check --no-default-features` PASS。

### 一个前瞻观察（本轮不修，记 backlog，Batch F 需注意）
`ChatKind::Thread` 消费方判定不一致：`is_group_like()` 视 Thread 为群，但 `evaluate_inbound_policy`（mod.rs:425）按字符串 `"group"` 匹配 + `scope_chat_type()` 对 Thread 返回 `"thread"` → 落入 DM 策略分支。当前无渠道产生 Thread 故不可触发。**Batch F 的 prompt 注入块在渲染 Type 行时对 Thread 要优雅处理**（显示为 `thread` 或归入 group 语义，二选一但要自洽），不要新引入 Thread 渠道。

## Batch F GO：按 task-2026-07-06-chat-awareness.md 的 item 6-10 施工

无新增规格，照原派工单 Batch F 全部执行。重申硬约束：

1. **chat_profiles 表 sqlite + postgres 双实现**（照 identity_bindings 模板），长度上限 purpose≤300 / notes≤1KB / tags≤10（超限截断+提示）。
2. **自动回填**：入站 upsert (channel, chat_id) 的 chat_kind/title（title 仅渠道给值时覆盖；不碰 purpose/notes/tags）。
3. **chat_profile_update 工具**：目标一律取可信 `_zc_scope.channel/chat_id`（loop_.rs 注入），模型不可指定目标（天然防串写）；updated_by='agent'；群 turn 允许；SideEffectGate=Medium。
4. **prompt 注入**：O(1) 主键查询、总预算 ≤150 token、无画像行仍注入 Platform/You/Type 三行、**只按 (channel,chat_id) 精确匹配绝不跨 chat**、**TUI 路径不注入**、plain 无关。
5. **设计红线（ACL）**：跨 chat 隔离测试 + 群 turn 打标 updated_by='agent' 只写本群行 + prompt 块快照测试（有/无/超长三态）+ **绝不写回 memories 表**（防 Owner 1=1 把私聊画像带进群）。

## 验收门（同前）
干净已提交状态自检、每 commit 前 + 每份 receipt 必实跑 `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` 写进 receipt、no-default-features check、全量 test。**真机 E2E 必做**：真实 wacli/telegram 会话让 bot 打标 → 重启 daemon → 新 turn prompt 里画像仍在（持久化+注入闭环）。

receipt：`collab-outbox/receipt-2026-07-06-chat-awareness-batchF.md`。禁止 push。
