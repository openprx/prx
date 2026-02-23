# PRX Memory Access Control & Topic Association Design

> Status: DRAFT
> Author: David (AI Architect)
> Date: 2026-02-23
> Review: Pending audit

## 1. Problem Statement

当前 memories 表是扁平结构（id, key, content, category, embedding, timestamps），存在三个关键缺陷：

1. **无上下文来源** — 不知道记忆来自哪个渠道、会话、用户
2. **无事务关联** — 跨渠道讨论同一件事无法聚合
3. **无访问控制** — 任何用户查询都能检索全部记忆，存在信息泄漏风险

### 场景示例

```
AK 私聊: "im-ops 服务器 SSH 密钥换了"     → 存入记忆
cc 私聊: "部署服务器是什么配置?"           → 搜到上条记忆 → 泄漏!

AK 群聊: "openpr 登录页有 bug"            → 存入记忆 A
AK 私聊: "那个登录 bug 修了吗"            → 存入记忆 B
OpenPR webhook: "issue#42 closed"         → 存入记忆 C
→ A/B/C 是同一件事，但完全无法关联
```

## 2. Design Goals

- **G1**: 每条记忆携带完整上下文元数据（渠道、会话类型、发送者）
- **G2**: 跨渠道同一事务可通过 topic 关联聚合
- **G3**: 查询时根据用户身份自动过滤可见范围
- **G4**: 与 USER.md 权限模型对齐，无需额外配置
- **G5**: 对现有 memory_search/memory_get 工具向后兼容
- **G6**: 写入时自动分类，不增加用户负担
- **G7**: 性能：权限过滤在 SQL 层完成，不拖慢查询

## 3. Schema Design

### 3.1 memories 表扩展

```sql
-- Migration: 0001_memory_access_control.sql

-- 新增上下文字段
ALTER TABLE memories ADD COLUMN channel TEXT;           -- signal/whatsapp/openpr/system/cron
ALTER TABLE memories ADD COLUMN chat_type TEXT;          -- dm/group/webhook/cron/internal
ALTER TABLE memories ADD COLUMN chat_id TEXT;            -- 具体会话ID（群JID/用户UUID/webhook源）
ALTER TABLE memories ADD COLUMN sender_id TEXT;          -- 发送者标识
ALTER TABLE memories ADD COLUMN topic_id TEXT;           -- 关联 topic（可为 NULL）
ALTER TABLE memories ADD COLUMN visibility TEXT          -- 访问控制级别
    NOT NULL DEFAULT 'private';
ALTER TABLE memories ADD COLUMN sensitivity TEXT         -- 敏感度标记
    NOT NULL DEFAULT 'normal';

-- 索引
CREATE INDEX IF NOT EXISTS idx_mem_channel ON memories(channel);
CREATE INDEX IF NOT EXISTS idx_mem_chat_id ON memories(chat_id);
CREATE INDEX IF NOT EXISTS idx_mem_sender ON memories(sender_id);
CREATE INDEX IF NOT EXISTS idx_mem_topic ON memories(topic_id);
CREATE INDEX IF NOT EXISTS idx_mem_visibility ON memories(visibility);
CREATE INDEX IF NOT EXISTS idx_mem_sensitivity ON memories(sensitivity);

-- 复合索引：最常用的查询模式
CREATE INDEX IF NOT EXISTS idx_mem_vis_cat ON memories(visibility, category);
CREATE INDEX IF NOT EXISTS idx_mem_sender_chat ON memories(sender_id, chat_id);
```

### 3.2 topics 表（新增）

```sql
CREATE TABLE IF NOT EXISTS topics (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,              -- 人类可读标题: "OpenPR 登录页 bug"
    project     TEXT,                       -- 项目标识: openpr/lc/sm/prx
    external_id TEXT,                       -- 外部引用: issue#42, PR#123, ticket-xxx
    external_url TEXT,                      -- 外部链接
    status      TEXT NOT NULL DEFAULT 'open', -- open/in_progress/resolved/archived
    tags        TEXT,                       -- JSON array: ["bug","frontend","urgent"]
    summary     TEXT,                       -- LLM 生成的最新摘要
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    resolved_at TEXT                        -- 解决时间
);

CREATE INDEX IF NOT EXISTS idx_topic_project ON topics(project);
CREATE INDEX IF NOT EXISTS idx_topic_status ON topics(status);
CREATE INDEX IF NOT EXISTS idx_topic_external ON topics(external_id);
CREATE INDEX IF NOT EXISTS idx_topic_updated ON topics(updated_at);

-- topic 全文搜索
CREATE VIRTUAL TABLE IF NOT EXISTS topics_fts
    USING fts5(title, summary, tags, content='topics', content_rowid='rowid');
```

