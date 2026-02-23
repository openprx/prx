# Memory Access Control 设计审计报告

- 被审计文档: `docs/memory-access-control-design.md`
- 审计日期: 2026-02-23
- 审计结论: 设计方向正确，但当前版本存在多处高风险安全与实现断层问题，直接落地会出现越权、不可验证的分类错误和迁移行为不一致。

## 一、关键问题（按严重级别）

### P0（必须先修，不修不要上线）

1. `private` 与 `group` 规则完全同构，导致 DM/群语义混乱，可能越权。
- 证据: `private` 与 `group` 都是 `chat_id == current_chat_id`（`docs/memory-access-control-design.md:118`, `docs/memory-access-control-design.md:120`, `docs/memory-access-control-design.md:141`, `docs/memory-access-control-design.md:143`）。
- 风险: 任何 chat_id 复用/映射错误都会把“仅会话可见”和“群内共享可见”混为一谈。
- 建议:
1. 明确改为三元组判定: `(channel, chat_type, chat_id)`。
2. `private` 强制 `chat_type=dm`，`group` 强制 `chat_type=group`。
3. SQL 增加 `AND chat_type = 'dm'/'group'`，并建联合索引 `(chat_type, chat_id, visibility)`。

2. “owner 不可伪造”假设过强，缺失跨渠道身份绑定机制。
- 证据: 仅声称 `sender_id` 来自协议层不可篡改（`docs/memory-access-control-design.md:484`）。
- 风险: 协议层 ID 可信不等于“同一自然人身份可信”；跨 Signal/WA/Telegram 的 owner 映射没有设计，容易被伪关联或错关联。
- 建议:
1. 引入 `identity_bindings`（渠道账号 -> 内部 user_id）并要求 owner 通过绑定表判定。
2. 增加绑定变更审计日志与双人确认（至少 owner+operator）。
3. 未绑定身份一律降级为 `Unknown`，即使 sender_id 存在。

3. 过度依赖关键词判敏感，误判/漏判会直接改变访问级别。
- 证据: `contains_sensitive_keywords` 决定 `Visibility::Owner` 与 `Sensitivity::Secret`（`docs/memory-access-control-design.md:177`, `docs/memory-access-control-design.md:201`, `docs/memory-access-control-design.md:215`）。
- 风险: 漏掉同义词、缩写、编码文本时会把机密落到 `private/group/project`，造成真实泄漏。
- 建议:
1. 把“关键词命中”从授权决策降级为“风险信号”。
2. 默认策略改为: 高风险来源（webhook/system/tool输出）先 `owner`，再通过白名单规则降权。
3. 引入可解释分类记录（规则命中项）与人工纠错入口。

4. 设计与现状实现断层太大，“向后兼容”声明不成立。
- 证据: 文档说“改造 memory_search/memory_get 内部权限过滤”（`docs/memory-access-control-design.md:34`, `docs/memory-access-control-design.md:423`）；实际 `memory_search`/`memory_get` 仍只读 `MEMORY.md` 和 `memory/*.md`（`src/tools/memory_search.rs:9`, `src/tools/memory_search.rs:124`, `src/tools/memory_get.rs:9`, `src/tools/memory_get.rs:89`），`brain.db` schema 也无 visibility/topic 字段（`src/memory/sqlite.rs:162`）。
- 风险: 上线后会出现“DB 权限模型设计了但工具链未接入”的假安全。
- 建议:
1. 先出“桥接阶段”设计: 文件层工具与 SQLite 层权限模型如何并存。
2. 给每个 phase 定义可验收开关（feature gate）与回滚脚本。
3. 先把 `memory_search/memory_get` 数据源统一到 SQLite，再谈 ACL。

### P1（高风险，应在上线前完成）

5. `topic_participants` 设计了但查询授权未使用，属于安全冗余。
- 证据: 表已定义（`docs/memory-access-control-design.md:96`），但 `project` 可见只看 `topic.project`（`docs/memory-access-control-design.md:121`, `docs/memory-access-control-design.md:144`）。
- 风险: 只要项目命中，非参与者也可看全部 topic 历史，最小权限失效。
- 建议:
1. `project` 可见改为“项目成员 AND topic_participants 命中（或显式 observer）”。
2. 对历史 topic 批量回填参与者（至少创建者、提及者、事件来源）。

6. `visibility_ceiling` 配置存在但未在 `can_see` 执行，策略漂移。
- 证据: 配置有 `visibility_ceiling`（`docs/memory-access-control-design.md:287`, `docs/memory-access-control-design.md:293`, `docs/memory-access-control-design.md:311`），`can_see` 未使用（`docs/memory-access-control-design.md:315` 之后）。
- 风险: 管理员以为限制生效，实际未生效，属于“配置欺骗风险”。
- 建议:
1. 在 SQL scope 构建阶段就裁剪可见级别，不要仅应用层兜底。
2. 增加启动时策略一致性检查: 配置字段存在但执行路径未引用则报错。

7. 后置过滤依赖 `memory.content.contains(kw)`，大小写/变体/分词都可绕过。
- 证据: `blocked_keywords` 过滤逻辑（`docs/memory-access-control-design.md:324`）。
- 风险: `SSH`/`s s h`/同义词可轻松绕过。
- 建议:
1. 统一标准化（lowercase、去噪、NFKC）。
2. 引入正则与词边界匹配。
3. 对中文加分词或最小词典匹配。

8. topic FTS 只建表未定义同步触发器，索引很快失真。
- 证据: 只有 `topics_fts` 建表（`docs/memory-access-control-design.md:91`），无 insert/update/delete trigger。
- 风险: topic 召回率持续下降，错误聚合增多。
- 建议:
1. 参照 memories_fts 触发器模式实现 topics_fts 三类 trigger。
2. 增加 nightly `rebuild fts` 维护任务与校验指标。

