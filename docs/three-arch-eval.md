# ZeroClaw 三大架构需求技术评估

> 评估日期：2026-02-23
> 评估范围：`src/tools/sessions_spawn.rs`、`src/memory/sqlite.rs`、`src/config/mod.rs`/`src/config/schema.rs`、`src/agent/loop_.rs`、`src/channels/*`、`src/tools/nodes.rs`、`src/channels/signal_native.rs`、`src/channels/wacli.rs`、`src/gateway/mod.rs`

---

## 需求 1：主进程/子进程完全隔离

### 1. 当前状态分析（现有代码支持什么）

结论：**当前 `sessions_spawn` 不是 OS 子进程，而是同进程异步任务（`tokio::spawn`）**。因此是“会话级逻辑隔离”，不是“进程级资源隔离”。

关键证据：
- `src/tools/sessions_spawn.rs`
  - 通过 `tokio::spawn(async move { ... })` 启动子任务。
  - 子任务内部调用 `run_sub_agent_task(...)`，并在同一进程内执行 `run_tool_call_loop(...)`。
  - `tools` 直接复用父进程工具注册表 `Arc<Vec<Box<dyn Tool>>>`。
- `src/memory/sqlite.rs`
  - SQLite 默认路径是 `workspace/memory/brain.db`。
- `src/memory/mod.rs`
  - memory backend 工厂按 `workspace_dir` 创建；并无 `sessions_spawn` 级别的独立 memory 实例。
- `src/tools/memory_store.rs` / `src/tools/memory_recall.rs`
  - 工具调用 memory 时 `session_id` 传 `None`，默认不分会话隔离。
- `src/channels/mod.rs` / `src/agent/loop_.rs`
  - 主流程在启动时构建一次 `mem` 与 `tools_registry`，`sessions_spawn` 复用同一套工具对象。
- `src/tools/file_read.rs` / `src/tools/file_write.rs` + `src/security/policy.rs`
  - 文件访问根路径来自同一个 `SecurityPolicy.workspace_dir`，因此工作区共享。

#### 共享矩阵（当前实现）

| 资源 | 当前状态 | 说明 |
|---|---|---|
| OS 进程地址空间 | 共享 | `tokio::spawn`，非 `std::process::Command` |
| Tokio Runtime / 线程池 | 共享 | 子任务与主任务在同一 runtime |
| Tool 实例 | 共享 | `sessions_spawn` 注入同一 `tools_registry` |
| Memory 实例 | 共享 | `MemoryStoreTool/RecallTool` 共享同一 `Arc<dyn Memory>` |
| SQLite DB 文件 | 共享（默认） | 都指向同一 `workspace/memory/brain.db` |
| Config 对象 | 部分共享 | `sessions_spawn` 持有构造时快照字段；工具内可能持有 `SharedConfig(ArcSwap)` |
| Workspace 文件系统 | 共享 | 同一 `workspace_dir` + 同一安全策略根路径 |
| 环境变量/进程级状态 | 共享 | 同一进程 |
| 子会话历史（history） | 隔离 | 每个 run 单独 `history` 缓冲 |
| 子会话取消令牌 | 隔离 | 每个 run 单独 `CancellationToken` |

### 2. 推荐方案（含替代方案比较）

#### 方案 A（推荐）：多进程 Worker 隔离（同机）
- `sessions_spawn` 新增 `mode=process`（默认可保留 `task` 兼容）。
- 主进程通过 `std::process::Command` 启动 `zeroclaw --session-worker ...`。
- Worker 进程接收最小输入（task、identity、workspace_root、tool_allowlist、timeout），独立初始化：
  - 独立 runtime
  - 独立 memory backend（独立 DB 路径）
  - 独立 security policy
  - 独立 tool registry
- 主进程与 worker 用本地 IPC（Unix Domain Socket / stdio JSON-RPC）通信。

优点：
- 满足“主/子进程完全隔离”核心诉求。
- 对现有架构侵入可控，可渐进迁移。

缺点：
- 会增加进程管理、日志聚合、生命周期清理复杂度。

#### 方案 B：容器级隔离（Docker runtime per run）
- 每个 sub-agent 在临时容器中执行。
- 强隔离（FS、进程、网络）最强。

