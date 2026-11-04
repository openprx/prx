//! Live provider circuits remain alongside rate-limit telemetry.
use crate::providers::circuit_breaker::{BreakerOutcome, BreakerParams, shared_registry};
use anyhow::Context as _;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn r5a_snapshot_json_keeps_rate_limits_and_live_open_circuits() -> anyhow::Result<()> {
    let registry = shared_registry();
    let key = Arc::<str>::from("prx_health_circuit_r5a");
    let _lease = registry.register([(Arc::clone(&key), Arc::clone(&key))]);
    let params = BreakerParams {
        failure_threshold: 1,
        open_for: Duration::from_secs(30),
        max_open_for: Duration::from_secs(300),
    };
    registry
        .admit(&key, params)
        .map_err(|ms| anyhow::anyhow!("unexpected refusal ({ms}ms)"))?
        .context("missing permit")?
        .complete(BreakerOutcome::Failure);
    let snapshot = super::snapshot_json();
    assert!(snapshot.get("rate_limits").is_some());
    let row = snapshot
        .get("provider_circuits")
        .context("missing circuits")?
        .get(key.as_ref())
        .context("missing provider")?;
    assert_eq!(row.get("state").context("missing state")?, "open");
    Ok(())
}
