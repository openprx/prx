# PRX 测试审计报告与回归补充计划

> 审计日期: 2026-03-17 | 排除: LLM API 调用
> 目标: 从单点到交叉连续，覆盖多平台兼容性
> 执行方式: `/loop 10m /test-regression` 自主完成

---

## 一、审计总览

| 维度 | 数值 |
|------|------|
| 源码模块 | 17 个 |
| 源码文件 (*.rs) | ~190 个 |
| 现有单元测试 | ~3076 个 |
| 现有集成测试 | 104 个 (13 文件) |
| **零测试文件 (CRITICAL)** | **8 个** |
| **低覆盖文件 (<50%)** | **12 个** |
| 无集成测试的模块 | 23 个 |
| 缺失的跨模块场景 | 10 个 |

---

## 二、零测试文件 (P0 — 必须补充)

| # | 文件 | 行数 | 公共 API | 风险等级 | 说明 |
|---|------|------|----------|----------|------|
| 1 | `tools/canvas.rs` | 254 | 5 action | CRITICAL | 无任何测试 — 安全检查、参数验证、5 种 action 全部缺失 |
| 2 | `tools/tts.rs` | 176 | execute | CRITICAL | 无任何测试 — 语音生成、通道发送、收件人解析 |
| 3 | `gateway/api/ui.rs` | ~200 | 5 handler | CRITICAL | 路径遍历防护 (line 33-37) 零验证 |
| 4 | `plugins/mod.rs` | ~500 | 20+ | CRITICAL | PluginManager 初始化、加载、适配器创建全部缺失 |
| 5 | `router/capability.rs` | ~200 | 9 | CRITICAL | ELO/成功率追踪算法零测试 |
| 6 | `router/history.rs` | ~150 | 4 | HIGH | 模型延迟历史记录零测试 |
| 7 | `router/knn.rs` | ~200 | 5 | HIGH | KNN 搜索排序算法零测试 |
| 8 | `router/models.rs` | ~80 | 1 | MEDIUM | 模型配置加载零测试 |

---

## 三、低覆盖文件 (P1 — 需要扩充)

| # | 文件 | 现有测试 | 目标测试 | 缺失内容 |
|---|------|----------|----------|----------|
| 1 | `tools/nodes.rs` | 2 | 20+ | 6 种 action 路径、网络错误、节点解析 |
| 2 | `tools/mcp.rs` | 3 | 25+ | 工具发现、执行路径、HTTP/stdio 分支、安全拦截 |
| 3 | `tools/subagents.rs` | 4 | 12+ | 过滤组合、输出格式化、边界 (空消息、不存在的 run_id) |
| 4 | `nodes/client.rs` | 1 | 15+ | 熔断器、RPC 调用、健康检查 — 仅 8% 覆盖 |
| 5 | `nodes/protocol.rs` | 1 | 8+ | RPC 消息序列化、错误响应 |
| 6 | `router/elo.rs` | 1 | 8+ | ELO 公式、边界条件 (0/1/极值) |
| 7 | `cron/schedule.rs` | 2 | 10+ | Cron 表达式解析、时区处理、无效表达式 |
| 8 | `hooks/mod.rs` | 3 | 10+ | 子进程错误、超时、SIGTERM |
| 9 | `channels/wacli.rs` | 0 | 15+ | JSON-RPC 2.0、TCP 连接、重连逻辑 |
| 10 | `channels/signal_native.rs` | 2 | 12+ | 守护进程生命周期、端口分配、进程清理 |
| 11 | `channels/whatsapp_storage.rs` | 3 | 20+ | 40+ trait 方法实现 |
| 12 | `channels/whatsapp_web.rs` | 8 | 15+ | Bot 初始化、会话持久化、重连 |

---

## 四、通道模块系统性缺口

所有 22 个通道文件共性缺失:

| 缺失类型 | 覆盖通道数 | 总通道数 | 缺失通道 |
|----------|-----------|----------|----------|
| **重连逻辑测试** | 0 | 21 | 全部 |
| **WebSocket 消息大小限制** | 0 | 4 | discord, lark, dingtalk, qq |
| **速率限制测试** | 1 | 21 | 仅 qq.rs 有去重测试 |
| **Token 刷新/过期** | 0 | 5 | qq, lark, slack, matrix, mattermost |
| **平台兼容性** | 1 | 2 | imessage (macOS only), signal_native (Linux only) |

---

## 五、跨模块集成场景 (P2 — 缺失的端到端链路)

