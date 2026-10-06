use super::labels::{
    BoundedLabel, COMPONENT_LABEL_CAP, channel_label, direction_label, model_label, provider_label, tool_label,
};
use super::traits::{Observer, ObserverEvent, ObserverMetric};
use opentelemetry::metrics::{Counter, Gauge, Histogram};
use opentelemetry::trace::{Span, SpanKind, Status, Tracer};
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::any::Any;
use std::time::SystemTime;

/// OpenTelemetry-backed observer — exports traces and metrics via OTLP.
pub struct OtelObserver {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,

    // Metrics instruments
    agent_starts: Counter<u64>,
    agent_duration: Histogram<f64>,
    llm_calls: Counter<u64>,
    llm_duration: Histogram<f64>,
    tool_calls: Counter<u64>,
    tool_batches: Counter<u64>,
    tool_duration: Histogram<f64>,
    channel_messages: Counter<u64>,
    heartbeat_ticks: Counter<u64>,
    errors: Counter<u64>,
    request_latency: Histogram<f64>,
    tokens_used: Counter<u64>,
    active_sessions: Gauge<u64>,
    queue_depth: Gauge<u64>,

    // Decision-plane metrics
    causal_events: Counter<u64>,
    component_labels: BoundedLabel,
}

impl OtelObserver {
    /// Create a new OTel observer exporting to the given OTLP endpoint.
    ///
    /// Uses HTTP/protobuf transport (port 4318 by default).
    /// Falls back to `http://localhost:4318` if no endpoint is provided.
    pub fn new(endpoint: Option<&str>, service_name: Option<&str>) -> Result<Self, String> {
        let endpoint = endpoint.unwrap_or("http://localhost:4318");
        let service_name = service_name.unwrap_or("prx");

        // ── Trace exporter ──────────────────────────────────────
        let span_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .map_err(|e| format!("Failed to create OTLP span exporter: {e}"))?;

        let tracer_provider = SdkTracerProvider::builder()
            .with_batch_exporter(span_exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name(service_name.to_string())
                    .build(),
            )
            .build();

        global::set_tracer_provider(tracer_provider.clone());

        // ── Metric exporter ─────────────────────────────────────
        let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .map_err(|e| format!("Failed to create OTLP metric exporter: {e}"))?;

        let metric_reader = opentelemetry_sdk::metrics::PeriodicReader::builder(metric_exporter).build();

        let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
            .with_reader(metric_reader)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name(service_name.to_string())
                    .build(),
            )
            .build();

        let meter_provider_clone = meter_provider.clone();
        global::set_meter_provider(meter_provider);

        // ── Create metric instruments ────────────────────────────
        let meter = global::meter("prx");

        let agent_starts = meter
            .u64_counter("prx.agent.starts")
            .with_description("Total agent invocations")
            .build();

        let agent_duration = meter
            .f64_histogram("prx.agent.duration")
            .with_description("Agent invocation duration in seconds")
            .with_unit("s")
            .build();

        let llm_calls = meter
            .u64_counter("prx.llm.calls")
            .with_description("Total LLM provider calls")
            .build();

        let llm_duration = meter
            .f64_histogram("prx.llm.duration")
            .with_description("LLM provider call duration in seconds")
            .with_unit("s")
            .build();

        let tool_calls = meter
            .u64_counter("prx.tool.calls")
            .with_description("Total tool calls")
            .build();
        let tool_batches = meter
            .u64_counter("prx.tool.batches")
            .with_description("Total read-only tool batches")
            .build();
        let tool_duration = meter
            .f64_histogram("prx.tool.duration")
            .with_description("Tool execution duration in seconds")
            .with_unit("s")
            .build();

        let channel_messages = meter
            .u64_counter("prx.channel.messages")
            .with_description("Total channel messages")
            .build();

        let heartbeat_ticks = meter
            .u64_counter("prx.heartbeat.ticks")
            .with_description("Total heartbeat ticks")
            .build();

        let errors = meter
            .u64_counter("prx.errors")
            .with_description("Total errors by component")
            .build();

        let request_latency = meter
            .f64_histogram("prx.request.latency")
            .with_description("Request latency in seconds")
            .with_unit("s")
            .build();

