# ZeroClaw 全面审计报告（第三轮，2026-02-23）

审计时间：2026-02-23  
审计范围：`/opt/worker/code/agents/zeroclaw`

## 1. 总体评分 + 三轮趋势

**8.1 / 10**

三轮趋势：
- 第一轮：`6.4 / 10`（`docs/full-audit-2026-02-23.md:8`）
- 第二轮：`7.2 / 10`（`docs/full-audit-2026-02-23-v2.md:8`）
- 第三轮：`8.1 / 10`（本报告）

本轮主要加分项：
- HN1（symlink 沙箱逃逸）已修复，且补了读/写逃逸测试（`src/nodes/server.rs:594`, `src/nodes/server.rs:601`, `src/nodes/server.rs:699`, `src/nodes/server.rs:714`）。
- H3/MN4（文档契约）已补齐：`[nodes]`、`[nodes.server]`、`[sessions_spawn]`、`session-worker`、`zeroclaw-node`（`docs/config-reference.md:257`, `docs/config-reference.md:298`, `docs/config-reference.md:332`, `docs/commands-reference.md:135`, `docs/commands-reference.md:146`）。
- MN1/MN2 已修复：process 模式新增父层 timeout+kill，`--task` 传递原始字符串（`src/tools/sessions_spawn.rs:1195`, `src/tools/sessions_spawn.rs:1207`, `src/tools/sessions_spawn.rs:1214`）。
- H4 已清零：`cargo test` 全量 `0 failed`（2695 单测通过）。

仍扣分项：
- clippy 警告基线无下降（仍为 `108`）。
- 非测试 `unwrap()` 统计较第二轮上升（2194 -> 2208）。
- 新发现 1 个中风险并发/可靠性问题（见第 4 节 MN5）。

## 2. 基线采集（本轮）

按要求命令执行结果：

1. `cargo clippy --all-targets 2>&1 | grep -c 'warning:'`  
结果：`108`

2. `grep -rn 'unwrap()' src/ --include='*.rs' | grep -v '#\[test\]' | grep -v 'tests::' | grep -v '_test.rs' | wc -l`  
结果：`2208`

3. `grep -rn 'TODO\|FIXME\|HACK' src/`  
结果：
- `src/config/hotreload.rs:206`
- `src/security/pairing.rs:29`
- `src/security/pairing.rs:156`

4. `cargo test --lib 2>&1 | tail -5`  
结果：
- `test result: ok. 2680 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 1.77s`

5. `wc -l src/**/*.rs | tail -1`  
结果：`124413 total`

6. `cargo build --bin zeroclaw-node 2>&1 | tail -3`  
结果：
- `warning: zeroclaw (lib) generated 5 warnings ...`
- `Finished dev profile [unoptimized + debuginfo] target(s) in 19.82s`

## 3. 第二轮问题复查状态表

| ID | 第二轮结论 | 第三轮状态 | 核验证据 |
|---|---|---|---|
| HN1 | symlink 沙箱逃逸 | **已修复** | 路径校验改为 canonicalize+祖先校验（`src/nodes/server.rs:594`, `src/nodes/server.rs:601`, `src/nodes/server.rs:613`），并新增 symlink 逃逸测试（`src/nodes/server.rs:699`, `src/nodes/server.rs:714`） |
| H3/MN4 | 文档契约缺失 | **已修复** | 配置文档新增 `[nodes]`、`[nodes.server]`、`[sessions_spawn]`（`docs/config-reference.md:257`, `docs/config-reference.md:298`, `docs/config-reference.md:332`）；命令文档新增 `session-worker`、`zeroclaw-node`（`docs/commands-reference.md:135`, `docs/commands-reference.md:146`） |
| MN1 | process 模式无父层强制回收 | **已修复** | 新增 `wait_with_parent_timeout`，超时 `kill()+wait()`（`src/tools/sessions_spawn.rs:1207`, `src/tools/sessions_spawn.rs:1214`） |
| MN2 | task 参数二次编码 | **已修复** | `--task` 直接传 `manifest.task.clone()`（`src/tools/sessions_spawn.rs:1195`），测试覆盖（`src/tools/sessions_spawn.rs:1914`） |
| H4 | 测试仍失败 | **已修复** | `cargo test`：`2695 passed; 0 failed`；`cargo test --lib`：`2680 passed; 0 failed` |
| MN3 | 测试覆盖不足 | **基本修复** | 增加 nodes 参数失败路径测试（`src/tools/nodes.rs:322`, `src/tools/nodes.rs:338`）、session-worker 参数解析失败测试（`src/session_worker/runner.rs:268`, `src/session_worker/runner.rs:274`）、process timeout 测试（`src/tools/sessions_spawn.rs:1935`） |

