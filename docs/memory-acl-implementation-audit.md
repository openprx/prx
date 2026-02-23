# Memory ACL 实现最终审计报告（Phase 0-5）

审计日期：2026-02-23  
审计范围：`src/memory/principal.rs`、`src/memory/topic.rs`、`src/memory/sqlite.rs`、`src/memory/snapshot.rs`、`src/memory/mod.rs`、`src/webhook/mod.rs`、`src/tools/memory_search.rs`、`src/tools/memory_get.rs`、`src/config/schema.rs`  
设计对照：`docs/memory-access-control-design-v2.md`（v2.1）

## 结论摘要

- 总体实现方向与 v2.1 一致（Principal/SQL scope/topic/schema/observe mode 基本落地）。
- 存在 2 个高风险安全问题（P0），会导致 ACL 被旁路或身份可伪造。
- 存在 3 个中风险问题（P1），覆盖 webhook 幂等并发、策略一致性与敏感检测稳健性。
- SQL 注入面整体可控，主要查询均参数化。

最终评分：**6.8 / 10**

---

## 发现清单（按严重度）

### P0-1：`memory_get` 在 ACL 拒绝后仍可回退文件读取，形成权限旁路

- 位置：`src/tools/memory_get.rs:289`、`src/tools/memory_get.rs:322`、`src/tools/memory_get.rs:369`
- 问题：当 ACL 已启用且命中拒绝时，仅对“非文件路径”直接返回；若请求 `MEMORY.md`/`memory/*.md` 会继续走文件回退并返回内容，绕过 DB 层 ACL。
- 影响：可直接读取历史文件记忆，破坏“SQLite 单一策略源”与“静默拒绝”。
- 修复建议：
1. 在 `self.acl_enabled && principal.acl_enforced` 分支下，拒绝后**直接返回**，禁止任何文件回退。
2. 文件回退仅允许在显式“兼容模式”开关下启用，且默认关闭。
3. 补充测试：ACL deny + `MEMORY.md` 路径必须返回 denied/empty。

### P0-2：`_zc_scope` 直接来自工具入参，缺少可信来源约束，可伪造 Principal

- 位置：`src/tools/memory_search.rs:149`、`src/tools/memory_search.rs:479`、`src/tools/memory_get.rs:110`、`src/tools/memory_get.rs:281`
- 问题：Principal 解析依赖 `_zc_scope.channel/chat_id/sender`，但字段由工具调用参数直接提供，未校验“是否由 runtime 注入且不可伪造”。
- 影响：攻击者若可控制 tool args，可伪造已绑定 sender，提升可见范围，属于认证边界缺失。
- 修复建议：
1. 禁止从用户可控 JSON 读取 `_zc_scope`；改由 `ToolContext`/runtime 注入只读调用上下文。
2. 给工具层加签名或内部字段通道（非模型可写字段）。
3. 若缺少可信 scope，默认 `Anonymous + deny`，不要默认 `owner_principal()`。

---

### P1-1：`create_topic` 并发幂等不完整，对 `(project, external_id)` 冲突无 UPSERT 处理

- 位置：`src/memory/topic.rs:44`-`src/memory/topic.rs:47`；相关唯一约束 `src/memory/sqlite.rs:258`
- 问题：`create_topic` 仅 `ON CONFLICT(fingerprint)`；并发下若同 `project+external_id` 但 fingerprint 不同，会触发唯一约束错误并导致 webhook/topic 创建失败。
- 影响：高并发 webhook 场景出现 500、重复重试、topic 不一致。
- 修复建议：
1. 先按 `(project, external_id)` 查并复用（事务内）。
2. 或改为双路径 UPSERT：优先 external_id 唯一键，再回退 fingerprint。
3. 增加并发测试（两事务同时写同 external_id，不应报错，最终单 topic）。

### P1-2：Webhook 写入路径绕过统一分类策略，`visibility='project'` 与 v2.1 基线不一致

- 位置：`src/webhook/mod.rs:356`-`src/webhook/mod.rs:373`；对照设计 `docs/memory-access-control-design-v2.md:284`-`docs/memory-access-control-design-v2.md:289`
- 问题：设计中 `ChatType::Webhook` 基线应为 `Owner`，但 webhook 直接写 `project`，未复用 `classify_memory`。
- 影响：外部事件内容可被 project 成员读取，策略一致性被打破；若 webhook 内容含敏感字段，泄漏风险上升。
- 修复建议：
1. 统一走 `store_with_context + classify_memory`，避免旁路插入。
2. 若业务确需 `project` 可见，增加显式配置开关并默认关闭，文档同步标注偏差。
3. 为 webhook 内容执行风险信号检测并写入 `risk_signals/sensitivity`。