        let tokens_used = meter
            .u64_counter("prx.tokens.used")
            .with_description("Total tokens consumed (monotonic)")
            .build();

        let active_sessions = meter
            .u64_gauge("prx.sessions.active")
            .with_description("Current number of active sessions")
            .build();

        let queue_depth = meter
            .u64_gauge("prx.queue.depth")
            .with_description("Current message queue depth")
            .build();

        let causal_events = meter
            .u64_counter("prx.causal.events")
            .with_description("Decision-plane events by kind, backend and class")
            .build();

        Ok(Self {
            tracer_provider,
            meter_provider: meter_provider_clone,
            agent_starts,
            agent_duration,
            llm_calls,
            llm_duration,
            tool_calls,
            tool_batches,
            tool_duration,
            channel_messages,
            heartbeat_ticks,
            errors,
            request_latency,
            tokens_used,
            active_sessions,
            queue_depth,
            causal_events,
            component_labels: BoundedLabel::new(COMPONENT_LABEL_CAP),
        })
    }
}

impl Observer for OtelObserver {
    fn record_event(&self, event: &ObserverEvent) {
        let tracer = global::tracer("prx");

        match event {
            ObserverEvent::AgentStart { provider, model } => {
                self.agent_starts.add(
                    1,
                    &[
                        KeyValue::new("provider", provider_label(provider)),
                        KeyValue::new("model", model_label(model).into_owned()),
                    ],
                );
            }
            ObserverEvent::LlmRequest { .. } | ObserverEvent::ToolCallStart { .. } | ObserverEvent::TurnComplete => {}
            ObserverEvent::LlmResponse {
                provider,
                model,
                duration,
                success,
                error_message: _,
            } => {
                let secs = duration.as_secs_f64();
                let attrs = [
                    KeyValue::new("provider", provider_label(provider)),
                    KeyValue::new("model", model_label(model).into_owned()),
                    KeyValue::new("success", success.to_string()),
                ];
                self.llm_calls.add(1, &attrs);
                self.llm_duration.record(secs, &attrs);

                // Create a completed span for visibility in trace backends.
                let start_time = SystemTime::now().checked_sub(*duration).unwrap_or_else(SystemTime::now);
                let mut span = tracer.build(
                    opentelemetry::trace::SpanBuilder::from_name("llm.call")
                        .with_kind(SpanKind::Internal)
                        .with_start_time(start_time)
                        .with_attributes(vec![
                            KeyValue::new("provider", provider_label(provider)),
                            KeyValue::new("model", model_label(model).into_owned()),
                            KeyValue::new("success", *success),
                            KeyValue::new("duration_s", secs),
                        ]),
                );
                if *success {
                    span.set_status(Status::Ok);
                } else {
                    span.set_status(Status::error(""));
                }
                span.end();
            }
            ObserverEvent::AgentEnd {
                provider,
                model,
                duration,
                tokens_used,
                cost_usd,
            } => {
                let secs = duration.as_secs_f64();
                let start_time = SystemTime::now().checked_sub(*duration).unwrap_or_else(SystemTime::now);

                // Create a completed span with correct timing
                let mut span = tracer.build(
                    opentelemetry::trace::SpanBuilder::from_name("agent.invocation")
                        .with_kind(SpanKind::Internal)
                        .with_start_time(start_time)
                        .with_attributes(vec![
                            KeyValue::new("provider", provider_label(provider)),
                            KeyValue::new("model", model_label(model).into_owned()),
                            KeyValue::new("duration_s", secs),
                        ]),
                );
                if let Some(t) = tokens_used {
                    span.set_attribute(KeyValue::new("tokens_used", *t as i64));
                }
                if let Some(c) = cost_usd {
                    span.set_attribute(KeyValue::new("cost_usd", *c));
                }
                span.end();

                self.agent_duration.record(
                    secs,
                    &[
                        KeyValue::new("provider", provider_label(provider)),
                        KeyValue::new("model", model_label(model).into_owned()),
                    ],
                );
                // Note: tokens are recorded via record_metric(TokensUsed) to avoid
                // double-counting. AgentEnd only records duration.
            }
            ObserverEvent::ToolCall {
                tool,
                duration,
                success,
            } => {
                let secs = duration.as_secs_f64();
                let start_time = SystemTime::now().checked_sub(*duration).unwrap_or_else(SystemTime::now);

                let status = if *success { Status::Ok } else { Status::error("") };

                let mut span = tracer.build(
                    opentelemetry::trace::SpanBuilder::from_name("tool.call")
                        .with_kind(SpanKind::Internal)
                        .with_start_time(start_time)
                        .with_attributes(vec![
                            KeyValue::new("tool.name", tool_label(tool).to_string()),
                            KeyValue::new("tool.success", *success),
                            KeyValue::new("duration_s", secs),
                        ]),
                );
                span.set_status(status);
                span.end();

                let attrs = [
                    KeyValue::new("tool", tool_label(tool).to_string()),
                    KeyValue::new("success", success.to_string()),
                ];
                self.tool_calls.add(1, &attrs);
                self.tool_duration
                    .record(secs, &[KeyValue::new("tool", tool_label(tool).to_string())]);
            }
            ObserverEvent::ToolBatch {
                batch_size,
                concurrency_window,
            } => {
                self.tool_batches.add(
                    1,
                    &[
                        KeyValue::new("batch_size", i64::try_from(*batch_size).unwrap_or(i64::MAX)),
                        KeyValue::new(
                            "concurrency_window",
                            i64::try_from(*concurrency_window).unwrap_or(i64::MAX),
                        ),
                    ],
                );
            }
            ObserverEvent::ChannelMessage { channel, direction } => {
                self.channel_messages.add(
                    1,
                    &[
                        KeyValue::new("channel", channel_label(channel).to_string()),
                        KeyValue::new("direction", direction_label(direction).to_string()),
                    ],
                );
            }
            ObserverEvent::HeartbeatTick => {
                self.heartbeat_ticks.add(1, &[]);
            }
            ObserverEvent::Error { component, message } => {
                let (attributes, redacted) = self.error_attributes(component, message);
                // Create an error span for visibility in trace backends
                let mut span = tracer.build(
                    opentelemetry::trace::SpanBuilder::from_name("error")
                        .with_kind(SpanKind::Internal)
                        .with_attributes(attributes),
                );
                span.set_status(Status::error(redacted));
                span.end();

                self.errors.add(
                    1,
                    &[KeyValue::new(
                        "component",
                        self.component_labels.label(component).to_string(),
                    )],
                );
            }
            ObserverEvent::Causal(event) => {
                self.causal_events.add(1, &causal_metric_attributes(event));
                let mut span = tracer.build(
                    opentelemetry::trace::SpanBuilder::from_name(event.kind.as_str())
                        .with_kind(SpanKind::Internal)
                        .with_attributes(causal_span_attributes(event)),
                );
                span.end();
            }
        }
    }

