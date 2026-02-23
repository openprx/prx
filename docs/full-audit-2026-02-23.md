# ZeroClaw 全面审计报告（2026-02-23）

审计时间：2026-02-23  
审计范围：`/opt/worker/code/agents/zeroclaw` 全仓库静态审计 + 指定命令执行结果

## 1. 总体评分（10分制）

**6.4 / 10**

- 优点：模块化 trait/factory 架构完整，测试总体通过率高，工具面与通道能力覆盖广。
- 主要扣分：远程节点执行面的安全边界不足、`self_system` 未接入主运行链路、文档契约与实现存在漂移、lint 与工程债务较高。

## 2. 基线命令结果（按要求）

1. `cargo clippy --all-targets 2>&1`
- 退出码：0
- 汇总：`lib` 62 warnings，`lib test` 81 warnings，`bin` 62 warnings，`bench` 7 warnings。
- `warning:` 行总数（日志统计）：106。

2. `grep -rn 'unwrap()' src/ | wc -l`
- 结果：**2191**（含大量测试代码）。

3. `grep -rn 'TODO\|FIXME\|HACK' src/`
- `src/config/hotreload.rs:206`
- `src/security/pairing.rs:29`
- `src/security/pairing.rs:156`

4. `cargo test --lib 2>&1`
- 结果：**2665 passed / 3 failed**。
- 失败用例：
  - `onboard::wizard::tests::quick_setup_model_override_persists_to_config_toml`
  - `onboard::wizard::tests::quick_setup_without_model_uses_provider_default_model`
  - `providers::anthropic::tests::chat_with_tools_sends_full_history_and_native_tools`

5. `wc -l src/**/*.rs`
- 结果：**128605 total**。

附加：
- `cargo build --bin zeroclaw-node`：成功（退出码 0）。
- `cargo audit` / `cargo outdated`：本机未安装命令，无法完成自动 CVE/过时依赖扫描。

## 3. 按严重程度分类问题列表

## Critical

### C1. 远程节点 `exec_shell` 未被 `sandbox_root` 真正约束（主机级命令执行面）
- 文件：`src/nodes/server.rs:176`
- 证据：使用 `sh -lc` 执行任意 `cmd`，仅做首 token 黑名单校验（`src/nodes/server.rs:346`），`sandbox_root` 仅用于 `cwd/read/write` 路径解析（`src/nodes/server.rs:372`）。
- 风险：认证成功后可执行任意系统命令，越过“文件路径沙箱”预期。
- 修复建议：
1. 将 `exec_shell` 改为受限执行器（白名单命令 + 参数级校验），禁止通用 `sh -lc`。
2. 默认启用隔离运行时（容器/命名空间/chroot）并强制最小权限。
3. 为 `node.exec_shell` 增加独立策略层（allowlist + 审计 + deny-by-default）。

### C2. 节点传输层默认可明文 HTTP/2，Bearer/HMAC 可在非 TLS 链路传输
- 文件：`src/nodes/transport.rs:50`, `src/nodes/transport.rs:80`, `src/config/schema.rs:376`
- 证据：`endpoint` 未限制 `https://`，请求直接带 `Authorization: Bearer ...`；注释示例也使用 `http://`。
- 风险：在错误部署下导致中间人窃听/重放面扩大。
- 修复建议：
1. 默认拒绝非 `https://` endpoint（仅本地回环可例外）。
2. 增加 `tls_required = true` 默认值与启动时硬失败。
3. 补充证书校验/固定策略（至少支持 pin 或自定义 CA）。

## High

### H1. `self_system` 模块未接入主运行路径（定义多、调用少）
- 文件：`src/lib.rs:69`, `src/self_system/orchestrator.rs:1`
- 证据：仓库中除 `self_system` 目录内引用外，主流程无实际调用；配置根结构亦无 `[self_system]` 字段入口（`src/config/schema.rs:56` 起）。
- 风险：形成“看似存在但实际未生效”的架构空洞，审计与运维认知偏差。
- 修复建议：
1. 明确接入点（daemon/cron/command）并加开关配置。
2. 若暂不启用，改为实验模块并在 docs 明确“未接入”。

### H2. `nodes` 工具提示词契约与真实实现漂移
- 文件：`src/channels/mod.rs:2728`, `src/tools/nodes.rs:109`, `src/tools/nodes.rs:119`
- 证据：提示词仍写“stub + notify/invoke”；真实 schema 已是 `exec/read/write/cancel`。
- 风险：LLM 调用错误 action，造成功能失败与误诊。
- 修复建议：统一 `tool_descs` 与 `parameters_schema`，增加契约一致性测试。

### H3. 配置文档缺失关键新增面（`[nodes]` / `zeroclaw-node` / `sessions_spawn process`）
- 文件：`docs/config-reference.md:1`, `docs/commands-reference.md:1`
- 证据：`docs/config-reference.md` 无 `[nodes]`、`[sessions_spawn]` 条目；`docs/commands-reference.md` 无 `session-worker` 与 `zeroclaw-node` 相关说明。
- 风险：用户配置错误率高，发布契约不完整。
- 修复建议：补齐字段、默认值、迁移说明、最小安全配置示例。

