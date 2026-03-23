use super::traits::{Observer, ObserverEvent, ObserverMetric};
use prometheus::{Encoder, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounterVec, Registry, TextEncoder};

/// Prometheus-backed observer — exposes metrics for scraping via `/metrics`.
pub struct PrometheusObserver {
    registry: Registry,

    // Counters
    agent_starts: IntCounterVec,
    tool_calls: IntCounterVec,
    tool_batches: IntCounterVec,
    tool_timeouts: IntCounterVec,
    tool_cancellations: IntCounterVec,
    tool_degrades: IntCounterVec,
    tool_rollbacks: IntCounterVec,
    channel_messages: IntCounterVec,
    heartbeat_ticks: prometheus::IntCounter,
    errors: IntCounterVec,

    // Histograms
    agent_duration: HistogramVec,
    tool_duration: HistogramVec,
    request_latency: Histogram,

    // Gauges
    tokens_used: prometheus::IntGauge,
    active_sessions: GaugeVec,
    queue_depth: GaugeVec,

    // CTE metrics
    cte_runs: IntCounterVec,
    cte_extra_latency: Histogram,
}

impl PrometheusObserver {
    pub fn try_new() -> anyhow::Result<Self> {
        let registry = Registry::new();

        let agent_starts = IntCounterVec::new(
            prometheus::Opts::new("prx_agent_starts_total", "Total agent invocations"),
            &["provider", "model"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create agent_starts metric: {e}"))?;

        let tool_calls = IntCounterVec::new(
            prometheus::Opts::new("prx_tool_calls_total", "Total tool calls"),
            &["tool", "success"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create tool_calls metric: {e}"))?;
        let tool_batches = IntCounterVec::new(
            prometheus::Opts::new("prx_tool_batches_total", "Total read-only tool batches"),
            &["rollout_stage"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create tool_batches metric: {e}"))?;
        let tool_timeouts = IntCounterVec::new(
            prometheus::Opts::new("prx_tool_timeouts_total", "Total tool timeouts"),
            &["rollout_stage"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create tool_timeouts metric: {e}"))?;
        let tool_cancellations = IntCounterVec::new(
            prometheus::Opts::new("prx_tool_cancellations_total", "Total tool cancellations"),
            &["rollout_stage"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create tool_cancellations metric: {e}"))?;
        let tool_degrades = IntCounterVec::new(
            prometheus::Opts::new("prx_tool_degrades_total", "Total scheduler degradations"),
            &["rollout_stage"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create tool_degrades metric: {e}"))?;
        let tool_rollbacks = IntCounterVec::new(
            prometheus::Opts::new("prx_tool_rollbacks_total", "Total scheduler rollbacks"),
            &["rollout_stage"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create tool_rollbacks metric: {e}"))?;

        let channel_messages = IntCounterVec::new(
            prometheus::Opts::new("prx_channel_messages_total", "Total channel messages"),
            &["channel", "direction"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create channel_messages metric: {e}"))?;

        let heartbeat_ticks = prometheus::IntCounter::new("prx_heartbeat_ticks_total", "Total heartbeat ticks")
            .map_err(|e| anyhow::anyhow!("failed to create heartbeat_ticks metric: {e}"))?;

        let errors = IntCounterVec::new(
            prometheus::Opts::new("prx_errors_total", "Total errors by component"),
            &["component"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create errors metric: {e}"))?;

        let agent_duration = HistogramVec::new(
            HistogramOpts::new("prx_agent_duration_seconds", "Agent invocation duration in seconds")
                .buckets(vec![0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]),
            &["provider", "model"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create agent_duration metric: {e}"))?;

        let tool_duration = HistogramVec::new(
            HistogramOpts::new("prx_tool_duration_seconds", "Tool execution duration in seconds")
                .buckets(vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0]),
            &["tool"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create tool_duration metric: {e}"))?;

        let request_latency = Histogram::with_opts(
            HistogramOpts::new("prx_request_latency_seconds", "Request latency in seconds")
                .buckets(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]),
        )
        .map_err(|e| anyhow::anyhow!("failed to create request_latency metric: {e}"))?;

        let tokens_used = prometheus::IntGauge::new("prx_tokens_used_last", "Tokens used in the last request")
            .map_err(|e| anyhow::anyhow!("failed to create tokens_used metric: {e}"))?;

        let active_sessions = GaugeVec::new(
            prometheus::Opts::new("prx_active_sessions", "Number of active sessions"),
            &[],
        )
        .map_err(|e| anyhow::anyhow!("failed to create active_sessions metric: {e}"))?;

        let queue_depth = GaugeVec::new(prometheus::Opts::new("prx_queue_depth", "Message queue depth"), &[])
            .map_err(|e| anyhow::anyhow!("failed to create queue_depth metric: {e}"))?;

        let cte_runs = IntCounterVec::new(
            prometheus::Opts::new("prx_cte_runs_total", "Total CTE pipeline runs"),
            &["commit_succeeded", "circuit_breaker_tripped"],
        )
        .expect("valid metric");

        let cte_extra_latency = Histogram::with_opts(
            HistogramOpts::new(
                "prx_cte_extra_latency_seconds",
                "Extra latency from CTE pipeline in seconds",
            )
            .buckets(vec![0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5]),
        )
        .expect("valid metric");

        // Register all metrics
        registry.register(Box::new(cte_runs.clone())).ok();
        registry.register(Box::new(cte_extra_latency.clone())).ok();
        registry.register(Box::new(agent_starts.clone())).ok();
        registry.register(Box::new(tool_calls.clone())).ok();
        registry.register(Box::new(tool_batches.clone())).ok();
        registry.register(Box::new(tool_timeouts.clone())).ok();
        registry.register(Box::new(tool_cancellations.clone())).ok();
        registry.register(Box::new(tool_degrades.clone())).ok();
        registry.register(Box::new(tool_rollbacks.clone())).ok();
        registry.register(Box::new(channel_messages.clone())).ok();
        registry.register(Box::new(heartbeat_ticks.clone())).ok();
        registry.register(Box::new(errors.clone())).ok();
        registry.register(Box::new(agent_duration.clone())).ok();
        registry.register(Box::new(tool_duration.clone())).ok();
        registry.register(Box::new(request_latency.clone())).ok();
        registry.register(Box::new(tokens_used.clone())).ok();
        registry.register(Box::new(active_sessions.clone())).ok();
        registry.register(Box::new(queue_depth.clone())).ok();

        Ok(Self {
            registry,
            agent_starts,
            tool_calls,
            tool_batches,
            tool_timeouts,
            tool_cancellations,
            tool_degrades,
            tool_rollbacks,
            channel_messages,
            heartbeat_ticks,
            errors,
            agent_duration,
            tool_duration,
            request_latency,
            tokens_used,
            active_sessions,
            queue_depth,
            cte_runs,
            cte_extra_latency,
        })
    }

    /// Encode all registered metrics into Prometheus text exposition format.
    pub fn encode(&self) -> String {
        let encoder = TextEncoder::new();
        let families = self.registry.gather();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf).unwrap_or_default();
        String::from_utf8(buf).unwrap_or_default()
    }
}

impl Observer for PrometheusObserver {
    fn record_event(&self, event: &ObserverEvent) {
        match event {
            ObserverEvent::AgentStart { provider, model } => {
                self.agent_starts.with_label_values(&[provider, model]).inc();
            }
            ObserverEvent::AgentEnd {
                provider,
                model,
                duration,
                tokens_used,
                cost_usd: _,
            } => {
                // Agent duration is recorded via the histogram with provider/model labels
                self.agent_duration
                    .with_label_values(&[provider, model])
                    .observe(duration.as_secs_f64());
                if let Some(t) = tokens_used {
                    self.tokens_used.set(i64::try_from(*t).unwrap_or(i64::MAX));
                }
            }
            ObserverEvent::ToolCallStart { tool: _ }
            | ObserverEvent::TurnComplete
            | ObserverEvent::LlmRequest { .. }
            | ObserverEvent::LlmResponse { .. } => {}
            ObserverEvent::ToolCall {
                tool,
                duration,
                success,
            } => {
                let success_str = if *success { "true" } else { "false" };
                self.tool_calls.with_label_values(&[tool.as_str(), success_str]).inc();
                self.tool_duration
                    .with_label_values(&[tool.as_str()])
                    .observe(duration.as_secs_f64());
            }
            ObserverEvent::ToolBatch {
                rollout_stage,
                timeout_count,
                cancel_count,
                degraded,
                rollback,
                ..
            } => {
                self.tool_batches.with_label_values(&[rollout_stage.as_str()]).inc();
                self.tool_timeouts
                    .with_label_values(&[rollout_stage.as_str()])
                    .inc_by(u64::try_from(*timeout_count).unwrap_or(u64::MAX));
                self.tool_cancellations
                    .with_label_values(&[rollout_stage.as_str()])
                    .inc_by(u64::try_from(*cancel_count).unwrap_or(u64::MAX));
                if *degraded {
                    self.tool_degrades.with_label_values(&[rollout_stage.as_str()]).inc();
                }
                if *rollback {
                    self.tool_rollbacks.with_label_values(&[rollout_stage.as_str()]).inc();
                }
            }
            ObserverEvent::ChannelMessage { channel, direction } => {
                self.channel_messages.with_label_values(&[channel, direction]).inc();
            }
            ObserverEvent::HeartbeatTick => {
                self.heartbeat_ticks.inc();
            }
            ObserverEvent::Error { component, message: _ } => {
                self.errors.with_label_values(&[component]).inc();
            }
            ObserverEvent::CteRun {
                commit_succeeded,
                circuit_breaker_tripped,
                ..
            } => {
                let committed = if *commit_succeeded { "true" } else { "false" };
                let cb_tripped = if *circuit_breaker_tripped { "true" } else { "false" };
                self.cte_runs.with_label_values(&[committed, cb_tripped]).inc();
                // Latency is recorded via record_metric(CteExtraLatency) to avoid double-counting.
            }
        }
    }

    fn record_metric(&self, metric: &ObserverMetric) {
        match metric {
            ObserverMetric::RequestLatency(d) => {
                self.request_latency.observe(d.as_secs_f64());
            }
            ObserverMetric::TokensUsed(t) => {
                self.tokens_used.set(i64::try_from(*t).unwrap_or(i64::MAX));
            }
            ObserverMetric::ActiveSessions(s) => {
                self.active_sessions.with_label_values(&[] as &[&str]).set(*s as f64);
            }
            ObserverMetric::QueueDepth(d) => {
                self.queue_depth.with_label_values(&[] as &[&str]).set(*d as f64);
            }
            ObserverMetric::CteExtraLatency(d) => {
                self.cte_extra_latency.observe(d.as_secs_f64());
            }
        }
    }

    fn name(&self) -> &str {
        "prometheus"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn prometheus_observer_name() {
        assert_eq!(PrometheusObserver::try_new().unwrap().name(), "prometheus");
    }

    #[test]
    fn records_all_events_without_panic() {
        let obs = PrometheusObserver::try_new().unwrap();
        obs.record_event(&ObserverEvent::AgentStart {
            provider: "openrouter".into(),
            model: "claude-sonnet".into(),
        });
        obs.record_event(&ObserverEvent::AgentEnd {
            provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            duration: Duration::from_millis(500),
            tokens_used: Some(100),
            cost_usd: None,
        });
        obs.record_event(&ObserverEvent::AgentEnd {
            provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            duration: Duration::ZERO,
            tokens_used: None,
            cost_usd: None,
        });
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "shell".into(),
            duration: Duration::from_millis(10),
            success: true,
        });
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "file_read".into(),
            duration: Duration::from_millis(5),
            success: false,
        });
        obs.record_event(&ObserverEvent::ToolBatch {
            rollout_stage: "stage_b".into(),
            batch_size: 2,
            concurrency_window: 2,
            timeout_count: 1,
            cancel_count: 0,
            error_count: 1,
            degraded: true,
            rollback: true,
            rollback_reason: Some("timeout_rate".into()),
            kill_switch_applied: false,
        });
        obs.record_event(&ObserverEvent::ChannelMessage {
            channel: "telegram".into(),
            direction: "inbound".into(),
        });
        obs.record_event(&ObserverEvent::HeartbeatTick);
        obs.record_event(&ObserverEvent::Error {
            component: "provider".into(),
            message: "timeout".into(),
        });
        obs.record_event(&ObserverEvent::CteRun {
            branch_count: 3,
            chosen_branch: "branch-1".into(),
            chosen_label: "DirectAnswer".into(),
            extra_latency_ms: 42,
            commit_succeeded: true,
            circuit_breaker_tripped: false,
        });
        obs.record_event(&ObserverEvent::CteRun {
            branch_count: 0,
            chosen_branch: String::new(),
            chosen_label: String::new(),
            extra_latency_ms: 0,
            commit_succeeded: false,
            circuit_breaker_tripped: true,
        });
    }

    #[test]
    fn records_all_metrics_without_panic() {
        let obs = PrometheusObserver::try_new().unwrap();
        obs.record_metric(&ObserverMetric::RequestLatency(Duration::from_secs(2)));
        obs.record_metric(&ObserverMetric::TokensUsed(500));
        obs.record_metric(&ObserverMetric::TokensUsed(0));
        obs.record_metric(&ObserverMetric::ActiveSessions(3));
        obs.record_metric(&ObserverMetric::QueueDepth(42));
        obs.record_metric(&ObserverMetric::CteExtraLatency(Duration::from_millis(30)));
    }

    #[test]
    fn encode_produces_prometheus_text_format() {
        let obs = PrometheusObserver::try_new().unwrap();
        obs.record_event(&ObserverEvent::AgentStart {
            provider: "openrouter".into(),
            model: "claude-sonnet".into(),
        });
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "shell".into(),
            duration: Duration::from_millis(100),
            success: true,
        });
        obs.record_event(&ObserverEvent::HeartbeatTick);
        obs.record_metric(&ObserverMetric::RequestLatency(Duration::from_millis(250)));

        let output = obs.encode();
        assert!(output.contains("prx_agent_starts_total"));
        assert!(output.contains("prx_tool_calls_total"));
        assert!(output.contains("prx_heartbeat_ticks_total"));
        assert!(output.contains("prx_request_latency_seconds"));
    }

    #[test]
    fn counters_increment_correctly() {
        let obs = PrometheusObserver::try_new().unwrap();

        for _ in 0..3 {
            obs.record_event(&ObserverEvent::HeartbeatTick);
        }

        let output = obs.encode();
        assert!(output.contains("prx_heartbeat_ticks_total 3"));
    }

    #[test]
    fn tool_calls_track_success_and_failure_separately() {
        let obs = PrometheusObserver::try_new().unwrap();

        obs.record_event(&ObserverEvent::ToolCall {
            tool: "shell".into(),
            duration: Duration::from_millis(10),
            success: true,
        });
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "shell".into(),
            duration: Duration::from_millis(10),
            success: true,
        });
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "shell".into(),
            duration: Duration::from_millis(10),
            success: false,
        });

        let output = obs.encode();
        assert!(output.contains(r#"prx_tool_calls_total{success="true",tool="shell"} 2"#));
        assert!(output.contains(r#"prx_tool_calls_total{success="false",tool="shell"} 1"#));
    }

    #[test]
    fn errors_track_by_component() {
        let obs = PrometheusObserver::try_new().unwrap();
        obs.record_event(&ObserverEvent::Error {
            component: "provider".into(),
            message: "timeout".into(),
        });
        obs.record_event(&ObserverEvent::Error {
            component: "provider".into(),
            message: "rate limit".into(),
        });
        obs.record_event(&ObserverEvent::Error {
            component: "channels".into(),
            message: "disconnected".into(),
        });

        let output = obs.encode();
        assert!(output.contains(r#"prx_errors_total{component="provider"} 2"#));
        assert!(output.contains(r#"prx_errors_total{component="channels"} 1"#));
    }

    #[test]
    fn gauge_reflects_latest_value() {
        let obs = PrometheusObserver::try_new().unwrap();
        obs.record_metric(&ObserverMetric::TokensUsed(100));
        obs.record_metric(&ObserverMetric::TokensUsed(200));

        let output = obs.encode();
        assert!(output.contains("prx_tokens_used_last 200"));
    }
}