### 3.3 topic_participants 表（新增）

```sql
-- 跟踪哪些用户参与了某个 topic
CREATE TABLE IF NOT EXISTS topic_participants (
    topic_id    TEXT NOT NULL,
    user_id     TEXT NOT NULL,
    role        TEXT NOT NULL DEFAULT 'participant', -- owner/assignee/participant/observer
    joined_at   TEXT NOT NULL,
    PRIMARY KEY (topic_id, user_id),
    FOREIGN KEY (topic_id) REFERENCES topics(id) ON DELETE CASCADE
);
```

## 4. Visibility Model

### 4.1 级别定义

| 级别 | 含义 | 查询可见条件 |
|------|------|-------------|
| `system` | 系统内部 | 仅 AI 内部处理可见，不返回给任何用户 |
| `owner` | 仅 owner | `requester.role == 'owner'` |
| `private` | 仅原始会话 | `requester.chat_id == memory.chat_id` |
| `user` | 特定用户 | `requester.user_id == memory.sender_id OR requester.role == 'owner'` |
| `group` | 群组共享 | `requester.chat_id == memory.chat_id` (群成员) |
| `project` | 项目组可见 | `requester.projects CONTAINS memory.topic.project` |
| `public` | 所有人可见 | 无限制 |

### 4.2 sensitivity 标记

| 级别 | 含义 | 额外限制 |
|------|------|---------|
| `normal` | 无敏感内容 | 无 |
| `sensitive` | 含敏感信息 | 非 owner 查询时内容脱敏 |
| `secret` | 机密 | 仅 owner 可见，忽略 visibility 设置 |

### 4.3 查询过滤 SQL

```sql
-- 对于 owner (AK):
WHERE 1=1  -- 无限制，可见全部

-- 对于已知用户（如 melon，projects=['sm']）:
WHERE (
    visibility = 'public'
    OR (visibility = 'private' AND chat_id = :current_chat_id)
    OR (visibility = 'user' AND sender_id = :requester_id)
    OR (visibility = 'group' AND chat_id = :current_chat_id)
    OR (visibility = 'project' AND topic_id IN (
        SELECT id FROM topics WHERE project IN ('sm')
    ))
)
AND sensitivity != 'secret'

-- 对于未知用户:
WHERE visibility = 'public'
AND sensitivity = 'normal'
```

## 5. Write-Time Auto-Classification

### 5.1 上下文提取（零成本，从 MessageContext 获取）

```rust
struct MemoryContext {
    channel: String,        // 从 message.channel
    chat_type: ChatType,    // 从 message.chat_type (dm/group)
    chat_id: String,        // 从 message.chat_id
    sender_id: String,      // 从 message.sender_id
}
```

### 5.2 visibility 自动判定规则

```rust
fn classify_visibility(ctx: &MemoryContext, content: &str, user_role: &UserRole) -> Visibility {
    // Rule 1: 系统/cron 产生的记忆
    if ctx.chat_type == ChatType::Cron || ctx.chat_type == ChatType::Internal {
        return Visibility::System;
    }
    
    // Rule 2: 含敏感关键词 → owner only
    if contains_sensitive_keywords(content) {
        return Visibility::Owner;
    }
    
    // Rule 3: owner 私聊 → owner
    if user_role == &UserRole::Owner && ctx.chat_type == ChatType::Dm {
        return Visibility::Owner;
    }
    
    // Rule 4: 群聊 → group
    if ctx.chat_type == ChatType::Group {
        return Visibility::Group;
    }
    
    // Rule 5: 已知用户私聊 → private
    if user_role != &UserRole::Unknown {
        return Visibility::Private;
    }
    
    // Rule 6: 未知用户 → private (最严格)
    Visibility::Private
}

fn contains_sensitive_keywords(content: &str) -> bool {
    const SENSITIVE: &[&str] = &[
        "ssh", "api_key", "密钥", "password", "token", "secret",
        "im-ops", "服务器地址", "ip地址", "private key",
    ];
    let lower = content.to_lowercase();
    SENSITIVE.iter().any(|kw| lower.contains(kw))
}
```

### 5.3 sensitivity 自动判定

```rust
fn classify_sensitivity(content: &str, visibility: &Visibility) -> Sensitivity {
    if visibility == &Visibility::Owner && contains_sensitive_keywords(content) {
        return Sensitivity::Secret;
    }
    if contains_pii(content) {  // 手机号、邮箱、身份证
        return Sensitivity::Sensitive;
    }
    Sensitivity::Normal
}
```

### 5.4 topic 自动关联

