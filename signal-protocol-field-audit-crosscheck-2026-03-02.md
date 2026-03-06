# PRX Signal 协议字段覆盖交叉审计（Cross-check）

- 审计日期：2026-03-02
- 审计角色：独立审计（只读）
- 审计目标文件：`/opt/worker/code/agents/prx/src/channels/signal.rs`
- 交叉依据：
  - 本仓库 `signal.rs` 的反序列化结构与处理逻辑
  - `signal-cli` 常见 envelope/dataMessage 字段命名（用于“可能 JSON 路径”）

## 结论摘要

1. 原报告“已覆盖”中的 `message/attachments/groupInfo/source/sourceNumber/timestamp` 结论基本准确。  
2. `mentions` 为“部分覆盖”：解析了 `uuid/number/name/start/length`，但仅 `uuid/number` 进入 `ChannelMessage.mentioned_uuids`。  
3. `storyMessage` 为“弱覆盖”：只做存在性判断（用于 `ignore_stories`），未做内容解析。  
4. 原报告“缺失”列表整体方向正确；其中 `quote/reaction/remoteDelete` 在**接收侧缺失**，但发送侧分别有 quote/reaction/delete 能力。  
5. 明确存在静默丢弃路径：`process_envelope` 中 `if text.is_empty() && !has_attachments { return None; }`，会丢弃大量非文本事件。  

关键证据（代码行）：
- 反序列化字段定义：`signal.rs:233-257`
- 静默丢弃条件：`signal.rs:476-477`
- 无 `dataMessage` 直接丢弃：`signal.rs:463`
- `storyMessage` 仅用于过滤：`signal.rs:459-460`
- mentions 仅上抛 uuid/number：`signal.rs:550-567`
- 发送侧 quote/reaction/delete 能力：`signal.rs:773-835, 1066-1069, 1121-1123, 1247-1310`

## 覆盖矩阵表

说明：
- “当前代码是否解析”：指 `signal.rs` 接收路径（SSE/receive -> `process_envelope`）是否反序列化并使用。
- “是否会被过滤/静默丢弃”：指进入 `process_envelope` 后是否可能在无日志情况下被 `None` 返回丢弃。
- 影响等级：P1 高 / P2 中 / P3 低。

| 字段 | signal-cli 可能 JSON 路径 | 当前代码是否解析 | 是否会被过滤/静默丢弃 | 影响等级 | 修复复杂度 |
|---|---|---|---|---|---|
| message | `$.envelope.dataMessage.message` | 是 | 空文本且无附件会丢弃（`text.is_empty() && !has_attachments`） | P1 | 简单 |
| attachments | `$.envelope.dataMessage.attachments[*]` | 是（原样 `Value`） | `ignore_attachments=true` 且纯附件时会丢弃；下载失败不会丢弃消息本体 | P2 | 简单 |
| mentions | `$.envelope.dataMessage.mentions[*].{uuid,number,name,start,length}` | 部分（仅 uuid/number 实际上抛） | 不单独触发丢弃；但 mention-only 事件若无文本/附件会被文本门禁丢弃 | P2 | 简单 |
| groupInfo | `$.envelope.dataMessage.groupInfo.{groupId,groupName|name}` | 是（groupId/groupName） | group 过滤不匹配会丢弃；无 groupInfo 时可按 DM 规则处理 | P2 | 简单 |
| storyMessage | `$.envelope.storyMessage` | 部分（仅存在性） | `ignore_stories=true` 直接丢弃；`ignore_stories=false` 但无 `dataMessage` 仍会被丢弃 | P2 | 简单 |
| source/sourceNumber | `$.envelope.source` / `$.envelope.sourceNumber` | 是（`sourceNumber` 优先） | sender 缺失则丢弃；DM 且不在 allowlist 也丢弃 | P1 | 简单 |
| timestamp | `$.envelope.dataMessage.timestamp` -> `$.envelope.timestamp` | 是 | 无 timestamp 不丢弃（回退到当前系统时间） | P3 | 简单 |
| quote | `$.envelope.dataMessage.quote` | 否 | quote 元数据静默忽略；若该事件无文本/附件则整体被文本门禁丢弃 | P2 | 简单 |
| reaction | `$.envelope.dataMessage.reaction` | 否 | reaction-only 常见会被文本门禁静默丢弃 | P1 | 中等 |
| remoteDelete | `$.envelope.dataMessage.remoteDelete` | 否 | remoteDelete-only 常见会被文本门禁静默丢弃 | P1 | 中等 |
| sticker | `$.envelope.dataMessage.sticker` | 否 | sticker-only 常见会被文本门禁静默丢弃（非 `attachments`） | P1 | 中等 |
| expiresInSeconds / isExpirationUpdate | `$.envelope.dataMessage.expiresInSeconds` / `$.envelope.dataMessage.isExpirationUpdate` | 否 | 过期更新事件若无文本/附件会被文本门禁丢弃 | P2 | 简单 |
| typingMessage | `$.envelope.typingMessage` | 否 | 因 `dataMessage` 缺失在 `data_message.as_ref()?` 处静默丢弃 | P3 | 简单 |
| receiptMessage | `$.envelope.receiptMessage` | 否 | 同上，静默丢弃 | P3 | 简单 |
| syncMessage | `$.envelope.syncMessage` | 否 | 同上，静默丢弃 | P1 | 复杂 |
| editMessage | `$.envelope.editMessage` | 否 | edit-only 事件被 `data_message.as_ref()?` 静默丢弃 | P1 | 中等 |
| contactMessage（实际常见为 contacts） | `$.envelope.dataMessage.contacts[*]`（或实现层命名 `contactMessage`） | 否 | 若联系人共享事件无文本/附件，会被文本门禁丢弃 | P2 | 中等 |
| storyReply | 常见为 `$.envelope.dataMessage.storyContext` 或 story 子结构（实现差异） | 否（未见对应字段） | story reply 若仅上下文无文本/附件，会被静默丢弃；有文本则仅保留文本 | P2 | 中等 |
| group v2 updates | 常见出现在 `$.envelope.syncMessage.*group*` / 组更新子结构 | 否 | 多数将因缺失 `dataMessage` 被静默丢弃 | P2 | 复杂 |