优点：
- 安全边界最清晰。

缺点：
- 本地开发/跨平台复杂度显著上升，冷启动较慢。

#### 方案 C：仅逻辑隔离（继续 task + session_id）
- 只做 memory 分区和 workspace 子目录，不换进程。

优点：
- 变更最小。

缺点：
- **不满足“完全隔离”**（本需求不建议）。

### 3. 代码改动清单（文件 + 预估行数）

推荐方案 A（多进程）预估：**~900–1500 LoC**

- `src/tools/sessions_spawn.rs`：新增 process 模式、IPC 调用、worker 生命周期管理（+220~380）
- `src/main.rs` / `src/lib.rs`：新增 `session-worker` 子命令入口（+80~160）
- `src/agent/loop_.rs`：抽出可复用 worker 执行入口（+120~220）
- `src/config/schema.rs`：新增 spawn 隔离配置（如 `spawn.isolation_mode`、`spawn.workspace_strategy`）（+80~160）
- `src/memory/mod.rs` / `src/memory/sqlite.rs`：支持按 run 指定独立 DB 路径（+80~140）
- `src/security/policy.rs`：按 worker workspace 根初始化（+40~80）
- `src/tools/mod.rs`：按 worker allowlist 组装工具（+40~100）
- 新增 `src/session_worker/*`（IPC 协议 + worker runner）（+180~320）

### 4. 依赖关系和实现顺序

1. 定义隔离契约（run manifest、IPC 协议、worker 生命周期）
2. 新增 worker 入口并跑通“无工具单轮任务”
3. 接入独立 memory/workspace/security
4. 接入工具 allowlist + 超时/取消
5. 在 `sessions_spawn` 切换到 process 模式并保留 task 兼容开关
6. 增加回归测试（并发 run、崩溃回收、资源清理）

### 5. 风险点

- 子进程僵尸/资源泄漏（需强制超时 + kill + reap）
- 日志与可观测性分裂（需 run_id 贯通）
- 独立 workspace 的复制策略（深拷贝成本 vs COW 复杂度）
- Windows/Linux/macOS IPC 兼容差异

---

## 需求 2：多身份体系（spawn 指定 alpha/bravo 等）

### 1. 当前状态分析（现有代码支持什么）

结论：**主会话支持 workspace identity 注入；`sessions_spawn` 不支持身份模板，也不加载独立 SOUL/AGENTS/MEMORY。**

关键证据：
- `src/channels/mod.rs`
  - `build_system_prompt_with_mode(...)` 会注入 `AGENTS.md/SOUL.md/TOOLS.md/IDENTITY.md/USER.md/MEMORY.md`。
- `src/tools/sessions_spawn.rs`
  - 子会话 system prompt 为硬编码常量 `SYSTEM_PROMPT`，只含泛化指令。
  - 参数无 `agent`/`identity` 字段。
- `src/config/schema.rs`
  - 已有 `[agents]`（`DelegateAgentConfig`）：provider/model/system_prompt/allowed_tools/agentic 等。
  - 但该结构主要服务 `delegate` tool，不包含 identity 文件目录（SOUL/AGENTS/MEMORY）映射。
- `src/tools/delegate.rs`
  - 支持按 agent 配置 provider/model/system_prompt；是“模型与行为参数”维度，不是“workspace identity 文件集”维度。

### 2. 推荐方案（含替代方案比较）

#### 方案 A（推荐）：扩展现有 `[agents]`，让 `sessions_spawn` 复用
在 `DelegateAgentConfig` 基础上增加身份相关字段：
- `identity_dir`：例如 `identities/alpha/`
- `workspace_dir`（可选）：该身份专属工作区根
- `memory_scope`：`shared | isolated`
- `spawn_enabled`：是否允许被 `sessions_spawn` 调用

`sessions_spawn` 新增参数：
- `agent`（例如 `alpha` / `bravo`）

流程：
1. 解析 `agent` -> 读取对应 config
2. 以 `identity_dir` 加载 `SOUL.md/AGENTS.md/MEMORY.md`（缺失则显式标注）
3. 组装子会话系统提示词（可复用 `build_system_prompt_with_mode` 的子集或抽取公共 builder）
4. 按 `memory_scope` 选择共享或隔离 memory

