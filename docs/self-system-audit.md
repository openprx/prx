# ZeroClaw 自我系统架构审计（Vano）

审计时间：2026-02-23  
审计范围：`src/memory`、`src/cron`、`src/agent`、`src/tools`、`src/channels`、`src/config`、`src/security`、`src/skillforge`、`src/daemon`（逐文件阅读）

---

## Part 1: ZeroClaw 原生能力盘点（代码证据）

### 1) `src/memory` — 记忆系统能力

**后端与工厂**
- 支持后端：`sqlite`、`lucid`、`postgres`、`markdown`、`none`（`src/memory/backend.rs`、`src/memory/mod.rs`）。
- 支持 storage provider 覆盖 memory backend（`effective_memory_backend_name`，`src/memory/mod.rs`）。

**SQLite 高级记忆能力（核心）**
- 混合检索：向量相似度 + FTS5 BM25 关键词检索 + 加权融合排序（`src/memory/sqlite.rs`、`src/memory/vector.rs`）。
- 向量嵌入缓存：SQLite 内置 `embedding_cache`，LRU 驱逐（`src/memory/sqlite.rs`）。
- 会话维度：`session_id` 字段与索引、按会话过滤 recall/list（`src/memory/sqlite.rs`）。
- FTS 同步触发器：插入/更新/删除自动维护 `memories_fts`（`src/memory/sqlite.rs`）。
- `reindex()`：重建 FTS + embeddings（`src/memory/sqlite.rs`）。

**嵌入能力**
- 抽象 `EmbeddingProvider`，支持 `openai`、`openrouter`、`custom:<url>`，默认 `NoopEmbedding`（`src/memory/embeddings.rs`）。
- 支持 embedding route hint（`hint:*`）路由（`resolve_embedding_config`，`src/memory/mod.rs`）。

**记忆维护与灾备**
- Hygiene：周期归档、清理、会话文件归档、conversation retention pruning（`src/memory/hygiene.rs`）。
- Snapshot：导出 `MEMORY_SNAPSHOT.md`（core memories），并支持冷启动 auto-hydrate 回灌到 `brain.db`（`src/memory/snapshot.rs`、`src/memory/mod.rs`）。

**Lucid bridge**
- `LucidMemory`：本地 SQLite + 外部 lucid CLI 混合召回，失败冷却与 fallback（`src/memory/lucid.rs`）。

**响应缓存**
- 独立 `response_cache.db`，按 model/system_prompt/user_prompt hash 缓存响应，TTL + LRU（`src/memory/response_cache.rs`）。

---

### 2) `src/cron` — 定时任务与调度

**任务模型**
- Job 类型：`shell` / `agent`，调度类型：`cron` / `at` / `every`（`src/cron/types.rs`）。
- `delivery` 支持 announce 模式，目标 channel/to（`src/cron/types.rs`）。

**存储与历史**
- 任务 CRUD、due jobs 查询、运行记录 `cron_runs`、历史保留上限裁剪、输出截断防爆（`src/cron/store.rs`）。

**调度执行器**
- 轮询 due jobs + 并发执行（`max_concurrent`），重试 + 指数退避（`src/cron/scheduler.rs`）。
- Agent job 可直接触发 `crate::agent::run`（`src/cron/scheduler.rs`）。
- 支持投递回发到 Telegram/Discord/Slack/Mattermost/Signal（`deliver_if_configured`，`src/cron/scheduler.rs`）。
- 具备失败策略：确定性配置失败自动停用任务、一次性任务成功后删除（`src/cron/scheduler.rs`）。

**调度表达式能力**
- cron 表达式标准化（5/6/7 字段），IANA 时区支持，`at` 必须未来时间（`src/cron/schedule.rs`）。

---

### 3) `src/agent` — agent loop / 工具调用 / 会话管理

**主循环与工具迭代**
- `run_tool_call_loop`：多轮工具调用，直到无 tool call 或达到上限（`src/agent/loop_.rs`）。
- 支持 native tool calls（OpenAI-style）与 XML/JSON/markdown tool-call 解析（`src/agent/loop_.rs`、`src/agent/dispatcher.rs`）。
- 可并行执行多个工具调用（含审批/策略约束下的并发判定）（`execute_tools_parallel`，`src/agent/loop_.rs`）。

