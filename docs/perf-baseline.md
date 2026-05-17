# OpenPRX Performance Baseline

本文档记录 OpenPRX Pure 模式 100-turn 基线性能数字，作为 v0.4.0 首版基准。

## 度量方法

- 测试入口：`tests/chat_perf_baseline.rs::s5_release_p0_2_pure_perf_baseline`
- Provider: `MockEnvProvider` (test-mock feature) + `OPENPRX_MOCK_RESPONSE=mock8byt`
- 度量项：
- **M1 chunk→snapshot**: stream 每个 chunk 从 `next().await` 到消费完的耗时
- **M2 end-to-end-turn**: 一次完整 stream call 的总耗时 (含 stream 构造 + 所有 chunk)
- **M3 peak RSS delta**: Linux `/proc/self/status` 的 `VmHWM` 100 turn 前后差值
- 工具：无外部 crate (Codex 铁律 9，禁 criterion/wiremock/mockall)
- 阈值规则：v0.4.0 仅 sanity ceiling；v0.4.1+ 使用 2x 偏离规则与此基线对比

## v0.4.0 基线 (N=100 turns)

| 度量 | p50 | p95 | p99 |
|------|-----|-----|-----|
| chunk→snapshot | 50ns | 60ns | 80ns |
| end-to-end-turn | 481ns | 541ns | 4.709µs |

**RSS** (Linux only):
- 起始 VmHWM: 8292 KB
- 终态 VmHWM: 9396 KB
- delta: 1104 KB

## Sanity Ceilings (v0.4.0)

- `p99 chunk→snapshot < 50ms` — mock provider 无网络，超过此值表明 dispatcher
或 reducer 有不该有的同步阻塞
- `p99 end-to-end-turn < 500ms` — 100 turn 无网络下应远低于此值
- `peak_rss_delta < 100MB` — 100 turn 后内存增长 ≥100MB 表明潜在泄漏

## v0.4.1+ 对比规则

后续版本以本基线为参照，p99 / RSS delta 任一项偏离 >2x 则视为回归，
需在 PR 说明中给出原因或修复。
