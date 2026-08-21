# [FIX-ROUND] Batch F 真机 E2E 缺口:chat_profile_update 对模型不可见

## 现象(真机 WhatsApp E2E 抓到,单测未覆盖)
部署 `3c0095f7` 到 daemon 后真机测试:
- bot 的**会话自我意识注入生效**(私聊准确描述渠道/发送者;群里正确答"群聊")——这部分 ACCEPT,不用动。
- 但**自主打标失败**:
  1. 用户说"记录该群用途/写入记忆" → 模型调**旧的 memory_store**(写进 memories 表),不调 chat_profile_update。
  2. 用户明确说"请使用 chat_profile_update" → 模型吐出**字面文本** `<function=chat_profile_update>{...}</function>`,**未被执行**,chat_profiles 表 purpose 仍空、updated_by 仍 auto。

## 根因(已定位)
`src/agent/loop_.rs:4896-4918` 每轮用 `select_tools_for_intent` 按意图过滤工具发给模型。规则(`src/tools/intent.rs:132-150`):
- `ToolTier::Core` → 每轮无条件包含
- `ToolTier::Standard` → 仅当用户消息命中该工具 category 的意图关键词才包含
- Memory 类关键词只有:记住/remember/forget/忘记/memory/记忆/存储(intent.rs:48-54)

`chat_profile_update` 是 **`ToolTier::Standard` + 仅 `ToolCategory::Memory`**(chat_profile_update.rs:244-249)。于是消息不含上述关键词时(如"更新会话画像""这个群做什么用")工具**根本不在发给模型的 function 列表**里 → 模型被每轮注入的提示"Update via chat_profile_update…"教着知道有这工具,就**手搓文本调用**,不被解析执行(现象2)。含"记忆"时它虽在列表但模型偏好 memory_store(现象1)。

**设计矛盾**:该工具的"## Current Conversation"提示是**每轮注入**的(要求模型随时可维护画像),但工具本身却被意图过滤成大部分时候不可见。二者必须一致。

## 必修

### Fix 1(核心):chat_profile_update 改 Core 层,每轮必发
`src/tools/chat_profile_update.rs:244` `fn tier()` 从 `ToolTier::Standard` 改为 **`ToolTier::Core`**。理由:与每轮注入的提示一致,工具必须每轮对模型可见;它是写工具(Medium 门 + _zc_scope 锁),与既有 Core 写工具 file_write/shell 同级,安全模型不变。改后模型会像 memory_store 一样**原生调用并真执行**,文本手搓问题消失。

### Fix 2:回归测试(mutation 要能验牙)
在 `src/tools/intent.rs` 测试模块加:
- **`chat_profile_update_is_offered_without_memory_keywords`**:构造一个**不含**任何 Memory 关键词的消息(如 `"这个群是做什么的"` / `"what is this group about"`),断言 `select_tools_for_intent(...)` 返回的工具名集合**包含** `chat_profile_update`。(mutation 验牙:把 tier 改回 Standard → 该测试必须变红。)
- 可选:断言 `ChatProfileUpdateTool::tier() == ToolTier::Core`。

### Fix 3(UX,建议同轮做):让模型优先选 chat_profile_update 而非 memory_store 记录会话用途
两处轻量加强,别过度:
- `chat_profile_update.rs:111` description 里点明"**this is the correct tool to record what the current chat/group is for — prefer it over general memory tools for conversation purpose/notes/tags**"。
- 每轮注入块那行提示(channels/mod.rs `build_current_conversation_prompt`)从"Update via chat_profile_update when you learn what this chat is for."加强为明确"**use the chat_profile_update tool (not memory_store)**"。
- 若已有 prompt 快照测试,同步更新快照断言。

## 不改
- 会话自我意识注入(已真机验证生效)、chat_profiles 表/工具目标锁/memories 红线(Batch F 已审计 ACCEPT)——都不动。

## 验收门(同前)
干净已提交自检、每 commit 前实跑 `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` 写进 receipt、`cargo check --workspace --no-default-features`、全量 test。receipt:`collab-outbox/receipt-2026-07-06-chat-awareness-batchF-fixround.md`,列清 Fix1/2/3 各自的 file:line + 回归测试名。**禁止 push**。改完我会重新部署到 daemon 跑真机 E2E 复验(用户手机实发)。
