# PRX 全模块回归审计报告

**日期:** 2026-03-19
**范围:** `/opt/worker/code/agents/prx/src/` 全部 266 个 Rust 文件, 170K+ LOC
**审计员:** Claude Explore (功能/逻辑/记忆/架构) + Codex CLI (安全/并发/DB/性能/资源)
**构建状态:** `cargo check` ✅ | `cargo clippy` ✅ 零 warning | `cargo test` 3411 passed, 0 failed

---

## 执行摘要

| 严重度 | 数量 | 说明 |
|--------|------|------|
| **CRITICAL** | 3 | 必须在发布前修复 |
| **HIGH** | 8 | 高优先级，影响数据完整性或安全性 |
| **MEDIUM** | 11 | 中优先级，影响可靠性或健壮性 |
| **LOW** | 4 | 低优先级，代码质量改进 |
| **总计** | **26** | |

---

## CRITICAL — 阻塞发布

### C-1: SQLite 主记忆后端未启用外键约束
- **文件:** `memory/sqlite.rs:159-165`
- **来源:** Codex
- **描述:** `SqliteMemory` 初始化时设置了 WAL、mmap 等性能 PRAGMA，但**遗漏了 `PRAGMA foreign_keys = ON`**。SQLite 默认 FK 不生效，所有 `ON DELETE CASCADE` 约束被静默忽略。
- **影响:** `conversation_turns` 删除父 `sessions` 时不级联，`topic_participants` 同理。长期运行将积累大量孤儿记录。
- **对比:** `cron/store.rs:516`、`xin/store.rs:481`、`memory/mod.rs:378` 均正确设置。
- **修复:** 在 PRAGMA 列表中添加 `PRAGMA foreign_keys = ON;`

### C-2: Cron 调度器无原子任务认领 (双重执行风险)
- **文件:** `cron/scheduler.rs:36-48`, `cron/store.rs:154-175`
- **来源:** Codex
- **描述:** `due_jobs()` 仅执行 SELECT 查询，无 `WHERE status = 'pending'` + `UPDATE status = 'running'` 的原子认领步骤。多实例场景下同一任务会被重复执行。
- **影响:** cron 任务可能产生重复副作用（重复发送消息、重复部署等）。
- **对比:** `xin/store.rs:200-212` 的 `claim_task()` 正确实现了原子认领。
- **修复:** 仿照 xin 的 `claim_task` 模式，添加 `claim_job()` 函数。

### C-3: PostgreSQL 记忆后端 SQL 注入风险 (降级: 已有防护)
- **文件:** `memory/postgres.rs:339, 352` 等多处
- **来源:** Claude Explore
- **描述:** 动态表/schema 名通过 `format!()` 插值到 SQL。
- **实际风险:** **已降级** — `postgres.rs:30-31` 在构造函数中调用 `validate_identifier()` + `quote_identifier()`，使用严格正则 `^[a-zA-Z_][a-zA-Z0-9_]{0,62}$` 验证后才赋值给 `qualified_table`。SQL 注入在当前代码中无法触发。
- **建议:** 在 `format!()` 处添加注释说明安全性已由构造函数保证。

---

## HIGH — 高优先级

### H-1: Cron DB 缺少 `busy_timeout`
- **文件:** `cron/store.rs:505-513`
- **来源:** Codex
- **描述:** `with_connection()` 打开连接后未设置 `busy_timeout`，并发访问时立即返回 `SQLITE_BUSY` 错误。
- **对比:** xin (`5s`)、memory (`5s`)、webhook (`5s`) 均已设置。
- **修复:** 添加 `conn.busy_timeout(Duration::from_secs(5))?;`

### H-2: Cron 持久化错误被静默丢弃
- **文件:** `cron/scheduler.rs:223, 239, 255`
- **来源:** Codex
- **描述:** `record_run` 和 `record_last_run` 的错误通过 `let _ =` 丢弃，运行历史可能静默丢失。
- **修复:** 改为 `if let Err(e) = ... { tracing::warn!(...) }`

### H-3: SSRF 防护仅检查主机名字符串 (DNS 重绑定绕过)
- **文件:** `tools/http_request.rs:382`, `tools/web_fetch.rs:82`, `tools/browser.rs:427`
- **来源:** Codex
- **描述:** `is_private_or_local_host()` 仅检查主机名字符串，不解析 DNS。攻击者控制的域名解析到内网 IP 可绕过所有私网检查。
- **修复:** 在发起请求前额外解析 DNS 并验证解析后 IP 非私网段。

