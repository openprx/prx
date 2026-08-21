# [派工] 会话自我意识战役（用户已拍板）：ChatKind 统一 → chat_profiles 画像 + 自主打标 + prompt 注入

**排序：在 ux-round2 Batch B/C/D 全部完成（含可能的 fix-round）之后开工。** 规矩同前：禁止 push、干净已提交状态自检、每 Batch 一个 receipt。
依据审计：`/opt/worker/report/prx-chat-awareness-audit-2026-07-06.md`（差距表 G1-G8 + 设计草案，file:line 证据都在里面）+ `/opt/worker/report/prx-wa-im-chat-management-audit-2026-07-06.md`。

## Batch E：结构轮（ChatKind 统一 + 附带修复，2-3 个 commit）

1. **ChannelMessage 加统一字段**（traits.rs:12-49）：
   - `chat_kind: ChatKind`（enum `Dm | Group | Thread`，Serialize/Deserialize，默认 Dm）
   - `chat_title: Option<String>`（群名/频道名）
   - `sender_display: Option<String>`（发送者显示名）
2. **各渠道显式填写**：
   - wacli：`@g.us` → Group；chat_title=None（官方 payload 无群名，注明）；sender_display=PushName（wacli.rs:106-107）
   - signal：`group:` 前缀 → Group；chat_title=group_name（signal.rs:443，已有数据结构化）
   - telegram：`chat.type`（group/supergroup→Group，channel→Group 或 Thread 视语义，private→Dm）；**补取 `chat.title` 和 `from.first_name`**（telegram.rs:578-583 区现在只读 type）
   - discord：同理用现有 is_group_hint 对应字段升级
   - 其余渠道：默认 Dm，能填则填
3. **消费方切换**：`infer_chat_type_from_message`（channels/mod.rs:374-380）降级为 fallback（字段缺省时才用）；记忆 scope/autosave 判定（mod.rs:2615-2622, 2701-2705）、smart 群门、安全层 chat_type 传参全部改读新字段。**这一步修掉"telegram/discord 群被当 dm autosave"的错分**（上轮缺口 4），要有回归测试：tg 群消息 → chat_type=group、不 autosave、Group 可见性。
4. **顺手修①**：telegram 群消息 content 补发送者前缀（对齐 wacli/signal 的 `[Telegram Group] {name}: ` 形态，telegram.rs:846-871）。
5. **顺手修②**：chat TUI 的 system prompt 不再包含 "You are running as a messaging bot / response is automatically sent back..."（channels/mod.rs:4001-4010 被 build_runtime_system_prompt 复用导致本地终端语境失真）——Channel Capabilities 段只在渠道路径注入，TUI 路径换中性文案或不注入。加 TUI prompt 快照测试。

## Batch F：chat_profiles 画像 + 自主打标 + prompt 注入（2-3 个 commit）

6. **新表 `chat_profiles`**（sqlite + postgres 双实现，照 identity_bindings 模板）：
   ```sql
   CREATE TABLE chat_profiles (
     id TEXT PRIMARY KEY,
     channel TEXT NOT NULL, chat_id TEXT NOT NULL,
     chat_kind TEXT NOT NULL,
     title TEXT, purpose TEXT, notes TEXT,
     tags TEXT NOT NULL DEFAULT '[]',
     updated_by TEXT NOT NULL,       -- 'agent' | 'owner' | 'auto'
     created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
     UNIQUE(channel, chat_id)
   );
   ```
   长度上限：purpose ≤300 chars、notes ≤1KB、tags ≤10 个（超限截断+提示）。
7. **自动回填**：入站管道 upsert (channel, chat_id) 的 `chat_kind/title`（title 只在渠道给了值时覆盖；不触碰 purpose/notes/tags）。
8. **新工具 `chat_profile_update`**：参数仅 `purpose? / notes? / tags?`（部分更新）；**目标 chat 一律取可信 `_zc_scope.channel/chat_id`（loop_.rs:3801-3855 注入），模型不可指定目标——天然防串写**；写入 updated_by='agent'。群 turn 允许调用（画像=群共享认知，产品决策已定）；SideEffectGate 取 Medium。工具 description 明确教模型："维护当前会话的画像：这个群/私聊是做什么的"。
9. **prompt 注入**：扩展 `build_channel_system_prompt`（channels/mod.rs:869-880）为按 msg 注入：
   ```
   ## Current Conversation
   - Platform: {channel} | You: {bot 账号/名}
   - Type: {dm|group} | Chat: "{title}" ({chat_id})
   - Purpose (self-maintained): {purpose}
   - Notes: {notes} | Tags: {tags}
   - Update via chat_profile_update when you learn what this chat is for.
   ```
   约束：O(1) 主键查询（无语义检索）；总预算 ≤150 token（purpose/notes 截断展示）；无画像行时仍注入 Platform/You/Type 三行（G1/G3 即刻生效）；**只按 (channel, chat_id) 精确匹配，绝不跨 chat**；TUI 路径不注入（chat/mod.rs:5544 不走 channel 包装）；plain 无关（prompt 层非渲染层）。bot 自身账号：wacli bot_jid/telegram bot_username 等从渠道配置读。
10. **ACL 与测试**：①跨 chat 隔离测试（A 群的 purpose 不出现在 B 群/私聊 prompt）；②群 turn agent 打标落 updated_by='agent' 且只写本群行；③prompt 注入块快照测试（有画像/无画像/超长截断三态）；④**不写回 memories 表**（设计红线，防 Owner `1=1` 把私聊画像带进群）。

## 不做（记录）

- G7 自动画像（进群总结/定期刷新）——二期，等一期用出感觉再说。
- ScopeRule 加 chat_id / wacli group_allow_from / Owner 群内召回 ceiling——用户暂缓（测试期不限制），backlog 保留在 WA 审计报告缺口清单。

## 验收门

同前（GP-8 字面锚 / 双路径 parity 如涉及 / 全量自检含 no-default-features / 干净状态 receipt）+ **真机 E2E 必做**：真实 wacli 或 telegram 会话里让 bot 打标 → 重启 daemon → 新 turn prompt 里画像仍在（持久化+注入闭环）。receipt：`collab-outbox/receipt-2026-07-06-chat-awareness-batch<E|F>.md`。