### P1-3：敏感模式匹配未做 NFKC 规范化，易被 Unicode 混淆绕过

- 位置：`src/memory/principal.rs:405`-`src/memory/principal.rs:412`；对照设计 `docs/memory-access-control-design-v2.md:335`-`docs/memory-access-control-design-v2.md:339`
- 问题：实现使用 `to_ascii_lowercase`，未做 NFKC；全角/兼容字符可绕过关键词检测。
- 影响：`classify_memory` 可能漏判敏感内容，导致 visibility/sensitivity 低估。
- 修复建议：
1. 改为 `content.nfkc().collect::<String>().to_lowercase()`。
2. 保留去空格匹配逻辑。
3. 增加 Unicode 混淆样例测试（全角 `ａｐｉ＿ｋｅｙ` 等）。

---

### P2-1：Webhook token 比较为普通字符串比较，缺少常量时序比较

- 位置：`src/webhook/mod.rs:132`-`src/webhook/mod.rs:146`
- 问题：使用 `token.trim() == expected_token`，理论上存在时序侧信道。
- 影响：通常为低风险（受网络噪声影响大），但在同网段高频探测下不理想。
- 修复建议：
1. 使用常量时间比较（如 `subtle`/`ring` 的 constant-time eq）。
2. 保留统一长度/格式预处理，避免提前返回差异过多。

### P2-2：`memory_search` 的文件回退与“单一数据源”目标存在偏差

- 位置：`src/tools/memory_search.rs:467`-`src/tools/memory_search.rs:476`、`src/tools/memory_search.rs:568`-`src/tools/memory_search.rs:640`
- 问题：DB 不可用时会回退扫描 markdown 文件，且不受 ACL SQL scope 约束。
- 影响：策略漂移与潜在信息暴露；与 v2.1 “SQLite 唯一查询源”目标不一致。
- 修复建议：
1. 生产默认关闭文件回退，仅在显式兼容模式开启。
2. 回退模式下也应最少执行 principal 级 deny 保护并记录高优先级告警。

### P2-3：topic 上下文搜索存在小规模 N+1 查询

- 位置：`src/tools/memory_search.rs:386`-`src/tools/memory_search.rs:394`
- 问题：每个 topic 命中都调用一次 `query_topic_context`，当前上限 3 还可接受，但在未来放宽上限时会放大。
- 影响：性能退化风险（中长期）。
- 修复建议：
1. 合并为单 SQL：`topic_id IN (...)` + ACL scope 一次查询。
2. 保持 topic 命中数上限并加性能回归测试。

---

## 分维度审计结论

1. 安全性  
- 发现 P0 级旁路（文件回退）和身份伪造面（`_zc_scope` 入参可信边界缺失）。  
- token 认证具备基本门槛，但仍可强化（常量时序、限流/重放保护）。

2. 设计一致性（对照 v2.1）  
- Principal / SQL scope / topic 基本一致。  
- 偏差点：文件回退仍在主路径；webhook 写入未复用分类策略。

3. 代码质量  
- 整体错误处理较稳健，未见明显 panic 热路径。  
- `log_access` 失败吞掉是合理权衡，但建议增加失败计数指标。

4. SQL 注入  
- 本次范围内未发现直接 SQL 注入；动态 SQL 均使用参数绑定值。  
- `scope_sql` 来源于内部枚举逻辑，可控。

5. 并发安全  
- topic 创建在 fingerprint 冲突上具备一定幂等，但 external_id 唯一键冲突路径不完整（P1）。

6. 测试覆盖  
- 已有 ACL/observe/webhook/topic 基础测试。  
- 缺口：P0 路径（文件旁路、scope 伪造）与并发幂等冲突测试。

7. 性能  
- 关键索引已建立（`idx_mem_vis_chan_type_chat`、`idx_mem_topic_time` 等）。  
- 存在受限 N+1（topic context），当前规模可接受但应预防扩张。

---

## 建议优先级（修复顺序）

1. 立即修复 P0-1、P0-2（阻断 ACL 绕过与身份伪造）。  
2. 修复 P1-1（并发幂等），避免 webhook 生产故障。  
3. 修复 P1-2、P1-3（策略统一与敏感检测稳健性）。  
4. 完成 P2 硬化（常量时序、回退策略、性能优化）。
