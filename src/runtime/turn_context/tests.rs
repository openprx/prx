//! R22: the execution-owned turn runtime context carries the per-turn request
//! parameters, the pinned provider catalog, the G6 budget and the recursive
//! money without a decision service.

use super::*;
use crate::agent::test_admission::AdmittedTurn;
use crate::cost::decision_attempt::{DecisionAttemptReceipt, DecisionUsage};
use crate::runtime::decision_accounting::PhysicalEstimate;
use crate::runtime::turn_cleanup::CleanupTracker;
use std::time::{Duration, Instant};

const PROVIDER: &str = "openrouter";
const MODEL: &str = "prx-turn-context";

fn priced_config(workspace: &std::path::Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.cost.prices.insert(
        format!("{PROVIDER}/{MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config
}

/// The value `TurnDecisionContext::remaining_g6_tokens` returned before the
/// move: the opening slice of the decision's trusted state narrowed by the
/// accounting scope.
fn pre_move_remaining_g6_tokens(runtime: &TurnRuntimeContext) -> u64 {
    let trusted = runtime.trusted.budgets.tokens;
    runtime
        .accounting
        .as_ref()
        .map_or(trusted, |scope| trusted.min(scope.remaining_budgets().tokens))
}

#[tokio::test]
async fn turn_context_carries_parameters_catalog_budgets_and_recursive_money_without_decision_service()
-> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let cleanup = Arc::new(CleanupTracker::default());
    let runtime = cleanup
        .scope(Box::pin(async {
            let admitted =
                AdmittedTurn::with_config(priced_config(workspace.path()), "hello", Arc::new(Vec::new())).await;
            let decision = admitted.decision();
            let runtime = Arc::clone(decision.context.runtime());
            let proof = decision.proof;

            // Recursive money: a turn with an accounting scope funds its
            // children from that scope's money owner, issued once.
            let scope = runtime
                .accounting
                .clone()
                .ok_or_else(|| anyhow::anyhow!("an opened turn under a cleanup owner has an accounting scope"))?;
            let issued = runtime
                .issue_recursive_allowance(proof)
                .ok_or_else(|| anyhow::anyhow!("a root turn issues its recursive allowance"))?;
            assert!(Arc::ptr_eq(&issued.money(), &scope.money_owner()));
            let again = runtime
                .issue_recursive_allowance(proof)
                .ok_or_else(|| anyhow::anyhow!("the allowance is retained"))?;
            assert!(Arc::ptr_eq(&issued, &again), "the allowance is issued once per turn");
            assert!(runtime.recursive_allowance().is_some());

            // Publication money funds a child publication through the holder
            // alone; no decision service is consulted.
            let money = Arc::new(AtomicU64::new(30_000));
            let holder = TurnRuntimeHolder::new(
                runtime.holder.recovery_scope(),
                Some(Arc::clone(&money)),
                None,
                &priced_config(workspace.path()),
                &crate::security::SecurityPolicy::default(),
            );
            let funded = holder
                .recursive_money(proof)
                .ok_or_else(|| anyhow::anyhow!("publication money funds recursive children"))?;
            assert!(Arc::ptr_eq(&funded, &money));
            assert!(holder.spend_ceiling(proof)?.live()? <= 30_000);

            // G6: spend real tokens on the turn's one accounting scope.
            let binding = [0; 32];
            let before = runtime.remaining_g6_tokens();
            let receipt = scope
                .begin_physical(PhysicalEstimate {
                    purpose: "decision",
                    provider: PROVIDER,
                    model: MODEL,
                    body: b"{}",
                    transport_binding: &binding,
                    request_id: "prx-turn-context",
                    deadline: Instant::now() + Duration::from_secs(5),
                    remaining_attempts: 1,
                })
                .await?;
            Box::new(receipt)
                .settle(DecisionUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(2),
                    ..Default::default()
                })
                .await?;
            assert!(runtime.remaining_g6_tokens() < before, "the scope spent tokens");
            assert_eq!(runtime.remaining_g6_tokens(), pre_move_remaining_g6_tokens(&runtime));
            drop(admitted);
            Ok::<_, anyhow::Error>(runtime)
        }))
        .await?;
    cleanup.drain((Instant::now() + Duration::from_secs(5)).into()).await?;

    // The decision service and its context are gone; the runtime context
    // still carries the request parameters, the pinned catalog and budgets.
    let catalog_revision = runtime.model_catalog().map(|catalog| catalog.revision.clone());
    assert_eq!(
        catalog_revision,
        runtime.model_catalog.as_deref().map(|catalog| catalog.revision.clone())
    );
    let capture = runtime.capture_main_parameters(true);
    assert_eq!(runtime.prepared_main_parameters(), capture.snapshot());
    assert_eq!(runtime.remaining_g6_tokens(), pre_move_remaining_g6_tokens(&runtime));
    runtime.retire_recursive_allowance();
    Ok(())
}