优点：
- 利用现有 `[agents]`，概念统一、学习成本低。
- 与 `delegate` 保持一致命名与治理。

缺点：
- 需要给 `DelegateAgentConfig` 增字段，涉及兼容校验。

#### 方案 B：新增 `[identities]`，与 `[agents]` 解耦
- `agents` 管模型路由；`identities` 管人格与文档集；spawn 时两者组合。

优点：
- 结构清晰，扩展性更高。

缺点：
- 配置复杂度更高，改造面更大。

#### 方案 C：仅运行时参数传 identity 文件路径
- 不改 config，调用时传绝对/相对路径。

优点：
- 开发最快。

缺点：
- 缺少治理与可审计性，不推荐长期使用。

### 3. 代码改动清单（文件 + 预估行数）

推荐方案 A 预估：**~500–900 LoC**

- `src/config/schema.rs`：扩展 `DelegateAgentConfig` 身份字段 + 校验（+120~220）
- `src/tools/sessions_spawn.rs`：新增 `agent` 参数、身份解析、prompt 构建、memory scope（+180~320）
- `src/channels/mod.rs`：抽取可复用 identity prompt builder（+60~140）
- `src/tools/agents_list.rs`：展示 identity/spawn 能力标签（+20~60）
- `src/tools/mod.rs`：必要的工具注册/筛选补充（+20~60）
- `docs/config-reference.md` / `docs/commands-reference.md`：配置与工具文档更新（+80~160）

### 4. 依赖关系和实现顺序

1. 先定配置模型（`agents` 扩展字段）
2. 抽取/复用身份 prompt 构建器
3. 给 `sessions_spawn` 接入 `agent` 参数与身份加载
4. 接入 memory scope（shared/isolated）
5. 完成工具说明与文档、测试

### 5. 风险点

- identity 文件缺失或冲突（需 fail-fast + 明确错误）
- 多身份与安全策略耦合（不同身份工具权限需显式）
- 配置兼容：旧配置必须无感升级

---

## 需求 3：远程代理（Remote Agent Proxy）

### 1. 当前状态分析（现有代码支持什么）

结论：**当前 `nodes` 工具是配置存根，不具备远程执行能力。现有 JSON-RPC 实现存在于 channel 侧（Signal/Wacli），可复用协议经验但不是可直接复用的执行框架。**

关键证据：
- `src/tools/nodes.rs`
  - `list/status/notify/invoke` 均为 stub，明确 `no network call was made`。
- `src/channels/wacli.rs`
  - JSON-RPC 2.0 over TCP（line-delimited）实现完整，但无 TLS、无强认证（主要靠 allowlist）。
- `src/channels/signal.rs` / `src/channels/signal_native.rs`
  - 使用 signal-cli JSON-RPC（HTTP）+ SSE；属于 channel 适配，不是通用远程执行协议。
- `src/gateway/mod.rs` + `src/security/pairing.rs`
  - 已有可复用安全机制：pairing code -> bearer token、限流、幂等、webhook secret/HMAC。

### 2. 推荐方案（含替代方案比较）

#### 目标架构

- `ZeroClaw Core`（控制面）
  - 调用 `nodes` 工具向 remote node 发起执行请求
- `zeroclaw-node`（远程轻量 binary，执行面）
  - 提供受限 API：shell/file/tool 执行
  - 本地强制应用 `SecurityPolicy`
  - 回传结构化结果与状态

#### 方案 A（推荐）：HTTPS + JSON-RPC 2.0（先落地）
- 传输：HTTPS
- 协议：JSON-RPC 2.0（便于复用现有 signal/wacli 经验）
- 认证：pairing/bearer + 可选 HMAC 请求签名
- 授权：节点侧 tool allowlist + workspace boundary
- 接口建议：
  - `node.ping`
  - `node.exec_shell`
  - `node.read_file`
  - `node.write_file`
  - `node.run_tool`
  - `node.cancel`
  - `node.metrics`

优点：
- 与现有代码风格最贴近（axum + serde + JSON）。
- 研发速度快。

缺点：
- 若不加 mTLS，零信任强度一般。

#### 方案 B：gRPC + mTLS（长期最优）
优点：
- 强类型、双向流、mTLS 原生友好。

缺点：
- 引入新栈，短期改造成本高。

