//! S2.5 T2.5-2: chat 路径专用 Prometheus 指标 + tracing helper.
//!
//! 指标:
//! - `prx_chat_actions_total{action_kind}`   每个 dispatch 的 Action 类型计数
//! - `prx_chat_effects_total{effect_kind}`   每个 Effect 执行的类型计数
//! - `prx_chat_stream_chunks_total`          stream 累计 chunk 计数
//! - `prx_chat_dispatch_drops_total{reason}` try_dispatch 失败原因（P1-A 预留）
//!
//! 使用独立 Registry（不与 `PrometheusObserver` 的 Registry 合并），避免改动现有
//! observer 接线。注册失败（理论上不会发生，名字与 label 都是 compile-time-constant）
//! 时静默降级为 None，counter helper 走 no-op；调用方无需处理 Result。

use prometheus::{IntCounter, IntCounterVec, Opts, Registry};
use std::sync::LazyLock;

static CHAT_REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

/// 构造 `IntCounterVec` 并尝试注册；失败返回 `None`（避免在生产路径 panic）.
fn try_build_counter_vec(name: &str, help: &str, labels: &[&str]) -> Option<IntCounterVec> {
    let opts = Opts::new(name, help);
    let metric = IntCounterVec::new(opts, labels).ok()?;
    let _ = CHAT_REGISTRY.register(Box::new(metric.clone()));
    Some(metric)
}

/// 构造 `IntCounter` 并尝试注册；失败返回 `None`。
fn try_build_counter(name: &str, help: &str) -> Option<IntCounter> {
    let metric = IntCounter::new(name, help).ok()?;
    let _ = CHAT_REGISTRY.register(Box::new(metric.clone()));
    Some(metric)
}

static ACTIONS_TOTAL: LazyLock<Option<IntCounterVec>> = LazyLock::new(|| {
    try_build_counter_vec(
        "prx_chat_actions_total",
        "Chat reducer Action dispatch count by kind",
        &["action_kind"],
    )
});

static EFFECTS_TOTAL: LazyLock<Option<IntCounterVec>> = LazyLock::new(|| {
    try_build_counter_vec(
        "prx_chat_effects_total",
        "Chat EffectExecutor effect execution count by kind",
        &["effect_kind"],
    )
});

static STREAM_CHUNKS_TOTAL: LazyLock<Option<IntCounter>> = LazyLock::new(|| {
    try_build_counter(
        "prx_chat_stream_chunks_total",
        "Chat stream chunks pushed through StreamBoundaryBuffer",
    )
});

static DISPATCH_DROPS_TOTAL: LazyLock<Option<IntCounterVec>> = LazyLock::new(|| {
    try_build_counter_vec(
        "prx_chat_dispatch_drops_total",
        "Chat try_dispatch failures by reason (P1-A)",
        &["reason"],
    )
});

/// 累加 chat Action 计数指标（注册失败时静默 no-op）.
pub fn inc_action(kind: &str) {
    if let Some(m) = ACTIONS_TOTAL.as_ref() {
        m.with_label_values(&[kind]).inc();
    }
}

/// 累加 chat Effect 计数指标。
pub fn inc_effect(kind: &str) {
    if let Some(m) = EFFECTS_TOTAL.as_ref() {
        m.with_label_values(&[kind]).inc();
    }
}

/// 累加 stream chunk 计数指标。
pub fn inc_stream_chunk() {
    if let Some(m) = STREAM_CHUNKS_TOTAL.as_ref() {
        m.inc();
    }
}

/// 累加 dispatch drop 计数指标（P1-A 由 dispatch_or_log 调用）.
pub fn inc_dispatch_drop(reason: &str) {
    if let Some(m) = DISPATCH_DROPS_TOTAL.as_ref() {
        m.with_label_values(&[reason]).inc();
    }
}

/// 读 chat Action 计数（测试用：返回当前 kind 的累计值，注册失败时返回 0）.
#[cfg(test)]
#[must_use]
pub fn get_action_count(kind: &str) -> u64 {
    ACTIONS_TOTAL
        .as_ref()
        .map(|m| m.with_label_values(&[kind]).get())
        .unwrap_or_default()
}

/// 读 chat Effect 计数（测试用）.
#[cfg(test)]
#[must_use]
pub fn get_effect_count(kind: &str) -> u64 {
    EFFECTS_TOTAL
        .as_ref()
        .map(|m| m.with_label_values(&[kind]).get())
        .unwrap_or_default()
}

/// 读 stream chunk 累计（测试用）.
#[cfg(test)]
#[must_use]
pub fn get_stream_chunks_count() -> u64 {
    STREAM_CHUNKS_TOTAL.as_ref().map(IntCounter::get).unwrap_or_default()
}

/// 读 dispatch drops 计数（测试用，P1-A 验证）.
#[cfg(test)]
#[must_use]
pub fn get_dispatch_drops_count(reason: &str) -> u64 {
    DISPATCH_DROPS_TOTAL
        .as_ref()
        .map(|m| m.with_label_values(&[reason]).get())
        .unwrap_or_default()
}

/// 取 chat 模块独立 Registry 引用（暴露给 /metrics 端点合并使用，当前不接线）。
#[must_use]
pub fn chat_registry() -> &'static Registry {
    &CHAT_REGISTRY
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S2.5 T2.5-2: actions_total 计数递增正确（单 kind）.
    #[test]
    fn s2_5_t2_5_2_dispatch_metrics_increment() {
        let kind = "s2_5_test_action";
        let before = get_action_count(kind);
        inc_action(kind);
        inc_action(kind);
        let after = get_action_count(kind);
        assert_eq!(after - before, 2);
    }

    /// S2.5 T2.5-2: stream_chunks_total 单 chunk 递增 1.
    #[test]
    fn s2_5_t2_5_2_stream_chunks_metric_per_chunk() {
        let before = get_stream_chunks_count();
        inc_stream_chunk();
        inc_stream_chunk();
        inc_stream_chunk();
        let after = get_stream_chunks_count();
        assert_eq!(after - before, 3);
    }

    /// S2.5 T2.5-2: effects_total 不同 kind 标签独立计数.
    #[test]
    fn s2_5_t2_5_2_effect_metrics_per_kind() {
        let k1 = "s2_5_RequestRedraw_test";
        let k2 = "s2_5_SaveSession_test";
        let before_k1 = get_effect_count(k1);
        let before_k2 = get_effect_count(k2);
        inc_effect(k1);
        inc_effect(k2);
        inc_effect(k2);
        let after_k1 = get_effect_count(k1);
        let after_k2 = get_effect_count(k2);
        assert_eq!(after_k1 - before_k1, 1, "{k1} should be incremented once");
        assert_eq!(after_k2 - before_k2, 2, "{k2} should be incremented twice");
    }
}
