# ZeroClaw P0 自我系统实现计划

更新时间：2026-02-23

## 0. 结论先行（零代码优先评估）

- `P0-1 Fitness Reporter`：**零 Rust 代码方案不够**（可做 PoC，但指标稳定性和可审计性不足）。
- `P0-2 Self-Memory Bridge`：**可先零 Rust 代码落地**（`cron agent job + file_read + memory_store`），再补一个小型 Rust 桥接器提升确定性。

建议顺序：
1. 先上 `P0-2` 零代码版本（当天可交付）
2. 并行实现 `P0-1` 最小 Rust 版本（P0 正式版）
3. 最后把 `P0-2` 从“LLM驱动同步”升级为“确定性 Rust 同步器”

---

## 1. 现有能力盘点（按你指定源码）

### 1.1 指标来源能力

- `src/health/mod.rs`
  - 提供进程级快照：`pid/uptime/components/status/restart_count/last_error`。
  - 可作为稳定健康输入，但颗粒度是“组件级”，不是“任务级”。

- `src/observability/traits.rs` + `src/agent/loop_.rs`
  - 有 `ToolCall{success}`、`TurnComplete`、`Error`、`HeartbeatTick` 事件。
  - `src/observability/prometheus.rs` 能累计 `zeroclaw_tool_calls_total{tool,success}` 等计数。
  - 问题：当前缺少“统一持久化观测事件仓库”。日志/Prometheus有，但内存 fitness 计算器没有直接读取接口。

- `src/cost/tracker.rs`
  - 有 `CostTracker`、`CostSummary`、`state/costs.jsonl` 持久化能力。
  - 问题：在 `src/agent/loop_.rs` 当前路径里，`AgentEnd` 仍是 `tokens_used: None, cost_usd: None`，说明成本链路未完整接入主 agent loop。

- `src/agent/loop_.rs`
  - 能识别每次工具调用 success/failure。
  - 能在 turn 完成后发 `TurnComplete`。
  - 但没有“用户重复要求”与“主动发现问题”的结构化字段。

### 1.2 Memory 写入能力

- `src/memory/sqlite.rs`
  - `store/get/recall/list/forget` 完整，支持 `MemoryCategory::Core/Daily/Conversation/Custom`。
  - key 唯一，适合写结构化 fitness 记录。

- `src/memory/snapshot.rs`
  - 只导出 `category='core'` 到 `MEMORY_SNAPSHOT.md`。
  - 这意味着 fitness 若要进快照，建议写入 `Core` 或额外导出逻辑。

- `src/tools/memory_store.rs`
  - 现成可写 memory（可在 cron agent job 中零代码调用）。

---

## 2. Fitness 公式映射设计（P0-1）

公式：

`fitness = 任务完成质量*0.35 + 用户无需重复要求*0.25 + 主动发现问题*0.20 + 学到新东西*0.10 + 资源效率*0.10`

### 2.1 指标映射建议（含可信度）

1. `任务完成质量`（0.35）
- 候选数据源：`ObserverEvent::ToolCall.success`、`TurnComplete`。
- P0 计算：`tool_success_ratio = success_calls / total_calls`，再与 `turn_completion_ratio` 融合。
- 可信度：中（工具成功不等于任务完成，但可量化）。

2. `用户无需重复要求`（0.25）
- 候选数据源：当前无直接字段。
- P0 近似：
  - 文本规则计数（同会话内“再说一次/你没做/重复”等短语）
  - 或同一意图短时间重复触发计数（需要新增轻量意图指纹）
- 可信度：低-中（需新增规则或事件）。

3. `主动发现问题`（0.20）
- 候选数据源：`ObserverEvent::Error`、heartbeat/cron 自检任务输出中“发现风险/异常”的结构化标记。
- P0 计算：`proactive_findings_count`（仅计“未被用户显式要求”的发现事件）。
- 可信度：中（需定义判定标准，建议先规则化）。

4. `学到新东西`（0.10）
- 候选数据源：`memory.store` 次数、新 key 数、core 记忆新增量。
- P0 计算：`new_memory_keys_count`（去重后）+ `core_memory_delta`。
- 可信度：中-高（可直接量化）。

5. `资源效率`（0.10）
- 候选数据源：`cost tracker`（优先）、其次 token/时延。
- P0 现实：若 cost 未接线，先用 `响应时延 + tool调用数` 代替；接线后改成 `单位完成成本`。
- 可信度：当前中，接入 cost 后高。

---

## 3. P0-1：Fitness Reporter 计划