### H4. 单测存在环境耦合失败，影响 CI 稳定性
- 文件：`src/onboard/wizard.rs:4898`, `src/onboard/wizard.rs:4921`, `src/providers/anthropic.rs:1221`
- 证据：`cargo test --lib` 在受限环境下 3 例失败（权限与外部行为耦合）。
- 风险：PR 验证不稳定，回归信号污染。
- 修复建议：
1. 将外部依赖测试隔离为 integration + feature gate。
2. 对 HOME/权限敏感路径做 test sandbox 注入。

## Medium

### M1. `clippy` 告警债务较大，含潜在行为问题
- 文件：`/tmp/zeroclaw-clippy-2026-02-23.log`（汇总）
- 重点示例：
  - `src/nodes/server.rs:192` 等多处截断 cast
  - `src/tools/web_fetch.rs:289` 测试断言恒真表达式
  - 多处 unused import / complexity / derivable impl
- 风险：可维护性下降，真实问题被噪音掩盖。
- 修复建议：先清理 `unused`、`cast`、`overly_complex_bool_expr`、`match_same_arms` 四类高信号告警。

### M2. 生产代码中存在 `unwrap()` 潜在 panic 点（非测试）
- 文件：`src/peripherals/uno_q_setup.rs:53`, `src/peripherals/uno_q_setup.rs:101`, `src/peripherals/nucleo_flash.rs:66`
- 证据：`Path::to_str().unwrap()` 在非 UTF-8 路径上可 panic。
- 风险：边缘环境（非 UTF-8 文件名）进程崩溃。
- 修复建议：改为 `to_string_lossy()` 或显式错误返回。

### M3. 配置热重载“存储已更新”但“运行组件未完全重建”
- 文件：`src/config/hotreload.rs:149`, `src/tools/config_reload.rs:4`, `src/gateway/mod.rs:351`, `src/gateway/mod.rs:455`
- 证据：配置对象可原子替换，但 `SecurityPolicy`、`SessionsSpawnTool` 等在启动时克隆快照，未随热重载重建。
- 风险：用户误以为新配置已生效，实际行为仍旧。
- 修复建议：给每类配置定义“live / restart required”矩阵并在 reload 输出中明确。

### M4. 依赖安全审计链路未在仓库内固化
- 文件：`Cargo.toml:17`
- 证据：本地未具备 `cargo audit`/`cargo outdated`，无法自动输出 CVE 与过时依赖。
- 风险：供应链风险发现滞后。
- 修复建议：在 CI 增加 `cargo-audit` / `cargo-deny` 任务，失败门禁化。

### M5. 测试覆盖存在盲区（启发式）
- 文件：`src/tools/nodes.rs:1`, `src/session_worker/runner.rs:1`, `src/tools/tts.rs:1`, `src/channels/wacli.rs:1`, `src/bin/zeroclaw-node.rs:1`
- 证据：按文件扫描未发现直接测试注解（启发式，不等同精准覆盖率）。
- 风险：关键新路径回归保护不足。
- 修复建议：优先补 node/session_worker/tts 的失败路径和权限边界测试。

## Low

### L1. TODO 技术债
- 文件：`src/config/hotreload.rs:206`, `src/security/pairing.rs:29`, `src/security/pairing.rs:156`
- 建议：转 issue 并标注 owner + 截止版本。

### L2. 文档中存在历史结论与当前实现不一致
- 文件：`docs/three-arch-eval.md:206`
- 证据：文档仍描述 `nodes` 为 stub，但代码已有 `client/server/bin` 闭环。
- 建议：补版本注记，避免误导架构决策。

## 4. 架构完整性专项结论

1. 模块导出与连接
- `nodes`、`session_worker`、`self_system` 均在 `lib` 导出（`src/lib.rs:61`, `src/lib.rs:69`, `src/lib.rs:71`）。
- `session_worker` 子命令已在 `main` 注册并早返回执行（`src/main.rs:396`, `src/main.rs:703`, `src/main.rs:711`）。

2. P1/P2/P3 接入状态
- P1（多身份）：`sessions_spawn` 已支持 `agent`、`memory_scope`、`identity_dir`（`src/tools/sessions_spawn.rs:232`, `src/config/schema.rs:241`）。
- P2（进程隔离）：`sessions_spawn mode=process` + `session-worker` 已打通（`src/tools/sessions_spawn.rs:415`, `src/tools/sessions_spawn.rs:1242`）。
- P3（远程代理）：`nodes` + `zeroclaw-node` 可编译并调用（`src/tools/nodes.rs:109`, `src/bin/zeroclaw-node.rs:81`），但安全边界仍不足（见 Critical）。