## 与原报告一致/不一致点

### 一致点

1. “已覆盖：message, attachments, mentions, groupInfo, storyMessage, source/sourceNumber, timestamp”总体方向正确。  
2. “缺失：quote, reaction, remoteDelete, sticker, expiresInSeconds/isExpirationUpdate, typingMessage, receiptMessage, syncMessage, editMessage, contactMessage, storyReply, group v2 updates”在接收侧判断基本正确。  
3. 确认存在 `text.is_empty() && !has_attachments` 的静默丢弃路径。  

### 不一致/需补充点

1. `mentions` 不是完整覆盖：`name/start/length` 解析后未上抛，仅 `uuid/number` 被使用。  
2. `storyMessage` 不是完整解析覆盖：仅用于“是否忽略 story”的开关判断。  
3. `quote/reaction/remoteDelete` 缺失应限定为“接收侧缺失”；发送/动作侧已有能力：  
   - quote 发送字段：`quoteTimestamp/quoteAuthor`  
   - reaction 发送：`sendReaction`  
   - delete 动作：`remoteDelete`  
4. `contactMessage` 命名存在实现差异，signal-cli 常见字段更接近 `contacts`（`dataMessage` 下）。

## 建议修复顺序

1. **先修门禁丢弃策略（P1）**  
   - 将 `text.is_empty() && !has_attachments` 改为“允许已识别非文本事件通过”，至少覆盖：`reaction/remoteDelete/sticker/edit/expiration update`。  
2. **补齐 Envelope 顶层事件解析（P1/P2）**  
   - 增加 `typingMessage/receiptMessage/syncMessage/editMessage` 字段与分发分支，避免在 `data_message.as_ref()?` 早退。  
3. **补齐 DataMessage 非文本字段（P1/P2）**  
   - 增加 `quote/reaction/remoteDelete/sticker/expiresInSeconds/isExpirationUpdate/contacts/storyContext` 结构并标准化映射。  
4. **完善 group v2 与 sync 事件（P2）**  
   - 对 `syncMessage` 子结构做最小可用映射（至少不静默丢弃，先透传 meta）。  
5. **最后做字段完整性增强（P2/P3）**  
   - mentions 补齐 `name/start/length` 上抛；storyMessage 做显式分类而非仅存在性判断。

## 最小回归用例清单

1. `reaction-only`：`dataMessage.reaction` 存在、无 `message/attachments`，期望**不被静默丢弃**。  
2. `remoteDelete-only`：`dataMessage.remoteDelete` 存在、无文本附件，期望不丢弃。  
3. `sticker-only`：`dataMessage.sticker` 存在、无文本附件，期望不丢弃。  
4. `expiration-update-only`：`isExpirationUpdate=true`、无文本附件，期望不丢弃。  
5. `typingMessage` 顶层事件：无 `dataMessage`，期望不在 `data_message.as_ref()?` 早退丢弃。  
6. `receiptMessage` 顶层事件：同上。  
7. `syncMessage`（含组更新/已发送副本）顶层事件：同上。  
8. `editMessage` 顶层事件：同上。  
9. `quote + text`：文本保留且 quote 元数据可见（至少 meta 透传）。  
10. `mentions` 完整性：`uuid/number/name/start/length` 均可被映射验证。  
11. `storyMessage` 与 `ignore_stories`：开关开启时丢弃，关闭时可按设计处理而非无条件早退。  
12. `text.is_empty() && !has_attachments` 回归：仅对“真正空消息”丢弃，不误杀已识别事件。

