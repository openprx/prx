//! Status and cross-surface secret-redaction regressions.
use super::*;
use crate::config::Config;
use crate::providers::circuit_breaker::{BreakerOutcome, BreakerParams, shared_registry};
use anyhow::Context as _;
use std::sync::Arc;
use std::time::Duration;

const fn params() -> BreakerParams {
    BreakerParams {
        failure_threshold: 1,
        open_for: Duration::from_secs(30),
        max_open_for: Duration::from_secs(300),
    }
}
fn config(temp: &tempfile::TempDir) -> Config {
    Config {
        config_path: temp.path().join("config.toml"),
        workspace_dir: temp.path().join("workspace"),
        ..Config::default()
    }
}
#[tokio::test(start_paused = true)]
async fn r6a_status_exposes_live_open_provider_circuit() -> anyhow::Result<()> {
    let registry = shared_registry();
    let key = Arc::<str>::from("prx_status_circuit_r6a");
    let _lease = registry.register([(Arc::clone(&key), Arc::clone(&key))]);
    registry
        .admit(&key, params())
        .map_err(|ms| anyhow::anyhow!("unexpected refusal ({ms}ms)"))?
        .context("missing permit")?
        .complete(BreakerOutcome::Failure);
    let temp = tempfile::TempDir::new()?;
    let Json(response) = get_status(State(super::super::test_support::app_state(config(&temp)))).await;
    let body = serde_json::to_value(response)?;
    let row = body
        .get("provider_circuits")
        .context("missing circuits")?
        .as_array()
        .context("circuits must be an array")?
        .iter()
        .find(|row| row.get("provider").is_some_and(|provider| provider == key.as_ref()))
        .context("missing provider row")?;
    assert_eq!(row.get("state").context("missing circuit field")?, "open");
    assert!(
        row.get("retry_in_ms")
            .context("missing circuit field")?
            .as_u64()
            .context("missing circuit field")?
            > 0
    );
    Ok(())
}

