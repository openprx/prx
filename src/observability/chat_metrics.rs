//! S2.5 T2.5-2: Prometheus metrics for the chat path, plus a tracing helper.
//!
//! Metrics:
//! - `prx_chat_actions_total{action_kind}`   Action count per dispatch, by kind
//! - `prx_chat_effects_total{effect_kind}`   Effect execution count, by kind
//! - `prx_chat_stream_chunks_total`          cumulative stream chunk count
//! - `prx_chat_dispatch_drops_total{reason}` try_dispatch failures, by reason (P1-A)
//!
//! These live in their own Registry rather than the `PrometheusObserver` one,
//! so the existing observer wiring stays untouched. Registration cannot
//! realistically fail (names and labels are compile-time constants); if it
//! does, the metric degrades silently to `None`, the counter helpers become
//! no-ops, and callers never have to handle a `Result`.

use prometheus::{IntCounter, IntCounterVec, Opts, Registry};
use std::sync::LazyLock;

static CHAT_REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

/// Build an `IntCounterVec` and try to register it; `None` on failure, so the
/// production path never panics.
fn try_build_counter_vec(name: &str, help: &str, labels: &[&str]) -> Option<IntCounterVec> {
    let opts = Opts::new(name, help);
    let metric = IntCounterVec::new(opts, labels).ok()?;
    let _ = CHAT_REGISTRY.register(Box::new(metric.clone()));
    Some(metric)
}

/// Build an `IntCounter` and try to register it; `None` on failure.
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

/// Bump the chat Action counter; a no-op when registration failed.
pub fn inc_action(kind: &str) {
    if let Some(m) = ACTIONS_TOTAL.as_ref() {
        m.with_label_values(&[kind]).inc();
    }
}

/// Bump the chat Effect counter.
pub fn inc_effect(kind: &str) {
    if let Some(m) = EFFECTS_TOTAL.as_ref() {
        m.with_label_values(&[kind]).inc();
    }
}

/// Bump the stream chunk counter.
pub fn inc_stream_chunk() {
    if let Some(m) = STREAM_CHUNKS_TOTAL.as_ref() {
        m.inc();
    }
}

/// Bump the dispatch-drop counter; called by `dispatch_or_log` (P1-A).
pub fn inc_dispatch_drop(reason: &str) {
    if let Some(m) = DISPATCH_DROPS_TOTAL.as_ref() {
        m.with_label_values(&[reason]).inc();
    }
}

/// Read the chat Action counter (tests only): the running total for `kind`,
/// or 0 when registration failed.
#[cfg(test)]
#[must_use]
pub fn get_action_count(kind: &str) -> u64 {
    ACTIONS_TOTAL
        .as_ref()
        .map(|m| m.with_label_values(&[kind]).get())
        .unwrap_or_default()
}

/// Read the chat Effect counter (tests only).
#[cfg(test)]
#[must_use]
pub fn get_effect_count(kind: &str) -> u64 {
    EFFECTS_TOTAL
        .as_ref()
        .map(|m| m.with_label_values(&[kind]).get())
        .unwrap_or_default()
}

/// Read the cumulative stream chunk counter (tests only).
#[cfg(test)]
#[must_use]
pub fn get_stream_chunks_count() -> u64 {
    STREAM_CHUNKS_TOTAL.as_ref().map(IntCounter::get).unwrap_or_default()
}

/// Read the dispatch-drop counter (tests only, P1-A verification).
#[cfg(test)]
#[must_use]
pub fn get_dispatch_drops_count(reason: &str) -> u64 {
    DISPATCH_DROPS_TOTAL
        .as_ref()
        .map(|m| m.with_label_values(&[reason]).get())
        .unwrap_or_default()
}

/// The chat module's own Registry. The gateway's /metrics handler merges it
/// with the PrometheusObserver registry for exposition (S2.5 P1-A).
#[must_use]
pub fn chat_registry() -> &'static Registry {
    &CHAT_REGISTRY
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S2.5 T2.5-2: actions_total increments correctly for a single kind.
    #[test]
    fn s2_5_t2_5_2_dispatch_metrics_increment() {
        let kind = "s2_5_test_action";
        let before = get_action_count(kind);
        inc_action(kind);
        inc_action(kind);
        let after = get_action_count(kind);
        assert_eq!(after - before, 2);
    }

    /// S2.5 T2.5-2: stream_chunks_total goes up by one per chunk.
    #[test]
    fn s2_5_t2_5_2_stream_chunks_metric_per_chunk() {
        let before = get_stream_chunks_count();
        inc_stream_chunk();
        inc_stream_chunk();
        inc_stream_chunk();
        let after = get_stream_chunks_count();
        assert_eq!(after - before, 3);
    }

    /// S2.5 T2.5-2: effects_total counts each kind label independently.
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
