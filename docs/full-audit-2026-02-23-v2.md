# ZeroClaw 全面审计报告（第二轮，2026-02-23）

审计时间：2026-02-23  
审计范围：`/opt/worker/code/agents/zeroclaw`

## 1. 总体评分（10分制）+ 与上轮对比

**7.2 / 10（上轮 6.4 / 10，+0.8）**

提升点：
- C1 安全收敛已落地：`node.exec_shell` 不再使用 `sh -lc`，改为解析+白名单路径（`src/nodes/server.rs:153`, `src/nodes/server.rs:388`, `src/nodes/server.rs:465`）。
- C2 强制 TLS 基本落地：传输端拒绝远端明文 HTTP，仅允许 `https://` 或 loopback HTTP（`src/nodes/transport.rs:49`）。
- H1 `self_system` 已接入实际运行路径：`daemon` 启动时会拉起 runtime（`src/main.rs:52`, `src/main.rs:888`）。

主要扣分项：
- `cargo test --lib` 仍有 3 个失败用例，H4 未清零。
- 文档契约仍未补齐（`[nodes]`、`[sessions_spawn]`、`session-worker`、`zeroclaw-node`）。
- 新发现 1 个 High 安全边界问题（node 沙箱对 symlink 逃逸防护不足）。
- clippy warning 与 unwrap 基线未下降。

## 2. 基线数据采集结果

按要求执行结果：

1. `cargo clippy --all-targets 2>&1 | grep -c 'warning:'`  
结果：`108`

2. `grep -rn 'unwrap()' src/ --include='*.rs' | grep -v '#\[test\]' | grep -v 'tests::' | grep -v '_test.rs' | wc -l`  
结果：`2194`

3. `grep -rn 'TODO\|FIXME\|HACK' src/`  
结果：
- `src/config/hotreload.rs:206`
- `src/security/pairing.rs:29`
- `src/security/pairing.rs:156`

4. `cargo test --lib 2>&1 | tail -3`  
结果：
- `test result: FAILED. 2672 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.89s`
- `error: test failed, to rerun pass '--lib'`

5. `wc -l src/**/*.rs | tail -1`（使用等价稳定统计）  
结果：`129073 total`

6. `cargo build --bin zeroclaw-node 2>&1 | tail -3`  
结果：
- `warning: zeroclaw (lib) generated 5 warnings ...`
- `Finished dev profile [unoptimized + debuginfo] target(s) in 0.28s`

## 3. 上轮问题复查状态表

| ID | 上轮问题 | 本轮状态 | 证据 |
|---|---|---|---|
| C1 | exec_shell 安全收敛（`sh -lc`） | **已修复** | 已改为 `Command::new(program)` + `args`，无 `sh -lc`（`src/nodes/server.rs:179`, `src/nodes/server.rs:388`） |
| C2 | 强制 TLS | **基本修复** | `validate_endpoint` 拒绝远端 HTTP，仅允许 HTTPS 或 loopback HTTP（`src/nodes/transport.rs:49`） |
| H1 | self_system 接入 | **已修复** | `spawn_self_system_runtime` 已接入 `daemon`（`src/main.rs:52`, `src/main.rs:888`） |
| H2 | nodes 提示词与 API 一致性 | **已修复** | 描述与 schema 均为 `list/status/exec/read/write/cancel`（`src/channels/mod.rs:2728`, `src/tools/nodes.rs:109`, `src/tools/nodes.rs:119`） |
| H3 | 配置文档覆盖 nodes/session_worker | **未修复** | `docs/config-reference.md`、`docs/commands-reference.md` 未见 `[nodes]`/`[sessions_spawn]`/`session-worker`/`zeroclaw-node` 专节 |
| H4 | 测试修复（0 failed） | **未修复** | `cargo test --lib` 仍 3 failed |
| M1 | clippy 告警下降 | **未改善** | 上轮 106，本轮 108（+2） |
| M2 | 非测试 unwrap 下降 | **未改善** | 本轮非测试 `unwrap()` 统计为 2194，未见下降证据 |
| M3 | 热重载改进 | **部分改进** | 已有 `config_reload` 工具与热更说明，但 provider/model 仍提示需重启（`src/config/hotreload.rs:206`, `src/tools/config_reload.rs:10`） |

## 4. 新发现问题（按严重程度）

## High

### HN1. Node 沙箱路径校验未防 symlink 逃逸
- 位置：`src/nodes/server.rs:557`, `src/nodes/server.rs:565`
- 问题：`resolve_sandbox_path` 仅做字符串级 `normalize_path + starts_with`，未 `canonicalize` 最终路径。
- 风险：若沙箱目录内存在指向外部的符号链接，`node.read_file` / `node.write_file` 可越过沙箱边界访问外部文件。
- 建议：
1. 读写前对目标路径（及父目录）做 `canonicalize` 并校验前缀；
2. 对不存在目标的写入路径，先校验其父目录 canonical 路径；
3. 增加 symlink 逃逸单测。

## Medium