## 3.1 零代码方案（先评估）

方案：用 `cron agent job` 定时发 prompt，让 agent 自己 `file_read`/`memory_recall` 后 `memory_store` fitness。

可行点：
- 已有 `cron`（agent job）
- 已有 `memory_store`
- 可通过 prompt 约束输出 JSON

不足（P0 不建议作为正式版）：
- 指标口径不稳定（LLM 主观解释）
- 无法可靠拿到全量 tool success/failure 统计
- “用户重复要求”无法稳定判定
- 审计可重复性差（同输入可能不同分数）

结论：**零代码可做临时演示，不足以作为 P0 正式交付。**

## 3.2 推荐最小 Rust 方案

### 3.2.1 模块形态

- 新建：`src/self_system/fitness.rs`
- 新建：`src/self_system/mod.rs`
- 以“内部函数 + 调度入口”实现，不新增复杂 trait。

原因：
- 该功能是跨 `health/observability/memory` 的汇聚器，不是 provider/channel/tool 扩展点。
- 用独立模块最清晰，避免污染 `agent/loop_.rs`。

### 3.2.2 触发方式

首选：`cron`（固定间隔）
- 原因：可控、可追溯、与 `cron_runs` 对齐。

备选：`heartbeat`
- 当前 heartbeat 实现是“读取 HEARTBEAT.md 文本任务并触发 agent run”，更偏自然语言任务，不适合确定性指标管线。

结论：**P0 使用 cron 触发；heartbeat 保留为业务任务。**

### 3.2.3 存储位置与 schema

存储后端：SQLite memory（通过 `Memory::store`）
- 原因：天然接向量检索，且可被现有 `memory_recall` 直接检出。

key schema（避免与用户记忆冲突）：
- `self/fitness/latest`
- `self/fitness/daily/YYYY-MM-DD`
- `self/fitness/hourly/YYYY-MM-DDTHH`

content schema（JSON 字符串）：
- `version`
- `window_start/window_end`
- `subscores`: `task_quality`, `no_repeat`, `proactive`, `learning`, `efficiency`
- `weights`
- `final_score`
- `evidence`: 每个子分的原始计数与来源

category：`core`（便于 snapshot）

### 3.2.4 具体文件修改清单（P0-1）

必改：
- `src/self_system/mod.rs`（新增）
- `src/self_system/fitness.rs`（新增）
- `src/lib.rs`（导出模块）
- `src/main.rs`（如需新增命令：`self fitness-run`）
- `src/cron/scheduler.rs`（可选：增加内部调用入口，或继续走 agent job）
- `src/observability/*`（可选：暴露轻量聚合快照接口）

可选但强烈建议：
- `src/config/schema.rs`（新增 `[self_system]` 配置：启用开关、窗口、最小样本）

测试：
- `src/self_system/fitness.rs` 同文件单元测试
- `src/cron/scheduler.rs` 集成测试（触发与写入验证）

### 3.2.5 预估代码行数（P0-1）

- 核心逻辑：180-260 LOC
- 配置与接线：60-120 LOC
- 测试：120-220 LOC
- 合计：**360-600 LOC**

### 3.2.6 依赖关系

- 依赖 `Memory` trait（写 fitness 结果）
- 依赖 `health::snapshot_json()`
- 依赖可观测事件聚合（若无现成，需要在 observability 层加聚合缓存）
- 可选依赖 `cost tracker`（若未接入，则效率分先降级）

### 3.2.7 风险点

- 指标语义偏差：`tool success != 用户满意`
- 当前 cost 数据链路可能不完整
- 历史回溯窗口若过大，计算开销上升
- 若写 `core` 过于频繁，`MEMORY_SNAPSHOT.md` 体积膨胀

缓解：
- 加 `min_samples`；样本不足时标记 `confidence=low`
- 先写 hourly 到 `daily` 类别，daily 聚合再写 `core`
- 对 fitness 内容启用固定 JSON schema 版本

---

## 4. P0-2：Self-Memory Bridge 计划

## 4.1 零代码方案（优先）

方案：用现有 `cron agent job` 定时执行以下流程：
1. `file_read` 读取 `SOUL.md/IDENTITY.md/USER.md`（存在则读）
2. 按提示词抽取关键字段
3. 调 `memory_store` 写入约定 key

为什么可行：
- 工具链已齐备（`cron + file_read + memory_store`）
- 无需改 Rust 即可上线
- 对向量检索立即生效

限制：
- 依赖 LLM 抽取，字段稳定性一般
- 文件缺失/格式漂移时可能写脏数据

