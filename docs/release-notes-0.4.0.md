# OpenPRX v0.4.0 Release Notes

发布日期: 2026-05-17 (S5 Release Prep)

## 概览

v0.4.0 是首个面向稳定接口契约的版本，重点完成 S5 release prep 阶段的 P0 安全
修复与协议层回归基线。所有 default-feature 路径 (channel-matrix / llm-router /
wasm-plugins) 通过 `cargo audit` 零告警 + `cargo clippy -D warnings` 零警告。

## BREAKING Changes

### `OPENPRX_APPROVAL_OVERRIDE` 默认 fail-safe deny

supervised 模式 `Effect::RequestApproval` 路径在 TUI 卡片 + Y/N 键盘接线 (T5-1
完整版，留 Task #11) 未完成期间，**不再静默 auto-approve**：

| env 值 (大小写不敏感、trim 后) | 行为 (v0.4.0) | 行为 (v0.3.x) |
|------|------|------|
| `allow` / `y` / `yes` / `1` | 允许 | 允许 |
| `deny` / `n` / `no` / `0` / 其他 | 拒绝 | 拒绝 |
| **unset** | **拒绝 (BREAKING)** + tracing::warn | auto-approve |

`autonomy_level=Full` 不走此路径，行为不变。

### `wasmtime` 版本约束 42 → 44

`Cargo.toml` 内 `wasmtime` / `wasmtime-wasi` 版本从 `"42"` 升到 `"44"` 以关闭
14 个 RUSTSEC 告警 (含 2 个 9.0 CRITICAL 沙箱逃逸 RUSTSEC-2026-0095/0096)。
PRX 源码未使用因升级而变更的 API，下游消费者重新构建即可。

## Security

- **RUSTSEC-2026-0085..0096 + 0114 (wasmtime, 14 entries)** 关闭，含 2 个 9.0
  CRITICAL 沙箱逃逸。升级 wasmtime 42.0.1 → 44.0.1。
- **RUSTSEC-2026-0098 / 0099 / 0104 (rustls-webpki, 3 entries)** 关闭。升级
  0.103.10 → 0.103.13。
- **RUSTSEC-2026-0141 (lettre 0.11.19 boring-tls path 9.1 CRITICAL)** PRX 启用
  `rustls-tls` feature，boring-tls 路径不可达。在 `deny.toml` +
  `.cargo/audit.toml` 加 explicit ignore + 注释引向 SECURITY.md。
- **`sec-audit.yml` 阻断门**：`deny / advisories` 矩阵分支移除
  `continue-on-error`，CVE 真正阻断 `main` 合并 (Codex S5 P0-4)。
- 详见 [`SECURITY.md`](../SECURITY.md) "Known Vulnerability Status (v0.4.0)" 章节。

## Testing

### 协议级 PTY 回归 (S5 P0-1)

环境无 ANTHROPIC / OPENAI / GEMINI / COHERE / GROQ / MISTRAL 任意 API key
配置，**真实 API PTY 回归在本版本不可执行**。延后至 **v0.4.1**。

本版本通过 `MockEnvProvider` 在 streaming 路径覆盖三家 provider 协议层细节：

- `OPENPRX_MOCK_SCRIPT` — 完整流式脚本 (JSON 序列化 chunks 列表)
- `OPENPRX_MOCK_PROVIDER_FLAVOR` — `anthropic` / `openai` / `gemini`
- `OPENPRX_MOCK_DELAY_MS_PER_CHUNK` — cancel-mid-stream 测试窗口 (ms)

新增 PTY 测试 (3) + dispatcher 单测 (3)：

| 测试 | 覆盖点 |
|------|--------|
| `s5_release_p0_1_anthropic_full_turn_via_real_path` | 5 delta chunks + final |
| `s5_release_p0_1_openai_tool_call_turn_via_real_path` | tool_call → execute → continue |
| `s5_release_p0_1_gemini_cancel_midstream_via_real_path` | cancel ≤4s |
| `s5_release_p0_1_retryable_http_io_triggers_retry` | StreamError::Io retry 分类 |
| `s5_release_p0_1_context_overflow_triggers_compact` | 三家 provider 措辞识别 |
| `s5_release_p0_1_parallel_tool_calls_serialize` | 并行 ToolCallChunk 数据结构 |

### Pure 模式 100-turn 性能基线 (S5 P0-2)

详见 [`perf-baseline.md`](perf-baseline.md)。

## Roadmap (v0.4.1)

- **T5-1 完整 TUI approval 卡片 + Y/N 键盘接线** — 接通 supervised 模式 UI 后
  `OPENPRX_APPROVAL_OVERRIDE` 不再需要，env 仍保留为 CI/E2E hook
- **真实 API PTY 回归** — 至少覆盖 anthropic + openai + gemini 各一条最小 turn
- **v0.4.0 基线对比** — 性能基线 v0.4.1+ 使用 2x 偏离规则
