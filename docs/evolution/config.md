# Evolution 配置说明

配置文件：`evolution_config.toml`（建议放在 workspace 根目录）。

完整示例：`config/evolution_config.example.toml`。

## runtime

- `mode`: `shadow` / `auto`。
- `storage_dir`: 进化数据目录。
- `batch_size`: JSONL 缓冲批量阈值。
- `poll_interval_secs`: 热重载轮询间隔。

### runtime.retention

- `hot_days`: 热层保留天数。
- `warm_days`: 温层保留天数。
- `cold_days`: 冷层保留天数。

### runtime.data_thresholds

- `decision_log`: 决策日志样本门槛。
- `memory_access`: 记忆访问样本门槛。
- `same_failure`: 同类失败门槛。

## retrieval

- `vector_retrieval_threshold`: 启用向量增强阈值。

### retrieval.score_weights

- `recency`
- `access_freq`
- `category_weight`
- `useful_ratio`
- `source_confidence`

以上权重用于记忆检索打分融合。

## gate

- `min_improvement`: 最小平均提升。
- `max_regression`: 最大可接受回归。
- `max_token_degradation`: 最大可接受 token 劣化。

## memory (L1)

- `max_tokens`: L1 输出 token 上限。

### memory.retrieval_fusion

- `bm25`
- `vector`
- `metadata`

## prompt (L2)

- `mutable_files`: 可变 prompt 文件。
- `immutable_files`: 不可变 prompt 文件。
- `human_approval_severity`: 进入人工审批的严重度阈值。
- `max_rollback_versions`: prompt 历史保留上限。
- `blocked_keywords`: 禁止新增关键词。

## strategy (L3)

- `decision_policy_path`: 策略文件路径。
- `param_mutation_range`: 参数变异幅度。

## rollback

- `max_versions`: 回滚快照保留数。
- `circuit_breaker_threshold`: 熔断触发阈值。
- `cooldown_after_rollback_hours`: 熔断冷却时间。
