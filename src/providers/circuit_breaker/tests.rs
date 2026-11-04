//! Paused-clock state, lease and stale-permit regression witnesses.
use super::*;
use anyhow::Context as _;
use tokio::time::advance;

const fn params(threshold: u32) -> BreakerParams {
    BreakerParams {
        failure_threshold: threshold,
        open_for: Duration::from_secs(30),
        max_open_for: Duration::from_secs(300),
    }
}
fn fixture() -> (Arc<CircuitBreakerRegistry>, Arc<str>, ChainLease) {
    let registry = Arc::new(CircuitBreakerRegistry::default());
    let key = Arc::<str>::from("prx_provider");
    let lease = registry.register([(Arc::clone(&key), Arc::clone(&key))]);
    (registry, key, lease)
}
fn permit(registry: &Arc<CircuitBreakerRegistry>, key: &Arc<str>, threshold: u32) -> anyhow::Result<AttemptPermit> {
    registry
        .admit(key, params(threshold))
        .map_err(|ms| anyhow::anyhow!("unexpected circuit refusal ({ms}ms)"))?
        .context("missing attempt permit")
}
fn fail(registry: &Arc<CircuitBreakerRegistry>, key: &Arc<str>, threshold: u32) -> anyhow::Result<()> {
    permit(registry, key, threshold)?.complete(BreakerOutcome::Failure);
    Ok(())
}
fn row(registry: &CircuitBreakerRegistry) -> anyhow::Result<ProviderCircuitStats> {
    registry
        .snapshot()
        .into_values()
        .next()
        .context("missing circuit snapshot row")
}

#[tokio::test(start_paused = true)]
async fn r1a_threshold_opens_on_fifth_failure() -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    for _ in 0..4 {
        fail(&r, &k, 5)?;
    }
    assert_eq!(row(&r)?.state, "closed");
    assert_eq!(row(&r)?.consecutive_failures, 4);
    fail(&r, &k, 5)?;
    assert_eq!(row(&r)?.state, "open");
    assert_eq!(row(&r)?.opened_total, 1);
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1bc_deadline_and_single_probe() -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    fail(&r, &k, 1)?;
    assert!(r.admit(&k, params(1)).is_err());
    advance(Duration::from_millis(29_999)).await;
    assert!(r.admit(&k, params(1)).is_err());
    advance(Duration::from_millis(1)).await;
    let probe = permit(&r, &k, 1)?;
    assert!(matches!(probe.kind, PermitKind::Probe(_)));
    assert!(r.admit(&k, params(1)).is_err());
    assert_eq!(row(&r)?.state, "half_open");
    probe.complete(BreakerOutcome::Answered);
    assert_eq!(row(&r)?.state, "closed");
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1de_probe_backoff_doubles_caps_and_resets() -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    fail(&r, &k, 1)?;
    let mut wait = 30;
    for expected in [60, 120, 240, 300, 300] {
        advance(Duration::from_secs(wait)).await;
        fail(&r, &k, 1)?;
        assert_eq!(row(&r)?.retry_in_ms, Some(expected * 1000));
        wait = expected;
    }
    advance(Duration::from_secs(wait)).await;
    permit(&r, &k, 1)?.complete(BreakerOutcome::Answered);
    assert_eq!(row(&r)?.consecutive_failures, 0);
    fail(&r, &k, 1)?;
    assert_eq!(row(&r)?.retry_in_ms, Some(30_000));
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1f_abandoned_probe_releases_slot() -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    fail(&r, &k, 1)?;
    advance(Duration::from_secs(30)).await;
    drop(permit(&r, &k, 1)?);
    let replacement = r.admit(&k, params(1));
    assert!(replacement.is_ok(), "abandoned probe must release its slot");
    replacement
        .map_err(|ms| anyhow::anyhow!("unexpected refusal ({ms}ms)"))?
        .context("missing replacement")?
        .complete(BreakerOutcome::NotSent);
    let _replacement = permit(&r, &k, 1)?;
    assert!(r.admit(&k, params(1)).is_err());
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1g_answered_resets_closed_failures() -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    for _ in 0..4 {
        fail(&r, &k, 5)?;
    }
    permit(&r, &k, 5)?.complete(BreakerOutcome::Answered);
    for _ in 0..4 {
        fail(&r, &k, 5)?;
        assert_eq!(
            row(&r)?.state,
            "closed",
            "answered response must reset consecutive failures"
        );
    }
    assert_eq!(row(&r)?.state, "closed");
    assert_eq!(row(&r)?.consecutive_failures, 4);
    Ok(())
}

