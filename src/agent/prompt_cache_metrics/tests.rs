//! Counter, bound, isolation and redaction regressions.
use super::*;

fn series(name: &str) -> Series {
    Series {
        key: Sha256::digest(name).into(),
        unkeyed: false,
    }
}

fn record(metrics: &mut Metrics, identity: &str, system: &str, tools: &str, usage: &TokenUsage) {
    metrics.record(
        &RoundTrip {
            series: series(identity),
            provider: "prx_provider",
            model: "model",
            system_prompt_sha256: Some(system),
            tools_sha256: tools,
            usage,
        },
        "prx_provider",
        "model",
    );
}

#[test]
fn only_reported_cache_reads_count_as_reported_hits() {
    for (source, reads, reported, hits) in [
        (TokenUsageSource::Reported, Some(300), 1, 1),
        (TokenUsageSource::Reported, Some(0), 1, 0),
        (TokenUsageSource::Reported, None, 0, 0),
        (TokenUsageSource::Estimated, Some(5), 0, 0),
    ] {
        let mut metrics = Metrics::default();
        let usage = TokenUsage {
            source,
            cache_read_input_tokens: reads,
            ..TokenUsage::default()
        };
        record(&mut metrics, "a", "s", "t", &usage);
        let stats = metrics.rows.get("prx_provider/model").unwrap();
        assert_eq!(stats.requests_total, 1);
        assert_eq!(
            stats.usage_reported_total,
            u64::from(source == TokenUsageSource::Reported)
        );
        assert_eq!(stats.cache_reported_total, reported);
        assert_eq!(stats.cache_hit_requests_total, hits);
    }
}

#[test]
fn series_tracks_both_prefixes_independently() {
    let mut metrics = Metrics::default();
    for (system, tools) in [("s1", "t1"), ("s1", "t1"), ("s2", "t1"), ("s2", "t2")] {
        record(&mut metrics, "a", system, tools, &TokenUsage::default());
    }
    let stats = metrics.rows.get("prx_provider/model").unwrap();
    assert_eq!((stats.series_first_total, stats.series_continuations_total), (1, 3));
    assert_eq!(
        (stats.system_prefix_changed_total, stats.tools_prefix_changed_total),
        (1, 1)
    );
}

#[test]
fn interleaved_conversations_do_not_change_each_others_prefixes() {
    let mut metrics = Metrics::default();
    for identity in ["a", "b", "a", "b"] {
        record(&mut metrics, identity, identity, identity, &TokenUsage::default());
    }
    let stats = metrics.rows.get("prx_provider/model").unwrap();
    assert_eq!((stats.series_first_total, stats.series_continuations_total), (2, 2));
    assert_eq!(
        (stats.system_prefix_changed_total, stats.tools_prefix_changed_total),
        (0, 0)
    );
}

#[test]
fn series_cap_evicts_oldest_insertion_with_constant_record_work() {
    let mut metrics = Metrics::default();
    for index in 0..4096 {
        record(&mut metrics, &index.to_string(), "s", "t", &TokenUsage::default());
    }
    record(&mut metrics, "0", "s", "t", &TokenUsage::default());
    record(&mut metrics, "4096", "s", "t", &TokenUsage::default());
    assert_eq!(metrics.series.len(), 4096);
    assert_eq!(metrics.insertion_order.len(), 4096);
    assert!(!metrics.series.contains_key(&series("0").key));
    assert!(metrics.series.contains_key(&series("1").key));
    assert_eq!(
        (metrics.series_lookups, metrics.row_lookups, metrics.evictions),
        (4098, 8196, 1)
    );
    record(&mut metrics, "0", "s", "t", &TokenUsage::default());
    assert_eq!(metrics.rows.get("prx_provider/model").unwrap().series_first_total, 4098);
}

