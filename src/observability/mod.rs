pub mod chat_metrics;
pub mod labels;
pub mod log;
pub mod multi;
pub mod noop;
pub mod otel;
pub mod prometheus;
pub mod traits;
pub mod verbose;

pub use self::log::LogObserver;
pub use noop::NoopObserver;
pub use otel::OtelObserver;
pub use prometheus::PrometheusObserver;
pub use traits::{Observer, ObserverEvent, ObserverMetric};

use crate::config::ObservabilityConfig;

/// Factory: create the right observer from config.
///
/// Every backend is wrapped so it sees provider labels with URL userinfo
/// redacted; see [`RedactingObserver`].
pub fn create_observer(config: &ObservabilityConfig) -> Box<dyn Observer> {
    Box::new(RedactingObserver {
        inner: create_backend(config),
    })
}

fn create_backend(config: &ObservabilityConfig) -> Box<dyn Observer> {
    match config.backend.as_str() {
        "log" => Box::new(LogObserver::new()),
        "prometheus" => match PrometheusObserver::try_new() {
            Ok(obs) => Box::new(obs),
            Err(e) => {
                tracing::error!("Failed to create Prometheus observer: {e}. Falling back to noop.");
                Box::new(NoopObserver)
            }
        },
        "otel" | "opentelemetry" | "otlp" => {
            match OtelObserver::new(config.otel_endpoint.as_deref(), config.otel_service_name.as_deref()) {
                Ok(obs) => {
                    // The endpoint may carry `user:password@`; the log (also
                    // served by `GET /api/logs`) names the host only.
                    tracing::info!(
                        endpoint = %crate::providers::redact_endpoint_userinfo(
                            config.otel_endpoint.as_deref().unwrap_or("http://localhost:4318")
                        ),
                        "OpenTelemetry observer initialized"
                    );
                    Box::new(obs)
                }
                Err(e) => {
                    tracing::error!("Failed to create OTel observer: {e}. Falling back to noop.");
                    Box::new(NoopObserver)
                }
            }
        }
        "none" | "noop" => Box::new(NoopObserver),
        _ => {
            tracing::warn!(
                "Unknown observability backend '{}', falling back to noop",
                config.backend
            );
            Box::new(NoopObserver)
        }
    }
}

/// Redacts URL userinfo from the provider labels and error texts of every
/// event before the backend sees it.
///
/// A provider label may be `custom:http://user:password@host/v1`. The agent
/// loop, the gateway and the chat surfaces emit that label on every turn, and
/// every backend prints it: the log backend at INFO (`llm.request`), Prometheus
/// as a metric label on `/metrics`, OpenTelemetry as a span attribute. This is
/// the one place all of them pass through.
struct RedactingObserver {
    inner: Box<dyn Observer>,
}

impl Observer for RedactingObserver {
    fn record_event(&self, event: &ObserverEvent) {
        match redacted_event(event) {
            Some(redacted) => self.inner.record_event(&redacted),
            None => self.inner.record_event(event),
        }
    }

    fn record_metric(&self, metric: &ObserverMetric) {
        self.inner.record_metric(metric);
    }

    fn flush(&self) {
        self.inner.flush();
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        // Backend-specific callers (the `/metrics` handler) downcast to the
        // concrete backend, which this wrapper must not hide.
        self.inner.as_any()
    }
}

fn carries_userinfo(text: &str) -> bool {
    matches!(
        crate::providers::redact_endpoint_userinfo(text),
        std::borrow::Cow::Owned(_)
    )
}

fn redact_in_place(text: &mut String) {
    if let std::borrow::Cow::Owned(redacted) = crate::providers::redact_endpoint_userinfo(text) {
        *text = redacted;
    }
}

/// The texts of an event that can quote a provider label: the label itself
/// and the error message.
const fn label_texts(event: &mut ObserverEvent) -> [Option<&mut String>; 2] {
    match event {
        ObserverEvent::AgentStart { provider, .. }
        | ObserverEvent::LlmRequest { provider, .. }
        | ObserverEvent::AgentEnd { provider, .. } => [Some(provider), None],
        ObserverEvent::LlmResponse {
            provider,
            error_message,
            ..
        } => [Some(provider), error_message.as_mut()],
        ObserverEvent::Error { message, .. } => [Some(message), None],
        _ => [None, None],
    }
}

