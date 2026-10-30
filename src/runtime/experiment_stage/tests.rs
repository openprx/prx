#![cfg(test)]
#![allow(clippy::indexing_slicing)]
//! Runtime-owner tests retain the sealed physical accounting boundary.
use crate::skills::lifecycle::experiment::test_specification as specification;
use crate::skills::lifecycle::experiment::*;

#[tokio::test]
async fn experiment_stage_forwards_child_accounting_and_enforces_durable_cancel() -> anyhow::Result<()> {
    use crate::decision::budget::DecisionAttemptReceipt;
    use crate::tools::traits::Tool as _;
    use crate::{
        config::Config,
        runtime::{
            decision_accounting::{AccountingScope, PhysicalEstimate},
            experiment_stage::ExperimentStageRuntime,
            turn_cleanup::CleanupScope,
        },
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio_util::sync::CancellationToken;
    let root = tempfile::tempdir()?;
    let scratch = tempfile::tempdir()?;
    let mut spec = specification();
    spec.created_at = chrono::Utc::now();
    spec.expires_at = spec.created_at + chrono::Duration::seconds(60);
    spec.resources.micro_usd = 500_000;
    spec.stages[0].allocation.micro_usd = 100_000;
    spec.stages[0].allocation.calls = 1;
    spec.stages[0].allocation.tool_calls = 1;
    let plan = spec.freeze()?;
    let store = Arc::new(crate::skills::lifecycle::SkillLifecycleStore::open(
        root.path(),
        plan.spec().owner_scope.clone(),
    )?);
    let mut journal = store.admit_experiment(&plan, chrono::Utc::now())?;
    for (id, event) in [
        (
            "reserve",
            ExperimentEvent::Reserved {
                stage: "proposal".into(),
            },
        ),
        (
            "dispatch",
            ExperimentEvent::Dispatched {
                stage: "proposal".into(),
            },
        ),
    ] {
        journal = store.append_experiment(
            &plan.spec().id,
            plan.digest(),
            journal.next_sequence(),
            &journal.head_sha256,
            id,
            event,
            chrono::Utc::now(),
        )?;
    }
    let mut config = Config {
        workspace_dir: root.path().into(),
        ..Config::default()
    };
    config.cost.prices.insert(
        "openrouter/prx-stage".into(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let cancel = CancellationToken::new();
    let cleanup = CleanupScope::new(None, CancellationToken::new())?;
    let runtime = cleanup
        .scope_admission(async {
            ExperimentStageRuntime::new(&config, &plan, "proposal", cancel.clone(), store.clone())
        })
        .await?;
    let key = crate::decision::DecisionKey::load_or_create(root.path())?;
    cleanup
        .run(runtime.scope(async {
            let mut child_config = config.clone();
            child_config.workspace_dir = scratch.path().into();
            let child = Arc::new(AccountingScope::new(
                Arc::new(child_config),
                key,
                crate::runtime::turn_cleanup::current_tracker().unwrap(),
                [9; 32],
                crate::causal_tree::state::TurnBudgets {
                    tokens: 1024,
                    micro_usd: 10_000,
                    latency_ms: 30_000,
                },
                Instant::now() + Duration::from_secs(30),
            ));
            let binding = [4; 32];
            let estimate = || PhysicalEstimate {
                purpose: "decision",
                provider: "openrouter",
                model: "prx-stage",
                body: br#"{"messages":[{"role":"user","content":"stage"}],"max_tokens":32}"#,
                transport_binding: &binding,
                request_id: "experiment-child",
                deadline: Instant::now() + Duration::from_secs(20),
                remaining_attempts: 1,
            };
            let receipt = child.begin_physical(estimate()).await?;
            Box::new(receipt)
                .settle(crate::decision::protocol::DecisionUsage {
                    input_tokens: Some(5),
                    output_tokens: Some(2),
                    cost_micro_usd: None,
                })
                .await?;
            runtime.flush().await?;
            let usage = crate::cost::tracker::CostTracker::for_workspace(config.cost.clone(), root.path())?
                .physical_scope_usage(&runtime.scope_id())?;
            assert_eq!((usage.calls, usage.settled_calls, usage.tokens), (1, 1, 7));
            assert!(!usage.unresolved);
            assert!(!scratch.path().join("state/costs.reservations.jsonl").exists());
            // Replace-all expansion must be rejected before allocating or
            // touching the file, through the real native edit implementation.
            let file = scratch.path().join("bounded.txt");
            std::fs::write(&file, "a".repeat(100))?;
            let _scratch_root = runtime.register_scratch(scratch.path(), 100)?;
            let edit = crate::tools::file_edit::FileEditTool::new(Arc::new(crate::security::SecurityPolicy {
                workspace_dir: scratch.path().canonicalize()?,
                autonomy: crate::security::AutonomyLevel::Full,
                ..Default::default()
            }));
            let (operation, _, _, _) = crate::security::approval_operation::file_operation(
                "file_edit", &scratch.path().canonicalize()?, &file.canonicalize()?);
            assert!(
                edit.execute(serde_json::json!({"path":"bounded.txt", "old_string":"a",
                "new_string":"x".repeat(1024), "replace_all":true,
                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG: crate::security::policy::ApprovalGrant::for_resource_operation("file_edit", &operation, "test", None)}))
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(file)?, "a".repeat(100));
            assert!(child.begin_physical(estimate()).await.is_err());
            assert!(runtime.claim_tool("shell").is_err());
            assert!(runtime.claim_tool("sessions_spawn").is_err());
            runtime.claim_tool("file_read")?;
            assert!(runtime.claim_tool("file_read").is_err());
            journal = store.append_experiment(
                &plan.spec().id,
                plan.digest(),
                journal.next_sequence(),
                &journal.head_sha256,
                "cancel",
                ExperimentEvent::Cancelled,
                chrono::Utc::now(),
            )?;
            // A spawned provider task lost the task-local but retained its sealed
            // source accounting. The parent still refuses it after durable cancel.
            let join = tokio::spawn(async move {
                child
                    .begin_physical(PhysicalEstimate {
                        purpose: "decision",
                        provider: "openrouter",
                        model: "prx-stage",
                        body: br#"{"messages":[],"max_tokens":32}"#,
                        transport_binding: &[4; 32],
                        request_id: "late-child",
                        deadline: Instant::now() + Duration::from_secs(10),
                        remaining_attempts: 1,
                    })
                    .await
                    .is_err()
            });
            assert!(join.await?);
            Ok(())
        }))
        .await?;
    assert!(journal.state.cancelled);
    Ok(())
}

#[tokio::test]
async fn live_foreign_parent_is_refused_before_physical_admission_and_unparented_scope_redirects() -> anyhow::Result<()>
{
    use crate::decision::budget::DecisionAttemptReceipt;
    use crate::runtime::decision_accounting::{AccountingScope, PhysicalEstimate};
    use crate::runtime::{experiment_stage::ExperimentStageRuntime, turn_cleanup::CleanupScope};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio_util::sync::CancellationToken;
    let root = tempfile::tempdir()?;
    let mut spec = specification();
    spec.created_at = chrono::Utc::now();
    spec.expires_at = spec.created_at + chrono::Duration::seconds(60);
    spec.resources.micro_usd = 500_000;
    spec.max_concurrent = 2;
    for stage in &mut spec.stages {
        stage.dependencies.clear();
        stage.allocation.calls = 1;
        stage.allocation.micro_usd = 100_000;
    }
    let plan = spec.freeze()?;
    let store = Arc::new(crate::skills::lifecycle::SkillLifecycleStore::open(
        root.path(),
        plan.spec().owner_scope.clone(),
    )?);
    let mut journal = store.admit_experiment(&plan, chrono::Utc::now())?;
    for stage in ["proposal", "verify"] {
        for (name, event) in [
            ("reserve", ExperimentEvent::Reserved { stage: stage.into() }),
            ("dispatch", ExperimentEvent::Dispatched { stage: stage.into() }),
        ] {
            journal = store.append_experiment(
                &plan.spec().id,
                plan.digest(),
                journal.next_sequence(),
                &journal.head_sha256,
                &format!("{stage}-{name}"),
                event,
                chrono::Utc::now(),
            )?;
        }
    }
    let mut config = crate::config::Config {
        workspace_dir: root.path().into(),
        ..Default::default()
    };
    config.cost.prices.insert(
        "openrouter/prx-stage".into(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let cancel = CancellationToken::new();
    let cleanup = CleanupScope::new(None, cancel.clone())?;
    let (first, second) = cleanup
        .scope_admission(async {
            Ok::<_, anyhow::Error>((
                ExperimentStageRuntime::new(&config, &plan, "proposal", cancel.clone(), store.clone())?,
                ExperimentStageRuntime::new(&config, &plan, "verify", cancel.clone(), store.clone())?,
            ))
        })
        .await?;
    cleanup
        .run(async {
            let key = crate::decision::DecisionKey::load_or_create(root.path())?;
            let child = || {
                Arc::new(AccountingScope::new(
                    Arc::new(config.clone()),
                    key.clone(),
                    crate::runtime::turn_cleanup::current_tracker().unwrap(),
                    [9; 32],
                    crate::causal_tree::state::TurnBudgets {
                        tokens: 1024,
                        micro_usd: 10_000,
                        latency_ms: 30_000,
                    },
                    Instant::now() + Duration::from_secs(30),
                ))
            };
            // Both admitted owners remain active. Cancellation cannot explain the
            // refusal: this is a retained envelope carrying the wrong live parent.
            let retained = first.scope(async { child() }).await;
            let unparented = child();
            assert_eq!(retained.scope_id_for_tests(), first.scope_id());
            let estimate = || PhysicalEstimate {
                purpose: "evolution_judge",
                provider: "openrouter",
                model: "prx-stage",
                body: br#"{"messages":[],"max_tokens":32}"#,
                transport_binding: &[4; 32],
                request_id: "wrong-parent",
                deadline: Instant::now() + Duration::from_secs(20),
                remaining_attempts: 1,
            };
            first.ensure_active()?;
            second.ensure_active()?;
            let refused = second.scope(retained.begin_physical(estimate())).await;
            assert!(matches!(refused, Err(crate::decision::DecisionError::BudgetExceeded)));
            let tracker = crate::cost::tracker::CostTracker::for_workspace(config.cost.clone(), root.path())?;
            for runtime in [&first, &second] {
                let usage = tracker.physical_scope_usage(&runtime.scope_id())?;
                assert_eq!((usage.calls, usage.settled_calls), (0, 0));
                assert!(usage.attempt_ids.is_empty() && !usage.unresolved);
            }
            assert!(!root.path().join("state/costs.reservations.jsonl").exists());
            // The existing no-parent redirect is still legal and settles only in
            // the current second owner; same-parent forwarding has its own test.
            let receipt = second.scope(unparented.begin_physical(estimate())).await?;
            Box::new(receipt)
                .settle(crate::decision::protocol::DecisionUsage {
                    input_tokens: Some(5),
                    output_tokens: Some(2),
                    cost_micro_usd: None,
                })
                .await?;
            second.flush().await?;
            let actual = tracker.physical_scope_usage(&second.scope_id())?;
            assert_eq!((actual.calls, actual.settled_calls, actual.tokens), (1, 1, 7));
            assert_eq!(actual.attempt_ids.len(), 1);
            assert!(!actual.unresolved);
            assert_eq!(tracker.physical_scope_usage(&first.scope_id())?.calls, 0);
            Ok(())
        })
        .await
}