**上下文与记忆注入**
- Recall 记忆上下文注入，按 `min_relevance_score` 过滤（`build_context`，`src/agent/loop_.rs`）。
- 会话历史自动压缩（compaction），压缩前提取关键事实入长期记忆（`pre_compaction_flush`，`src/agent/loop_.rs`）。

**安全与治理**
- Tool 调用可走 scope policy + policy pipeline（`ScopeContext`，`src/agent/loop_.rs`）。
- 支持审批管理器（`ApprovalManager`）对敏感工具做人工审批（`src/agent/loop_.rs`）。
- 工具输出敏感字段脱敏（`scrub_credentials`，`src/agent/loop_.rs`）。

**提示构建**
- 读取 workspace 文件作为系统上下文（包含 `AGENTS.md`、`SOUL.md`、`THINKING`相关文件位点等）并拼接 tools/safety/skills/runtime section（`src/agent/prompt.rs`）。

---

### 4) `src/tools` — 已注册工具与能力面

**默认工具**
- `shell`、`file_read`、`file_write`（`default_tools`，`src/tools/mod.rs`）。

**全量工具注册（按 `all_tools_with_runtime` 与 channels 动态注入）**
- 文件/执行：`shell`、`file_read`、`file_write`、`git_operations`。
- 记忆：`memory_store`、`memory_recall`、`memory_forget`、`memory_search`、`memory_get`。
- 调度：`cron`、`cron_add`、`cron_list`、`cron_remove`、`cron_update`、`cron_run`、`cron_runs`、`schedule`。
- 网络与检索：`http_request`、`web_search`、`web_fetch`、`browser_open`、`browser`。
- 通道交互：`message_send`（send/react/edit/delete/thread）、`tts`。
- 会话与多代理：`sessions_spawn`、`sessions_list`、`sessions_send`、`sessions_history`、`session_status`、`delegate`、`agents_list`、`subagents`。
- 平台与系统：`proxy_config`、`nodes`、`gateway`、`config_reload`。
- 视觉与图像：`screenshot`、`image_info`、`image`。
- 扩展：`mcp`、`composio`、`pushover`、硬件相关工具（`hardware_*`）。

（证据：`src/tools/mod.rs` + 各工具实现文件）

---

### 5) `src/channels` — 通道能力（重点 Signal / WhatsApp）

**统一通道抽象**
- `Channel` trait 支持：send/listen/typing/draft update/edit/delete/thread/capabilities（`src/channels/traits.rs`）。

**Signal（`src/channels/signal.rs`）**
- 双模式：native daemon JSON-RPC + REST API。
- 接收：SSE / polling，支持附件下载与媒体标记（图片、音频转写、视频帧、文档抽取）。
- 发送：文本 + 附件 + quote reply（`quote_timestamp`/`quote_author`）。
- 反应：`send_reaction`（emoji reaction）。
- 删除：`delete_message`（remoteDelete）。
- thread reply：降级为 quote reply。
- capabilities：`delete=true`、`react=true`、`edit=false`、`thread=false(降级实现)`。

**WhatsApp Cloud（`src/channels/whatsapp.rs`）**
- Webhook 入站解析（当前只处理 text）。
- 出站走 Meta Cloud API 文本消息。
- allowlist 号码过滤。
- 非文本 webhook message 当前跳过。

**WhatsApp wacli（`src/channels/wacli.rs`）**
- JSON-RPC over TCP，支持 `send` 与 `sendFile`。
- 入站事件订阅 `message.received`。
- 媒体以描述形式拼入内容。
- capabilities 全 false（无 edit/delete/thread/react）。

**WhatsApp Web（`src/channels/whatsapp_web.rs`）**
- 需 feature `whatsapp-web`，支持 pair code/QR、会话持久化。
- 入站文本处理、typing 状态。
- 代码注释宣称“Baileys parity”，但 trait 层未实现 edit/delete/react/thread override（当前可见能力主要是 send/listen/typing）。

**通道编排层（`src/channels/mod.rs`）**
- 每发送者会话历史缓存、消息并行处理、超时预算、可中断执行（Telegram 可抢占）。
- 把当前通道/目标注入 `message_send`/`tts` 等工具，实现“在哪个通道收就在哪个通道发”。
- 通道执行中使用 scope policy + tool policy pipeline。