### P2（中风险，建议迭代内完成）

9. 复合索引设计偏弱，无法覆盖主查询路径。
- 证据: 当前只给 `(visibility, category)`、`(sender_id, chat_id)`（`docs/memory-access-control-design.md:65`, `docs/memory-access-control-design.md:66`）。
- 风险: 实际查询还要按 `topic_id/created_at/chat_type/sensitivity` 过滤，10K+ 后会频繁回表。
- 建议:
1. 增加 `(visibility, chat_type, chat_id, sensitivity, created_at DESC)`。
2. topic 上增加 `(topic_id, created_at DESC)`。
3. 用 `EXPLAIN QUERY PLAN` 固化基准，不达标就拒绝合并。

10. `query_topic_context` 先全拉再应用层过滤，和“SQL 层过滤”目标冲突。
- 证据: 先 `SELECT * WHERE topic_id=?` 再 `filter(can_see)`（`docs/memory-access-control-design.md:371` 到 `docs/memory-access-control-design.md:379`）。
- 风险: topic 很大时 O(n) 拉全量，延迟和内存压力上升。
- 建议:
1. 把权限条件下推 SQL。
2. 增加分页（limit/offset 或 keyset）与最大返回上限。

11. 未定义并发写入下 topic 创建幂等，可能产生重复 topic。
- 证据: `resolve_topic` 找不到就创建（`docs/memory-access-control-design.md:245` 到 `docs/memory-access-control-design.md:251`），无唯一约束说明。
- 风险: 多 worker 同时写入同一事件会裂变 topic。
- 建议:
1. 对 `(project, external_id)` 加唯一约束。
2. 创建流程使用事务 + `INSERT ... ON CONFLICT DO UPDATE`。

## 二、按审计维度结论

### 1) 安全性
- 主要问题: 访问判定维度不完整、身份绑定缺失、自动分类直接参与授权。
- 结论: 当前是“策略草案”，不是“可抗绕过”的访问控制方案。

### 2) 架构合理性
- 分层方向对，但职责边界还没落地。
- 过度设计: `topic_participants` 有表无授权价值链。
- 欠设计: identity binding、policy engine、审计事件模型。

### 3) 性能
- 文档对性能的判断偏乐观。
- 10K 只是起点，若 topic 聚合 + embedding 混合检索，关键瓶颈在“过滤前候选规模”和“未覆盖索引”。

### 4) 可扩展性
- 新增渠道时，`chat_id` 语义不统一会不断制造特例。
- 建议把访问主体抽象为 `Principal(user_id, channel_account_id, roles, projects)`，渠道只负责映射。

### 5) 实施风险
- 最大风险是“数据路径双轨”: 文件记忆 + SQLite 记忆并存时策略可能不一致。
- 迁移安全建议:
1. 先只写新字段，不启用拒绝策略（observe mode）。
2. 记录“新策略与旧策略差异日志”至少 7 天。
3. 再灰度开启 deny，先对非 owner 生效。
4. 全程可回滚: 保留旧查询路径开关。

## 三、Open Questions（5个问题的建议）

1. topic 自动合并策略
- 建议: 只允许“软合并”。保留原 topic_id，不做物理合并；新增 `topic_aliases(from_topic_id, to_topic_id, reason, operator, created_at)`。
- 原因: 可回滚、可审计，避免误合并不可逆。

2. topic 生命周期
- 建议: 默认永久保留元数据，内容按分层保留。
1. `normal`: 365 天后冷存储。
2. `sensitive`: 180 天后脱敏归档。
3. `secret`: 不自动清理，仅手工审批删除。

3. 群成员变动
- 建议: 明确策略二选一并写入文档。
1. 严格模式: 退群即失去历史可见性。
2. 合规模式: 保留“其在群期间产生/可见”的历史窗口。
- 推荐严格模式，默认最小权限。

4. 性能基准
- 建议给硬阈值，不要只问“可接受吗”。
1. P50 < 80ms。
2. P95 < 250ms。
3. P99 < 500ms。
4. 数据集: 10K/100K/1M 三档，含 30% topic 关联。

5. LLM 分类准确度与成本
- 建议分层决策。
1. 规则高置信直接决策。
2. 低置信走小模型。
3. 仍低置信标记 `needs_review`，默认保守权限（`owner` 或 `private`）。
- 指标: 每周抽样人工复核，追踪 precision/recall 与单条分类成本。

## 四、遗漏项（文档没覆盖但必须补）

1. 缺少“拒绝原因编码”与审计事件结构。
- 建议定义统一事件: `memory_access_denied{requester, memory_id, policy_rule, timestamp}`。

2. 缺少“策略版本化”。
- 建议每条 memory 记录 `policy_version`，方便回放与回归测试。

3. 缺少“密级降级流程”。
- 建议 `secret/sensitive` 降级必须有审批人与审计链，不能自动降级。

4. 缺少“删除与右撤回”设计。
- topic 聚合下单条记忆删除会影响摘要/embedding，需要重算策略。

5. 缺少“灾难恢复演练”要求。
- 建议至少定义季度演练: DB 恢复、索引重建、权限策略回滚。

## 五、建议的最小落地顺序

1. 先统一数据源到 SQLite（不做 ACL 拒绝，只做记录）。
2. 实现 identity binding + policy engine + SQL 下推过滤。
3. 补齐 topics_fts trigger 与关键联合索引。
4. observe mode 跑 7 天，验证误拒绝/误放行。
5. 灰度开启 ACL，最后再启用 LLM 自动 topic。

