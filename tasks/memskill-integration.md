# MemSkill 洞察集成计划

> 来源: /tmp/memskill-vs-prx-analysis.md
> 目标: 将 MemSkill 的两个核心洞察落地到 PRX

## Feature 1: useful_count 记忆质量反馈

### 现状
- `MemoryEntry` 已有 `useful_count` 字段但从未被递增
- 记忆存取无反馈循环，hygiene.rs 只做基于时间的清理

### 目标
- 记忆被召回且实际出现在 LLM 回复中时，useful_count +1
- hygiene.rs 使用 useful_count 作为保留/清理的信号之一

### 实现步骤
- [ ] 1.1 找到记忆召回点（memory recall in loop_.rs / agent loop）
- [ ] 1.2 跟踪本轮召回的 memory IDs
- [ ] 1.3 LLM 回复生成后，检查回复是否引用了召回的记忆内容（简单文本匹配或语义相关性）
- [ ] 1.4 对被使用的记忆调用 increment_useful_count()
- [ ] 1.5 在 Memory trait 中添加 increment_useful_count 方法（如果不存在）
- [ ] 1.6 SQLite backend 实现 UPDATE useful_count = useful_count + 1
- [ ] 1.7 hygiene.rs 修改：清理决策时考虑 useful_count（高 useful_count 的记忆不清理）
- [ ] 1.8 编译验证 cargo build --release

## Feature 2: Skill RAG（动态技能选择）

### 现状
- 所有 SKILL.md 在启动时加载，无差别注入 system prompt
- 技能多了之后 prompt 膨胀，不相关技能干扰 LLM

### 目标
- 加载时 embed 每个 skill 的 description
- query 时按用户消息相似度取 top-K 注入
- 保留 always_inject 机制（核心技能始终注入）

### 实现步骤
- [ ] 2.1 在 Skill struct 中添加 embedding: Option<Vec<f32>> 字段
- [ ] 2.2 技能加载时 embed description（使用现有 embedding provider）
- [ ] 2.3 添加 skill_select(query: &str, top_k: usize) 方法
- [ ] 2.4 在 build_context() / system prompt 构建处，替换全量注入为 top-K 注入
- [ ] 2.5 配置项：skill_rag.enabled (bool), skill_rag.top_k (usize, default 5), skill_rag.always_inject (Vec<String>)
- [ ] 2.6 编译验证 cargo build --release

## 优先级
Feature 1 先做（改动更小，风险更低），Feature 2 后做。

## 验证
- cargo build --release 通过
- 单元测试（如果有相关测试）通过
- 不破坏现有功能
