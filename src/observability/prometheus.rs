use super::labels::{
    BoundedLabel, COMPONENT_LABEL_CAP, channel_label, direction_label, model_label, provider_label, tool_label,
};
use super::traits::{Observer, ObserverEvent, ObserverMetric};
use prometheus::{
    Encoder, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Registry, TextEncoder,
};

/// Prometheus-backed observer — exposes metrics for scraping via `/metrics`.
///
/// Every label value is bounded (OBS-LABELS, `observability::labels`):
/// providers, tools, channels and directions map onto fixed sets, models onto
/// the configured-model allowlist (D-059), and error components onto a capped
/// set per observer, so no input can grow the number of series without bound.
pub struct PrometheusObserver {
    registry: Registry,
    component_labels: BoundedLabel,

    // Counters
    agent_starts: IntCounterVec,
    tool_calls: IntCounterVec,
    tool_batches: IntCounter,
    tool_batch_size: Histogram,
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

    // Decision-plane metrics (SP-CDP-A 5.11): bounded labels only (AG-11).
    causal_decisions: IntCounterVec,
    causal_failures: IntCounterVec,
    causal_fallbacks: IntCounterVec,
    causal_circuit_transitions: IntCounterVec,
    causal_latency: HistogramVec,
    causal_outcome_delay: Histogram,
    causal_store_dropped: IntCounter,
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
        let tool_batches = IntCounter::new("prx_tool_batches_total", "Total read-only tool batches")
            .map_err(|e| anyhow::anyhow!("failed to create tool_batches metric: {e}"))?;
        let tool_batch_size = Histogram::with_opts(
            prometheus::HistogramOpts::new("prx_tool_batch_size", "Read-only tool batch size")
                .buckets(vec![1.0, 2.0, 4.0, 8.0, 16.0, 32.0]),
        )
        .map_err(|e| anyhow::anyhow!("failed to create tool_batch_size metric: {e}"))?;

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

