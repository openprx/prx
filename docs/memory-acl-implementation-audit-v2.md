# Memory ACL 实现审计报告（第二轮）

审计日期：2026-02-23  
审计范围：
- `src/memory/principal.rs`
- `src/memory/topic.rs`
- `src/memory/sqlite.rs`
- `src/webhook/mod.rs`
- `src/tools/memory_search.rs`
- `src/tools/memory_get.rs`

基线输入：
- 上轮报告：`docs/memory-acl-implementation-audit.md`
- 上轮修复提交：`008579f`

## 结论
- 上轮 2xP0 + 3xP1 + 3xP2：**5 项已修复，1 项部分修复，1 项未完全修复**。
- 本轮发现 1 个新的 P0 级问题（可绕过 ACL）。
- 最终评分：**6.9 / 10**。
- 是否达到生产部署标准：**否**。

## 一、上轮问题复核（逐项）

1. P0-1 `memory_get` ACL deny 后回退文件导致旁路  
状态：**已修复**  
证据：`src/tools/memory_get.rs:313-350,389-397` 在 `acl_enabled=true` 时拒绝后直接返回，不再走文件回退。

2. P0-2 `_zc_scope` 可伪造  
状态：**已修复（在当前主调用链）**  
证据：
- `src/tools/memory_get.rs:110-117`
- `src/tools/memory_search.rs:149-156`  
要求 `_zc_scope_trusted=true` 才解析。
- `src/agent/loop_.rs:1005-1025` 统一注入可信 scope，且在无可信上下文时移除用户 `_zc_scope` 并强制 `trusted=false`。

3. P1-1 `create_topic` 并发幂等不完整  
状态：**未完全修复**  
证据：`src/memory/topic.rs:39-77` 先查 `(project, external_id)` 再插入，但插入仍仅 `ON CONFLICT(fingerprint)`。并发窗口仍可能触发 `UNIQUE(project, external_id)` 失败。

4. P1-2 webhook 写入绕过统一分类策略  
状态：**已修复**  
证据：`src/webhook/mod.rs:383-391` 改为 `store_with_context`；分类逻辑在 `src/memory/principal.rs:377-383` 对 `Webhook` 基线为 `Owner`。

5. P1-3 敏感匹配缺少 NFKC 规范化  
状态：**已修复**  
证据：`src/memory/principal.rs:405-412` 使用 `nfkc().to_lowercase()`；并新增混淆字符测试 `:652-654`。

6. P2-1 webhook token 非常量时间比较  
状态：**已修复**  
证据：`src/webhook/mod.rs:151-154` 使用 `subtle::ConstantTimeEq`。

7. P2-2 `memory_search` 文件回退与单一数据源偏差  
状态：**已修复（ACL 启用场景）**  
证据：`src/tools/memory_search.rs:490-517` ACL 启用时 DB 不可用直接返回空，不走文件回退。

8. P2-3 topic context N+1  
状态：**部分修复**  
证据：`src/tools/memory_search.rs:401-403` 将 topic 命中数限制为 3，降低风险；但 `:410-412` 仍是逐 topic 查询，N+1 仍在。

## 二、本轮新增/遗留问题

### P0-NEW：SQLite 备份文件可被其他工具读取，形成 ACL 旁路
- 位置：
  - `src/memory/sqlite.rs:423-459,582`（每次写入都会追加到 `MEMORY.md` 或 `memory/*.md`）
  - `src/tools/file_read.rs:23-27,43-137`（允许读取工作区文件）
- 问题：即使 `memory_get/memory_search` 已禁止 ACL 模式文件回退，敏感内存内容仍会落盘到 markdown 备份；`file_read` 可直接读取这些文件。
- 影响：绕过 ACL 读取受限记忆（尤其 webhook/敏感文本），破坏“策略单一执行面”。
- 建议：默认关闭 `append_backup_entry`（至少在 ACL 启用时关闭），或将备份迁移到受 ACL 控制存储，并禁止工具直接读取。

### P1-NEW：webhook 按 external_id 全局查 topic，存在跨项目串话风险
- 位置：
  - `src/webhook/mod.rs:337-353`
  - `src/memory/topic.rs:93-105`
- 问题：`persist_event` 先 `find_topic_by_external(external_id)`，忽略 `project/source`。当不同项目存在相同 `issue#42` 时，可能复用错误 topic。
- 影响：topic 归属错误、参与者/可见域污染，进而带来 ACL 范围错配。
- 建议：按 `(project, external_id)` 精确查询；若 `project` 为空，也应引入 `source` 维度避免碰撞。

### P1-遗留：`create_topic` 并发冲突窗口仍在
- 位置：`src/memory/topic.rs:39-77`
- 问题：预查询不是原子保障；并发请求仍可能在插入处因 `(project, external_id)` 唯一约束报错。
- 建议：改为单语句 UPSERT（以 `(project, external_id)` 为主冲突目标）或事务内重试策略。

## 三、综合评估
- 安全性：修复了上轮主要工具层旁路，但出现新的存储层旁路（P0）。
- 一致性：webhook 分类路径已统一；topic 识别键仍存在边界不一致。
- 稳定性：并发幂等问题仍未完全闭环。

## 四、最终评分与上线结论
- 最终评分：**6.9 / 10**
- 生产部署标准：**未达到**

必须先完成：
1. 关闭/重构 markdown 备份旁路（P0-NEW）。
2. 修复 webhook topic 键冲突策略（P1-NEW）。
3. 完成 `create_topic` 并发原子幂等（P1-遗留）。