结论：**P0 可先用零 Rust 方案上线。**

## 4.2 需要入库的 workspace 内容（P0 字段集）

建议最小字段（避免过度抽取）：
- `SOUL.md`：`mission`, `fitness_formula`, `hard_constraints`
- `IDENTITY.md`：`role`, `non_goals`, `style`, `invariants`
- `USER.md`：`owner_preferences`, `tool_boundaries`, `forbidden_actions`, `communication_prefs`

另外保留原文摘要：
- 每个文件一条 `summary`
- 每个文件一条 `sha256`（检测漂移）

说明：当前仓库根目录未发现这些文件（只看到 `crates/robot-kit/SOUL.md`），桥接任务必须“文件存在才同步，不存在跳过且写状态”。

## 4.3 触发时机

分三层：
1. 启动后一次（cold-start）
2. 定时（每 15-60 分钟）
3. 手动（`cron run` 或未来 `self sync` 命令）

P0 推荐：先用 1+2（通过 cron）；文件变更监听（watcher）放 P1。

## 4.4 key naming schema（避免与用户记忆冲突）

统一前缀：`self/context/`

示例：
- `self/context/soul/summary`
- `self/context/soul/mission`
- `self/context/soul/fitness_formula`
- `self/context/identity/role`
- `self/context/user/tool_boundaries`
- `self/context/meta/source_hash/soul_md`
- `self/context/meta/last_sync_at`

category：
- 关键稳定事实：`core`
- 高频变动中间态：`daily`

## 4.5 零代码 vs Rust 实现判断

- 零代码（cron+tool）是否足够：**是，足够先交付 P0 可用版**。
- 是否建议补 Rust：**建议**，用于确保确定性、幂等和 schema 约束。

Rust 最小升级（P0.5/P1）：
- 新建 `src/self_system/memory_bridge.rs`
- 直接读取文件并结构化提取（规则优先，LLM可选）
- 用固定 schema 写 memory，含 hash 与版本

### 4.5.1 具体文件修改清单（P0-2 Rust升级版）

- `src/self_system/memory_bridge.rs`（新增）
- `src/self_system/mod.rs`（扩展）
- `src/lib.rs`（导出）
- `src/main.rs`（可选：`self sync` 命令）
- `src/config/schema.rs`（可选：`[self_system.bridge]` 源文件列表/interval）

测试：
- `src/self_system/memory_bridge.rs` 单元测试（缺文件、变更、幂等）

### 4.5.2 预估代码行数（P0-2）

- 零代码版：0 Rust LOC（仅配置/cron 任务内容）
- Rust 升级版：**180-320 LOC**（含测试可到 300-450 LOC）

### 4.5.3 依赖关系

- 依赖 `Memory` trait
- 依赖 workspace 文件路径约定
- 可选依赖 hash（`sha2` 已在项目中使用）

### 4.5.4 风险点

- 文件不存在或格式漂移
- 过度写入导致 core 噪声
- 与用户手写 memory key 冲突

缓解：
- 统一 `self/context/` 前缀
- 加 `source_hash`，内容未变不重写
- 抽取字段设白名单，未知字段不入库

---

## 5. 两个 P0 的依赖与实施顺序

1. 先实施 `P0-2` 零代码版（无 Rust 依赖，最快产出）
2. 再实施 `P0-1` Rust 版（需要稳定指标管线）
3. 最后将 `P0-2` 升级为 Rust 确定性桥接（可并入同一 `self_system` 模块）

依赖图：
- `P0-1` 依赖：观测聚合可读接口 + memory 写入
- `P0-2` 零代码：仅依赖 cron 和工具
- `P0-2` Rust升级：仅依赖 memory 与文件读取

---

## 6. 验收标准（建议）

### P0-1 验收

- 每个计算周期产生 `self/fitness/daily/YYYY-MM-DD` 记录
- 记录包含 5 个子分 + 权重 + final_score + evidence
- 样本不足时不会伪造高分，且给出 `confidence`

### P0-2 验收

- 启动后/定时可在 memory 中检索到 `self/context/*` key
- `memory_recall` 对 `soul/identity/user` 关键词可命中对应记录
- 文件未变化时不重复写入（或写入次数可控）

---

## 7. 本次建议的最终决策

- `P0-1 Fitness Reporter`：走 **最小 Rust 实现**（不建议只靠 cron+prompt）。
- `P0-2 Self-Memory Bridge`：先走 **零代码实现**，并预留 Rust 升级路径。