#[derive(Clone)]
struct LogCapture(Arc<parking_lot::Mutex<Vec<u8>>>);
impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
async fn surfaces_and_logs(labels: &[String]) -> anyhow::Result<(serde_json::Value, String)> {
    let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let writer = LogCapture(Arc::clone(&captured));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let registry = shared_registry();
    let members: Vec<_> = labels
        .iter()
        .map(|label| {
            (
                Arc::<str>::from(format!("{label}#private_hash")),
                Arc::<str>::from(crate::providers::redact_endpoint_userinfo(label).as_ref()),
            )
        })
        .collect();
    let _lease = registry.register(members.clone());
    // The guard only surrounds synchronous transitions; never held across await.
    {
        let _subscriber = tracing::subscriber::set_default(subscriber);
        for (key, _) in &members {
            registry
                .admit(key, params())
                .map_err(|ms| anyhow::anyhow!("unexpected refusal ({ms}ms)"))?
                .context("missing permit")?
                .complete(BreakerOutcome::Failure);
        }
    }
    let snapshot = crate::health::snapshot_json();
    let temp = tempfile::TempDir::new()?;
    let mut config = Config {
        default_provider: labels.first().cloned(),
        default_model: Some("prx_surface_model".to_string()),
        ..config(&temp)
    };
    config.reliability.fallback_providers = labels.iter().skip(1).cloned().collect();
    std::fs::write(crate::daemon::state_file_path(&config), serde_json::to_vec(&snapshot)?)?;
    let doctor = crate::doctor::diagnose(&config)
        .items
        .into_iter()
        .filter(|item| item.category == "provider-circuits")
        .map(|item| item.message)
        .collect::<Vec<_>>()
        .join("\n");
    let Json(status) = get_status(State(super::super::test_support::app_state(config))).await;
    let logs = String::from_utf8(captured.lock().clone())?;
    assert!(
        logs.contains("Provider circuit open"),
        "transition logs must be captured"
    );
    Ok((
        snapshot.clone(),
        format!("{snapshot}\n{}\n{doctor}\n{logs}", serde_json::to_string(&status)?),
    ))
}
#[tokio::test(start_paused = true)]
async fn r5b_secret_label_is_redacted_in_logs_snapshot_status_and_doctor() -> anyhow::Result<()> {
    let credential = ["sk", "-live-SECRET"].concat();
    let label = format!("custom:https://user:{credential}@example.test/v1?key=QUERYSECRET");
    let (snapshot, all) = surfaces_and_logs(std::slice::from_ref(&label)).await?;
    for secret in [credential.as_str(), "QUERYSECRET", "user:"] {
        assert!(!all.contains(secret), "leaked {secret}");
    }
    let display = crate::providers::redact_endpoint_userinfo(&label);
    let public = crate::security::redaction::redact_provider_text(&display);
    assert!(
        snapshot
            .get("provider_circuits")
            .context("missing circuit field")?
            .get(&public)
            .is_some()
    );
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r5c_colliding_public_labels_fold_without_private_keys() -> anyhow::Result<()> {
    let labels = [
        "custom:https://a:s1@h.test/v1?key=K1".to_string(),
        "custom:https://b:s2@h.test/v1?key=K2".to_string(),
    ];
    let (snapshot, all) = surfaces_and_logs(&labels).await?;
    let public = crate::security::redaction::redact_provider_text(&crate::providers::redact_endpoint_userinfo(
        labels.first().context("missing first label")?,
    ));
    assert_eq!(
        public,
        crate::security::redaction::redact_provider_text(&crate::providers::redact_endpoint_userinfo(
            labels.get(1).context("missing second label")?
        ))
    );
    let rows = snapshot
        .get("provider_circuits")
        .context("missing circuit field")?
        .as_object()
        .context("missing circuit field")?;
    let matches = rows
        .iter()
        .filter(|(name, _)| name.starts_with("custom:https://") && name.contains("h.test"))
        .count();
    assert_eq!(matches, 1, "sanitized labels must fold into one row");
    assert_eq!(
        rows.get(&public)
            .context("missing circuit field")?
            .get("opened_total")
            .context("missing circuit field")?,
        2
    );
    for secret in ["s1", "s2", "K1", "K2", "#private_hash"] {
        assert!(!all.contains(secret), "leaked {secret}");
    }
    Ok(())
}

#[tokio::test]
async fn get_status_exposes_redacted_prompt_cache_row() {
    use crate::agent::prompt_cache_metrics::{RoundTrip, Series, record_round_trip};
    let provider = [
        "custom:https://user:",
        "sk-live-",
        "SECRET@example.test/v1?key=",
        "QUERYSECRET",
    ]
    .concat();
    record_round_trip(RoundTrip {
        series: Series::for_turn(None, &crate::agent::turn_request::ToolLoopMemory::none()),
        provider: &provider,
        model: "status-cache-fixture",
        system_prompt_sha256: None,
        tools_sha256: "tools",
        usage: &crate::llm::route_decision::TokenUsage::reported_with_cache(
            Some(1000),
            Some(10),
            Some(1010),
            Some(0),
            Some(800),
        ),
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = crate::config::Config {
        workspace_dir: workspace.path().into(),
        config_path: workspace.path().join("config.toml"),
        ..crate::config::Config::default()
    };
    let state = super::super::test_support::app_state(config);
    let body = serde_json::to_value(get_status(State(state)).await.0).unwrap();
    let rows = body.get("prompt_cache").unwrap().as_array().unwrap();
    let row = rows
        .iter()
        .find(|row| {
            row.get("provider_model")
                .unwrap()
                .as_str()
                .unwrap()
                .ends_with("/status-cache-fixture")
        })
        .unwrap();
    assert_eq!(row.get("requests_total"), Some(&serde_json::json!(1)));
    assert_eq!(row.get("cache_reported_total"), Some(&serde_json::json!(1)));
    assert_eq!(row.get("cache_hit_requests_total"), Some(&serde_json::json!(1)));
    assert_eq!(row.get("cache_read_tokens_total"), Some(&serde_json::json!(800)));
    let text = body.to_string();
    for secret in [["sk-live-", "SECRET"].concat(), "QUERYSECRET".into(), "user:".into()] {
        assert!(!text.contains(&secret), "credential leaked");
    }
}
