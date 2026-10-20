#![cfg(test)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::super::{BootstrapProfile, RuntimeBootstrap};
use crate::agent::entry::{test_cli_agent_turn_proof, test_recorded_cli_agent_turn_proof};
use crate::causal_tree::directive::TurnExecutionDirective;
use crate::decision::factory::ServedBy;
use crate::runtime::turn_decision::{AgentTurnKind, TurnDisposition};

/// G-07 runs this exact selector in each feature configuration. In particular,
/// this module is not gated on llm-router: the published decision service must
/// perform a real decision even when model routing is not compiled.
#[tokio::test]
async fn decision_service_present_in_every_model_turn_build() {
    for enabled in [false, true] {
        let workspace = tempfile::tempdir().expect("feature contract workspace");
        let mut config = super::test_config(workspace.path());
        config.causal_tree.enabled = enabled;
        config.memory.backend = "sqlite".to_owned();
        let context = RuntimeBootstrap::build(config, BootstrapProfile::AgentLoop)
            .await
            .expect("actual AgentLoop bootstrap");
        let service = context
            .turn_decisions
            .as_ref()
            .expect("model-turn bootstrap must publish the decision service");
        assert_eq!(service.status().durable_key, enabled);
        assert_eq!(service.status().recordable, enabled);
        let proof = if enabled {
            test_recorded_cli_agent_turn_proof(
                &context.config_generation,
                Arc::clone(context.memory.as_ref().expect("bootstrap memory")),
            )
            .await
        } else {
            test_cli_agent_turn_proof(&context.config_generation, "ollama", "prx-local-model")
        };
        let cancellation = CancellationToken::new();
        let decision = Box::pin(service.decide_agent_initial(&proof, AgentTurnKind::SingleShot, &cancellation))
            .await
            .expect("published service must return a source-bound directive");
        assert!(matches!(
            decision.directive(),
            TurnExecutionDirective::AnswerDirectly
                | TurnExecutionDirective::Refuse { .. }
                | TurnExecutionDirective::Clarify { .. }
                | TurnExecutionDirective::RequestApproval { .. }
        ));
        assert_eq!(
            decision
                .engine()
                .prediction
                .as_ref()
                .map(|prediction| prediction.served_by),
            Some(ServedBy::Builtin),
            "fallback_cause={:?}, elapsed_ms={}",
            decision.engine().fallback_cause,
            decision.engine().elapsed_ms
        );
        assert!(!decision.engine().pre_commit.outcomes.is_empty());
        assert!(matches!(
            decision.disposition(),
            Ok(TurnDisposition::Run | TurnDisposition::Close { .. })
        ));
        assert_eq!(decision.hop_ids().last(), Some(&decision.decision_id()));
        assert!(service.drain_writes(Instant::now() + Duration::from_secs(2)).await);
        let key = workspace.path().join("self/causal_tree/.decision_key");
        let store = workspace.path().join("self/causal_tree/decisions.db");
        assert_eq!(key.exists(), enabled, "off publication must not create a key");
        assert_eq!(store.exists(), enabled, "off decision must not create a store");
        // Source/context/service owners leave scope before their temporary DB.
        drop(decision);
        drop(proof);
        drop(context);
    }
}