/// The event with its provider label and error text redacted, or `None` when
/// neither carries URL userinfo (the common case, which allocates nothing).
fn redacted_event(event: &ObserverEvent) -> Option<ObserverEvent> {
    let dirty = match event {
        ObserverEvent::AgentStart { provider, .. }
        | ObserverEvent::LlmRequest { provider, .. }
        | ObserverEvent::AgentEnd { provider, .. } => carries_userinfo(provider),
        ObserverEvent::LlmResponse {
            provider,
            error_message,
            ..
        } => carries_userinfo(provider) || error_message.as_deref().is_some_and(carries_userinfo),
        ObserverEvent::Error { message, .. } => carries_userinfo(message),
        _ => false,
    };
    if !dirty {
        return None;
    }
    let mut redacted = event.clone();
    for text in label_texts(&mut redacted).into_iter().flatten() {
        redact_in_place(text);
    }
    Some(redacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_none_returns_noop() {
        let cfg = ObservabilityConfig {
            backend: "none".into(),
            ..ObservabilityConfig::default()
        };
        assert_eq!(create_observer(&cfg).name(), "noop");
    }

    #[test]
    fn factory_noop_returns_noop() {
        let cfg = ObservabilityConfig {
            backend: "noop".into(),
            ..ObservabilityConfig::default()
        };
        assert_eq!(create_observer(&cfg).name(), "noop");
    }

    #[test]
    fn factory_log_returns_log() {
        let cfg = ObservabilityConfig {
            backend: "log".into(),
            ..ObservabilityConfig::default()
        };
        assert_eq!(create_observer(&cfg).name(), "log");
    }

    #[test]
    fn factory_prometheus_returns_prometheus() {
        let cfg = ObservabilityConfig {
            backend: "prometheus".into(),
            ..ObservabilityConfig::default()
        };
        assert_eq!(create_observer(&cfg).name(), "prometheus");
    }

    #[test]
    fn factory_otel_returns_otel() {
        let cfg = ObservabilityConfig {
            backend: "otel".into(),
            otel_endpoint: Some("http://127.0.0.1:19999".into()),
            otel_service_name: Some("test".into()),
        };
        assert_eq!(create_observer(&cfg).name(), "otel");
    }

    #[test]
    fn factory_opentelemetry_alias() {
        let cfg = ObservabilityConfig {
            backend: "opentelemetry".into(),
            otel_endpoint: Some("http://127.0.0.1:19999".into()),
            otel_service_name: Some("test".into()),
        };
        assert_eq!(create_observer(&cfg).name(), "otel");
    }

    #[test]
    fn factory_otlp_alias() {
        let cfg = ObservabilityConfig {
            backend: "otlp".into(),
            otel_endpoint: Some("http://127.0.0.1:19999".into()),
            otel_service_name: Some("test".into()),
        };
        assert_eq!(create_observer(&cfg).name(), "otel");
    }

    #[test]
    fn factory_unknown_falls_back_to_noop() {
        let cfg = ObservabilityConfig {
            backend: "xyzzy_unknown".into(),
            ..ObservabilityConfig::default()
        };
        assert_eq!(create_observer(&cfg).name(), "noop");
    }

    #[test]
    fn factory_empty_string_falls_back_to_noop() {
        let cfg = ObservabilityConfig {
            backend: String::new(),
            ..ObservabilityConfig::default()
        };
        assert_eq!(create_observer(&cfg).name(), "noop");
    }

    #[test]
    fn factory_garbage_falls_back_to_noop() {
        let cfg = ObservabilityConfig {
            backend: "xyzzy_garbage_123".into(),
            ..ObservabilityConfig::default()
        };
        assert_eq!(create_observer(&cfg).name(), "noop");
    }

    /// R-1: every backend the factory builds receives the provider label
    /// with URL userinfo redacted. The log backend prints `llm.request` at
    /// INFO on every turn.
    #[test]
    fn every_backend_sees_provider_labels_without_url_userinfo() {
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let sink = std::sync::Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || CaptureWriter(std::sync::Arc::clone(&sink)))
            .finish();
        let observer = create_observer(&ObservabilityConfig {
            backend: "log".into(),
            ..ObservabilityConfig::default()
        });
        tracing::subscriber::with_default(subscriber, || {
            observer.record_event(&ObserverEvent::LlmRequest {
                provider: "custom:http://alice:SECRET-fixture-pw@127.0.0.1:1/v1".into(),
                model: "fixture-model".into(),
                messages_count: 1,
            });
            observer.record_event(&ObserverEvent::LlmResponse {
                provider: "custom:http://alice:SECRET-fixture-pw@127.0.0.1:1/v1".into(),
                model: "fixture-model".into(),
                duration: std::time::Duration::from_millis(1),
                success: false,
                error_message: Some("provider=custom:http://alice:SECRET-fixture-pw@127.0.0.1:1/v1 failed".into()),
            });
        });
        let logged = String::from_utf8_lossy(&captured.lock()).into_owned();
        assert!(logged.contains("llm.request"), "the capture saw nothing: {logged}");
        assert!(logged.contains("custom:http://[redacted]@127.0.0.1:1/v1"), "{logged}");
        assert!(
            !logged.contains("SECRET-fixture-pw"),
            "an observer line leaked the password: {logged}"
        );
    }

    /// The wrapper must not hide the concrete backend from the `/metrics`
    /// handler, which downcasts to it.
    #[test]
    fn the_redacting_wrapper_still_downcasts_to_its_backend() {
        let observer = create_observer(&ObservabilityConfig {
            backend: "prometheus".into(),
            ..ObservabilityConfig::default()
        });
        assert!(observer.as_any().downcast_ref::<PrometheusObserver>().is_some());
    }

    struct CaptureWriter(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// R-C2BUg G-1: observer error texts that start with a URL keep none of
    /// their credentials.
    #[test]
    fn observer_texts_redact_every_url_in_url_first_text() {
        crate::providers::label_redaction_tests::assert_every_url_redacted("observer", &|text| {
            let event = ObserverEvent::Error {
                component: "fixture".into(),
                message: text.to_string(),
            };
            format!("{:?}", redacted_event(&event).unwrap_or(event))
        });
    }
}
