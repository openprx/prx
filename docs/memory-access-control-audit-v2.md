# Memory Access Control v2 二轮审计报告

- 被审计文档: `docs/memory-access-control-design-v2.md`
- 对照基线: `docs/memory-access-control-audit.md`
- 审计日期: 2026-02-23
- 结论: 相比上一轮明显改进，但仍存在 2 个高风险逻辑缺口，暂不建议直接按当前设计进入 deny 默认启用阶段。

## 1) 上轮问题修复核对

### P0（4项）

1. `private/group` 同构导致语义混淆
- 结论: **部分修复**
- 现状: 已区分 `chat_type=dm/group`，但查询条件未纳入 `channel`，仍未达到“(channel, chat_type, chat_id) 三元组”完整约束。

2. owner 不可伪造假设过强（缺身份绑定）
- 结论: **部分修复**
- 现状: 新增 `identity_bindings` 与未绑定降级 Anonymous；但缺少绑定变更审计链和高风险变更确认机制。

3. 关键词直接决定授权级别
- 结论: **已修复**
- 现状: 改为“风险信号仅可提权到更严格级别”，不再直接放宽可见性。

4. 设计与实现路径断层（文件/SQLite 双轨）
- 结论: **部分修复（设计层）**
- 现状: 已给出单一数据源与 phase 迁移路线；但仍是方案级，不是已落地状态。

### P1（4项）

5. `topic_participants` 已建未用于授权
- 结论: **部分修复且有逻辑偏差**
- 现状: 已接入查询，但 SQL 用的是 `project 匹配 OR 参与者命中`，未实现“项目匹配 AND 参与者命中”的最小权限目标。

6. `visibility_ceiling` 配置未执行
- 结论: **已修复**
- 现状: 已在 SQL scope 构建阶段裁剪权限。

7. 后置过滤可被大小写/变体绕过
- 结论: **已修复**
- 现状: 引入标准化和 regex 匹配，优于字符串 contains。

8. `topics_fts` 缺同步触发器
- 结论: **已修复**
- 现状: 已补齐 insert/update/delete trigger。

### P2（3项）

9. 复合索引不足
- 结论: **已修复**
- 现状: 增加了面向主查询路径的复合索引。

10. `query_topic_context` 先全拉再过滤
- 结论: **已修复（设计层）**
- 现状: 改为 SQL 下推并加 limit。

11. topic 并发创建缺幂等
- 结论: **部分修复**
- 现状: 有 `UNIQUE(project, external_id)` + UPSERT；但当 `external_id` 为空时，SQLite 唯一约束无法防重复，仍可能并发裂变。

## 2) v2 新引入/新暴露问题

### P0

1. `project` 授权条件与声明不一致（实际放宽）
- 问题: 设计文字宣称“project + participant 双条件”，SQL 实际为 `OR`。
- 风险: 项目成员可见全部项目 topic 历史，最小权限失效。
- 建议: 改为严格 `AND`，如需旁路权限，单独引入显式 `observer` 角色，不要混在同一条件。

2. `private/group` 仍缺 `channel` 约束
- 问题: 查询仅按 `chat_type + chat_id`。
- 风险: 跨渠道 `chat_id` 冲突时可能串读。
- 建议: 所有会话级可见性条件统一加入 `channel = current_channel`。

### P1

3. `memory_get` 走 `can_see`，与 SQL scope 策略链不统一
- 问题: `search` 用 SQL scope，`get` 用 `can_see`，存在策略漂移点。
- 风险: 单条读取与检索结果不一致，可能形成绕过窗口。
- 建议: `memory_get` 也改为“按 key + scope SQL”一次查询命中，不走第二套判定。

4. topic 幂等在无 `external_id` 场景仍不成立
- 问题: `ON CONFLICT(project, external_id)` 对 `NULL external_id` 无效。
- 风险: 高频并发写入时重复 topic 持续出现。
- 建议: 增加可稳定冲突键（如 `topic_fingerprint`）并建立唯一约束。

## 3) 整体评分（1-10）

**7/10**

- 加分: 上轮大部分结构性问题已被正面吸收，尤其是 identity、ceiling、FTS、索引、SQL 下推。
- 扣分: 仍有会导致越权/策略漂移的关键逻辑问题（project 条件 OR、缺 channel 维度、get/search 双轨判定）。

## 4) 最终建议

1. 先修复本报告的 2 个 P0，再进入 ACL deny 灰度。
2. 将 `memory_get` 收敛到与 `memory_search` 同一 SQL scope 策略链，避免双实现。
3. 为 topic 创建补齐“无 external_id 幂等键”，否则 observe 期日志会持续噪声并污染关联质量。
4. 在 Phase 2 observe mode 增加两类强制指标：
- 越权回放用例 0 通过（含跨渠道 chat_id 冲突场景）
- `search` 与 `get` 一致性差异率 = 0