#### 方案 C：复用现有 gateway `/webhook`
优点：
- 最少新模块。

缺点：
- 语义偏 chat webhook，不适合作为标准远程执行面。

### 3. 代码改动清单（文件 + 预估行数）

方案 A（HTTPS + JSON-RPC）预估：**~1400–2400 LoC**

- `src/tools/nodes.rs`：从 stub 改为真实 client（+260~420）
- 新增 `src/nodes/protocol.rs`：RPC 请求/响应模型（+180~320）
- 新增 `src/nodes/client.rs`：Core 侧 RPC client + 重试/超时（+220~360）
- 新增 `src/nodes/server.rs`：node 侧 RPC server（+300~520）
- 新增 `src/bin/zeroclaw-node.rs`：轻量二进制入口（+80~160）
- `src/config/schema.rs`：`[nodes.*]` 补齐 endpoint/auth/tls/allowlist（+160~260）
- `src/security/*`：节点令牌管理、签名校验、审计事件（+140~240）
- `src/runtime/*`（可选）：把 remote 执行抽象为 `RuntimeAdapter` 实现（+120~240）
- 文档（`docs/operations-runbook.md`、`docs/config-reference.md`、`docs/security/*`）（+120~220）

### 4. 依赖关系和实现顺序

1. 定义 `nodes` 协议与配置模型（含认证/授权字段）
2. 先实现 node server 的 `ping/status`
3. 实现 core client + `nodes list/status`
4. 加入 `exec_shell/read/write`，打通最小闭环
5. 接入认证（bearer + HMAC）与审计
6. 加入取消、重试、幂等等可靠性特性
7. 完善 runbook 与安全文档

### 5. 风险点

- 远程命令执行面安全风险高（认证、授权、审计缺一不可）
- 网络不稳定导致幂等与重复执行问题
- 版本兼容（core 与 node 协议版本漂移）
- 节点密钥/令牌泄露后的横向风险

---

## 综合实施计划

### 总体策略

建议按“低风险先行、能力复用、逐步放权”的路线推进：
1. **先做需求 2（多身份）**：低到中风险，快速交付可见价值。
2. **再做需求 1（进程级隔离）**：建立本地强隔离基础。
3. **最后做需求 3（远程代理）**：复用需求 1 的 worker/隔离执行模型外延到远程节点。

### 里程碑计划（建议）

#### Phase 1（1~2 周）：多身份最小可用
- `sessions_spawn(agent=...)`
- `agents` 扩展 identity_dir
- 子会话加载独立 SOUL/AGENTS/MEMORY

交付标准：
- alpha/bravo 两个身份可并行 spawn，提示词与记忆可区分。

#### Phase 2（2~4 周）：本地多进程隔离
- `sessions_spawn mode=process`
- worker 独立 memory/workspace/security
- IPC + run_id 追踪 + 超时/取消

交付标准：
- 子任务崩溃不影响主进程；DB/文件默认不共享（可配置共享策略）。

#### Phase 3（3~6 周）：远程代理闭环
- `zeroclaw-node` binary
- `nodes` 工具真实远程执行（ping/status/exec/read/write）
- bearer + HMAC + 审计

交付标准：
- Core 可安全调度至少 1 台远程 node 执行受限操作。

### 跨需求依赖图（简化）

- 需求 2 -> 无硬依赖，可先做
- 需求 1 -> 需求 3 的理想前置（可复用 worker 执行契约）
- 需求 3 -> 依赖统一协议与安全基线

### 总体风险控制建议

- 安全红线：默认 deny、最小权限、敏感日志脱敏、全链路审计
- 回滚策略：每阶段 feature flag + 可逆提交
- 测试策略：
  - 单测：协议、配置、权限校验
  - 集成：spawn 隔离、节点断连、幂等重试
  - 故障演练：子进程崩溃、网络抖动、token 失效

---

## 结论

- **需求 1**：当前不满足“完全隔离”；必须引入多进程或容器隔离。
- **需求 2**：现有架构已具备 `agents` 与 identity 基础，改造成本可控，建议优先落地。
- **需求 3**：当前 `nodes` 仅存根；建议基于 HTTPS + JSON-RPC 先落地，再演进到更强零信任方案。