        let causal_decisions = IntCounterVec::new(
            prometheus::Opts::new("prx_causal_decisions_total", "Decisions served, by backend and source"),
            &["backend", "locality", "served_by"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create causal_decisions metric: {e}"))?;
        let causal_failures = IntCounterVec::new(
            prometheus::Opts::new(
                "prx_causal_prediction_failures_total",
                "Decision attempts that failed or were not made, by backend and error class",
            ),
            &["backend", "error_class"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create causal_failures metric: {e}"))?;
        let causal_fallbacks = IntCounterVec::new(
            prometheus::Opts::new("prx_causal_fallbacks_total", "Decisions that moved to the next backend"),
            &["from", "to"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create causal_fallbacks metric: {e}"))?;
        let causal_circuit_transitions = IntCounterVec::new(
            prometheus::Opts::new(
                "prx_causal_circuit_transitions_total",
                "Decision backend circuit breaker transitions",
            ),
            &["backend", "state"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create causal_circuit_transitions metric: {e}"))?;
        let causal_latency = HistogramVec::new(
            HistogramOpts::new(
                "prx_causal_decision_latency_seconds",
                "Decision attempt latency in seconds",
            )
            .buckets(vec![0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5]),
            &["backend"],
        )
        .map_err(|e| anyhow::anyhow!("failed to create causal_latency metric: {e}"))?;
        let causal_outcome_delay = Histogram::with_opts(
            HistogramOpts::new(
                "prx_causal_outcome_delay_seconds",
                "Elapsed time from committed decision to a newly committed outcome",
            )
            .buckets(vec![0.01, 0.1, 1.0, 10.0, 60.0, 300.0, 3600.0, 86400.0]),
        )
        .map_err(|e| anyhow::anyhow!("failed to create causal_outcome_delay metric: {e}"))?;
        let causal_store_dropped = IntCounter::new(
            "prx_causal_store_dropped_total",
            "Decision or outcome records the store's queue dropped",
        )
        .map_err(|e| anyhow::anyhow!("failed to create causal_store_dropped metric: {e}"))?;

        // Register all metrics
        registry.register(Box::new(causal_decisions.clone())).ok();
        registry.register(Box::new(causal_failures.clone())).ok();
        registry.register(Box::new(causal_fallbacks.clone())).ok();
        registry.register(Box::new(causal_circuit_transitions.clone())).ok();
        registry.register(Box::new(causal_latency.clone())).ok();
        registry.register(Box::new(causal_outcome_delay.clone())).ok();
        registry.register(Box::new(causal_store_dropped.clone())).ok();
        registry.register(Box::new(agent_starts.clone())).ok();
        registry.register(Box::new(tool_calls.clone())).ok();
        registry.register(Box::new(tool_batches.clone())).ok();
        registry.register(Box::new(tool_batch_size.clone())).ok();
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
            component_labels: BoundedLabel::new(COMPONENT_LABEL_CAP),
            agent_starts,
            tool_calls,
            tool_batches,
            tool_batch_size,
            channel_messages,
            heartbeat_ticks,
            errors,
            agent_duration,
            tool_duration,
            request_latency,
            tokens_used,
            active_sessions,
            queue_depth,
            causal_decisions,
            causal_failures,
            causal_fallbacks,
            causal_circuit_transitions,
            causal_latency,
            causal_outcome_delay,
            causal_store_dropped,
        })
    }

    /// Encode all registered metrics into Prometheus text exposition format.
    pub fn encode(&self) -> String {
        self.encode_with_extras(&[])
    }

    /// Encode this observer's registry plus any extra registries into one Prometheus output.
    ///
    /// Lets the /metrics endpoint expose physically separate registries such as
    /// `chat_metrics::CHAT_REGISTRY` alongside this one, so a scrape loses
    /// nothing (S2.5 P1-A).
    pub(crate) fn encode_with_extras(&self, extras: &[&Registry]) -> String {
        let mut all_families = self.registry.gather();
        for reg in extras {
            all_families.extend(reg.gather());
        }
        encode_families(&all_families)
    }
}

/// Encode the metrics of several registries into one Prometheus text body,
/// for the noop-observer fallback path.
pub(crate) fn encode_registries(registries: &[&Registry]) -> String {
    let families: Vec<_> = registries.iter().flat_map(|r| r.gather()).collect();
    encode_families(&families)
}

/// Internal: encode a list of MetricFamily values as Prometheus text; on an
/// encoding failure, warn and return an empty string.
fn encode_families(families: &[prometheus::proto::MetricFamily]) -> String {
    let encoder = TextEncoder::new();
    let mut buf = Vec::new();
    match encoder.encode(families, &mut buf) {
        Ok(()) => match String::from_utf8(buf) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("prometheus encode: UTF-8 conversion failed: {e}");
                String::new()
            }
        },
        Err(e) => {
            tracing::warn!("prometheus encode: TextEncoder failed: {e}");
            String::new()
        }
    }
}

impl PrometheusObserver {
    /// A decision-plane event: counters and histograms with bounded labels
    /// only; the lineage is never a label (AG-11).
    fn record_causal(&self, event: &crate::decision::telemetry::CausalEvent) {
        use crate::decision::telemetry::{CausalEventKind, IncompleteReason};
        let fields = &event.fields;
        let backend = fields.backend.map_or("none", crate::decision::BackendKind::as_str);
        match event.kind {
            CausalEventKind::SnapshotCreated
            | CausalEventKind::CandidatesGenerated
            | CausalEventKind::CandidateGated
            | CausalEventKind::CandidateScored => {}
            CausalEventKind::DecisionCommitted | CausalEventKind::DirectiveApplied => {}
            CausalEventKind::OutcomeRecorded => {
                if let Some(delay_ms) = fields.latency_ms {
                    self.causal_outcome_delay
                        .observe(std::time::Duration::from_millis(delay_ms).as_secs_f64());
                }
            }
            CausalEventKind::PredictionStarted => {}
            CausalEventKind::PredictionSucceeded => {
                let locality = fields.locality.map_or("none", crate::decision::BackendLocality::as_str);
                let served = fields
                    .served
                    .map_or("none", crate::decision::telemetry::ServedLabel::as_str);
                self.causal_decisions
                    .with_label_values(&[backend, locality, served])
                    .inc();
                if let Some(latency) = fields.latency_ms {
                    self.causal_latency
                        .with_label_values(&[backend])
                        .observe(std::time::Duration::from_millis(latency).as_secs_f64());
                }
            }
            CausalEventKind::PredictionFailed => {
                self.causal_failures
                    .with_label_values(&[backend, fields.error_class.unwrap_or("none")])
                    .inc();
            }
            CausalEventKind::BackendFallback => {
                let to = fields.to_backend.map_or("none", crate::decision::BackendKind::as_str);
                self.causal_fallbacks.with_label_values(&[backend, to]).inc();
            }
            CausalEventKind::BackendCircuitOpened => {
                let state = fields
                    .circuit
                    .map_or("none", crate::decision::telemetry::CircuitState::as_str);
                self.causal_circuit_transitions
                    .with_label_values(&[backend, state])
                    .inc();
            }
            CausalEventKind::OutcomeIncomplete => {
                if fields.incomplete == Some(IncompleteReason::StoreDropped) {
                    self.causal_store_dropped.inc();
                }
            }
        }
    }
}

impl Observer for PrometheusObserver {
    fn record_event(&self, event: &ObserverEvent) {
        match event {
            ObserverEvent::AgentStart { provider, model } => {
                let provider = provider_label(provider);
                self.agent_starts
                    .with_label_values(&[provider.as_str(), &model_label(model)])
                    .inc();
            }
            ObserverEvent::AgentEnd {
                provider,
                model,
                duration,
                tokens_used,
                cost_usd: _,
            } => {
                // Agent duration is recorded via the histogram with provider/model labels
                let provider = provider_label(provider);
                self.agent_duration
                    .with_label_values(&[provider.as_str(), &model_label(model)])
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
                let tool = tool_label(tool);
                self.tool_calls.with_label_values(&[tool, success_str]).inc();
                self.tool_duration
                    .with_label_values(&[tool])
                    .observe(duration.as_secs_f64());
            }
            ObserverEvent::ToolBatch {
                batch_size,
                concurrency_window: _,
            } => {
                self.tool_batches.inc();
                #[allow(clippy::cast_precision_loss)]
                self.tool_batch_size.observe(*batch_size as f64);
            }
            ObserverEvent::ChannelMessage { channel, direction } => {
                self.channel_messages
                    .with_label_values(&[channel_label(channel), direction_label(direction)])
                    .inc();
            }
            ObserverEvent::HeartbeatTick => {
                self.heartbeat_ticks.inc();
            }
            ObserverEvent::Error { component, message: _ } => {
                self.errors
                    .with_label_values(&[self.component_labels.label(component)])
                    .inc();
            }
            ObserverEvent::Causal(event) => self.record_causal(event),
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
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// CDP-08: each causal kind lands on its metric with enum labels only;
    /// the lineage and the configured model are never labels.
    #[test]
    fn causal_events_land_on_their_metrics() {
        use crate::decision::telemetry::{
            CausalEvent, CausalEventFields, CausalEventKind, CausalLineage, CircuitState, IncompleteReason, ServedLabel,
        };
        use crate::decision::{BackendKind, BackendLocality};
        let obs = PrometheusObserver::try_new().expect("test: observer");
        let lineage = CausalLineage {
            workspace_id: "WORKSPACE-SENTINEL".into(),
            session_key: Some("SESSION-SENTINEL".into()),
            ..CausalLineage::default()
        };
        let backend = CausalEventFields {
            backend: Some(BackendKind::CompatibleHttp),
            locality: Some(BackendLocality::Loopback),
            configured_model: Some("MODEL-SENTINEL".into()),
            ..CausalEventFields::default()
        };
        let emit = |kind, fields: CausalEventFields| {
            obs.record_event(&ObserverEvent::Causal(Box::new(CausalEvent {
                kind,
                lineage: lineage.clone(),
                fields,
            })));
        };
        emit(CausalEventKind::PredictionStarted, backend.clone());
        emit(
            CausalEventKind::PredictionSucceeded,
            CausalEventFields {
                served: Some(ServedLabel::Fallback),
                latency_ms: Some(40),
                ..backend.clone()
            },
        );
        emit(
            CausalEventKind::PredictionFailed,
            CausalEventFields {
                error_class: Some("transport"),
                ..backend.clone()
            },
        );
        emit(
            CausalEventKind::BackendFallback,
            CausalEventFields {
                to_backend: Some(BackendKind::Builtin),
                ..backend.clone()
            },
        );
        emit(
            CausalEventKind::BackendCircuitOpened,
            CausalEventFields {
                circuit: Some(CircuitState::OpenForGeneration),
                ..backend
            },
        );
        emit(
            CausalEventKind::OutcomeIncomplete,
            CausalEventFields {
                incomplete: Some(IncompleteReason::StoreDropped),
                ..CausalEventFields::default()
            },
        );
        emit(
            CausalEventKind::OutcomeRecorded,
            CausalEventFields {
                latency_ms: Some(125),
                ..CausalEventFields::default()
            },
        );
        let text = obs.encode();
        for expected in [
            r#"prx_causal_decisions_total{backend="compatible_http",locality="loopback",served_by="fallback"} 1"#,
            r#"prx_causal_prediction_failures_total{backend="compatible_http",error_class="transport"} 1"#,
            r#"prx_causal_fallbacks_total{from="compatible_http",to="builtin"} 1"#,
            r#"prx_causal_circuit_transitions_total{backend="compatible_http",state="open_for_generation"} 1"#,
            r#"prx_causal_decision_latency_seconds_count{backend="compatible_http"} 1"#,
            "prx_causal_store_dropped_total 1",
            "prx_causal_outcome_delay_seconds_count 1",
        ] {
            assert!(text.contains(expected), "{expected} missing: {text}");
        }
        for sentinel in ["WORKSPACE-SENTINEL", "SESSION-SENTINEL", "MODEL-SENTINEL"] {
            assert!(!text.contains(sentinel), "{sentinel} became a label: {text}");
        }
    }

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
            batch_size: 2,
            concurrency_window: 2,
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
    }

    #[test]
    fn records_all_metrics_without_panic() {
        let obs = PrometheusObserver::try_new().unwrap();
        obs.record_metric(&ObserverMetric::RequestLatency(Duration::from_secs(2)));
        obs.record_metric(&ObserverMetric::TokensUsed(500));
        obs.record_metric(&ObserverMetric::TokensUsed(0));
        obs.record_metric(&ObserverMetric::ActiveSessions(3));
        obs.record_metric(&ObserverMetric::QueueDepth(42));
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

    /// The number of series one metric family holds after `gather`.
    fn series_count(obs: &PrometheusObserver, family: &str) -> usize {
        obs.registry
            .gather()
            .iter()
            .find(|metric_family| metric_family.name() == family)
            .map_or(0, |metric_family| metric_family.get_metric().len())
    }

    /// OBS-LABELS (D-040 OQ-A12, X-7): labels come from free text, so every
    /// distinct `custom:` endpoint and every model string used to open a new
    /// series. A thousand turns against a thousand endpoints must stay a
    /// bounded number of series; a catalog provider keeps its canonical name.
    #[test]
    fn agent_labels_stay_bounded_for_free_text_providers_and_models() {
        let obs = PrometheusObserver::try_new().unwrap();
        for i in 0..1_000 {
            obs.record_event(&ObserverEvent::AgentStart {
                provider: format!("custom:http://h{i}/v1"),
                model: format!("model-{i}-{}", i * 7919),
            });
            obs.record_event(&ObserverEvent::AgentEnd {
                provider: format!("anthropic-custom:https://gw{i}.example/v1"),
                model: format!("m{i}"),
                duration: Duration::from_millis(5),
                tokens_used: None,
                cost_usd: None,
            });
        }
        obs.record_event(&ObserverEvent::AgentStart {
            provider: "OpenAI".into(),
            model: "gpt-4o".into(),
        });

        let starts = series_count(&obs, "prx_agent_starts_total");
        let durations = series_count(&obs, "prx_agent_duration_seconds");
        assert!(starts <= 4, "prx_agent_starts_total holds {starts} series");
        assert!(durations <= 4, "prx_agent_duration_seconds holds {durations} series");

        let output = obs.encode();
        assert!(
            !output.contains("h999"),
            "a custom endpoint URL must not become a label value"
        );
        assert!(
            output.contains(r#"prx_agent_starts_total{model="unlisted",provider="custom"}"#),
            "a model the configuration does not name is `unlisted` (D-059): {output}"
        );
        assert!(!output.contains("model-999"), "{output}");
        assert!(
            output.contains(r#"provider="openai""#),
            "a catalog provider keeps its canonical name"
        );
    }

    /// OBS-LABELS: a tool name is whatever the model asked for, so an
    /// invented name used to open a series. Registered names stay, an MCP
    /// alias is `mcp`, a skill alias is `skill`, anything else is `other`;
    /// channel, direction and component values are bounded the same way.
    #[test]
    fn tool_channel_and_component_labels_stay_bounded() {
        let obs = PrometheusObserver::try_new().unwrap();
        for i in 0..500 {
            obs.record_event(&ObserverEvent::ToolCall {
                tool: format!("hallucinated_tool_{i}"),
                duration: Duration::from_millis(1),
                success: i % 2 == 0,
            });
            obs.record_event(&ObserverEvent::ChannelMessage {
                channel: format!("chan-{i}"),
                direction: format!("dir-{i}"),
            });
            obs.record_event(&ObserverEvent::Error {
                component: format!("component-{i}"),
                message: "boom".into(),
            });
        }
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "mcp__docs__search".into(),
            duration: Duration::from_millis(1),
            success: true,
        });
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "skill__demo__run".into(),
            duration: Duration::from_millis(1),
            success: true,
        });
        obs.record_event(&ObserverEvent::ChannelMessage {
            channel: "telegram".into(),
            direction: "inbound".into(),
        });

        assert!(series_count(&obs, "prx_tool_calls_total") <= 8);
        assert!(series_count(&obs, "prx_tool_duration_seconds") <= 4);
        assert!(series_count(&obs, "prx_channel_messages_total") <= 4);
        assert!(series_count(&obs, "prx_errors_total") <= 40);

        let output = obs.encode();
        assert!(!output.contains("hallucinated_tool_"), "{output}");
        assert!(output.contains(r#"prx_tool_calls_total{success="true",tool="mcp"} 1"#));
        assert!(output.contains(r#"prx_tool_calls_total{success="true",tool="skill"} 1"#));
        assert!(output.contains(r#"prx_channel_messages_total{channel="telegram",direction="inbound"} 1"#));
        assert!(output.contains(r#"prx_channel_messages_total{channel="other",direction="other"} 500"#));
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