## 4. 新发现问题

### Medium

#### MN5. process 模式存在潜在 stdout/stderr 管道阻塞风险
- 位置：`src/tools/sessions_spawn.rs:1297` 之后。
- 现状：先 `child.wait()`，后读取 `stdout/stderr`（`read_to_end`）。
- 风险：若子进程输出超过管道缓冲区，子进程可能阻塞在写端，父进程阻塞在等待退出，直到父层超时触发 kill，表现为“假超时”或吞吐抖动。
- 建议：并发消费 `stdout/stderr`（`tokio::join!` + 异步读取）或改回 `wait_with_output` 并保留父层 watchdog。

### Low

#### LN2. 节点认证与 HMAC 比对未使用常量时间比较
- 位置：`src/nodes/server.rs:335`, `src/nodes/server.rs:372`。
- 现状：Bearer 与 HMAC 使用普通字符串比较。
- 风险：在极端高精度侧信道场景下存在理论 timing leak。
- 建议：统一使用 `constant_time_eq` 做凭据比较。

## 5. 全面扫描结论（安全 / 架构 / 质量 / 测试 / 性能）

- 安全：
  - 认证授权边界存在且默认拒绝：Bearer + 可选 HMAC（`src/nodes/server.rs:325`, `src/nodes/server.rs:342`）。
  - 路径穿越与 symlink 逃逸本轮已封堵（见 HN1）。
  - 未发现新的明显注入路径回归（`node.exec_shell` 仍拒绝 shell 运算符，非 `sh -lc` 路径）。
  - 未发现明显 token 明文日志泄露路径。

- 架构：
  - `nodes`、`sessions_spawn` 已在工具装配链中接入（`src/tools/mod.rs:254`, `src/gateway/mod.rs:445`, `src/channels/mod.rs:3044`）。
  - 对应配置 `Default` 存在且结构完整（`src/config/schema.rs:336`, `src/config/schema.rs:362`, `src/config/schema.rs:510`, `src/config/schema.rs:524`）。
  - 文档契约与实现已明显收敛。

- 质量：
  - clippy warning：`108`（与第二轮持平）。
  - 非测试 `unwrap()`：`2208`（较第二轮 `2194` 上升）。
  - TODO/FIXME/HACK：集中在 hotreload/pairing，数量少但仍存在。

- 测试：
  - 全量 `cargo test` 通过，`0 failed`。
  - 第二轮关注的关键修复点均有对应测试落地。

- 性能与并发：
  - 未发现明确死锁证据，但 process 模式的 I/O 读取顺序存在可触发阻塞的结构性风险（MN5）。

## 6. 与 OpenClaw 功能对比

### 6.1 ZeroClaw 独有能力（当前仓库可证据）

1. 远程节点代理二进制与 RPC 工具链（`nodes` + `zeroclaw-node`）。
2. process 隔离子会话（`sessions_spawn mode=process` + `session-worker` IPC）。
3. 硬件外设系统（`peripherals` trait/模块）。
4. 更激进的资源目标（README 基准声明 `<5MB RAM`、毫秒级启动，见 `README.md:79`）。

### 6.2 已覆盖 OpenClaw 兼容能力

1. memory 迁移：`migrate openclaw`。
2. identity 格式兼容：`openclaw` + `aieos`。
3. OpenClaw skills 仓库对接（可拉取并加载 skills）。

### 6.3 仍缺失/未确认能力

- 基于当前仓库可见证据，**未确认“OpenClaw 已有而 ZeroClaw 缺失”的明确能力项**。  
- 若需严格差距清单，需要引入 OpenClaw 同日期（2026-02-23）功能清单或源码做逐项对照。

## 7. 代码量统计（按模块）

说明：本节使用 `find src -name '*.rs'` 递归统计，口径用于模块化分解。

| 模块 | 行数 |
|---|---:|
| core（agent/providers/config/memory） | 36464 |
| channels | 24781 |
| tools | 24589 |
| nodes | 1367 |
| session_worker | 352 |
| self_system | 2885 |
| peripherals | 1617 |
| security | 5322 |
| runtime | 1283 |
| gateway | 2599 |
| cron | 2373 |
| total（递归） | 129298 |

## 8. 推荐下一步

1. **P0**：修复 MN5（process 模式并发读取 stdout/stderr），并补“高输出压力”回归测试。  
2. **P1**：处理 LN2（认证与 HMAC 改为常量时间比较）。  
3. **P1**：继续压降 clippy（优先未使用导入/变量、可自动修复项）。  
4. **P2**：对非测试 `unwrap()` 做热点治理（优先高风险路径：`security/`、`gateway/`、`tools/`）。