    fn record_metric(&self, metric: &ObserverMetric) {
        match metric {
            ObserverMetric::RequestLatency(d) => {
                self.request_latency.record(d.as_secs_f64(), &[]);
            }
            ObserverMetric::TokensUsed(t) => {
                self.tokens_used.add(*t as u64, &[]);
            }
            ObserverMetric::ActiveSessions(s) => {
                self.active_sessions.record(*s as u64, &[]);
            }
            ObserverMetric::QueueDepth(d) => {
                self.queue_depth.record(*d as u64, &[]);
            }
        }
    }

    fn flush(&self) {
        if let Err(e) = self.tracer_provider.force_flush() {
            tracing::warn!("OTel trace flush failed: {e}");
        }
        if let Err(e) = self.meter_provider.force_flush() {
            tracing::warn!("OTel metric flush failed: {e}");
        }
    }

    fn name(&self) -> &str {
        "otel"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The metric attributes of a causal event: bounded values only (AG-11).
fn causal_metric_attributes(event: &crate::decision::telemetry::CausalEvent) -> [KeyValue; 3] {
    let fields = &event.fields;
    [
        KeyValue::new("kind", event.kind.as_str()),
        KeyValue::new(
            "backend",
            fields.backend.map_or("none", crate::decision::BackendKind::as_str),
        ),
        KeyValue::new("class", fields.error_class.unwrap_or("none")),
    ]
}

/// The span attributes of a causal event: the lineage identifiers and the
/// bounded fields, the model through the configured-model allowlist; never
/// state, text or a claimed model (D-060 L-1).
fn causal_span_attributes(event: &crate::decision::telemetry::CausalEvent) -> Vec<KeyValue> {
    let (fields, lineage) = (&event.fields, &event.lineage);
    let mut attributes = vec![
        KeyValue::new("causal.kind", event.kind.as_str()),
        KeyValue::new("causal.workspace", lineage.workspace_id.to_string()),
        KeyValue::new(
            "causal.backend",
            fields.backend.map_or("none", crate::decision::BackendKind::as_str),
        ),
        KeyValue::new("causal.class", fields.error_class.unwrap_or("none")),
    ];
    for (key, value) in [
        ("causal.owner", &lineage.owner_id),
        ("causal.session", &lineage.session_key),
        ("causal.run", &lineage.run_id),
        ("causal.task", &lineage.task_id),
        ("causal.source_event", &lineage.source_event_id),
        ("causal.causation_event", &lineage.causation_event_id),
        ("causal.decision", &lineage.decision_id),
    ] {
        if let Some(value) = value {
            attributes.push(KeyValue::new(key, value.to_string()));
        }
    }
    if let Some(model) = &fields.configured_model {
        attributes.push(KeyValue::new("causal.model", model_label(model).into_owned()));
    }
    if let Some(latency) = fields.latency_ms {
        attributes.push(KeyValue::new(
            "causal.latency_ms",
            i64::try_from(latency).unwrap_or(i64::MAX),
        ));
    }
    attributes
}

impl OtelObserver {
    /// The error span's attributes and status text: the component through
    /// the bounded label, the message with secrets redacted (D-060 L-1).
    fn error_attributes(&self, component: &str, message: &str) -> (Vec<KeyValue>, String) {
        let redacted = crate::security::redaction::redact_secrets(message);
        (
            vec![
                KeyValue::new("component", self.component_labels.label(component).to_string()),
                KeyValue::new("error.message", redacted.clone()),
            ],
            redacted,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// D-060 L-1: OpenTelemetry attributes are bounded. A causal event's
    /// metric attributes carry no lineage, its span shows an unlisted model
    /// as `unlisted`, and an error's message is redacted and its component
    /// capped.
    #[test]
    fn otel_attributes_are_bounded() {
        use crate::decision::telemetry::{CausalEvent, CausalEventFields, CausalEventKind, CausalLineage};
        let event = CausalEvent {
            kind: CausalEventKind::PredictionFailed,
            lineage: CausalLineage {
                workspace_id: "workspace-1".into(),
                session_key: Some("SESSION-SENTINEL-41".into()),
                ..CausalLineage::default()
            },
            fields: CausalEventFields {
                backend: Some(crate::decision::BackendKind::CompatibleHttp),
                configured_model: Some("HOSTILE-UNLISTED-MODEL-77".into()),
                error_class: Some("transport"),
                ..CausalEventFields::default()
            },
        };
        let metric = causal_metric_attributes(&event);
        let rendered = format!("{metric:?}");
        assert!(!rendered.contains("SESSION-SENTINEL-41"), "{rendered}");
        assert!(!rendered.contains("workspace-1"), "{rendered}");
        assert!(!rendered.contains("HOSTILE"), "{rendered}");
        let span = format!("{:?}", causal_span_attributes(&event));
        assert!(
            span.contains("SESSION-SENTINEL-41"),
            "the lineage goes to the span: {span}"
        );
        assert!(!span.contains("HOSTILE-UNLISTED-MODEL-77"), "{span}");
        assert!(span.contains(crate::observability::labels::UNLISTED_MODEL), "{span}");

        let obs = test_observer();
        let secret = "sk-ant-api03-OTELSENTINELabcdefghijklmnopqrstuvwxyz0123456789";
        let (attributes, status) = obs.error_attributes("provider", &format!("failed with {secret}"));
        let rendered = format!("{attributes:?} {status}");
        assert!(!rendered.contains("OTELSENTINEL"), "{rendered}");
        for index in 0..(COMPONENT_LABEL_CAP + 8) {
            let _ = obs.error_attributes(&format!("component-{index}"), "x");
        }
        let (attributes, _) = obs.error_attributes("component-past-the-cap", "x");
        assert!(format!("{attributes:?}").contains("\"other\""), "{attributes:?}");
    }

    // Note: OtelObserver::new() requires an OTLP endpoint.
    // In tests we verify the struct creation fails gracefully
    // when no collector is available, and test the observer interface
    // by constructing with a known-unreachable endpoint (spans/metrics
    // are buffered and exported asynchronously, so recording never panics).

    fn test_observer() -> OtelObserver {
        // Create with a dummy endpoint — exports will silently fail
        // but the observer itself works fine for recording
        OtelObserver::new(Some("http://127.0.0.1:19999"), Some("openprx-test"))
            .expect("observer creation should not fail with valid endpoint format")
    }

    #[test]
    fn otel_observer_name() {
        let obs = test_observer();
        assert_eq!(obs.name(), "otel");
    }

    #[test]
    fn records_all_events_without_panic() {
        let obs = test_observer();
        obs.record_event(&ObserverEvent::AgentStart {
            provider: "openrouter".into(),
            model: "claude-sonnet".into(),
        });
        obs.record_event(&ObserverEvent::LlmRequest {
            provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            messages_count: 2,
        });
        obs.record_event(&ObserverEvent::LlmResponse {
            provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            duration: Duration::from_millis(250),
            success: true,
            error_message: None,
        });
        obs.record_event(&ObserverEvent::AgentEnd {
            provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            duration: Duration::from_millis(500),
            tokens_used: Some(100),
            cost_usd: Some(0.0015),
        });
        obs.record_event(&ObserverEvent::AgentEnd {
            provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            duration: Duration::ZERO,
            tokens_used: None,
            cost_usd: None,
        });
        obs.record_event(&ObserverEvent::ToolCallStart { tool: "shell".into() });
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
        obs.record_event(&ObserverEvent::TurnComplete);
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
        let obs = test_observer();
        obs.record_metric(&ObserverMetric::RequestLatency(Duration::from_secs(2)));
        obs.record_metric(&ObserverMetric::TokensUsed(500));
        obs.record_metric(&ObserverMetric::TokensUsed(0));
        obs.record_metric(&ObserverMetric::ActiveSessions(3));
        obs.record_metric(&ObserverMetric::QueueDepth(42));
    }

    #[test]
    fn flush_does_not_panic() {
        let obs = test_observer();
        obs.record_event(&ObserverEvent::HeartbeatTick);
        obs.flush();
    }

    // ── §8.2 OTel export failure resilience tests ────────────

    #[test]
    fn otel_records_error_event_without_panic() {
        let obs = test_observer();
        // Simulate an error event — should not panic even with unreachable endpoint
        obs.record_event(&ObserverEvent::Error {
            component: "provider".into(),
            message: "connection refused to model endpoint".into(),
        });
    }

    #[test]
    fn otel_records_llm_failure_without_panic() {
        let obs = test_observer();
        obs.record_event(&ObserverEvent::LlmResponse {
            provider: "openrouter".into(),
            model: "missing-model".into(),
            duration: Duration::from_millis(0),
            success: false,
            error_message: Some("404 Not Found".into()),
        });
    }

    #[test]
    fn otel_flush_idempotent_with_unreachable_endpoint() {
        let obs = test_observer();
        // Multiple flushes should not panic even when endpoint is unreachable
        obs.flush();
        obs.flush();
        obs.flush();
    }

    #[test]
    fn otel_records_zero_duration_metrics() {
        let obs = test_observer();
        obs.record_metric(&ObserverMetric::RequestLatency(Duration::ZERO));
        obs.record_metric(&ObserverMetric::TokensUsed(0));
        obs.record_metric(&ObserverMetric::ActiveSessions(0));
        obs.record_metric(&ObserverMetric::QueueDepth(0));
    }

    #[test]
    fn otel_observer_creation_with_valid_endpoint_succeeds() {
        // Even though endpoint is unreachable, creation should succeed
        let result = OtelObserver::new(Some("http://127.0.0.1:12345"), Some("openprx-test"));
        assert!(
            result.is_ok(),
            "observer creation must succeed even with unreachable endpoint"
        );
    }
}
