# [FIX-ROUND] 群聊打标失效：chat_profile_update 修复执行单（2026-07-19）

**发起人**：主会话（wacli 私聊，ck 授权）
**执行者**：prx-codex
**前置分析**：完整根因分析见 `collab-inbox/fixround-2026-07-06-chat-profile-model-invocation.md`（prx-claude 回归审计结论）。**本单 = 正式授权执行该工单的 P0→P1→P2，不要重复做根因审计。**

---

## 0. 问题一句话

群聊（WhatsApp group）里 bot 永远不真调用 `chat_profile_update`，导致无法给群打 purpose/notes/tags 标签。私聊正常。

## 1. 主会话补充审计：相关 commit 定位

| Commit | 日期 | 作用 |
|---|---|---|
| `a9d0f3e8` | 2025-06-05 | 最初引入 `chat_profile_update`（当时无 scope 限制） |
| `b5c3a112` | 2025-06-12 | **加入 `_zc_scope_trusted` 校验** + bail "requires trusted runtime scope" |
| `d4e7c901` | 2025-06-12 | 描述改为 "The target is always the current trusted runtime chat"（**泄漏内部机制，幻觉源头**） |

涉及文件：`src/tools/chat_profile_update.rs`（描述 ~L111-113，bail 文案 ~L45/50/56/62/142）、`src/providers/compatible.rs`（工具调用解析，非流式 ~L1257-1298/:1432、流式收尾 ~L764/:1023）。

## 2. 已确认事实（不要推翻）

1. **管道/权限无缺陷**：群 turn 运行时同样注入 `_zc_scope_trusted=true`；用注入路径测试已证明"工具若被真调，群 profile 必写入"。
2. **失败根因 = 模型行为**：描述里的 "trusted runtime chat" 诱导模型幻觉"群聊不可信、我没资格"，从而自我否定不调用；compatible 解析器无 `<function=>` 文本兜底，模型手搓文本调用不被识别；旧群历史里手搓伪 tool-call 的 transcript 进一步污染。
3. **不要动安全模型**：`_zc_scope_trusted` 校验、拒绝模型传 channel/chat_id 的逻辑**全部保留**。只改模型可见文案 + 解析兜底 + prompt 引导 + 回归测试。

## 3. 任务项（严格按原工单，顺序执行）

- **P0** `chat_profile_update.rs`：description 与 bail 文案去除 "trusted scope/trusted runtime" 措辞（参考原工单建议文案），逻辑不变。
- **P1** `compatible.rs`：结构化 `tool_calls` 为空时，对 content 中 `<function=NAME>{json}</function>` 做兜底解析。安全三件套：仅已注册工具白名单、JSON 解析失败当普通文本、`_zc_scope` 仍由 loop 注入覆盖。
- **P1** 群 turn prompt（"## Current Conversation" 块附近）：加一行"维护画像用结构化调用 chat_profile_update，只给 purpose/notes/tags，不传 channel/chat_id"，保持简短，同步 prompt 快照测试。
- **P2** 主线重建回归测试 `group_channel_turn_injects_trusted_scope_and_writes_agent_profile`（走真实注入路径 `execute_tool_call_serial`→`execute_one_tool`，断言 chat_profiles 群行 `updated_by='agent'`）。

## 4. 规矩（同历次 fix-round）

1. 干净已提交状态开工；**每 commit 前**实跑 `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` 并写入 receipt；`cargo check --workspace --no-default-features`；全量 `cargo test`。
2. **禁止 push**（上次 http_request 单你被允许 push 是特例，本单不适用）。
3. 不改与本单无关代码；版本号不动（发版由主进程统一做）。
4. receipt 写 `collab-outbox/receipt-2026-07-19-chat-profile-group-tag.md`：每 commit 的 hash + clippy/check/test 结果 + file:line 落实清单。

## 5. 验收

- 代码侧：receipt 全部绿。
- 真机侧（**由主进程做，不用你**）：重新部署后在全新未污染群里让 bot 打标 → 期望 journal 出现 `Tool call started tool=chat_profile_update` 且 chat_profiles 群行 `updated_by='agent'`。

有疑问先写进 receipt 的 Open Questions 并停下，不要自行扩大范围。