### MN1. process 模式对子进程缺少外层硬超时/强制回收
- 位置：`src/tools/sessions_spawn.rs:1274`, `src/tools/sessions_spawn.rs:1281`
- 问题：父进程直接 `wait_with_output()`，未在父层做 kill-after-timeout；若 worker 进程卡死（例如阻塞于初始化），可能长期占用资源。
- 建议：父层增加 watchdog，超时后 `kill()` + 回收。

### MN2. process 模式 `--task` 参数出现 JSON 二次编码
- 位置：`src/tools/sessions_spawn.rs:1255`, `src/tools/sessions_spawn.rs:1261`
- 问题：`task_json = serde_json::to_string(task)` 后作为纯字符串参数传入 `--task`，worker 直接当普通文本使用，可能把引号也带入任务内容。
- 建议：`--task` 直接传原始字符串；JSON 仅用于 stdin manifest。

### MN3. 新增关键路径测试覆盖不均衡
- 位置：`src/tools/nodes.rs`, `src/session_worker/runner.rs`
- 现状：`nodes` 工具执行路径和 `session_worker::runner` 主流程缺少同文件直接测试；而 `sessions_spawn`、`nodes transport/server` 测试相对充分。
- 建议：补至少 3 类失败路径测试：
1. nodes tool 参数/权限拒绝；
2. worker 子进程异常输出解析失败；
3. process 模式超时/清理策略。

### MN4. 文档契约仍与新增能力不对齐
- 位置：`docs/config-reference.md:1`, `docs/commands-reference.md:1`
- 问题：缺 `[nodes]`、`[nodes.server]`、`[sessions_spawn]` 字段说明，以及 `session-worker` 与 `zeroclaw-node` 命令说明。
- 建议：补齐默认值、最小安全配置、迁移与回滚说明。

## Low

### LN1. TODO/HACK 技术债仍在关键模块
- `src/config/hotreload.rs:206`
- `src/security/pairing.rs:29`
- `src/security/pairing.rs:156`

## 5. 新问题扫描结论（P1/P2/P3）

- P1（多身份）总体评价：功能链路完整（agent 选择、memory_scope、identity_dir），错误处理整体明确。
- P2（进程隔离）总体评价：功能可用，但当前更接近“进程分离”而非“强隔离”，缺少父层强制回收与更严边界控制。
- P3（远程代理）总体评价：相较上轮明显加固（TLS/命令解析/白名单），但 symlink 沙箱边界仍需修补。

安全边界专项：
- 路径穿越：普通 `..` 已拦截，但 symlink 逃逸仍有缺口（HN1）。
- 注入：node 侧已禁止常见 shell 操作符，且不走 shell 解释器。
- 认证绕过：Bearer + 可选 HMAC 存在，未见直接绕过路径。

## 6. 功能完整性检查

1. 所有 tool 是否在 registry 注册  
结论：**主链路已注册，部分工具按运行模式动态注册**。  
- 基础 registry：`src/tools/mod.rs:229` 起。  
- channel/gateway 追加注册：`src/channels/mod.rs:3017`, `src/gateway/mod.rs:425`。  

2. 所有子命令是否在 `main.rs` 注册  
结论：**已覆盖**（含 `SessionWorker`）。  
- 定义：`src/main.rs:202`  
- 执行分支：`src/main.rs:767`, `src/main.rs:854`

3. 所有配置字段是否有 `Default`  
结论：**本轮新增核心字段有默认值**（`nodes`/`node server`/`sessions_spawn`/`self_system`）。  
证据：`src/config/schema.rs:362`, `src/config/schema.rs:469`, `src/config/schema.rs:524`, `src/config/schema.rs:560`

4. 所有新增 struct 是否有 `Serialize/Deserialize`  
结论：**协议/配置结构体基本具备；运行时内部结构体并非全部序列化**（如 `SubAgentRun`、`HistoryEntry`）。  
评估：若要求“所有 struct 必须可序列化”，当前不满足；若按“跨边界数据结构必须可序列化”，当前满足。

## 7. 代码量统计（按模块）

总计（`src/**/*.rs`）：**129073 行**

| 模块 | 行数 |
|---|---:|
| 核心（agent/provider/config/memory） | 36464 |
| 通道（channels/） | 24781 |
| 工具（tools/） | 24475 |
| 节点（nodes/） | 1281 |
| 会话隔离（session_worker/） | 331 |
| 自我系统（self_system/） | 2885 |
| 外设（peripherals/） | 1617 |
| 安全（security/） | 5322 |
| 其他 | 31917 |

## 8. 推荐优先级

1. **P0（本周）**：修复 `nodes` symlink 沙箱逃逸（HN1），补边界测试。  
2. **P0（本周）**：补文档契约（H3/MN4）：`config-reference` + `commands-reference` 覆盖 nodes/session_worker。  
3. **P1**：修复 process 模式父层超时回收（MN1）与 task 参数二次编码（MN2）。  
4. **P1**：补 `nodes tool` + `session_worker runner` 失败路径测试（MN3）。  
5. **P2**：继续压降 clippy/unwrap 基线，并把热重载“需重启项”输出成明确矩阵。

