# [FIX-ROUND] 会话画像 chat_profile_update 真机 E2E：模型从不真调用

依据回归审计 `/opt/worker/report/prx-chat-profile-e2e-regression-audit-2026-07-06.md`。
**定性：无代码缺陷**——管道/权限/注入全对（群 turn 也拿到 `_zc_scope_trusted=true`，工具若被真调必执行，审计已用注入路径测试证明）。E2E 失败 = 模型手搓 `<function=>` 文本 + 老群伪 tool-call 历史污染 + **描述泄漏 "trusted scope" 诱导模型自我否定** + compatible 无文本兜底。本轮把这些"让模型不肯/不能真调用"的诱因逐个消除。

规矩同前：干净已提交自检；每 commit 前 + receipt 实跑 `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` 写进 receipt；`cargo check --workspace --no-default-features`；全量 test。**禁止 push**。receipt `collab-outbox/receipt-2026-07-06-chat-profile-model-invocation.md`。

## P0：description + bail 文案去掉 "trusted scope" 泄漏（最可能直接消除幻觉拒绝）
`_zc_scope`/trusted 机制**本该对模型完全透明**——模型只需给 purpose/notes/tags，目标会话由运行时自动锁定。当前把内部机制写进了模型可见文案，诱导它纠结"我没资格"。
1. **description**（`src/tools/chat_profile_update.rs:111-113`）：删掉 "The target is always the current **trusted runtime chat**." 这类暴露内部 trust/scope 的措辞，改成对模型透明的说法，例如：
   > "Record what the current conversation (this group or direct chat) is for: its purpose, useful notes, and short tags. You only provide purpose/notes/tags — the target conversation is determined automatically. Prefer this over general memory tools for conversation purpose/notes/tags."
2. **bail/错误文案**（`:45/:50/:56/:62/:142`）：面向模型的 error 字符串去掉 "trusted runtime scope" 等内部术语（这些在模型 file_read 读源码或报错回传时会污染它）。改成中性内部错误，如 "chat_profile_update: internal scope unavailable"（`:45/50/56/62`）；`:142` 保留"不要传 channel/chat_id"语义但去掉 "trusted runtime scope" 词。
3. **不改** trusted_scope 的实际逻辑/安全性（`_zc_scope_trusted` 校验、拒绝模型传 channel/chat_id 一律保留），只改**文案**。

## P1：compatible provider 加 `<function=…>` 文本 tool-call 兜底解析（根治手搓）
`src/providers/compatible.rs`：当模型把工具调用写进正文 `content`（形如 `<function=NAME>{json}</function>`）而结构化 `tool_calls` 为空时，兜底解析成工具调用。
- 解析点：非流式聚合响应（`compatible.rs:1257-1298`/`:1432` 附近）+ 流式收尾（`:764`/`:1023`，finish 时对累积 content 扫一遍）。二选一或都覆盖，receipt 说明。
- **安全**：①只解析**已注册工具白名单**内的名字（避免乱执行）；②`{json}` 必须 `serde_json` 解析通过才转成调用，失败则当普通文本；③解析出的调用照常走注入（`_zc_scope` 由 loop 注入，模型即使在文本里塞了 channel/chat_id 也被覆盖/拒绝，安全模型不变）。
- 测试：喂一段含 `<function=chat_profile_update>{"purpose":"x"}</function>` 的 assistant content → 断言被解析成一次 chat_profile_update 调用；非白名单名 / 坏 JSON → 当普通文本不执行。

## P1：群 turn 注入提示强化"用结构化调用"
"## Current Conversation" 块或渠道提示里，明确一行：模型维护画像时**用结构化工具调用 chat_profile_update（不要把调用写成正文文本），只给 purpose/notes/tags，不要传 channel/chat_id**。别过长（画像块 token 预算已紧）。同步更新相关 prompt 快照测试。

## P2：回归护栏——把执行力测试移植到 HEAD
审计在临时 worktree 写的 `group_channel_turn_injects_trusted_scope_and_writes_agent_profile`（走 `execute_tool_call_serial`→`execute_one_tool`，模型只给 purpose/notes/tags、不给 `_zc_scope`，由 loop 注入，断言 chat_profiles 群行 `updated_by='agent'` 被写）随 worktree 清掉了。**在主线重建这个测试**作永久回归护栏——它覆盖"真实注入路径"，比现有 `chat_profile_update.rs:283`（手工塞 `_zc_scope` 绕过注入）更接近真机。

## 验收 + 复验
每项 file:line 落实 + 测试。**真机复验由主进程做**：改完主进程重新部署 + 用**全新未污染群**让 bot 打标（自然指令、不提工具名/不诱导读源码）→ 期望 journal 出现 `Tool call started tool=chat_profile_update` + chat_profiles 群行 `updated_by='agent'`。禁止 push。
