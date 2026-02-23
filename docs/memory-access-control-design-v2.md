# PRX Memory Access Control & Topic Association Design v2

> Status: REVISED v2.1 (based on round-2 audit feedback)
> Author: David (AI Architect)
> Audit: Codex Security Audit Round 1 + Round 2
> Date: 2026-02-23

---

## 1. 架构总览

```
┌──────────────────────────────────────────────────┐
│                   Query Layer                     │
│  memory_search / memory_get / topic_query         │
│         ↓ Principal + Scope                       │
├──────────────────────────────────────────────────┤
│                  Policy Engine                    │
│  SQL scope 注入 → 权限过滤在 DB 层完成            │
│  observe mode → deny mode 灰度切换               │
├──────────────────────────────────────────────────┤
│               Identity Resolver                   │
│  channel_account → identity_bindings → user_id    │
│  未绑定 → Unknown（最低权限）                     │
├──────────────────────────────────────────────────┤
│                  Storage Layer                     │
│  SQLite: memories + topics + identity_bindings    │
│  单一数据源（文件记忆迁移到 SQLite）               │
└──────────────────────────────────────────────────┘
```

## 2. 数据源统一（Phase 0，前置条件）

### 问题
当前 memory_search/memory_get 读文件（MEMORY.md, memory/*.md），brain.db 另有一套。双轨 = 策略不一致。

### 方案
1. brain.db 作为唯一数据源
2. 启动时将 MEMORY.md + memory/*.md 水合到 SQLite（已有逻辑）
3. memory_search/memory_get 工具改为查询 SQLite
4. 文件保留为人类可读备份，但不再作为查询源
5. 写入时双写（SQLite 为主，文件为副本）

### 回滚
保留旧的文件查询路径，通过 feature gate `memory_acl_enabled` 控制。默认 false。

## 3. Schema

### 3.1 identity_bindings（新增）

```sql
CREATE TABLE IF NOT EXISTS identity_bindings (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL,           -- 内部统一 user_id
    channel         TEXT NOT NULL,           -- signal/whatsapp/openpr/telegram
    channel_account TEXT NOT NULL,           -- Signal UUID / WA JID / etc
    display_name    TEXT,
    bound_at        TEXT NOT NULL,
    bound_by        TEXT NOT NULL,           -- 谁绑定的（owner UUID 或 'system'）
    UNIQUE(channel, channel_account)
);

CREATE INDEX IF NOT EXISTS idx_ib_user ON identity_bindings(user_id);
CREATE INDEX IF NOT EXISTS idx_ib_channel_account ON identity_bindings(channel, channel_account);
```

### 3.2 user_policies（新增）

```sql
CREATE TABLE IF NOT EXISTS user_policies (
    user_id             TEXT PRIMARY KEY,
    role                TEXT NOT NULL DEFAULT 'guest',    -- owner/member/guest
    projects            TEXT NOT NULL DEFAULT '[]',       -- JSON array
    visibility_ceiling  TEXT NOT NULL DEFAULT 'private',  -- 最高可见级别
    blocked_patterns    TEXT NOT NULL DEFAULT '[]',       -- JSON array of regex patterns
    policy_version      INTEGER NOT NULL DEFAULT 1,
    updated_at          TEXT NOT NULL
);
```

### 3.3 memories 表扩展

```sql
ALTER TABLE memories ADD COLUMN channel TEXT;
ALTER TABLE memories ADD COLUMN chat_type TEXT;            -- dm/group/webhook/cron/internal
ALTER TABLE memories ADD COLUMN chat_id TEXT;
ALTER TABLE memories ADD COLUMN sender_id TEXT;            -- 内部 user_id（经过 identity 解析）
ALTER TABLE memories ADD COLUMN raw_sender TEXT;           -- 原始渠道账号（审计用）
ALTER TABLE memories ADD COLUMN topic_id TEXT;
ALTER TABLE memories ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private';
ALTER TABLE memories ADD COLUMN sensitivity TEXT NOT NULL DEFAULT 'normal';
ALTER TABLE memories ADD COLUMN risk_signals TEXT DEFAULT '[]';  -- JSON: 命中的风险规则
ALTER TABLE memories ADD COLUMN policy_version INTEGER DEFAULT 1;

-- 核心查询索引（覆盖三元组 + sensitivity）
CREATE INDEX IF NOT EXISTS idx_mem_vis_chan_type_chat
    ON memories(visibility, channel, chat_type, chat_id, sensitivity, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_mem_sender ON memories(sender_id);
CREATE INDEX IF NOT EXISTS idx_mem_topic_time ON memories(topic_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_mem_channel ON memories(channel);
```

### 3.4 topics 表

```sql
CREATE TABLE IF NOT EXISTS topics (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    project         TEXT,
    external_id     TEXT,
    external_url    TEXT,
    fingerprint     TEXT,                    -- SHA256(normalized_title + project)，幂等键
    status          TEXT NOT NULL DEFAULT 'open',
    tags            TEXT DEFAULT '[]',       -- JSON array
    summary         TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    resolved_at     TEXT,
    UNIQUE(project, external_id),            -- 有 external_id 时防重复
    UNIQUE(fingerprint)                      -- 无 external_id 时用 fingerprint 防重复
);

CREATE INDEX IF NOT EXISTS idx_topic_project ON topics(project);
CREATE INDEX IF NOT EXISTS idx_topic_status ON topics(status);
CREATE INDEX IF NOT EXISTS idx_topic_external ON topics(external_id);

-- FTS + 同步触发器
CREATE VIRTUAL TABLE IF NOT EXISTS topics_fts
    USING fts5(title, summary, tags, content='topics', content_rowid='rowid');

CREATE TRIGGER IF NOT EXISTS topics_ai AFTER INSERT ON topics BEGIN
    INSERT INTO topics_fts(rowid, title, summary, tags)
    VALUES (new.rowid, new.title, new.summary, new.tags);
END;

CREATE TRIGGER IF NOT EXISTS topics_ad AFTER DELETE ON topics BEGIN
    INSERT INTO topics_fts(topics_fts, rowid, title, summary, tags)
    VALUES ('delete', old.rowid, old.title, old.summary, old.tags);
END;

CREATE TRIGGER IF NOT EXISTS topics_au AFTER UPDATE ON topics BEGIN
    INSERT INTO topics_fts(topics_fts, rowid, title, summary, tags)
    VALUES ('delete', old.rowid, old.title, old.summary, old.tags);
    INSERT INTO topics_fts(rowid, title, summary, tags)
    VALUES (new.rowid, new.title, new.summary, new.tags);
END;
```

### 3.5 topic_participants 表

```sql
CREATE TABLE IF NOT EXISTS topic_participants (
    topic_id    TEXT NOT NULL,
    user_id     TEXT NOT NULL,
    role        TEXT NOT NULL DEFAULT 'participant',
    joined_at   TEXT NOT NULL,
    PRIMARY KEY (topic_id, user_id),
    FOREIGN KEY (topic_id) REFERENCES topics(id) ON DELETE CASCADE
);
```

### 3.6 topic_aliases（软合并）

```sql
CREATE TABLE IF NOT EXISTS topic_aliases (
    from_topic_id TEXT NOT NULL,
    to_topic_id   TEXT NOT NULL,
    reason        TEXT,
    operator      TEXT NOT NULL,              -- 谁合并的
    created_at    TEXT NOT NULL,
    PRIMARY KEY (from_topic_id),
    FOREIGN KEY (to_topic_id) REFERENCES topics(id)
);
```

### 3.7 access_audit_log（审计事件）

```sql
CREATE TABLE IF NOT EXISTS access_audit_log (
    id          TEXT PRIMARY KEY,
    timestamp   TEXT NOT NULL,
    requester   TEXT NOT NULL,                -- user_id
    action      TEXT NOT NULL,                -- search/get/denied
    query       TEXT,
    memory_id   TEXT,
    policy_rule TEXT,                         -- 命中的策略规则
    result      TEXT NOT NULL                 -- allowed/denied/filtered
);

CREATE INDEX IF NOT EXISTS idx_audit_time ON access_audit_log(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_audit_requester ON access_audit_log(requester);
```

## 4. Principal 模型

### 4.1 身份解析流程

```
收到消息 (channel=signal, raw_sender=UUID-xxx)
    ↓
1. 查 identity_bindings WHERE channel='signal' AND channel_account='UUID-xxx'
    ├─ 找到 → user_id = bindings.user_id
    └─ 没找到 → user_id = 'anonymous:{channel}:{raw_sender}'
    ↓
2. 查 user_policies WHERE user_id = ?
    ├─ 找到 → 加载 role/projects/ceiling/blocked_patterns
    └─ 没找到 → 默认 guest: role=guest, projects=[], ceiling=private
    ↓
3. 构建 Principal
```

### 4.2 Principal 结构

```rust
struct Principal {
    user_id: String,
    role: Role,                    // Owner / Member / Guest / Anonymous
    projects: Vec<String>,
    visibility_ceiling: Visibility,
    blocked_patterns: Vec<Regex>,  // 预编译正则
    current_channel: String,
    current_chat_id: String,
    current_chat_type: ChatType,
}

enum Role { Owner, Member, Guest, Anonymous }

enum Visibility {
    System,     // 0 - AI 内部
    Owner,      // 1 - 仅 owner
    Private,    // 2 - 仅原始 DM 会话
    User,       // 3 - 特定用户所有会话
    Group,      // 4 - 群组内共享
    Project,    // 5 - 项目组可见
    Public,     // 6 - 所有人
}
```

### 4.3 identity_bindings 初始化

从 config.toml 或启动时自动创建：

```toml
[[identity_bindings]]
user_id = "ak"
channel = "signal"
channel_account = "d26c8bda-58c5-4eb4-9997-0b011129fd58"

[[identity_bindings]]
user_id = "ak"
channel = "whatsapp"
channel_account = "995551518602@s.whatsapp.net"

[[identity_bindings]]
user_id = "melon"
channel = "signal"
channel_account = "e5dceaeb-8dff-4158-b650-95a85c9d577b"

[[identity_bindings]]
user_id = "cc"
channel = "signal"
channel_account = "ce76fb41-ac9b-4f5e-8deb-e33724fbdb96"
```

AK 从 Signal 私聊和 WA 私聊发消息 → 都解析为 `user_id=ak` → owner 权限。

## 5. Visibility 判定（修订）

### 5.1 写入时分类

```rust
fn classify_memory(ctx: &MessageContext, content: &str, principal: &Principal) -> MemoryMeta {
    let mut risk_signals = Vec::new();
    
    // Step 1: 风险信号检测（不直接决定 visibility）
    if matches_sensitive_patterns(content) {
        risk_signals.push("sensitive_keyword_match");
    }
    if contains_pii(content) {
        risk_signals.push("pii_detected");
    }
    
    // Step 2: 基于来源的默认 visibility
    let base_visibility = match (&principal.role, &ctx.chat_type) {
        // owner 私聊 → owner（最严格）
        (Role::Owner, ChatType::Dm) => Visibility::Owner,
        // webhook/cron/system → owner
        (_, ChatType::Webhook | ChatType::Cron | ChatType::Internal) => Visibility::Owner,
        // 群聊 → group
        (_, ChatType::Group) => Visibility::Group,
        // 已知用户私聊 → private
        (Role::Member, ChatType::Dm) => Visibility::Private,
        // 未知用户 → private
        _ => Visibility::Private,
    };
    
    // Step 3: 风险信号提权（只能变更严格，不能变更宽松）
    let visibility = if !risk_signals.is_empty() && base_visibility > Visibility::Owner {
        Visibility::Owner  // 有风险信号 → 提权到 owner
    } else {
        base_visibility
    };
    
    // Step 4: sensitivity
    let sensitivity = if risk_signals.iter().any(|s| s == &"sensitive_keyword_match") {
        Sensitivity::Sensitive
    } else if risk_signals.iter().any(|s| s == &"pii_detected") {
        Sensitivity::Sensitive
    } else {
        Sensitivity::Normal
    };
    
    MemoryMeta {
        channel: ctx.channel.clone(),
        chat_type: ctx.chat_type.clone(),
        chat_id: ctx.chat_id.clone(),
        sender_id: principal.user_id.clone(),
        raw_sender: ctx.raw_sender.clone(),
        visibility,
        sensitivity,
        risk_signals: serde_json::to_string(&risk_signals).unwrap(),
        policy_version: CURRENT_POLICY_VERSION,
    }
}
```

关键改进（对应 P0-3）：
- 风险信号**不直接决定** visibility，只作为提权依据
- 基础 visibility 由 (role, chat_type) 二元组决定，规则清晰
- 只能变更严格（Owner 方向），不能变宽松

### 5.2 敏感模式匹配（修订）

```rust
fn matches_sensitive_patterns(content: &str) -> bool {
    // NFKC 标准化 + lowercase
    let normalized = content.nfkc().collect::<String>().to_lowercase();
    // 去除空格干扰: "s s h" → "ssh"
    let no_space = normalized.replace(' ', "");
    
    static PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| vec![
        Regex::new(r"\bssh\b").unwrap(),
        Regex::new(r"\bapi[_-]?key\b").unwrap(),
        Regex::new(r"密钥|私钥|秘钥").unwrap(),
        Regex::new(r"\bpassw(or)?d\b").unwrap(),
        Regex::new(r"\btok(en)?\b").unwrap(),
        Regex::new(r"\bsecret\b").unwrap(),
        Regex::new(r"im-ops|服务器地址").unwrap(),
        Regex::new(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b").unwrap(), // IP
        Regex::new(r"\bprivate[_\s]?key\b").unwrap(),
    ]);
    
    PATTERNS.iter().any(|re| re.is_match(&normalized) || re.is_match(&no_space))
}
```

## 6. Policy Engine（查询时）

### 6.1 SQL Scope 构建

```rust
impl Principal {
    fn build_sql_scope(&self) -> (String, Vec<Value>) {
        match self.role {
            Role::Owner => {
                // Owner 看全部
                ("1=1".to_string(), vec![])
            }
            Role::Member | Role::Guest => {
                // 阶梯式权限，受 ceiling 限制
                let ceiling_ord = self.visibility_ceiling.ordinal();
                let mut conditions = vec!["visibility = 'public'".to_string()];
                let mut params: Vec<Value> = vec![];
                let mut idx = 1;
                
                // private: 同 channel + chat_type=dm + chat_id
                // 三元组约束防止跨渠道 chat_id 冲突串读
                if ceiling_ord >= Visibility::Private.ordinal() {
                    conditions.push(format!(
                        "(visibility = 'private' AND chat_type = 'dm' AND channel = ?{} AND chat_id = ?{})",
                        idx, idx + 1
                    ));
                    params.push(self.current_channel.clone().into());
                    params.push(self.current_chat_id.clone().into());
                    idx += 2;
                }
                
                // user: sender_id 匹配（跨渠道有效，因为已做 identity 绑定）
                if ceiling_ord >= Visibility::User.ordinal() {
                    conditions.push(format!(
                        "(visibility = 'user' AND sender_id = ?{})", idx
                    ));
                    params.push(self.user_id.clone().into());
                    idx += 1;
                }
                
                // group: 同 channel + chat_type=group + chat_id
                if ceiling_ord >= Visibility::Group.ordinal() {
                    conditions.push(format!(
                        "(visibility = 'group' AND chat_type = 'group' AND channel = ?{} AND chat_id = ?{})",
                        idx, idx + 1
                    ));
                    params.push(self.current_channel.clone().into());
                    params.push(self.current_chat_id.clone().into());
                    idx += 2;
                }
                
                // project: topic.project IN user.projects AND (参与者 OR 显式 observer)
                // 最小权限: 项目匹配是必要条件，参与或 observer 是充分条件
                if ceiling_ord >= Visibility::Project.ordinal() && !self.projects.is_empty() {
                    let placeholders = self.projects.iter()
                        .enumerate()
                        .map(|(i, _)| format!("?{}", idx + i))
                        .collect::<Vec<_>>()
                        .join(",");
                    let user_param_idx = idx + self.projects.len();
                    conditions.push(format!(
                        "(visibility = 'project' AND topic_id IN (\
                            SELECT t.id FROM topics t \
                            INNER JOIN topic_participants tp ON tp.topic_id = t.id \
                            WHERE t.project IN ({}) \
                            AND tp.user_id = ?{}\
                        ))",
                        placeholders,
                        user_param_idx
                    ));
                    for p in &self.projects {
                        params.push(p.clone().into());
                    }
                    params.push(self.user_id.clone().into());
                }
                
                // 排除 secret
                let where_clause = format!(
                    "({}) AND sensitivity != 'secret'",
                    conditions.join(" OR ")
                );
                
                (where_clause, params)
            }
            Role::Anonymous => {
                // 只能看 public + normal
                ("visibility = 'public' AND sensitivity = 'normal'".to_string(), vec![])
            }
        }
    }
}
```

关键改进（对应 P0-1, P1-5, P1-6）：
- `private` 强制 `chat_type = 'dm'`
- `group` 强制 `chat_type = 'group'`
- `visibility_ceiling` 在 SQL 构建阶段就裁剪
- `project` 可见需要 `topic.project` 匹配 **且** `topic_participants` 参与

### 6.2 后置过滤（blocked_patterns）

```rust
fn post_filter(memories: Vec<Memory>, principal: &Principal) -> Vec<Memory> {
    if principal.role == Role::Owner { return memories; }
    if principal.blocked_patterns.is_empty() { return memories; }
    
    memories.into_iter().filter(|m| {
        let normalized = m.content.nfkc().collect::<String>().to_lowercase();
        !principal.blocked_patterns.iter().any(|re| re.is_match(&normalized))
    }).collect()
}
```

### 6.3 审计日志

```rust
async fn log_access(
    requester: &str,
    action: &str,
    query: Option<&str>,
    memory_id: Option<&str>,
    policy_rule: Option<&str>,
    result: &str,  // allowed/denied/filtered
    db: &Connection,
) {
    // 仅记录非 owner 的访问
    db.execute(
        "INSERT INTO access_audit_log (id, timestamp, requester, action, query, memory_id, policy_rule, result)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            Uuid::new_v4().to_string(),
            Utc::now().to_rfc3339(),
            requester, action, query, memory_id, policy_rule, result,
        ],
    ).ok(); // 审计失败不阻塞主流程
}
```

## 7. Topic 系统

### 7.1 自动关联（修订）

```rust
async fn resolve_topic(
    content: &str,
    ctx: &MessageContext,
    principal: &Principal,
    db: &Connection,
) -> Option<String> {
    // Step 1: 规则判断 — 是否需要 topic
    if !needs_topic(content) { return None; }
    
    // Step 2: 有 external_id 时精确匹配
    if let Some(ext_id) = extract_external_ref(content) {
        if let Some(topic) = find_topic_by_external(ext_id, db).await {
            add_participant(&topic.id, &principal.user_id, db).await;
            return Some(topic.id);
        }
    }
    
    // Step 3: FTS 搜索现有 topics
    let candidates = search_topics_fts(content, db, 5).await;
    
    // Step 4: embedding 相似度 (仅对 top-5)
    if let Ok(embedding) = get_embedding(content).await {
        if let Some(best) = candidates.iter()
            .filter(|t| cosine_similarity(&embedding, &t.embedding) > 0.78)
            .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap())
        {
            // 解析 aliases
            let real_id = resolve_alias(&best.id, db).await.unwrap_or(best.id.clone());
            add_participant(&real_id, &principal.user_id, db).await;
            touch_topic(&real_id, db).await;
            return Some(real_id);
        }
    }
    
    // Step 5: 创建新 topic（事务 + UPSERT 防并发）
    let project = infer_project(content);
    let title = generate_topic_title(content);  // 规则提取，不用 LLM
    
    let topic_id = Uuid::new_v4().to_string();
    let fingerprint = sha256_hex(&format!(
        "{}:{}",
        project.as_deref().unwrap_or(""),
        normalize_title(&title)
    ));
    let actual_id: String = db.query_row(
        "INSERT INTO topics (id, title, project, fingerprint, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6)
         ON CONFLICT(fingerprint) DO UPDATE SET updated_at = ?6
         RETURNING id",
        params![topic_id, title, project, fingerprint, now(), now()],
    ).await.unwrap_or(topic_id);  // fallback to generated id
    
    add_participant(&actual_id, &principal.user_id, db).await;
    Some(actual_id)
}
```

### 7.2 规则判断

```rust
fn needs_topic(content: &str) -> bool {
    let lower = content.to_lowercase();
    let len = content.chars().count();
    
    // 短消息（<15字）且无任务词 → 不需要 topic
    if len < 15 {
        let task_words = ["bug", "修复", "部署", "实现", "开发", "问题",
                          "需求", "fix", "deploy", "issue", "error", "todo"];
        return task_words.iter().any(|kw| lower.contains(kw));
    }
    
    // 纯寒暄 → 不需要
    let greetings = ["你好", "谢谢", "ok", "好的", "收到", "嗯", "哈哈"];
    if greetings.iter().any(|g| lower == *g) { return false; }
    
    // 其他 → 需要
    true
}

fn infer_project(content: &str) -> Option<String> {
    let lower = content.to_lowercase();
    if lower.contains("openpr") || lower.contains("治理") { return Some("openpr".into()); }
    if lower.contains("lc") || lower.contains("彩票") { return Some("lc".into()); }
    if lower.contains("sm") || lower.contains("量表") || lower.contains("心理") { return Some("sm".into()); }
    if lower.contains("prx") || lower.contains("zeroclaw") || lower.contains("vano") { return Some("prx".into()); }
    None
}
```

### 7.3 跨渠道 topic 查询（SQL 下推）

```rust
async fn query_topic_context(
    topic_id: &str,
    principal: &Principal,
    db: &Connection,
    limit: usize,
) -> TopicContext {
    let (scope_sql, scope_params) = principal.build_sql_scope();
    
    // 权限过滤在 SQL 层完成，带分页
    let sql = format!(
        "SELECT * FROM memories WHERE topic_id = ?1 AND ({}) ORDER BY created_at DESC LIMIT ?",
        scope_sql
    );
    
    let memories = db.query(&sql, /* params */).await;
    let topic = db.get_topic(topic_id).await;
    
    TopicContext { topic, memories, total: count }
}
```

## 8. memory_search/memory_get 改造

```rust
// memory_search: 统一入口
async fn memory_search(query: &str, ctx: &ToolContext) -> Vec<SearchResult> {
    let principal = resolve_principal(&ctx.sender_id, &ctx.channel, &ctx.chat_id, &ctx.chat_type).await;
    let (scope_sql, scope_params) = principal.build_sql_scope();
    
    // FTS 搜索 + 权限 WHERE（SQL 层）
    let fts_results = db.query(&format!(
        "SELECT m.* FROM memories m
         JOIN memories_fts f ON m.rowid = f.rowid
         WHERE memories_fts MATCH ?1 AND ({})
         ORDER BY rank LIMIT 20",
        scope_sql
    ), /* params */).await;
    
    // embedding 语义搜索（在权限过滤后的候选集内）
    let semantic_results = semantic_search_with_scope(query, &scope_sql, &scope_params).await;
    
    // 合并 + 去重 + 后置过滤
    let merged = merge_and_dedup(fts_results, semantic_results);
    let filtered = post_filter(merged, &principal);
    
    // 审计
    if principal.role != Role::Owner {
        log_access(&principal.user_id, "search", Some(query), None, None,
            if filtered.is_empty() { "no_results" } else { "allowed" }).await;
    }
    
    filtered
}

