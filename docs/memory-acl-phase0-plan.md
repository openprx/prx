# Phase 0: 数据源统一 — 实施计划

## 目标
将 memory_search/memory_get 工具从文件读取切换到 SQLite 查询，为后续 ACL 打基础。

## 当前状态
- `memory_search` (`src/tools/memory_search.rs`): 读取 MEMORY.md + memory/*.md 文件
- `memory_get` (`src/tools/memory_get.rs`): 读取 MEMORY.md + memory/*.md 文件
- `brain.db` (`src/memory/sqlite.rs`): 已有 memories 表 + FTS + embedding
- 启动水合 (`src/memory/snapshot.rs`): 已有 MEMORY.md → SQLite 水合逻辑

## 任务

### Task 1: 扩展 memories 表 schema
在 `src/memory/snapshot.rs` 的 CREATE TABLE 中添加新字段（全部有默认值，向后兼容）:
```sql
channel TEXT,
chat_type TEXT,
chat_id TEXT,
sender_id TEXT,
raw_sender TEXT,
topic_id TEXT,
visibility TEXT NOT NULL DEFAULT 'private',
sensitivity TEXT NOT NULL DEFAULT 'normal',
risk_signals TEXT DEFAULT '[]',
policy_version INTEGER DEFAULT 1
```
添加索引:
```sql
CREATE INDEX IF NOT EXISTS idx_mem_vis_chan_type_chat
    ON memories(visibility, channel, chat_type, chat_id, sensitivity, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_mem_sender ON memories(sender_id);
CREATE INDEX IF NOT EXISTS idx_mem_topic_time ON memories(topic_id, created_at DESC);
```

### Task 2: 改造 memory_search
修改 `src/tools/memory_search.rs`:
- 从文件搜索改为 SQLite FTS + embedding 搜索
- 保持工具接口不变 (query: String, max_results: Option<usize>)
- 查询 brain.db 的 memories 表
- 返回格式不变: key, content, snippet

### Task 3: 改造 memory_get
修改 `src/tools/memory_get.rs`:
- 从文件读取改为 SQLite 按 key 查询
- 保持工具接口不变 (path/key: String, from: Option<usize>, lines: Option<usize>)
- 如果 key 匹配 memories 表的 key → 返回 content
- 如果 key 看起来像文件路径 → fallback 读文件（兼容期）

### Task 4: 水合增强
修改 `src/memory/snapshot.rs`:
- 水合时为旧记忆设置 visibility='owner' (最严格默认)
- memory/*.md 文件也水合到 SQLite (不只是 MEMORY.md)
- 水合后记录日志: "Hydrated N memories from files"

### Task 5: 写入双写
找到记忆写入点（可能在 `src/memory/sqlite.rs` 或 agent loop 中）:
- SQLite 为主写入
- 同时写入对应文件 (MEMORY.md 或 memory/YYYY-MM-DD.md) 作为备份
- 新写入的记忆携带完整上下文 (channel, chat_type 等，暂时可为 NULL)

### Task 6: Feature Gate
在 config.toml 中添加:
```toml
[memory]
acl_enabled = false
```
在 `src/config/` 中解析该字段。
当前 Phase 0 只做数据源切换，acl_enabled 始终 false，不启用任何权限过滤。

### Task 7: 测试
- 确保 `cargo test --lib` 全部通过
- 确保 `cargo check` 无错误
- 如果现有测试引用了 memory_search/memory_get 的文件读取逻辑，更新测试

## 参考文件
- 设计文档: `docs/memory-access-control-design-v2.md`
- 现有代码: `src/tools/memory_search.rs`, `src/tools/memory_get.rs`, `src/memory/snapshot.rs`, `src/memory/sqlite.rs`
- 配置: `src/config/mod.rs` 或 `src/config/types.rs`

## 约束
- 不改变工具的外部接口
- 不启用 ACL 过滤（Phase 3 才启用）
- 旧记忆全部默认 visibility='owner'
- 向后兼容: 如果 SQLite 查询失败，可 fallback 文件读取