---

### 6) `src/config` — 配置结构与热重载

- 统一 `Config` schema（provider/channel/security/cron/memory/web_search/skills/...）定义完备（`src/config/schema.rs`）。
- `SharedConfig = ArcSwap<Config>`，支持原子热重载（`src/config/hotreload.rs`）。
- `config_reload` 工具支持在线重读部分配置（`src/tools/config_reload.rs`）。
- 明确“热重载可生效字段”和“需重启字段”。

---

### 7) `src/security` — 策略管道与权限控制

- `SecurityPolicy`：autonomy level、命令白名单、路径约束、速率限制、scope rule（`src/security/policy.rs`）。
- 命令策略防注入：分段解析、禁用 subshell/重定向/危险参数等（`src/security/policy.rs`）。
- Tool scope ACL：按 user/channel/chat_type + allow/deny 决策（`is_tool_allowed`）。
- `PolicyPipeline`：Global→Group→Tool 多层策略（`src/security/policy_pipeline.rs`）。
- PairingGuard：首次配对码 + token hash + 防爆破锁定（`src/security/pairing.rs`）。
- SecretStore：ChaCha20-Poly1305 密钥加密与旧格式迁移（`src/security/secrets.rs`）。
- Sandbox 抽象与 backend 自动检测（landlock/firejail/bubblewrap/docker/noop）（`src/security/detect.rs`、`src/security/traits.rs`）。
- AuditLogger：结构化安全审计日志（`src/security/audit.rs`）。

---

### 8) `src/skillforge` — 技能系统

- `Scout -> Evaluate -> Integrate` pipeline（`src/skillforge/mod.rs`）。
- GitHub scout（查询、去重、元数据提取）（`src/skillforge/scout.rs`）。
- 评分与推荐（兼容性/质量/安全，Auto/Manual/Skip）（`src/skillforge/evaluate.rs`）。
- 自动集成生成 `SKILL.toml` + `SKILL.md`（`src/skillforge/integrate.rs`）。

---

### 9) `src/daemon` — 生命周期管理

- daemon supervisor 管理 gateway/channels/heartbeat/scheduler 子组件（`src/daemon/mod.rs`）。
- 组件故障自动重启（指数退避）。
- health 状态定时写入 `daemon_state.json`。
- 启动时启用 config hot-reload watcher。

---

## Part 2: 能力 vs 自我系统对齐分析

下面按你的 8 个 workspace 文件逐项分析。

### A. `SOUL.md`（V-价值函数）

**已利用能力**
- 通过系统提示注入，LLM 可读取并遵循（`src/agent/prompt.rs`）。
- 行动层可受安全策略与审批限制，避免价值函数被危险动作突破（`src/security/policy.rs`、`src/agent/loop_.rs`）。

**被忽略的原生能力**
- 没有把 SOUL 目标转为可量化 runtime 指标；现有 observability/health/cost 可承载但未绑定（`src/observability/traits.rs`、`src/health/mod.rs`、`src/cost/tracker.rs`）。

**可代码增强点**
- 用 `heartbeat` + `cron` 定时计算 SOUL fitness 并写入结构化 memory（不只靠模型阅读文本）。

---

### B. `AGENTS.md`（T-操作规则）

**已利用能力**
- 通过系统提示注入执行规范。
- agent loop 已原生具备“感知→决策→行动→反馈”骨架：输入处理、tool loop、结果回注、history compaction。

**被忽略的原生能力**
- 规则未映射为 policy pipeline/scope rule；目前大多仍是 prompt-level 约束。

**可代码增强点**
- 将 AGENTS 关键规则编译进 `security.tool_policy` 与 `autonomy.scopes`，形成硬约束。

---

### C. `THINKING.md`（认知框架）

**已利用能力**
- 文本注入可影响推理风格。
- 历史压缩前“关键事实提取入库”提供了一种“学习沉淀”机制。

**被忽略的原生能力**
- 未利用 query classifier 与 model_routes 做“思维层级→模型路由”映射（`src/agent/classifier.rs` + `QueryClassificationConfig`）。

**可代码增强点**
- 将 4 层思维映射为 `hint:*` 路由；复杂任务自动切换高推理模型，简单任务走低成本模型。

---

