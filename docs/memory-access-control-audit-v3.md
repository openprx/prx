# Memory Access Control v2 三轮审计报告（最终轮）

- 被审计文档: `docs/memory-access-control-design-v2.md`（v2.1）
- 对照基线: `docs/memory-access-control-audit-v2.md`
- 审计日期: 2026-02-23
- 结论: 二轮遗留的 2 个 P0 + 2 个 P1 已修复；发现 1 个新增 P1，修复后可实施。

## 1) 二轮问题修复核对（2xP0 + 2xP1）

1. P0: `project` 条件 `OR` 放宽（应为 `AND`）
- 结果: **已修复**
- 依据: v2.1 的 `build_sql_scope` 中 `project` 分支改为 `t.project IN (...) AND tp.user_id = ?`。

2. P0: `private/group` 缺 `channel` 约束
- 结果: **已修复**
- 依据: `private` 与 `group` 条件均包含 `channel = ? AND chat_id = ?`，并区分 `chat_type`。

3. P1: `memory_get` 与 `memory_search` 双轨判定不一致
- 结果: **已修复**
- 依据: `memory_get` 明确改为与 `memory_search` 共用 `build_sql_scope`，单次 SQL 过滤。

4. P1: 无 `external_id` 时 topic 幂等失效
- 结果: **已修复（设计层）**
- 依据: `topics` 增加 `fingerprint` 且 `UNIQUE(fingerprint)`，创建逻辑使用 `ON CONFLICT(fingerprint)`。

## 2) 新问题

### P1-NEW: topic upsert 返回值未用于参与者绑定
- 位置: `resolve_topic` 的 Step 5 示例代码
- 问题: SQL 写了 `RETURNING id`，但后续仍用新生成的 `topic_id` 执行 `add_participant(&topic_id, ...)`。
- 风险: 若命中 `fingerprint` 冲突，真实 topic 是已有 id；参与者可能写到错误 id（或触发外键失败），导致“幂等创建 + 参与者授权”链路断裂。
- 建议: 接收 `RETURNING id` 的实际值并用于后续 `add_participant/touch_topic`。

## 3) 最终评分（1-10）

**8.5/10**

- 加分: 二轮 4 个关键问题均已闭环，策略一致性明显提升。
- 扣分: 仍有 1 个实现级 P1 缺口，虽可快速修复，但影响 topic 参与者正确性。

## 4) 是否达到可实施标准

**有条件达到。**

- 若先修复上述新增 P1（预计小改动），可进入实施与灰度。
- 未修复前，不建议直接进入全面 deny 阶段。