// memory_get: 单条获取 — 与 search 共用 SQL scope，无双轨判定
async fn memory_get(key: &str, ctx: &ToolContext) -> Option<Memory> {
    let principal = resolve_principal(&ctx.sender_id, &ctx.channel, &ctx.chat_id, &ctx.chat_type).await;
    let (scope_sql, scope_params) = principal.build_sql_scope();
    
    // 单次 SQL 查询: key 匹配 + 权限过滤，策略与 search 完全一致
    let result = db.query_row(&format!(
        "SELECT * FROM memories WHERE key = ?1 AND ({})",
        scope_sql
    ), /* key + scope_params */).await;
    
    match result {
        Some(memory) => {
            let filtered = post_filter(vec![memory], &principal);
            filtered.into_iter().next()
        }
        None => {
            // 静默返回空，不暴露存在性
            log_access(&principal.user_id, "get_denied", None, Some(key),
                Some("scope_filter"), "denied").await;
            None
        }
    }
}
```

## 9. 实施计划

### Phase 0: 数据源统一（1-2天）
- [ ] memory_search/memory_get 切换到 SQLite 查询
- [ ] 启动水合: MEMORY.md + memory/*.md → brain.db
- [ ] 写入双写: SQLite 为主 + 文件为副本
- [ ] feature gate: `memory_acl_enabled = false`
- **验收**: 工具行为与旧版一致，无功能回退

### Phase 1: Schema + Identity（1天）
- [ ] 执行 migration: 新字段 + identity_bindings + user_policies + access_audit_log
- [ ] 从 config.toml 加载 identity_bindings 和 user_policies
- [ ] 实现 Principal 解析链
- [ ] 所有新记忆写入带上下文元数据
- [ ] 旧记忆: visibility='owner', channel=NULL
- **验收**: 新记忆有完整元数据，旧记忆安全默认

### Phase 2: Observe Mode（3天运行）
- [ ] 实现 Policy Engine (build_sql_scope)
- [ ] 启用 observe mode: 记录"新策略会拒绝哪些查询"但不实际拒绝
- [ ] access_audit_log 记录所有 would-be-denied
- [ ] 每天检查误拒绝/误放行比例
- **验收**: observe 日志显示策略合理，误拒绝率 < 5%

### Phase 3: ACL 灰度启用（1天）
- [ ] `memory_acl_enabled = true`
- [ ] 先对 Anonymous 生效 → Guest → Member
- [ ] Owner 始终不受限
- [ ] 后置过滤 blocked_patterns 生效
- **验收**: 非 owner 查询被正确过滤

### Phase 4: Topic 系统（2-3天）
- [ ] topics + topic_participants + topic_aliases 表
- [ ] topics_fts + 触发器
- [ ] resolve_topic 自动关联逻辑
- [ ] infer_project 规则匹配
- [ ] topic 查询 API
- **验收**: 跨渠道同一事务可通过 topic 聚合

### Phase 5: Integration（1-2天）
- [ ] OpenPR webhook → topic 同步
- [ ] topic 状态跟踪
- [ ] 审计日志定期清理（保留 90 天）
- **验收**: webhook 事件自动关联到 topic

### 总计: ~10天（含 3 天 observe）

## 10. 回滚策略

每个 Phase 独立可回滚：

| Phase | 回滚方式 |
|-------|---------|
| 0 | feature gate 切回文件查询 |
| 1 | 新字段有默认值，不影响旧逻辑 |
| 2 | observe mode 本身就是无副作用的 |
| 3 | `memory_acl_enabled = false` 一键关闭 |
| 4 | topic_id = NULL 的记忆正常工作 |
| 5 | webhook 入口可独立禁用 |

## 11. 性能目标

| 指标 | 10K 记忆 | 100K 记忆 | 1M 记忆 |
|------|----------|-----------|---------|
| P50 | < 20ms | < 50ms | < 80ms |
| P95 | < 50ms | < 150ms | < 250ms |
| P99 | < 100ms | < 300ms | < 500ms |

验证方式: EXPLAIN QUERY PLAN 确认索引命中，不达标不合并。

## 12. 安全保证

1. **默认拒绝**: Anonymous 仅 public + normal
2. **身份绑定**: 未绑定渠道账号 → Anonymous，不靠猜测
3. **风险信号 ≠ 授权决策**: 信号只能提权（变严格），不能降权
4. **三元组判定**: (visibility, chat_type, chat_id) 不再混淆 dm/group
5. **visibility_ceiling SQL 下推**: 配置即生效，不存在"配了没用"
6. **topic 参与者验证**: project 可见需要 project 匹配 + participants 参与
7. **静默拒绝**: 不暴露记忆存在性
8. **审计链**: 非 owner 所有访问记录可查
9. **策略版本化**: 每条记忆记录 policy_version，支持回放
10. **sensitivity 不可自动降级**: secret/sensitive 降级需 owner 手动操作
