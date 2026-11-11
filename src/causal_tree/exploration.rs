//! Opt-in, deterministic exploration among gated read-only decisions.

use std::collections::BTreeMap;

use super::branch::{CandidateId, CapabilityTarget, CausalActionKind, CausalCandidate, SideEffectClass};
use super::config::{EvaluationConfig, ExplorationMode};
use crate::decision::{DecisionKey, KeyDomain};

/// Candidate facts from the pinned catalog and validated prediction. The
/// policy excludes entire IM/filesystem/shell/session families because their
/// nominal read-only effect may hide delivery, process or child operations.
/// Treat any semantic risk at or above one half as too high for exploration;
/// this conservative local threshold does not depend on a model risk label.
pub(crate) struct CandidateFacts<'a> {
    pub candidate: &'a CausalCandidate,
    pub semantic_risk: f64,
}

const EXPLORATION_RISK_CEILING: f64 = 0.5;

pub(crate) fn eligible(facts: &CandidateFacts<'_>, config: &EvaluationConfig) -> bool {
    config.exploration == ExplorationMode::ReadOnly
        && config.exploration_rate.is_finite()
        && config.exploration_rate > 0.0
        && config.exploration_rate <= 0.10
        && matches!(
            facts.candidate.action(),
            CausalActionKind::Answer
                | CausalActionKind::Retrieve
                | CausalActionKind::Discover
                | CausalActionKind::UseTool
        )
        && facts.candidate.side_effect() == SideEffectClass::ReadOnly
        && !matches!(
            facts.candidate.target(),
            CapabilityTarget::Im | CapabilityTarget::Filesystem | CapabilityTarget::Shell | CapabilityTarget::Session
        )
        && facts.semantic_risk.is_finite()
        && facts.semantic_risk < EXPLORATION_RISK_CEILING
}

/// Draw only after all contenders have passed eligibility and selector checks.
/// The first contender is the deterministic top candidate; input ordering
/// beyond that cannot change the draw or the recorded distribution.
pub(crate) fn choose(
    key: &DecisionKey,
    decision_id: &str,
    top: &CandidateId,
    contenders: &[CandidateId],
    epsilon: f64,
) -> Option<(CandidateId, BTreeMap<CandidateId, f64>)> {
    if !(epsilon.is_finite() && epsilon > 0.0 && epsilon <= 0.10) {
        return None;
    }
    let mut ordered = contenders.to_vec();
    ordered.sort();
    ordered.dedup();
    if ordered.len() < 2 || !ordered.contains(top) {
        return None;
    }
    let n = ordered.len();
    let random = key.hmac(KeyDomain::ExplorationSeed, &[decision_id.as_bytes(), b"coin"]);
    let coin = u64::from_be_bytes(random[..8].try_into().ok()?);
    // Every accepted f64 epsilon in (0, .1] is a binary fraction whose
    // threshold is exact on a 64-bit uniform draw.
    let threshold = (epsilon * (u64::MAX as f64 + 1.0)) as u64;
    if threshold == 0 {
        return None;
    }
    let effective_epsilon = threshold as f64 / (u64::MAX as f64 + 1.0);
    let selected = if coin < threshold {
        // Rejection sampling removes modulo bias for an arbitrary band size.
        let range = u128::from(u64::MAX) + 1;
        let ceiling = range - range % n as u128;
        let mut counter = 0_u64;
        let index = loop {
            let sample = key.hmac(
                KeyDomain::ExplorationSeed,
                &[decision_id.as_bytes(), b"index", &counter.to_be_bytes()],
            );
            let value = u64::from_be_bytes(sample[..8].try_into().ok()?);
            if u128::from(value) < ceiling {
                break usize::try_from(u128::from(value) % n as u128).ok()?;
            }
            counter = counter.checked_add(1)?;
        };
        ordered.get(index)?.clone()
    } else {
        top.clone()
    };
    let uniform = effective_epsilon / n as f64;
    let propensities = ordered
        .into_iter()
        .map(|id| {
            let probability = uniform + if &id == top { 1.0 - effective_epsilon } else { 0.0 };
            (id, probability)
        })
        .collect();
    Some((selected, propensities))
}

#[cfg(test)]
mod tests {
    use super::super::branch::{ApprovalRequirement, CandidateSpec};
    use super::super::state::fixtures;
    use super::*;

    fn candidate(action: CausalActionKind, target: CapabilityTarget, effect: SideEffectClass) -> CausalCandidate {
        CausalCandidate::new(
            action,
            target,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: effect,
                approval: ApprovalRequirement::None,
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        )
    }

    #[test]
    fn exploration_forbidden_for_authority_and_hidden_effects() {
        let config = EvaluationConfig {
            exploration: ExplorationMode::ReadOnly,
            exploration_rate: 0.1,
        };
        let answer = candidate(
            CausalActionKind::Answer,
            CapabilityTarget::None,
            SideEffectClass::ReadOnly,
        );
        assert!(eligible(
            &CandidateFacts {
                candidate: &answer,
                semantic_risk: 0.1
            },
            &config
        ));
        assert!(!eligible(
            &CandidateFacts {
                candidate: &answer,
                semantic_risk: 0.5
            },
            &config
        ));
        assert!(!eligible(
            &CandidateFacts {
                candidate: &answer,
                semantic_risk: f64::NAN
            },
            &config
        ));
        assert!(!eligible(
            &CandidateFacts {
                candidate: &answer,
                semantic_risk: 0.1
            },
            &EvaluationConfig::default()
        ));
        for action in [
            CausalActionKind::Delegate,
            CausalActionKind::Clarify,
            CausalActionKind::RequestApproval,
            CausalActionKind::Refuse,
        ] {
            let value = candidate(action, CapabilityTarget::None, SideEffectClass::ReadOnly);
            assert!(!eligible(
                &CandidateFacts {
                    candidate: &value,
                    semantic_risk: 0.1
                },
                &config
            ));
        }
        for target in [
            CapabilityTarget::Im,
            CapabilityTarget::Filesystem,
            CapabilityTarget::Shell,
            CapabilityTarget::Session,
        ] {
            let value = candidate(CausalActionKind::UseTool, target, SideEffectClass::ReadOnly);
            assert!(!eligible(
                &CandidateFacts {
                    candidate: &value,
                    semantic_risk: 0.1
                },
                &config
            ));
        }
        let write = candidate(
            CausalActionKind::UseTool,
            CapabilityTarget::NativeTool,
            SideEffectClass::LocalWrite,
        );
        assert!(!eligible(
            &CandidateFacts {
                candidate: &write,
                semantic_risk: 0.1
            },
            &config
        ));
    }

    #[test]
    fn sub_draw_resolution_rate_creates_no_false_propensity() {
        let key = fixtures::key();
        let top = CandidateId::new(CausalActionKind::Answer, CapabilityTarget::None, None);
        let other = CandidateId::new(CausalActionKind::Retrieve, CapabilityTarget::Memory, None);
        assert!(choose(&key, "decision-a", &top, &[top.clone(), other], f64::MIN_POSITIVE).is_none());
    }
}