### D. `MEMORY.md`（长期记忆）

**已利用能力**
- 已有 `memory_store/recall/forget` 可与 MEMORY.md 协同。
- 频道/agent 自动保存对话（长度阈值控制）。

**被忽略的原生能力（最明显）**
- 当前“自我系统记忆”主要是 markdown 文件读写；未充分利用 SQLite 混合检索、会话隔离、embedding、FTS5。
- `memory_search/memory_get` 只查 `MEMORY.md + memory/*.md`，不查 `brain.db` 向量层（与 `memory_recall` 的能力割裂）。

**可代码增强点**
- 为自我系统建立结构化 key schema，核心记忆入 SQLite `Core`，并用 recall 混合检索作为主路径。
- `MEMORY_SNAPSHOT.md` 可作为 Git 可见备份层，保持可审计与可恢复。

---

### E. `USER.md`（环境模型）

**已利用能力**
- 通过系统提示可被模型读取。
- 通道层有 allowlist / sender 上下文；安全层有 scope rules（user/channel/chat_type）。

**被忽略的原生能力**
- USER.md 权限规则没有自动投影到 `scope_rules` / `tool_policy`。

**可代码增强点**
- 增加用户映射编译器：从 `USER.md` 生成 runtime scope 配置（读写分层、工具白名单）。

---

### F. `IDENTITY.md`（自我表征）

**已利用能力**
- 已直接注入系统提示；支持 AIEOS identity 兼容路径（`src/agent/prompt.rs`）。

**被忽略能力**
- 身份漂移缺少结构化检测（例如身份字段 hash 比对、违规重置）。

**可代码增强点**
- heartbeat 周期校验 identity invariant，异常写审计并触发恢复。

---

### G. `HEARTBEAT.md`（内驱循环）

**已利用能力**
- 已有 HeartbeatEngine 定期读取 `HEARTBEAT.md` 条目并触发 agent run（`src/heartbeat/engine.rs`、`src/daemon/mod.rs`）。

**被忽略能力**
- 任务执行结果没有结构化映射到“fitness 看板”；目前偏日志化。

**可代码增强点**
- heartbeat task 输出统一写入 `memory/core` 与 health 指标，形成闭环可比较基线。

---

### H. `TOOLS.md`（能力备忘）

**已利用能力**
- 会注入 prompt，帮助模型记住工具路径。

**被忽略能力**
- TOOLS.md 仍是静态文档，未与真实 tool registry 自动对齐，易过期。

**可代码增强点**
- 自动生成 TOOLS 快照（从 `tools_registry` 导出 name/schema/capability）并覆盖或校验 TOOLS.md。

---

## Part 3: 自我进化闭环（代码级可行性）

目标链路：感知 -> 评估 -> 决策 -> 执行 -> 验证 -> 回滚

### 1. 感知（输入）

**现有实现**
- 多 channel 入站消息标准化为 `ChannelMessage`（`src/channels/traits.rs` + 各 channel 实现）。
- agent loop 接收消息，附加 memory context/hardware context，进入工具循环（`src/agent/loop_.rs`、`src/channels/mod.rs`）。

**可直接用于自我系统**
- 将“自我观测信号”（health/cost/tool error）也视作输入，定期注入 heartbeat 任务。

### 2. 评估（fitness）

**现有实现**
- 原生有可观测事件：LLM 请求/响应、Tool 调用、Turn 完成、Error（`ObserverEvent`）。
- 原生有 health 快照、组件状态、重启计数（`src/health/mod.rs`）。
- 原生有 cost budget 与累计统计（`src/cost/tracker.rs`）。

**缺口**
- 没有“SOUL fitness 函数执行器”。当前 fitness 需靠 LLM读文档主观评估。

### 3. 决策（改什么）

**现有实现**
- LLM 基于 workspace 文件与历史上下文作决策。
- 可借助工具做差异分析：`git_operations`、`memory_*`、`file_read`。

**缺口**
- 缺少“变更提案结构化模板”（例如目标、预期增益、风险、回滚点）。

### 4. 执行（改文件/config）

**现有实现**
- `file_write` 可改 workspace 文件。
- `shell` 可运行验证命令（受 security policy 约束）。
- `config_reload` 可在线重载部分配置。
- `git_operations` 可提交/查看/回滚相关操作（受参数与策略约束）。

