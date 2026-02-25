# Evolution 运维手册

## 1. Shadow -> Auto 切换流程

1. 在 `shadow` 模式运行至少 3~7 天，观察 `evolution history` 与 `digest`。
2. 确认 Gate 拒绝率、Judge 评分、回滚频率在可接受区间。
3. 更新 `evolution_config.toml`：`runtime.mode = "auto"`。
4. 执行一次手动触发：`zeroclaw evolution trigger --layer L1`。
5. 观察 `status`（熔断状态/最近周期结果）后再逐层放开 L2/L3。

## 2. 故障排查

### 2.1 熔断恢复

1. 查看 `zeroclaw evolution status` 中 `CircuitBreaker` 状态。
2. 若为 `open`，等待 `rollback.cooldown_after_rollback_hours` 后再次触发。
3. 用 `history` 定位连续失败原因（`result=regressed/rejected` 与 trigger_reason）。

### 2.2 手动回滚

1. 确认目标文件对应的快照目录（`.evolution/rollback/<layer>/`）。
2. 选定版本后执行回滚（或通过调试脚本/调用 `RollbackManager`）。
3. 回滚后先切回 `shadow` 观察，再恢复 `auto`。

### 2.3 日志清理

1. 正常情况下依赖 retention 自动分层与淘汰。
2. 如需手工清理，仅删除过期 `cold` 日志，避免删除最新 `hot`。
3. 若启用 SQLite 索引，清理 JSONL 后需重新 `import_incremental` 同步索引。

## 3. 冷启动期说明

- 冷启动样本不足时，候选质量和稳定性都会偏低。
- 建议最少达到：
  - DecisionLog >= 800
  - MemoryAccess >= 200
- 冷启动期优先 `shadow`，并关注 `digest` 的 `unknown_annotation_ratio`。
- 可结合 `annotation` 管线补齐 `was_useful`，降低噪声记忆影响。