### H-4: `web_fetch` 未检查安全策略速率限制
- **文件:** `tools/web_fetch.rs:87-96, 131`
- **来源:** Codex
- **描述:** 缺少 `SecurityPolicy::record_action()` 和 `is_rate_limited()` 检查。`allowed_domains` 为空时仅 warn 但仍执行请求。
- **修复:** 添加速率限制检查，空 allowlist 时应 bail 而非 warn。

### H-5: Shell 工具未设置 `kill_on_drop`
- **文件:** `runtime/native.rs:59-61`, `tools/shell.rs:165`
- **来源:** Codex
- **描述:** shell 命令超时后 `tokio::time::timeout` 取消 future 但**不杀进程**。子进程成为孤儿，占用系统资源。
- **对比:** `cron/scheduler.rs:589`、`xin/runner.rs:320`、所有 tunnel 均正确设置。
- **修复:** `native.rs:60` 添加 `process.kill_on_drop(true);`

### H-6: 嵌入缓存 LRU 驱逐 SQL 可能无效
- **文件:** `memory/sqlite.rs:788-795`
- **来源:** Claude Explore
- **描述:** LRU 驱逐使用嵌套子查询 `MAX(0, (SELECT COUNT(*) ...) - ?1)`，SQLite 的 `MAX()` 作为标量函数在此上下文中行为可能异常，导致缓存无限增长。
- **修复:** 在 Rust 侧计算删除数量，分两步执行。

### H-7: 上下文压缩超时后激进裁剪可能丢失关键上下文
- **文件:** `agent/loop_.rs:2358-2376`
- **来源:** Claude Explore
- **描述:** 压缩超时 (300s) 后回退到 `aggressive_trim`，可能丢弃关键决策信息（URL、文件路径等），且无前置 flush。
- **修复:** 激进裁剪前先执行 `pre_compaction_flush` 将关键内容写入记忆。

### H-8: 工具屏障死锁风险
- **文件:** `agent/loop_.rs:1555-1584`
- **来源:** Claude Explore
- **描述:** 工具屏障使用 `tokio::sync::Mutex` 序列化访问。如果工具执行被取消时 guard 未正确释放（panic 场景），后续同组工具将永久阻塞。
- **修复:** 使用 `parking_lot::Mutex`（无 poison），或添加超时 guard。

---

## MEDIUM — 中优先级

### M-1: Read-Modify-Write 更新模式非原子 (Last-Write-Wins)
- **文件:** `cron/store.rs:177-244`, `xin/store.rs:143-193`
- **来源:** Codex
- **描述:** 更新流程: 读取整行 → 内存中应用 patch → 写回全行。并发修改时后写覆盖先写。
- **影响:** 配置更新可能被覆盖。

### M-2: Xin 完成/失败更新未验证影响行数
- **文件:** `xin/store.rs:215-228, 232-244`
- **来源:** Codex
- **描述:** `mark_completed` / `mark_failed` 不检查 UPDATE 影响行数。任务在认领和完成之间被删除时更新静默成功。

### M-3: FTS/向量搜索错误被静默吞没
- **文件:** `memory/sqlite.rs:1006, 1010`
- **来源:** Codex
- **描述:** `.unwrap_or_default()` 静默吞没搜索错误，FTS 索引损坏时返回空结果但无任何日志。
- **修复:** 添加 `tracing::warn!` 日志。

### M-4: 记忆 Schema 迁移竞态条件
- **文件:** `memory/sqlite.rs:440-493`
- **来源:** Codex
- **描述:** `PRAGMA table_info` → `ALTER TABLE ADD COLUMN` 非原子，并发进程可能触发 "duplicate column name" 错误。
- **对比:** `cron/store.rs:488-502` 正确捕获了此错误。

### M-5: MCP 工具调试日志泄露原始参数/结果
- **文件:** `tools/mcp.rs:386-390, 429-430`
- **来源:** Codex
- **描述:** `tracing::debug!` 输出原始 `args` 和 `result`，可能包含 API key、token 等敏感数据。

### M-6: `file_write` 符号链接检查 TOCTOU 漏洞
- **文件:** `tools/file_write.rs:127-149`
- **来源:** Codex
- **描述:** `symlink_metadata` 检查和 `tokio::fs::write` 之间存在竞态窗口，目标可在检查后被替换为符号链接。

### M-7: 工具结果截断可能丢失关键信息
- **文件:** `agent/loop_.rs:289-309`
- **来源:** Claude Explore
- **描述:** 工具结果在 30,000 字符处截断，JSON API 响应可能丢失关键数据。
- **建议:** 对结构化输出做摘要而非截断。

