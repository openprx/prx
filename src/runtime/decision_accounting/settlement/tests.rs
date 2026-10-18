use super::super::*;
use std::{task::Poll, time::Duration};

#[tokio::test]
async fn a_blocked_settlement_overlaps_admission_but_keeps_its_hold_and_cleanup_owner() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.cost.prices.insert(
        "openrouter/prx-settlement".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let cleanup = Arc::new(CleanupTracker::default());
    let budgets = TurnBudgets {
        tokens: 1024,
        micro_usd: 5000,
        latency_ms: 500,
    };
    let deadline = Instant::now() + Duration::from_millis(budgets.latency_ms);
    let scope = AccountingScope::new(
        Arc::new(config),
        DecisionKey::ephemeral(),
        Arc::clone(&cleanup),
        [3; 32],
        budgets,
        deadline,
    );
    let binding = [0; 32];
    let attempt = || PhysicalEstimate {
        purpose: "decision",
        provider: "openrouter",
        model: "prx-settlement",
        body: b"{}",
        transport_binding: &binding,
        request_id: "prx-settlement",
        deadline,
        remaining_attempts: 2,
    };
    let receipt = scope.begin_physical(attempt()).await?;
    let ledger_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(workspace.path().join("state/costs.jsonl.lock"))?;
    ledger_lock.lock()?;
    let mut next_send = Box::pin(scope.begin_physical(attempt()));
    assert!(
        futures::poll!(next_send.as_mut()).is_pending(),
        "another physical attempt must wait for durable settlement"
    );
    let mut settle = Box::new(receipt).settle(DecisionUsage {
        input_tokens: Some(10),
        output_tokens: Some(2),
        ..Default::default()
    });
    assert!(
        matches!(futures::poll!(settle.as_mut()), Poll::Ready(Ok(()))),
        "semantic admission must not wait on the blocked ledger write"
    );
    drop(settle);
    assert!(
        futures::poll!(next_send.as_mut()).is_pending(),
        "another send stays blocked while the handed-off settlement is not durable"
    );
    assert_eq!(scope.remaining_budgets().tokens, 1012);
    let before = super::super::transport_tests::journal(workspace.path())?;
    assert_eq!(before.len(), 2);
    assert_eq!(
        before
            .last()
            .and_then(|row| row.get("event"))
            .and_then(serde_json::Value::as_str),
        Some("dispatched")
    );
    drop(next_send);
    let mut drain = Box::pin(cleanup.drain((Instant::now() + Duration::from_secs(5)).into()));
    assert!(
        futures::poll!(drain.as_mut()).is_pending(),
        "cleanup must continue owning the blocked write"
    );
    ledger_lock.unlock()?;
    drain.await?;
    let after = super::super::transport_tests::journal(workspace.path())?;
    assert_eq!(
        after
            .last()
            .and_then(|row| row.get("event"))
            .and_then(serde_json::Value::as_str),
        Some("estimated_settled_v1")
    );
    Ok(())
}

#[tokio::test]
async fn an_unconfirmed_background_settlement_closes_further_accounting() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.cost.prices.insert(
        "openrouter/prx-settlement".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let cleanup = Arc::new(CleanupTracker::default());
    let budgets = TurnBudgets {
        tokens: 1024,
        micro_usd: 5000,
        latency_ms: 500,
    };
    let deadline = Instant::now() + Duration::from_millis(budgets.latency_ms);
    let scope = AccountingScope::new(
        Arc::new(config),
        DecisionKey::ephemeral(),
        Arc::clone(&cleanup),
        [4; 32],
        budgets,
        deadline,
    );
    let binding = [0; 32];
    let receipt = scope
        .begin_physical(PhysicalEstimate {
            purpose: "decision",
            provider: "openrouter",
            model: "prx-settlement",
            body: b"{}",
            transport_binding: &binding,
            request_id: "prx-settlement",
            deadline,
            remaining_attempts: 1,
        })
        .await?;
    std::fs::write(
        workspace.path().join("state/costs.reservations.jsonl"),
        "invalid transition\n",
    )?;
    Box::new(receipt).settle_provider_usage(crate::llm::route_decision::TokenUsage {
        prompt_tokens: Some(10),
        completion_tokens: Some(2),
        source: TokenUsageSource::Reported,
        ..Default::default()
    })?;
    assert!(
        cleanup
            .drain((Instant::now() + Duration::from_secs(5)).into())
            .await
            .is_err()
    );
    assert_eq!(scope.remaining_budgets().tokens, 0);
    assert_eq!(scope.remaining_budgets().micro_usd, 0);
    assert!(
        scope
            .begin_physical(PhysicalEstimate {
                purpose: "decision",
                provider: "openrouter",
                model: "prx-settlement",
                body: b"{}",
                transport_binding: &binding,
                request_id: "prx-after-failure",
                deadline,
                remaining_attempts: 1,
            })
            .await
            .is_err()
    );
    Ok(())
}