async fn stale_epoch_case(outcome: BreakerOutcome) -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    // A's outcome is delivered across a barrier only after the entire new cycle.
    let stale = permit(&r, &k, 2)?;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let ready = Arc::clone(&barrier);
    let task = tokio::spawn(async move {
        ready.wait().await;
        stale.complete(outcome);
    });
    fail(&r, &k, 2)?;
    fail(&r, &k, 2)?;
    advance(Duration::from_secs(30)).await;
    permit(&r, &k, 2)?.complete(BreakerOutcome::Answered);
    fail(&r, &k, 2)?;
    assert_eq!(row(&r)?.consecutive_failures, 1);
    barrier.wait().await;
    task.await?;
    assert_eq!(
        row(&r)?.consecutive_failures,
        1,
        "stale epoch outcome must preserve count 1"
    );
    assert_eq!(row(&r)?.state, "closed");
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1h_success_stale_epoch_cannot_reset_new_closed_count() -> anyhow::Result<()> {
    stale_epoch_case(BreakerOutcome::Answered).await?;
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1h_failure_stale_epoch_cannot_increment_new_closed_count() -> anyhow::Result<()> {
    stale_epoch_case(BreakerOutcome::Failure).await?;
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1h_open_stale_success_preserves_deadline() -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    let stale = permit(&r, &k, 2)?;
    fail(&r, &k, 2)?;
    fail(&r, &k, 2)?;
    let before = row(&r)?.retry_in_ms;
    stale.complete(BreakerOutcome::Answered);
    assert_eq!(row(&r)?.state, "open");
    assert_eq!(row(&r)?.retry_in_ms, before);
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1i_old_failure_does_not_complete_new_probe() -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    let stale = permit(&r, &k, 2)?;
    fail(&r, &k, 2)?;
    fail(&r, &k, 2)?;
    advance(Duration::from_secs(30)).await;
    let probe = permit(&r, &k, 2)?;
    stale.complete(BreakerOutcome::Failure);
    assert!(r.admit(&k, params(2)).is_err());
    probe.complete(BreakerOutcome::Answered);
    assert_eq!(row(&r)?.state, "closed");
    Ok(())
}
fn duplicate_drop_path(p: &AttemptPermit) -> AttemptPermit {
    // Only Drop is replayed. Never complete a permit twice.
    AttemptPermit {
        registry: Arc::clone(&p.registry),
        key: Arc::clone(&p.key),
        incarnation: p.incarnation,
        epoch: p.epoch,
        kind: p.kind,
        params: p.params,
    }
}
#[tokio::test(start_paused = true)]
async fn r1j_stale_release_cannot_free_replacement_probe() -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    fail(&r, &k, 1)?;
    advance(Duration::from_secs(30)).await;
    let p1 = permit(&r, &k, 1)?;
    let stale_release = duplicate_drop_path(&p1);
    drop(p1);
    let p2 = permit(&r, &k, 1)?;
    drop(stale_release);
    assert!(
        r.admit(&k, params(1)).is_err(),
        "stale token must not release replacement"
    );
    p2.complete(BreakerOutcome::Answered);
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1l_generations_share_health_and_use_probe_chain_params() -> anyhow::Result<()> {
    let (r, k, lease1) = fixture();
    let lease2 = r.register([(Arc::clone(&k), Arc::clone(&k))]);
    fail(&r, &k, 2)?;
    fail(&r, &k, 2)?;
    assert!(r.admit(&k, params(5)).is_err());
    drop(lease1);
    advance(Duration::from_secs(30)).await;
    let mut short = params(5);
    short.max_open_for = Duration::from_secs(40);
    r.admit(&k, short)
        .map_err(|ms| anyhow::anyhow!("unexpected refusal ({ms}ms)"))?
        .context("missing probe")?
        .complete(BreakerOutcome::Failure);
    assert_eq!(row(&r)?.retry_in_ms, Some(40_000));
    advance(Duration::from_secs(40)).await;
    permit(&r, &k, 5)?.complete(BreakerOutcome::Answered);
    assert!(r.admit(&k, params(2)).is_ok());
    drop(lease2);
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1m_retired_generations_expire_without_visiting_live_keys() -> anyhow::Result<()> {
    let (r, _k, _live) = fixture();
    for index in 0..50 {
        let key = Arc::<str>::from(format!("retired_{index}"));
        let lease = r.register([(Arc::clone(&key), Arc::clone(&key))]);
        fail(&r, &key, 1)?;
        drop(lease);
        assert!(r.inner.lock().retired.len() <= MAX_RETIRED_ENTRIES);
        assert!(r.inner.lock().maintenance_rows <= MAX_RETIRED_ENTRIES);
    }
    advance(Duration::from_secs(300)).await;
    assert_eq!(r.snapshot().len(), 1);
    assert_eq!(r.inner.lock().entries.len(), 1);
    assert_eq!(r.snapshot().keys().collect::<Vec<_>>(), vec!["prx_provider"]);
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1n_retired_cap_evicts_oldest_and_never_live_entries() -> anyhow::Result<()> {
    let (r, live, _lease) = fixture();
    fail(&r, &live, 1)?;
    for index in 0..300 {
        let key = Arc::<str>::from(format!("retired_{index}"));
        let lease = r.register([(Arc::clone(&key), Arc::clone(&key))]);
        fail(&r, &key, 1)?;
        drop(lease);
    }
    let state = r.inner.lock();
    assert!(
        state.entries.contains_key(&live),
        "live-leased circuit must never be evicted"
    );
    assert_eq!(state.retired.len(), 256);
    assert_eq!(state.entries.len(), 257);
    assert!(!state.entries.contains_key("retired_0"));
    assert!(!state.entries.contains_key("retired_43"));
    assert!(state.entries.contains_key("retired_44"));
    assert!(state.entries.contains_key(&live));
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1o_reload_keeps_open_protection_before_deadline() -> anyhow::Result<()> {
    let (r, k, lease) = fixture();
    fail(&r, &k, 1)?;
    drop(lease);
    let _new = r.register([(Arc::clone(&k), Arc::clone(&k))]);
    assert!(r.admit(&k, params(1)).is_err());
    Ok(())
}
async fn recreated_case(outcome: BreakerOutcome) -> anyhow::Result<()> {
    let (r, k, lease) = fixture();
    let stale = permit(&r, &k, 2)?;
    let old_incarnation = stale.incarnation;
    fail(&r, &k, 2)?;
    fail(&r, &k, 2)?;
    drop(lease);
    advance(Duration::from_secs(300)).await;
    assert!(r.snapshot().is_empty());
    assert_eq!(r.inner.lock().entries.len(), 0);
    let _new = r.register([(Arc::clone(&k), Arc::clone(&k))]);
    assert_ne!(
        r.inner
            .lock()
            .entries
            .get(&k)
            .context("missing recreated entry")?
            .incarnation,
        old_incarnation
    );
    fail(&r, &k, 2)?;
    stale.complete(outcome);
    assert_eq!(
        row(&r)?.consecutive_failures,
        1,
        "old incarnation outcome must preserve count 1"
    );
    assert_eq!(row(&r)?.state, "closed");
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1p_success_stale_incarnation_cannot_reset_recreated_count() -> anyhow::Result<()> {
    recreated_case(BreakerOutcome::Answered).await?;
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1p_failure_stale_incarnation_cannot_increment_recreated_count() -> anyhow::Result<()> {
    recreated_case(BreakerOutcome::Failure).await?;
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn r1q_stale_incarnation_release_cannot_free_recreated_token_one() -> anyhow::Result<()> {
    let (r, k, lease) = fixture();
    fail(&r, &k, 2)?;
    fail(&r, &k, 2)?;
    advance(Duration::from_secs(30)).await;
    let p1 = permit(&r, &k, 2)?;
    assert!(matches!(p1.kind, PermitKind::Probe(1)));
    drop(lease);
    assert!(r.snapshot().is_empty());
    assert_eq!(r.inner.lock().entries.len(), 0);
    let _new = r.register([(Arc::clone(&k), Arc::clone(&k))]);
    fail(&r, &k, 2)?;
    fail(&r, &k, 2)?;
    advance(Duration::from_secs(30)).await;
    let p2 = permit(&r, &k, 2)?;
    assert!(matches!(p2.kind, PermitKind::Probe(1)));
    drop(p1);
    assert!(
        r.admit(&k, params(2)).is_err(),
        "old incarnation drop must not free token 1"
    );
    p2.complete(BreakerOutcome::Answered);
    assert_eq!(row(&r)?.state, "closed");
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn incarnation_overflow_admits_new_keys_without_reusing_ids() -> anyhow::Result<()> {
    let (r, k, _lease) = fixture();
    r.inner.lock().incarnation = u64::MAX;
    let unknown = Arc::<str>::from("overflow_provider");
    let _lease2 = r.register([(Arc::clone(&unknown), Arc::clone(&unknown))]);
    assert!(
        r.admit(&unknown, params(1))
            .map_err(|ms| anyhow::anyhow!("unexpected refusal ({ms}ms)"))?
            .is_none()
    );
    assert!(
        r.admit(&k, params(1))
            .map_err(|ms| anyhow::anyhow!("unexpected refusal ({ms}ms)"))?
            .is_some()
    );
    assert_eq!(r.snapshot().len(), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn terminal_epoch_never_issues_a_reusable_permit() -> anyhow::Result<()> {
    let (registry, key, _lease) = fixture();
    registry
        .inner
        .lock()
        .entries
        .get_mut(&key)
        .context("missing live entry")?
        .epoch = u64::MAX;
    assert!(registry.admit(&key, params(1)).is_err());
    assert_eq!(
        registry
            .inner
            .lock()
            .entries
            .get(&key)
            .context("missing live entry")?
            .permits,
        0
    );
    Ok(())
}