| # | 场景 | 链路 | 现状 | 优先级 |
|---|------|------|------|--------|
| 1 | **Config 热重载 → 通道重启** | config hotreload → arc-swap → channel re-init | 零测试 | HIGH |
| 2 | **Webhook → Agent → Tool → Memory** | gateway → channel → agent.turn → tool.execute → memory.store | 部分 (各段独立测试) | HIGH |
| 3 | **并发 Agent 轮次 + Memory 冲突** | parallel agent.turn() → same memory keys | 仅 memory 并发写测试 | HIGH |
| 4 | **Provider 故障转移链** | primary fail → fallback → tertiary | 零测试 | MEDIUM |
| 5 | **Cron 调度 → 通道投递** | cron due → execute → deliver → telegram/discord | 单元测试覆盖，无集成 | MEDIUM |
| 6 | **Evolution 候选 → Config 热更换** | evolution apply → arc-swap → agents pick up | 部分 (shadow mode 测试) | MEDIUM |
| 7 | **Plugin 加载 → Tool 注册 → Agent 调用** | wasm load → capability registration → agent tool use | 零测试 | HIGH |
| 8 | **Memory 后端切换** | sqlite → postgres mid-session → data migration | 零测试 | MEDIUM |
| 9 | **Agent 历史裁剪** | messages > max_history → oldest pruned | 零测试 | LOW |
| 10 | **Node RPC → 远程执行 → 结果聚合** | client.exec → remote server → result return → agent | 零测试 (client 仅 8%) | HIGH |

---

## 六、平台兼容性缺口

| 模块 | 平台相关代码 | 现有平台测试 | 缺失 |
|------|-------------|-------------|------|
| `security/policy.rs` | 路径验证 | Unix 符号链接测试 | Windows `\` 分隔符、NTFS junction |
| `security/secrets.rs` | 密钥文件权限 | Unix `0o600` 测试 | Windows icacls 执行验证 |
| `security/landlock.rs` | `cfg(target_os="linux")` | 可用性探测 | Linux 功能性规则测试 |
| `security/bubblewrap.rs` | `--tmpfs /tmp` | 标志检查 | 符号链接逃逸验证 |
| `service/mod.rs` | systemd/openrc/launchd | 无 | 各 init 系统安装/启动/停止 |
| `channels/imessage.rs` | `cfg(target_os="macos")` | 无 | macOS AppleScript 调用 |
| `runtime/native.rs` | `sh -c` vs `cmd /c` | 无 | Windows 命令执行 |
| `tools/shell.rs` | PATH 硬编码 | 无 | Windows PATH 替换验证 |

---

## 七、改造执行计划 (Phase 1-5)

### Phase 1: P0 零测试文件 (8 文件, ~120 新测试)

```
Phase1 任务列表:
├── tools/canvas.rs        → 15 tests (5 action × 3: happy/error/edge)
├── tools/tts.rs           → 12 tests (arg validation, security, recipient, channel)
├── gateway/api/ui.rs      → 10 tests (path traversal, MIME, 404, static serve)
├── plugins/mod.rs         → 18 tests (init, load, unload, adapter, metrics)
├── router/capability.rs   → 15 tests (load, update, merge, ELO tracking)
├── router/history.rs      → 10 tests (record, query, concurrent, eviction)
├── router/knn.rs          → 12 tests (search, ranking, empty, distance)
├── router/models.rs       →  5 tests (load, invalid, missing, defaults)
└── channels/wacli.rs      → 15 tests (JSON-RPC, TCP, reconnect, filter)
验证: cargo test --all-features  零失败
```

### Phase 2: P1 低覆盖扩充 (12 文件, ~150 新测试)

```
Phase2 任务列表:
├── tools/nodes.rs              2→20  (+18: 6 actions × 3)
├── tools/mcp.rs                3→25  (+22: discovery, execute, security)
├── tools/subagents.rs          4→12  (+8: filter, format, edge)
├── nodes/client.rs             1→15  (+14: circuit breaker, RPC, health)
├── nodes/protocol.rs           1→8   (+7: serialize, error, roundtrip)
├── router/elo.rs               1→8   (+7: formula, boundary, ranking)
├── cron/schedule.rs            2→10  (+8: parse, timezone, invalid)
├── hooks/mod.rs                3→10  (+7: subprocess, timeout, signal)
├── channels/signal_native.rs   2→12  (+10: daemon, port, cleanup)
├── channels/whatsapp_storage.rs 3→20 (+17: 4 trait impls × 4)
├── channels/whatsapp_web.rs    8→15  (+7: init, session, reconnect)
└── memory/lucid.rs (修复 unwrap + 测试)  6→15 (+9)
验证: cargo test + cargo clippy -D warnings
```

### Phase 3: 通道重连 + WebSocket 限制 (系统性, ~60 新测试)

```
Phase3 任务列表:
├── 创建 tests/channel_reconnection.rs (新集成测试)
│   ├── mock_websocket_server (tokio TCP listener)
│   ├── discord 断连后自动重连
│   ├── lark 心跳超时重连
│   ├── dingtalk WebSocket 错误恢复
│   └── qq 网关刷新
├── WebSocket 消息大小限制 (4 文件)
│   ├── discord.rs  → connect_async_with_config + 2 tests
│   ├── lark.rs     → connect_async_with_config + 2 tests
│   ├── dingtalk.rs → connect_async_with_config + 2 tests
│   └── qq.rs       → connect_async_with_config + 2 tests
└── 通道速率限制测试框架 (5 tests)
验证: cargo test --all-features
```

### Phase 4: 跨模块集成测试 (~40 新测试)

```
Phase4 任务列表:
├── tests/config_hotreload_integration.rs (新)
│   ├── config_change_triggers_memory_restart
│   ├── config_change_triggers_channel_reconnect
│   └── config_change_preserves_running_sessions
├── tests/webhook_to_memory_pipeline.rs (新)
│   ├── webhook_receipt_triggers_agent_turn
│   ├── tool_execution_stores_to_memory
│   └── memory_context_enriches_next_turn
├── tests/plugin_tool_integration.rs (新)
│   ├── wasm_plugin_registers_tool
│   ├── agent_invokes_plugin_tool
│   └── plugin_writes_to_memory
├── tests/node_rpc_integration.rs (新)
│   ├── remote_exec_returns_result
│   ├── circuit_breaker_opens_on_failures
│   └── health_check_recovery
└── tests/concurrent_agent_memory.rs (新)
    ├── parallel_turns_no_data_loss
    ├── concurrent_memory_writes_consistent
    └── session_scoped_isolation