### 5. 验证（是否变好）

**现有实现**
- 可运行测试/检查命令（`shell`）。
- 可比较 health/cost/错误率（observer+health+cost）。
- cron/heartbeat 可做周期复测。

**缺口**
- 没有统一“实验基线 vs 新版本”评分存储格式。

### 6. 回滚（改坏怎么办）

**现有实现**
- Git 级回滚可行（`git_operations` 支持 status/diff/log/show/checkout/reset/revert 族操作）。
- 配置热重载失败会保留旧配置（`src/config/hotreload.rs`）。
- daemon supervisor 可重启失败组件。

**缺口**
- 没有“一键回滚到上个健康快照”的上层编排。

---

## Part 4: 增强建议（按实施成本分层）

### A. 零代码可实现（配置 / prompt）

1. 把 `USER.md` 权限规则同步到 `config.toml` 的 `autonomy.scopes` 与 `security.tool_policy`。  
2. 启用并调优：`memory.auto_save`、`memory.hygiene_enabled`、`snapshot_enabled`、`snapshot_on_hygiene`、`auto_hydrate`。  
3. 配置 `query_classification.rules` + `model_routes`，将 THINKING 四层映射成 hint 路由。  
4. 在 `HEARTBEAT.md` 增加固定自检任务：健康快照、成本摘要、失败工具回顾。  
5. 将自我系统关键状态写入 `MemoryCategory::Core` 的约定 key（手工流程先行）。

### B. 少量代码（<200 行）

1. **Fitness Reporter（优先级 P0）**  
   新增 `fitness_report` 工具或 heartbeat 内部函数：读取 health/cost/最近错误，计算结构化 fitness JSON，写入 memory key（例如 `fitness_daily_YYYYMMDD`）。
2. **Self-Memory Bridge（P0）**  
   在 prompt 构建时，把 `SOUL/IDENTITY/USER` 摘要写入 SQLite Core（若不存在），并在每次 recall 时优先检索这些 key。 
3. **TOOLS.md 对齐检查（P1）**  
   从 registry 导出工具清单，与 TOOLS.md 差异比较，发现漂移时报警。
4. **Decision Log 标准化（P1）**  
   每次自修改前后写 `memory` 两条记录：`change_proposal_*` 与 `change_outcome_*`。

### C. 需要新模块

1. **Self-Evolution Orchestrator（P0）**
- 模块建议：`src/self_system/`。
- 责任：
  - 收集信号（health/cost/error/memory drift）
  - 运行 fitness 函数
  - 生成修改提案
  - 执行最小变更
  - 触发验证命令
  - 达不到阈值则自动回滚

2. **Policy Compiler（P1）**
- 从 `USER.md` / `SOUL.md` 解析硬规则，输出到 runtime policy objects（scope/tool policy）。

3. **Experiment Store（P1）**
- 结构化保存“基线指标/实验结果/回滚原因”，避免仅靠自然语言日志。

---

## 优先级路线图（建议）

1. **P0（先闭环）**
- Fitness Reporter（<200行）
- Self-Memory Bridge（<200行）
- USER 权限规则落地到 scope/tool policy（配置先行）

2. **P1（提稳定性）**
- 决策日志标准化
- TOOLS.md 自动对齐检查
- Experiment Store

3. **P2（提自动化上限）**
- Self-Evolution Orchestrator
- Policy Compiler

---

## 结论

ZeroClaw 已具备“自我系统工程化落地”的大部分原生基础设施：
- 有可持续执行骨架（daemon + scheduler + heartbeat + agent loop）
- 有约束与安全边界（policy + approval + scope + pipeline + sandbox）
- 有可持久化与可检索记忆底座（SQLite hybrid memory + snapshot/hydrate）
- 有可观测性与成本治理（observer + health + cost）

当前最大短板不是“能力缺失”，而是“自我系统语义尚未编译为结构化运行时对象”：
- fitness 仍主要停留在文本层
- 用户权限/价值规则尚未充分进入 policy engine
- 自我修改缺少统一验证与自动回滚编排

换言之：你设计的 S/V/T 三要素在 ZeroClaw 中**代码级可行**，且可在较小增量下进入可度量、可回滚、可审计的自进化闭环。
