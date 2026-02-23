# Memory ACL 实现审计报告（第三轮 / 最终轮）

审计日期：2026-02-23  
审计范围：
- `src/memory/principal.rs`
- `src/memory/topic.rs`
- `src/memory/sqlite.rs`
- `src/webhook/mod.rs`
- `src/tools/memory_search.rs`
- `src/tools/memory_get.rs`
- `src/tools/file_read.rs`
- `src/agent/loop_.rs`
- `src/config/schema.rs`

基线输入：
- 第一轮：`docs/memory-acl-implementation-audit.md`
- 第二轮：`docs/memory-acl-implementation-audit-v2.md`
- v2 修复提交：`6e37238`

---

## 结论摘要

1. v2 指定的 3 个问题（`P0-NEW + P1-NEW + P1-遗留`）在当前代码中已落地修复。  
2. 修复本身未引入新的高危回归。  
3. 但仍存在 ACL 绕过风险路径：当工作区中存在历史/外部导入的记忆 markdown 文件时，`file_read` 可直接读取，绕过 ACL 查询面。  
4. 最终评分：**7.8 / 10**。  
5. 生产部署标准：**未达到**（需先封堵残留绕过路径并补齐边界治理）。

---

## 一、v2 三个问题复核

### 1) P0-NEW：SQLite 备份文件旁路 ACL

**结论：已修复（主路径）**

证据：
- `src/memory/sqlite.rs:623-625`：仅在 `!acl_enabled` 时才执行 `append_backup_entry`。
- `src/memory/sqlite.rs:1217-1228`：新增测试 `sqlite_acl_enabled_skips_markdown_backup`，验证 ACL 启用后不落盘 `MEMORY.md`。

审计判断：
- “ACL 开启时继续写 markdown 备份”这一原始 P0 已被修复。
- 但不代表“历史已存在 markdown 文件”自动失效（见后文残留绕过）。

### 2) P1-NEW：webhook topic 仅按 external_id 查找导致跨项目串话

**结论：已修复**

证据：
- `src/webhook/mod.rs:341-345`：改为 `find_topic_by_project_and_external(...)`。
- `src/memory/topic.rs:95-112`：存在按 `(project, external_id)` 的查询实现。
- `src/webhook/mod.rs:592-630`：新增测试 `same_external_id_in_different_projects_keeps_separate_topics`，验证同 `external_id` 不同 `project` 产生 2 个 topic。

### 3) P1-遗留：create_topic 并发窗口（external_id 幂等）

**结论：已修复（针对 project 非空主场景）**

证据：
- `src/memory/topic.rs:42-59`：当 `external_id` 存在时，使用
  `ON CONFLICT(project, external_id) DO UPDATE ... RETURNING id`，实现原子 UPSERT。
- `src/memory/topic.rs:507-537`：测试 `create_topic_reuses_existing_by_project_and_external_id`。

审计判断：
- 相比 v2 的“先查再插”并发窗口，现为单语句原子路径，修复成立。

---

## 二、修复引入新问题检查

**结论：未发现由本轮修复直接引入的新 P0/P1 回归。**

说明：
- webhook 现已统一走 `store_with_context`（`src/webhook/mod.rs:391-399`），与 ACL 分类逻辑一致。
- `_zc_scope` 可信注入链保持完整：
  - `src/agent/loop_.rs:1005-1018` 有 trusted scope 时强制写入。
  - `src/agent/loop_.rs:1020-1025` 无 trusted scope 时移除 `_zc_scope` 并强制 `_zc_scope_trusted=false`。
  - `src/tools/memory_get.rs:110-117`、`src/tools/memory_search.rs:149-156` 仅接受 trusted scope。

---

## 三、全量 ACL 复审（本轮发现）

### P1：仍存在 ACL 绕过条件路径（file_read 读取历史/外部记忆 markdown）

位置：
- `src/tools/file_read.rs:23-27,43-137`
- `src/tools/mod.rs:229-233`（默认工具集中始终注册 `file_read`）
- `src/tools/memory_get.rs:389-397` 与 `src/tools/memory_search.rs:492-517`（已禁止 ACL 模式回退，但仅限 memory 工具）

