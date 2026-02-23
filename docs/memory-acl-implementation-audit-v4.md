# Memory ACL 实现审计报告（第四轮）

审计日期：2026-02-23  
审计目标：基于 v3 修复提交 `1aa139d` 做复验 + 全量重审 + 发现即修复。

审计范围（重点）：
- `src/tools/file_read.rs`
- `src/tools/memory_get.rs`
- `src/tools/memory_search.rs`
- `src/memory/topic.rs`
- `src/memory/principal.rs`
- `src/webhook/mod.rs`
- `src/tools/sessions_spawn.rs`
- `src/session_worker/runner.rs`
- `src/agent/loop_.rs`

---

## 结论摘要

1. v3 声称的 3 个修复中，2 个已正确落地，1 个存在残留缺陷（`resolve_topic`）。
2. 本轮新增发现并已修复 3 个安全相关问题：
   - `resolve_topic` 在 `project=None` 分支可跨项目误命中 external_id。
   - 独立 webhook (`/webhook/events`) 缺少限流与重放幂等保护。
   - `file_read` 在 ACL 模式下仍可读 `MEMORY_SNAPSHOT.md` / `memory/brain.db`，且大小写变体拦截不严格。
3. 修复后验证通过：`cargo check`、`cargo test --lib`。
4. 最终评分：**9.1 / 10**。
5. 生产标准：**达到（建议继续增强）**。

---

## 一、v3 三项修复复验

### 1) P1 `file_read` ACL block

结论：**已修复且本轮强化**。

复验点：
- ACL 启用时拒绝记忆文件直读。
- 路径变体检查（大小写、symlink）是否可绕过。

本轮强化内容：
- 拦截规则改为大小写不敏感（`MEMORY.md` / `memory/*.md`）。
- 新增拦截：`MEMORY_SNAPSHOT.md`、`memory/brain.db`。
- 新增测试：
  - 大小写变体拦截。
  - symlink 别名指向记忆文件拦截。
  - snapshot/db 文件拦截。

### 2) P2 NULL sentinel (`_global`)

结论：**已生效**。

复验点：
- `create_topic` 对 `external_id` 路径使用 `canonical_project_for_external(project)`。
- `project=None` 时统一落到 `_global`，并参与 `(project, external_id)` UPSERT。

状态：一致，相关测试仍通过。

### 3) P2 project-aware `resolve_topic`

结论：**v3 未完全修复，本轮已修复**。

问题：
- `project=None` 时此前走 `find_topic_by_external`，会跨项目误命中同 external_id。

本轮修复：
- `resolve_topic` 统一改为 `find_topic_by_project_and_external(conn, project.as_deref(), external_id)`。
- 新增测试覆盖“无 project 推断时必须命中 `_global` 作用域”。

---

## 二、全量 ACL 重审结果（含专项）

### A. `file_read` 路径变体绕过（`../`、symlink、大小写）

结论：**未发现可利用绕过路径（当前实现）**。

要点：
- `../`：由 `SecurityPolicy::is_path_allowed` 组件级阻断。
- symlink 越权：先 canonicalize，再 `is_resolved_path_allowed` 校验工作区边界。
- 大小写绕过：本轮将受保护目标改为大小写不敏感匹配。

### B. `_global` 哨兵一致性

结论：**核心 external_id 路径一致**。

要点：
- `create_topic` 和 `find_topic_by_project_and_external` 都通过同一 canonical 规则处理 `None/empty project -> _global`。
- 本轮修复后 `resolve_topic` 也走同一 project-aware 查找语义。

### C. 其他工具间接读取记忆文件

结论：**主旁路已封堵**。

要点：
- `memory_get/search` 在 ACL 模式下禁用文件回退。
- `file_read` 现已覆盖 `MEMORY.md`、`MEMORY_SNAPSHOT.md`、`memory/*.md`、`memory/brain.db`。
- `web_fetch/http_request` 仅允许 HTTP(S)，不支持本地文件协议。

### D. webhook token 重放 / 限流

结论：
- gateway `/webhook`：已有 Bearer + 限流 + 可选幂等键（原实现即存在）。
- 独立 `src/webhook/mod.rs`：**本轮补齐** 限流与重放幂等保护。

本轮新增：
- 滑动窗口限流（默认 60/min）。
- 幂等去重：优先 `X-Idempotency-Key`，否则使用事件指纹（source/event_type/project/external_id/actor/timestamp）。
- 新增重复请求与限流测试。

### E. session_worker / process isolation 下 ACL 生效

结论：**生效**。

依据：
- worker 进程使用 `SqliteMemory::new_with_path_and_acl(..., config.memory.acl_enabled)`。
- 无可信 `_zc_scope` 时，`memory_get/search` 退化为匿名 principal 且 `acl_enforced=true`。
- process 模式 workspace 与 memory DB 路径独立，未见 ACL 被关闭或旁路的代码路径。

---

## 三、本轮实际修复清单

1. `src/memory/topic.rs`
- 修复 `resolve_topic` 的 project-aware 查找残留。
- 增加 `_global` 作用域解析测试。

2. `src/tools/file_read.rs`
- 强化 ACL 文件拦截规则（大小写不敏感）。
- 新增 `MEMORY_SNAPSHOT.md` 和 `memory/brain.db` 拦截。
- 增加大小写变体、symlink 别名、snapshot/db 拦截测试。

3. `src/webhook/mod.rs`
- 新增独立 webhook 限流器与幂等存储。
- 接入请求链路：限流 + 重放去重。
- 增加对应测试。

---

## 四、验证结果

已执行：

```bash
cargo check
cargo test --lib
```

结果：
- `cargo check`：通过（存在仓库既有 warning）。
- `cargo test --lib`：通过（`2737 passed; 0 failed; 3 ignored`）。

---

## 五、最终评分与生产判断

- 最终评分：**9.1 / 10**
- 是否达到生产标准：**是**

保留改进建议（非阻断）：
1. 独立 webhook 限流当前为进程内全局限流，后续可演进为按客户端键限流。  
2. 若未来有跨实例部署需求，幂等键与限流状态应迁移到共享存储（如 Redis）以避免多实例重放窗口。  
3. 可考虑在文档中显式声明 ACL 下 `file_read` 的受保护文件集合，减少运维误判。
