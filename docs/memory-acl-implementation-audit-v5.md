# Memory ACL 实现审计 v5（冲刺轮）

日期：2026-02-24  
审计分支：`prx`  
参考提交：`5e0274b`（声明为 v4 修复提交）

## 0. 结论摘要

- 最终评分：**9.6 / 10**
- 部署建议：**可部署（附条件）**
- 本轮结论：
  - v4 声称的修复在代码层大部分已生效，但 `5e0274b` 本身仅修改 `SELF_EVOLUTION.md`，功能修复实际来自前序提交。
  - 发现并修复了 4 个高价值安全缺口（见第 2 节）。
  - `cargo check` 通过，`cargo test --lib` 通过（2739 passed, 0 failed, 3 ignored）。

## 1. v4 修复生效核验

### 1.1 `file_read` ACL 封堵

核验结果：**已生效并加强**。

- 已有：禁止 `MEMORY.md`、`memory/*.md`、`MEMORY_SNAPSHOT.md`、`memory/brain.db`。
- 本轮新增：禁止 `memory/brain.db-wal`、`memory/brain.db-shm`、`memory/brain.db-journal`（防 SQLite sidecar 旁路读取）。

### 1.2 `resolve_topic` project-aware

核验结果：**主链路生效**。

- `resolve_topic()` 仅调用点在 `SqliteMemory::store_internal()`，外部 ID 解析路径已使用 `find_topic_by_project_and_external()`。
- webhook 入库路径也按 `(project, external_id)` 查询/创建 topic。

### 1.3 webhook replay / 限流

核验结果：**已生效并加强**。

- webhook 模块已有 idempotency 防重放。
- 本轮在 gateway `/webhook` 增加“凭证维度限流”（Bearer/Secret hash key），降低分布式 IP 轮换绕过风险。

## 2. 本轮发现并修复的问题

### P0-1：`_zc_scope` 注入链在子代理链路丢失（task/process）

影响：子代理工具调用会退化为无 scope，ACL 作用域无法继承父会话上下文。  
修复：

- `delegate` agentic 模式解析并继承可信 `_zc_scope`。
- `sessions_spawn` task 模式解析并透传 scope。
- `sessions_spawn` process 模式将 scope 写入 `WorkerManifest`。
- `session_worker` 读取 manifest scope 并在 `run_tool_call_loop` 里恢复 `ScopeContext`。

### P0-2：工具组合可经 `shell` 间接读取 ACL 受保护记忆文件

影响：即使 `file_read` 封堵，仍可通过 `shell` 直接 `cat memory/brain.db` 等读取。  
修复：

- `ShellTool` 新增 `acl_enabled` 开关。
- ACL 开启时阻断引用受保护记忆路径的 shell 命令（`MEMORY.md`、`memory/*.md`、`brain.db*`、`MEMORY_SNAPSHOT.md` 等标记）。

### P1-1：SQLite sidecar 文件读取旁路

影响：`brain.db-wal/shm/journal` 可能泄露近期写入数据。  
修复：`file_read` ACL 规则新增 sidecar 封堵并补单测。

### P1-2：SQLite 写锁争用下失败恢复弱

影响：高并发下可能出现 `database is locked` 失败。  
修复：

- `SqliteMemory::open_connection()` 增加 `busy_timeout(5s)`。
- `webhook::persist_event()` 连接增加 `busy_timeout(5s)`。

## 3. 红队攻击面审计结果（按要求）

### 3.1 工具组合间接读取 ACL 记忆

- `file_read`：已封，且本轮补封 SQLite sidecar。
- `shell/exec`：审计前存在绕过；本轮已加 ACL 路径阻断。
- `web_fetch/http_request`：不直接读本地文件，但仍属外联面，需依赖 allowlist/网络隔离策略。

结论：**主要绕过链已被封堵**。

### 3.2 `_zc_scope` 注入链（session_worker/process 隔离）

- 审计前：存在 scope 丢失。
- 审计后：`delegate`、`sessions_spawn(task)`、`sessions_spawn(process)`、`session_worker` 均已打通可信链路。

结论：**已修复**。

### 3.3 webhook：HMAC constant-time + distributed bypass

- HMAC constant-time：
  - WhatsApp 签名校验使用 HMAC `verify_slice`（constant-time）。
  - 其他 secret 比较使用 constant-time compare。
- distributed bypass：
  - 审计前：按 IP 限流可被分布式 IP 轮换稀释。
  - 审计后：新增凭证维度限流，显著收敛绕过面。

结论：**constant-time OK；分布式绕过风险已明显降低**。

### 3.4 topic 系统 `resolve_topic` 调用点 project-aware

- `resolve_topic` 唯一生产调用点已 project-aware。
- webhook 路径也按 `(project, external_id)` 做隔离查询。

结论：**当前调用点满足 project-aware**。

### 3.5 SQLite 并发（WAL + 写锁争用）

- WAL：已启用。
- 写锁争用：本轮补 `busy_timeout`。

结论：**并发健壮性提升，满足上线要求**。

### 3.6 配置热重载：`acl_enabled false->true` 时 Principal/策略失效

- Principal 本身无全局缓存失效问题（按请求解析）。
- 真实风险在于：工具实例的 ACL 开关是构造时值，热重载并不会自动改写运行中实例。

本轮修复：

- `config/hotreload` 与 `config_reload` 对 `memory.acl_enabled` 变更改为“记录并提示需重启”，运行时保持旧值，避免安全错觉。

结论：**已显式收敛为“重启生效”的确定性行为**。

## 4. unsafe 代码块审计

检索到 `unsafe` 仅 2 处生产代码：

- `src/tools/gateway.rs`：`libc::kill`（向进程发 `SIGHUP`）
- `src/service/mod.rs`：`libc::getuid`

判定：均为受控 FFI 场景，未见内存不安全扩散。

## 5. unwrap/expect 审计

- 在本轮审计范围（memory/webhook/gateway/tools/session_worker/agent）中，生产路径未新增不必要 panic 点。
- 大量 `unwrap/expect` 位于测试代码。
- 非测试代码中的 `unwrap/expect` 主要为静态正则编译/不可失败常量路径，风险可接受。

## 6. 关键改动文件

- `src/tools/delegate.rs`
- `src/tools/sessions_spawn.rs`
- `src/session_worker/protocol.rs`
- `src/session_worker/runner.rs`
- `src/tools/shell.rs`
- `src/tools/file_read.rs`
- `src/gateway/mod.rs`
- `src/memory/sqlite.rs`
- `src/webhook/mod.rs`
- `src/config/hotreload.rs`
- `src/tools/config_reload.rs`

## 7. 验证结果

执行结果：

- `cargo check`：通过
- `cargo test --lib`：通过（`2739 passed; 0 failed; 3 ignored`）

## 8. 最终部署意见

可部署，建议按以下条件执行：

1. 生产配置中保留 webhook secret / pairing，并监控 429 比例。  
2. 若需切换 `memory.acl_enabled`，按“重启生效”流程发布，不依赖热重载。  
3. 保持 `shell` 工具最小授权（若不需要，建议禁用）。