```rust
async fn resolve_topic(content: &str, ctx: &MemoryContext, db: &Connection) -> Option<String> {
    // Step 1: 关键词 + FTS 搜索现有 topics
    let candidates = search_topics_fts(content, db, 5).await;
    
    // Step 2: embedding 相似度匹配
    let embedding = get_embedding(content).await;
    let best_match = candidates.iter()
        .filter(|t| cosine_similarity(&embedding, &t.embedding) > 0.75)
        .max_by_key(|t| t.similarity_score);
    
    if let Some(topic) = best_match {
        // 关联到现有 topic
        update_topic_timestamp(topic.id, db).await;
        add_participant(topic.id, &ctx.sender_id, db).await;
        return Some(topic.id.clone());
    }
    
    // Step 3: 无匹配 → 轻量 LLM 判断是否需要创建新 topic
    // 使用子模型 (grok-fast) 节省成本
    // 简单对话不创建 topic，只有任务/问题/决策才创建
    let needs_topic = classify_needs_topic(content).await;
    if needs_topic {
        let topic = create_topic_from_content(content, ctx, db).await;
        return Some(topic.id);
    }
    
    None
}

fn classify_needs_topic(content: &str) -> bool {
    // 规则优先，LLM 兜底
    // 包含任务性关键词: bug, 修复, 部署, 实现, TODO, 问题, 需求
    // 排除: 闲聊, 问候, 确认
    let task_keywords = ["bug", "修复", "部署", "实现", "开发", "问题", 
                         "需求", "todo", "fix", "deploy", "issue"];
    let chat_keywords = ["你好", "谢谢", "ok", "好的", "收到", "嗯"];
    
    let lower = content.to_lowercase();
    let has_task = task_keywords.iter().any(|kw| lower.contains(kw));
    let is_chat = chat_keywords.iter().any(|kw| lower.contains(kw)) && content.len() < 20;
    
    has_task && !is_chat
}
```

## 6. User Permission Model

### 6.1 配置结构（从 config.toml 加载）

```toml
[users.ak]
uuid = "d26c8bda-58c5-4eb4-9997-0b011129fd58"
role = "owner"
# owner 无需 projects/visibility 配置，全部可见

[users.melon]
uuid = "e5dceaeb-8dff-4158-b650-95a85c9d577b"
role = "member"
projects = ["sm"]
visibility_ceiling = "project"

[users.cc]
uuid = "ce76fb41-ac9b-4f5e-8deb-e33724fbdb96"
role = "member"
projects = ["lc"]
visibility_ceiling = "private"      # 只能看自己会话的记忆
blocked_keywords = ["ssh", "im-ops", "服务器", "架构", "ip"]

[users.bafang]
uuid = "304202e7-0470-4955-b7da-974de0b58a3d"
role = "member"
projects = []
visibility_ceiling = "private"
```

### 6.2 运行时权限对象

```rust
#[derive(Debug, Clone)]
struct UserPermission {
    user_id: String,
    role: UserRole,             // Owner / Member / Guest / Unknown
    projects: Vec<String>,
    visibility_ceiling: Visibility,
    blocked_keywords: Vec<String>,
}

impl UserPermission {
    fn can_see(&self, memory: &Memory, current_chat_id: &str) -> bool {
        // Owner sees all
        if self.role == UserRole::Owner { return true; }
        
        // Secret: owner only
        if memory.sensitivity == Sensitivity::Secret { return false; }
        
        // Blocked keywords
        if self.blocked_keywords.iter().any(|kw| memory.content.contains(kw)) {
            return false;
        }
        
        // Visibility check
        match &memory.visibility {
            Visibility::Public => true,
            Visibility::Private => memory.chat_id == current_chat_id,
            Visibility::User => memory.sender_id == self.user_id,
            Visibility::Group => memory.chat_id == current_chat_id,
            Visibility::Project => {
                if let Some(topic) = &memory.topic {
                    self.projects.contains(&topic.project)
                } else {
                    false
                }
            }
            Visibility::Owner => false,
            Visibility::System => false,
        }
    }
}
```

## 7. Query Flow

### 7.1 完整查询流程

```
用户发消息 "openpr 登录 bug 进展"
    ↓
1. 识别用户身份 → UserPermission { role: member, projects: ["openpr"] }
    ↓
2. 构建 SQL WHERE (visibility 过滤)
    ↓
3. FTS + embedding 语义搜索 (在过滤后的结果集内)
    ↓
4. 后置过滤: blocked_keywords, sensitivity 脱敏
    ↓
5. topic 聚合: 如果命中 topic，拉取该 topic 下所有可见记忆
    ↓
6. 返回结果 (带 topic context)
```

### 7.2 跨渠道聚合查询