#[test]
fn row_cap_folds_overflow_preserves_every_counter_and_saturates() {
    let mut metrics = Metrics::default();
    let usage = TokenUsage::reported_with_cache(Some(1000), Some(10), Some(1010), Some(5), Some(800));
    for index in 0..300 {
        let model = index.to_string();
        for (system, tools) in [("s1", "t1"), ("s2", "t2")] {
            metrics.record(
                &RoundTrip {
                    series: Series {
                        unkeyed: true,
                        ..series(&model)
                    },
                    provider: "prx_provider",
                    model: &model,
                    system_prompt_sha256: Some(system),
                    tools_sha256: tools,
                    usage: &usage,
                },
                "prx_provider",
                &model,
            );
        }
    }
    let json = serde_json::to_value(&metrics.rows).unwrap();
    for (field, expected) in [
        ("requests_total", 600),
        ("usage_reported_total", 600),
        ("cache_reported_total", 600),
        ("cache_hit_requests_total", 600),
        ("prompt_tokens_total", 600_000),
        ("cache_read_tokens_total", 480_000),
        ("cache_creation_tokens_total", 3000),
        ("series_first_total", 300),
        ("series_continuations_total", 300),
        ("system_prefix_changed_total", 300),
        ("tools_prefix_changed_total", 300),
        ("series_unkeyed_total", 600),
    ] {
        assert_eq!(
            json.as_object()
                .unwrap()
                .values()
                .map(|row| row.get(field).unwrap().as_u64().unwrap())
                .sum::<u64>(),
            expected,
            "{field}"
        );
    }
    assert_eq!(metrics.rows.len(), 257);
    assert_eq!(metrics.rows.get("other").unwrap().requests_total, 88);
    assert_eq!(json.as_object().unwrap().len(), 257);
    let mut fields = serde_json::to_value(PromptCacheStats::default()).unwrap();
    for value in fields.as_object_mut().unwrap().values_mut() {
        *value = (u64::MAX - 1).into();
    }
    let saturated = serde_json::from_value(fields).unwrap();
    metrics.rows.insert("prx_provider/0".into(), saturated);
    for index in 0..5 {
        metrics.record(
            &RoundTrip {
                series: Series {
                    unkeyed: true,
                    ..series("0")
                },
                provider: "prx_provider",
                model: "0",
                system_prompt_sha256: Some(if index % 2 == 0 { "s3" } else { "s4" }),
                tools_sha256: if index % 2 == 0 { "t3" } else { "t4" },
                usage: &usage,
            },
            "prx_provider",
            "0",
        );
    }
    metrics.record(
        &RoundTrip {
            series: Series {
                unkeyed: true,
                ..series("saturating-first")
            },
            provider: "prx_provider",
            model: "0",
            system_prompt_sha256: Some("s"),
            tools_sha256: "t",
            usage: &usage,
        },
        "prx_provider",
        "0",
    );
    let row = serde_json::to_value(metrics.rows.get("prx_provider/0").unwrap()).unwrap();
    for (field, value) in row.as_object().unwrap() {
        assert_eq!(value.as_u64(), Some(u64::MAX), "{field}");
    }
}

#[derive(Clone, Default)]
struct Capture(std::sync::Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn snapshot_and_debug_event_redact_provider_credentials() {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.blocking_lock();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let subscriber = crate::config::secret_paths::fixture::capture_dispatch(subscriber);
    let provider = [
        "custom:https://user:",
        "sk-live-",
        "SECRET@example.test/v1?key=",
        "QUERYSECRET",
    ]
    .concat();
    tracing::dispatcher::with_default(&subscriber, || {
        record_round_trip(RoundTrip {
            series: series("redaction"),
            provider: &provider,
            model: "redaction-model",
            system_prompt_sha256: Some("system-digest"),
            tools_sha256: "tools-digest",
            usage: &TokenUsage::default(),
        })
    });
    let rows = snapshot();
    let event = String::from_utf8(capture.0.lock().clone()).unwrap();
    assert!(event.contains("openprx::prompt_cache"));
    assert!(event.contains("system-digest"));
    for output in [event, serde_json::to_string(&rows).unwrap()] {
        for secret in [["sk-live-", "SECRET"].concat(), "QUERYSECRET".into(), "user:".into()] {
            assert!(!output.contains(&secret), "credential leaked");
        }
    }
    let label = format!(
        "{}/redaction-model",
        crate::security::redaction::redact_provider_text(&provider)
    );
    assert_eq!(rows.get(&label).unwrap().requests_total, 1);
}

#[test]
fn snapshot_preserves_named_rows_and_accumulated_counts() {
    let usage = TokenUsage::reported_with_cache(Some(1000), Some(10), Some(1010), Some(0), Some(800));
    for model in ["snapshot-a", "snapshot-b", "snapshot-a"] {
        record_round_trip(RoundTrip {
            series: series("snapshot-fixture"),
            provider: "snapshot-cache-fixture",
            model,
            system_prompt_sha256: Some("system-digest"),
            tools_sha256: "tools-digest",
            usage: &usage,
        });
    }
    let rows = snapshot();
    let first = rows.get("snapshot-cache-fixture/snapshot-a").unwrap();
    let second = rows.get("snapshot-cache-fixture/snapshot-b").unwrap();
    assert_eq!((first.requests_total, first.cache_read_tokens_total), (2, 1600));
    assert_eq!((second.requests_total, second.cache_read_tokens_total), (1, 800));
    assert_eq!((first.series_first_total, first.series_continuations_total), (1, 1));
    assert_eq!((second.series_first_total, second.series_continuations_total), (0, 1));
}