3. 指定检查项
- `self_system`：当前“定义为主、接入弱”。
- `session_worker` 子命令：已正确注册。
- `zeroclaw-node`：可独立编译通过。
- `nodes` tool：已在 registry 注册（`src/tools/mod.rs:254`）。
- tool 注册链路：`all_tools_with_runtime` 为主，gateway/channels 会追加 channel-aware 工具（`src/gateway/mod.rs:366`, `src/gateway/mod.rs:425`, `src/gateway/mod.rs:445`）。

## 5. 配置完整性专项结论

1. Default 实现
- `NodesConfig` / `NodeServerConfig` / `SessionsSpawnConfig` 均有默认值实现（`src/config/schema.rs:358`, `src/config/schema.rs:432`, `src/config/schema.rs:474`）。

2. 文档覆盖
- `agents.*` 字段已记录（`docs/config-reference.md:81`）。
- `[nodes]`、`[sessions_spawn]` 缺失（见 H3）。

3. 热重载覆盖
- 配置对象可热替换（`src/config/hotreload.rs:149`）。
- 运行组件是否立即生效取决于组件是否持有 SharedConfig；当前存在快照化组件（见 M3）。

## 6. 安全审计专项结论

1. 认证/授权绕过
- gateway/node 均有认证机制，但 node 执行面默认权限过大（C1/C2）。

2. 路径穿越
- `file_read`/`file_write` 有 canonicalize + workspace 校验（`src/tools/file_read.rs:80`, `src/tools/file_write.rs:95`）。
- node `read/write` 有 sandbox 路径归一化（`src/nodes/server.rs:372`）。

3. shell 注入
- 本地 `shell` 有命令策略校验（`src/tools/shell.rs:77`）。
- 远程 node 仍使用 `sh -lc`，风险较高（C1）。

4. secret 硬编码
- 未发现生产代码硬编码真实密钥；样例/测试中存在 `sk-*` 占位值。

5. sandbox 限制
- 本地默认 runtime 为 `native`（`src/config/schema.rs:2253`），若未启用 docker/平台沙箱则隔离强度有限。

## 7. 功能差距矩阵（OpenClaw vs ZeroClaw）

说明：仓库内未包含 OpenClaw 全量源码，本矩阵基于本仓库文档与兼容注释进行“可证据对比”。

| 能力项 | OpenClaw（从本仓库可见信息） | ZeroClaw 现状 | 结论 |
|---|---|---|---|
| OpenClaw 记忆迁移 | 有（被目标支持） | `migrate openclaw` 已实现（`src/migration.rs:29`） | 等价/已覆盖 |
| OpenClaw identity 格式 | 有 | `identity.format = "openclaw"`（`docs/config-reference.md:186`） | 等价/已覆盖 |
| OpenClaw skills 生态 | 有 | 支持拉取 openclaw skills 仓库（`src/skills/mod.rs:13`） | 等价/已覆盖 |
| 多身份子代理 | 未见明确证据 | 已支持 `agents.*` + `sessions_spawn agent=` | ZeroClaw 独有增强 |
| 本地进程隔离子会话 | 未见明确证据 | 已支持 `session-worker` + process mode | ZeroClaw 独有增强 |
| 远程节点代理（node binary） | 未见明确证据 | 已有 `nodes` + `zeroclaw-node` | ZeroClaw 独有增强（安全待加固） |
| 多通道网关编排 | 未见明确证据 | Telegram/Discord/Signal/WhatsApp 等统一编排 | ZeroClaw 独有增强 |
| 硬件外设工具链 | 未见明确证据 | peripherals/hardware 完整子系统 | ZeroClaw 独有增强 |

当前判定的“OpenClaw 有但 ZeroClaw 没有”：**在本仓库可见证据中未确认到明确项**。如需精确差距，需额外引入 OpenClaw 最新功能清单或源码对照。

## 8. 运维就绪度结论

- 日志：主链路有日志，但热重载“哪些字段生效”提示仍可更明确。
- 错误友好性：大部分工具返回结构化错误；远程节点错误边界需再收敛。
- 配置示例：基础充分，但 nodes/session_worker 文档缺失。
- 部署文档：存在 runbook/troubleshooting，但未覆盖 `zeroclaw-node` 安全基线。

## 9. 推荐下一步优先级

1. **P0 安全修复**：收敛 `node.exec_shell` 权限边界（去 `sh -lc`、强约束执行器、默认 TLS）。
2. **P0 契约修复**：补齐 `docs/config-reference.md` 与 `docs/commands-reference.md` 的 nodes/session_worker/zeroclaw-node。
3. **P1 接入修复**：明确 `self_system` 是正式接入还是实验模块，并与配置/命令对齐。
4. **P1 质量修复**：处理高信号 clippy 告警（unused/cast/逻辑恒真），将 warning 基线压到可控范围。
5. **P1 测试修复**：剥离环境耦合单测，补 node/session_worker/tts/wacli 的失败路径测试。
6. **P2 供应链治理**：在 CI 固化 `cargo-audit`/`cargo-deny`。