/// R24: the rehomed turn-origin, retrieval-provenance and settlement items
/// keep refusing exactly what they refused before the move: a proof from
/// another publication, a drifted surface or model, and a foreign context.
#[tokio::test]
async fn rehomed_security_items_preserve_refusal() -> anyhow::Result<()> {
    let first_dir = tempfile::tempdir()?;
    let second_dir = tempfile::tempdir()?;
    let first = AdmittedTurn::empty(first_dir.path(), "hello").await;
    let second = AdmittedTurn::empty(second_dir.path(), "hello").await;
    let one = first.decision();
    let two = second.decision();

    // Turn origin: the pinned generation and surface admit their own proof only.
    validate_agent_turn_origin(&one.service.generation, one.service.surface, one.proof)?;
    assert_eq!(
        validate_agent_turn_origin(&one.service.generation, one.service.surface, two.proof),
        Err(TurnOriginMismatch)
    );
    let drifted = if one.service.surface == TurnSurfaceKind::Worker {
        TurnSurfaceKind::Interactive
    } else {
        TurnSurfaceKind::Worker
    };
    assert_eq!(
        validate_agent_turn_origin(&one.service.generation, drifted, one.proof),
        Err(TurnOriginMismatch)
    );

    // Retrieval provenance: the pinned trusted scope never admits another
    // turn's proof or a different serving model.
    let trusted = &one.context.runtime().trusted;
    let policy = trusted.policy_version.clone();
    assert_eq!(
        validate_agent_retrieval_provenance(two.proof, trusted, &trusted.resolved_model, &policy, &[], None),
        Err(TurnOriginMismatch)
    );
    assert_eq!(
        validate_agent_retrieval_provenance(one.proof, trusted, "prx-other-model", &policy, &[], None),
        Err(TurnOriginMismatch)
    );

    // Settlement: only the token holder issues a terminal link, and never
    // for a context another publication opened.
    assert!(matches!(
        one.service.terminal_settlement_ref(Arc::clone(two.context)),
        Err(crate::runtime::turn_decision::SettlementRefUnavailable)
    ));
    assert!(matches!(
        two.service.settlement_ref(Arc::clone(one.context)),
        Err(crate::runtime::turn_decision::SettlementRefUnavailable)
    ));
    Ok(())
}

/// R22: a child turn's recursive money is funded only through the incoming
/// parent grant. The funded owner is the grant's recursive child allowance,
/// the turn's spend ceiling never exceeds it, and an exhausted grant leaves
/// the child nothing to spend.
#[tokio::test]
async fn child_turn_context_funds_recursive_money_only_through_the_parent_grant() -> anyhow::Result<()> {
    use anyhow::Context as _;
    let child = crate::agent::turn_tests::engine_switch::admit_bound_child_segment().await;
    let result = (|| -> anyhow::Result<()> {
        let routing = child.turn.routing();
        let holder = &routing.runtime.holder;
        let grant = holder
            .parent_grant()
            .context("a bound child publication holds its parent grant")?;
        let granted = grant
            .recursive_child_allowance()
            .map(|owner| owner.money())
            .context("the parent grant funds a recursive child allowance")?;
        let funded = holder
            .recursive_money(routing.proof)
            .context("the child turn's recursive money")?;
        assert!(
            Arc::ptr_eq(&funded, &granted),
            "recursive money is the parent grant's allowance"
        );
        assert!(holder.spend_ceiling(routing.proof)?.live()? <= granted.load(Ordering::Acquire));
        let saved = granted.swap(0, Ordering::AcqRel);
        let exhausted = holder.spend_ceiling(routing.proof)?.live()?;
        granted.store(saved, Ordering::Release);
        assert_eq!(exhausted, 0, "an exhausted grant leaves nothing to spend");
        Ok(())
    })();
    child.shutdown().await;
    result
}
