#![cfg(test)]

use super::super::*;
use std::{task::Poll, time::Duration};

#[tokio::test]
async fn a_second_physical_attempt_reaches_the_gate_but_expires_while_the_first_is_in_flight() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.cost.prices.insert(
        "openrouter/prx-serial".into(),
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
    let attempt = || PhysicalEstimate {
        purpose: "decision",
        provider: "openrouter",
        model: "prx-serial",
        body: b"{}",
        transport_binding: &binding,
        request_id: "prx-serial",
        deadline,
        remaining_attempts: 2,
    };
    let first = scope.begin_physical(attempt()).await?;
    let reserved = scope.remaining_budgets();
    let capture = super::super::timing::Capture::new();
    let second = capture.scope(Box::pin(scope.begin_physical(attempt()))).await;
    assert!(matches!(second, Err(DecisionError::BudgetExceeded)));
    assert_eq!(scope.remaining_budgets().tokens, reserved.tokens);
    assert_eq!(scope.remaining_budgets().micro_usd, reserved.micro_usd);
    let rows = super::super::transport_tests::journal(workspace.path())?;
    assert_eq!(rows.len(), 2, "the waiting attempt must not write another reservation");
    drop(first);
    cleanup.drain((Instant::now() + Duration::from_secs(5)).into()).await?;
    assert!(
        capture.has_stage("settlement_wait_started"),
        "the real second attempt must reach the serialization gate"
    );
    assert!(
        !capture.has_stage("settlement_wait_acquired"),
        "the in-flight first receipt must retain the gate"
    );
    Ok(())
}

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

#[tokio::test]
async fn request_preparation_overlaps_the_gate_without_authorizing_an_expired_send() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_owned();
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
        [7; 32],
        budgets,
        deadline,
    );
    let remaining = scope.remaining.get_or_init(|| {
        Arc::new(RemainingBudgets {
            tokens: AtomicU64::new(budgets.tokens),
            micro_usd: AtomicU64::new(budgets.micro_usd),
            settlement_gate: Arc::new(tokio::sync::Mutex::new(())),
            settlement_failed: AtomicBool::new(false),
        })
    });
    // Model a prior settlement that cannot finish before this attempt's
    // original 500 ms deadline. No ledger permission is granted by preparation.
    let _pending_settlement = remaining.settlement_gate.lock().await;
    let capture = timing::Capture::new();
    let binding = [0; 32];
    let body = serde_json::json!({"input": "bounded preparation ".repeat(128)}).to_string();
    let result = capture
        .scope(Box::pin(scope.begin_physical(PhysicalEstimate {
            purpose: "decision",
            provider: "openrouter",
            model: "prx-settlement",
            body: body.as_bytes(),
            transport_binding: &binding,
            request_id: "prx-overlap",
            deadline,
            remaining_attempts: 2,
        })))
        .await;
    assert!(matches!(result, Err(DecisionError::BudgetExceeded)));
    cleanup.drain((Instant::now() + Duration::from_secs(5)).into()).await?;
    let path = workspace.path().join("preparation.json");
    capture.write(&path)?;
    let snapshot: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let rows = snapshot
        .get("rows")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("missing accounting timing"))?;
    assert_eq!(rows.len(), 1);
    let stages = rows
        .first()
        .and_then(|row| row.get("stages"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("missing accounting stages"))?;
    let prepared = stages
        .iter()
        .find(|row| row.get("stage").and_then(serde_json::Value::as_str) == Some("input_prepared"))
        .ok_or_else(|| anyhow::anyhow!("request was not prepared while settlement held the gate"))?;
    assert!(
        prepared
            .get("remaining_deadline_ns")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|remaining| remaining > 0)
    );
    assert!(
        !stages
            .iter()
            .any(|row| row.get("stage").and_then(serde_json::Value::as_str) == Some("reserve"))
    );
    assert_eq!(scope.remaining_budgets().tokens, budgets.tokens);
    assert_eq!(scope.remaining_budgets().micro_usd, budgets.micro_usd);
    assert!(!workspace.path().join("state/costs.reservations.jsonl").exists());
    Ok(())
}