```rust
async fn query_topic_context(topic_id: &str, permission: &UserPermission, chat_id: &str) -> TopicContext {
    let memories = db.query(
        "SELECT * FROM memories WHERE topic_id = ?1 ORDER BY created_at",
        [topic_id]
    ).await;
    
    let visible = memories.into_iter()
        .filter(|m| permission.can_see(m, chat_id))
        .collect::<Vec<_>>();
    
    let topic = db.get_topic(topic_id).await;
    
    TopicContext {
        topic,
        memories: visible,
        participant_count: db.count_participants(topic_id).await,
        channels_involved: visible.iter().map(|m| &m.channel).collect::<HashSet<_>>(),
    }
}
```

## 8. Integration Points

### 8.1 OpenPR Webhook → Topic 同步

```rust
async fn handle_openpr_webhook(event: OpenPREvent) {
    let topic = find_or_create_topic(
        project: "openpr",
        external_id: format!("issue#{}", event.issue_id),
        title: event.issue_title,
    ).await;
    
    let memory = Memory {
        content: format_webhook_event(&event),
        channel: "openpr",
        chat_type: "webhook",
        sender_id: "system:openpr",
        topic_id: Some(topic.id),
        visibility: Visibility::Project,
        sensitivity: Sensitivity::Normal,
    };
    
    store_memory(memory).await;
    
    // 同步 topic 状态
    if event.action == "closed" {
        update_topic_status(topic.id, "resolved").await;
    }
}
```

### 8.2 memory_search 工具改造

```rust
// 现有接口不变，内部增加权限过滤
async fn memory_search(query: &str, ctx: &ToolContext) -> Vec<SearchResult> {
    let permission = get_user_permission(&ctx.sender_id);
    let scope = build_visibility_scope(&permission, &ctx.chat_id);
    
    // 原有语义搜索 + 权限 WHERE
    let results = semantic_search_with_scope(query, scope).await;
    
    // 后置过滤
    post_filter(results, &permission)
}

// memory_get 同理
async fn memory_get(key: &str, ctx: &ToolContext) -> Option<Memory> {
    let memory = db.get_by_key(key).await?;
    let permission = get_user_permission(&ctx.sender_id);
    if permission.can_see(&memory, &ctx.chat_id) {
        Some(memory)
    } else {
        None  // 静默返回空，不暴露记忆存在
    }
}
```

## 9. Migration Strategy

### Phase 1: Schema + Context Capture (非破坏性)
- ALTER TABLE 添加新字段（默认值保证兼容）
- 所有新记忆写入时携带上下文元数据
- 现有记忆: visibility='owner', channel=NULL (向后兼容)
- 查询逻辑: 如果 channel=NULL 视为旧数据，仅 owner 可见

### Phase 2: Topic System
- 创建 topics + topic_participants 表
- 实现 topic 自动关联逻辑
- 新记忆自动关联 topic

### Phase 3: Query-Time Access Control
- 实现 UserPermission 结构
- 改造 memory_search/memory_get 添加权限过滤
- 从 config.toml 加载用户权限配置

### Phase 4: Integration
- OpenPR webhook → topic 同步
- topic 状态自动更新
- 跨渠道 topic 聚合查询

## 10. Performance Considerations

- **索引覆盖**: visibility + category 复合索引，避免全表扫描
- **权限 SQL 注入**: 在 SQL 层完成过滤，不在应用层遍历
- **topic 匹配**: FTS 先筛选，embedding 只对 top-5 做相似度计算
- **缓存**: UserPermission 按 session 缓存，不每次查询重建
- **旧数据迁移**: 懒迁移，查询到旧数据时自动补充元数据

## 11. Security Guarantees

1. **默认拒绝**: 未知用户只能看 `public` 级别（几乎为零）
2. **owner 字段不可伪造**: sender_id 从 Signal/WA 协议层获取，不可篡改
3. **静默拒绝**: 无权限时返回空结果，不暴露"记忆存在但你无权查看"
4. **敏感词兜底**: 即使 visibility 通过，blocked_keywords 仍然生效
5. **secret 级别绝对隔离**: sensitivity=secret 忽略所有 visibility 规则，仅 owner
6. **审计日志**: 记录非 owner 的 memory_search 调用（who/what/when/results_count）

## 12. Open Questions

1. topic 自动合并策略：发现两个 topic 其实是同一件事时如何处理？
2. topic 生命周期：archived 后多久清理？还是永久保留？
3. 群成员变动：用户退群后是否还能看到历史群记忆？
4. 性能基准：大量记忆（>10K）时权限过滤的查询延迟是否可接受？
5. LLM 分类准确度：topic 自动归类的误判率和成本如何平衡？
