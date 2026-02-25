# Evolution 模块总览

本目录描述 OpenPRX（ZeroClaw）自进化系统（`src/self_system/evolution/`）的运行结构、数据流与日常运维入口。

## 目标

- 在安全边界内持续优化 Memory / Prompt / Strategy 三层行为。
- 通过 `record -> analyze -> gate -> judge -> evolve -> rollback` 闭环降低退化风险。
- 提供可观测的仪表盘、历史追踪与人工触发能力。

## 模块关系（ASCII）

```text
Runtime / CLI
   |
   v
scheduler ----> pipeline ------------------------------+
   |              |                                    |
   |              +-> analyzer (daily digest/trend)    |
   |              +-> gate (门禁阈值)                  |
   |              +-> judge (质量评分)                 |
   |              +-> rollback / circuit breaker        |
   |              +-> engine (L1/L2/L3)                |
   |                                                    |
   +---------------------> storage (JSONL hot/warm/cold)
                          |
                          +-> index (JSONL -> SQLite)
                          +-> annotation (was_useful 推断)
```

## 子模块职责

- `record.rs`: 统一日志结构（Decision/MemoryAccess/Evolution）。
- `storage.rs`: JSONL 分层写入与保留策略。
- `analyzer.rs`: 日摘要与三日趋势，生成候选项。
- `gate.rs`: 候选门禁（改进、回归、token 劣化）。
- `judge.rs`: 结构化评分与漂移监控。
- `memory_evolution.rs`: L1 记忆策略进化引擎。
- `prompt_evolution.rs`: L2 Prompt 变异与安全校验。
- `strategy_evolution.rs`: L3 策略参数进化。
- `pipeline.rs`: 分层进化主流程。
- `scheduler.rs`: 定时调度与冻结窗口协调。
- `rollback.rs`: 快照回滚 + 熔断器。
- `index.rs`: JSONL 导入 SQLite 与检索接口。
- `annotation.rs`: `was_useful` 自动标注管线。

## CLI 入口

使用：

- `zeroclaw evolution status`
- `zeroclaw evolution history --limit 20`
- `zeroclaw evolution digest --date 2026-02-24`
- `zeroclaw evolution config`
- `zeroclaw evolution trigger --layer L1`

所有子命令支持 `--json` 输出。