问题描述：
- 当前修复只阻断了 **memory_get/memory_search** 的文件回退，以及 ACL 模式下新写备份。
- 但 `file_read` 仍可直接读取工作区任意文本文件。
- 若工作区中已存在：
  - 旧版本遗留 `MEMORY.md` / `memory/*.md`
  - 外部导入的记忆 markdown
  则可绕过 memory ACL SQL scope。

影响：
- 这是“跨工具面”的 ACL 旁路，不依赖 SQL 查询路径。
- 在升级场景（已有历史文件）风险现实存在。

建议：
1. ACL 启用时在 `file_read` 对 `MEMORY.md`、`memory/*.md`（可扩展到 snapshot）默认拒绝。  
2. 启动 ACL 时增加一次性迁移/清理策略（归档并加密、迁移入 DB、或强制删除旧备份）。  
3. 增加集成测试：ACL=on + 预置 `MEMORY.md` + `file_read` 必须拒绝。

### P2：`project=NULL` 的 external topic 幂等仍不强（SQLite NULL 唯一约束语义）

位置：
- `src/memory/topic.rs:42-59`
- `src/memory/sqlite.rs:298`（`UNIQUE(project, external_id)`）

问题描述：
- SQLite 中 `UNIQUE(project, external_id)` 对 `project=NULL` 不能形成冲突约束（NULL 互不冲突）。
- 当 `project` 为空、`external_id` 相同且并发写入时，仍可能产生重复 topic。

影响：
- 主要是 topic 一致性/幂等问题，不直接构成 ACL 读取越权。

建议：
1. 若业务允许 `project` 为空，增加稳定命名域（例如 `source` 维度）进入唯一键。  
2. 或在写入前将空 project 规约为固定哨兵值（如 `"_global"`）。

### P2：`resolve_topic` 仍按全局 external_id 复用 topic（无 project 维度）

位置：
- `src/memory/topic.rs:264-270`（调用 `find_topic_by_external`）
- `src/memory/topic.rs:81-93`

问题描述：
- 自动主题解析路径遇到 external ref 时，仍是全局 external_id 查询。
- 在多项目同号场景下可能错误复用 topic。

影响：
- topic 归属/参与者一致性风险，间接影响 project 可见域语义。
- 当前未见直接 ACL 越权读取证据，但属于策略一致性缺口。

建议：
1. 将 `resolve_topic` 切换为 project-aware 查找（至少优先 `(project, external_id)`）。

---

## 四、ACL 绕过路径专项结论

**是否还有 ACL 绕过路径：有。**

当前确认的可行路径：
1. `file_read` 读取历史/外部导入的记忆 markdown（条件成立时可稳定绕过）。

当前未发现的路径：
1. `memory_get` / `memory_search` 在 ACL enforced 下的文件回退旁路（已封堵）。  
2. 通过伪造 `_zc_scope` 直接提升 principal（主调用链已封堵）。

---

## 五、生产就绪评估

最终评分：**7.8 / 10**  
生产部署标准：**未达到**

阻断项（上线前必须处理）：
1. 封堵 `file_read` 对记忆 markdown 的跨工具 ACL 旁路。  
2. 增加 ACL 启用时的历史记忆文件治理（迁移/清理/拒读策略）。

建议项（可并行）：
1. 修复 `project=NULL` 下 external topic 幂等问题。  
2. 将 `resolve_topic` external ref 复用改为 project-aware。

---

## 六、回归验证建议（最小集）

1. `ACL=on`，预置 `MEMORY.md`，调用 `file_read` 读取应失败。  
2. `ACL=on`，同一 `external_id` + 不同 `project` webhook 并发写入，不串话。  
3. `ACL=on`，`_zc_scope` 由工具参数伪造但 runtime 无 trusted scope 时，应强制匿名并拒绝私有数据。  
4. `project=NULL` + 相同 `external_id` 并发压测，观察 topic 去重一致性。