验证: cargo test --all-features
```

### Phase 5: 平台兼容性 + 回归验证 (~30 新测试)

```
Phase5 任务列表:
├── security/policy.rs 补充
│   ├── #[cfg(windows)] windows_path_separator_handling
│   ├── #[cfg(windows)] ntfs_junction_detection
│   └── #[cfg(windows)] unc_path_validation
├── security/secrets.rs 补充
│   └── #[cfg(windows)] icacls_execution_verification
├── runtime/native.rs 补充
│   └── #[cfg(windows)] cmd_c_execution
├── tools/shell.rs 补充
│   └── #[cfg(windows)] windows_safe_path_override
├── service/mod.rs 补充
│   ├── #[cfg(target_os="linux")] systemd_unit_generation
│   └── #[cfg(target_os="macos")] launchd_plist_generation
└── 最终回归验证
    ├── cargo fmt --all -- --check
    ├── cargo clippy --all-features -- -D warnings
    ├── cargo test --all-features
    └── cargo build --release --all-features
```

---

## 八、/loop 自主执行方案

### 脚本设计

每个 Phase 作为独立的 `/loop` 周期，每 10 分钟检查进度:

```
/loop 10m "检查 TEST_AUDIT_REPORT.md Phase 进度:
1. 读取当前 Phase 状态 (Phase1-5)
2. 对于当前 Phase 的下一个未完成文件:
   a. 读取源文件，理解公共 API
   b. 编写测试 (#[test] 或 #[tokio::test])
   c. 运行 cargo test 验证
   d. 如果失败，修复并重试
   e. 通过后，更新进度标记
3. 当前 Phase 全部完成后:
   a. 运行 cargo clippy --all-features -- -D warnings
   b. 运行 cargo test --all-features
   c. 标记 Phase 完成，进入下一 Phase
4. 全部 Phase 完成后停止"
```

### 进度追踪文件

每个 Phase 完成后更新 `TEST_PROGRESS.md`:

```markdown
## Phase 进度

- [ ] Phase 1: P0 零测试文件 (0/8 files, 0/~120 tests)
- [ ] Phase 2: P1 低覆盖扩充 (0/12 files, 0/~150 tests)
- [ ] Phase 3: 通道重连 + WS 限制 (0/5 tasks, 0/~60 tests)
- [ ] Phase 4: 跨模块集成 (0/5 test files, 0/~40 tests)
- [ ] Phase 5: 平台兼容性 (0/6 tasks, 0/~30 tests)

### 验证检查点
- [ ] cargo fmt --check: PASS
- [ ] cargo clippy -D warnings: PASS
- [ ] cargo test --all-features: PASS (当前 3076, 目标 ~3476)
- [ ] 零 unwrap() 新增: VERIFIED
```

---

## 九、验收标准

| 指标 | 当前 | 目标 |
|------|------|------|
| 总测试数 | 3076 | ~3476+ |
| 零测试文件 | 8 | 0 |
| <50% 覆盖文件 | 12 | 0 |
| 集成测试文件 | 13 | 18+ |
| 跨模块场景 | 0 | 5+ |
| 平台条件测试 | 0 | 8+ |
| clippy warnings | 0 | 0 |
| cargo test 失败 | 0 | 0 |