### M-8: 嵌入缓存哈希截断碰撞风险
- **文件:** `memory/sqlite.rs:723-738`
- **来源:** Claude Explore
- **描述:** SHA-256 截断至 64 位作为缓存键，碰撞概率虽低但会导致返回错误的缓存嵌入。
- **建议:** 使用完整 SHA-256 或至少 128 位。

### M-9: `topic_aliases` 缺少 `from_topic_id` 外键
- **文件:** `memory/sqlite.rs:392-399`
- **来源:** Codex
- **描述:** 仅声明了 `to_topic_id` 的 FK，`from_topic_id` 无约束。

### M-10: 对话历史无自动清理策略
- **文件:** `memory/sqlite.rs` (全局设计)
- **来源:** Claude Explore
- **描述:** 有查询限制 (`MAX_CONVERSATION_QUERY_LIMIT = 500`) 但无插入限制，长期运行会无限增长。

### M-11: 工具参数未在执行前校验 Schema
- **文件:** `agent/loop_.rs:1474-1482`
- **来源:** Claude Explore
- **描述:** 解析后的工具参数未与 `parameters_schema()` 校验就直接执行。

---

## LOW — 代码质量

### L-1: HTTP 响应头渲染 Bug (key 当 value 显示)
- **文件:** `tools/http_request.rs:250-255`
- **来源:** Codex
- **描述:** `(k, _)` 解构丢弃了 header value，`format!("{}: {:?}", k, k)` 输出 key 两次。
- **修复:** 改为 `(k, v)` 并使用 `v.to_str()`。

### L-2: Daemon 状态写入错误被抑制
- **文件:** `daemon/mod.rs:233, 247`
- **来源:** Codex
- **描述:** `let _ =` 丢弃文件写入错误，状态文件失败不可见。

### L-3: 魔法数字散布
- **文件:** 多个文件
- **来源:** Claude Explore
- **描述:** `50`, `80`, `30_000`, `300` 等硬编码常量缺少文档说明。

### L-4: 测试覆盖率缺口
- **文件:** `agent/tests.rs` (1383 LOC)
- **来源:** Claude Explore
- **描述:** 缺少压缩失败模式、工具屏障竞争、对话清理、SQL 错误处理的测试。

---

## 已验证安全的领域

| 领域 | 结论 | 来源 |
|------|------|------|
| SQLite 参数化查询 | ✅ 全部使用 `params![]`，无字符串拼接 | Codex |
| PostgreSQL 标识符注入 | ✅ 构造函数中 `validate_identifier()` + `quote_identifier()` | Codex |
| `std::sync::Mutex` | ✅ 全部 9 处在 `#[cfg(test)]` 内 | Codex |
| 生产代码 `.unwrap()` | ✅ 3041 处均在测试模块内 | Codex |
| 进程 `kill_on_drop` | ✅ cron/xin/tunnel 均正确设置 (shell 除外) | Codex |
| `.expect()` 使用 | ✅ 仅用于 `LazyLock`/正则等编译期常量 | Claude Explore |

---

## 修复优先级建议

### 立即修复 (阻塞发布)
1. **C-1** — memory/sqlite.rs 添加 `PRAGMA foreign_keys = ON`
2. **C-2** — cron 添加原子 `claim_job()` (仿照 xin)
3. **H-1** — cron/store.rs 添加 `busy_timeout`
4. **H-5** — native.rs 添加 `kill_on_drop(true)`

### 本周修复
5. **H-2** — cron scheduler 错误日志
6. **H-3** — SSRF DNS 解析验证
7. **H-4** — web_fetch 速率限制
8. **H-6** — 嵌入缓存 LRU 修复
9. **L-1** — HTTP 响应头 bug

### 下一迭代
10. **M-1 ~ M-11** — 并发安全、错误处理、记忆清理

---

## 审计方法论

- **Codex CLI 子进程:** 自动化分析安全、并发、数据库、性能、资源管理，通过 `Grep`/`Read` 验证每个发现
- **Claude Explore 子进程:** 深度代码阅读 10 个子系统 (memory/agent/self_system/providers/channels/tools/cron/xin/daemon/config)
- **交叉验证:** 两份报告中重叠发现已合并去重，C-3 (PostgreSQL SQL 注入) 经 Codex 验证已有防护后降级
- **编译验证:** `cargo check` + `cargo clippy` + `cargo test` 全部通过
