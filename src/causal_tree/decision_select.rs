//! Deterministic selection after the pre-commit hard gates.
//!
//! A low-confidence or zero-mass prediction never widens the execution
//! surface. Retrieval and discovery are explicit recovery paths, not tool
//! execution.

use std::collections::{BTreeMap, BTreeSet};

use super::branch::{CandidateId, CapabilityTarget, CausalActionKind, CausalCandidate, OpId, SideEffectClass};
use super::candidate_generator::{CandidateSet, PendingApproval};
use super::config::{CausalPolicy, EvaluationConfig};
use super::decision_score::{CandidateScore, PredictionEvidence};
use super::exploration::{self, CandidateFacts};
use super::gates::{GateOutcome, GateReport};
use crate::decision::DecisionKey;

/// A selected working candidate or an explicit terminal disposition.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Selection {
    CollectApprovalArguments(super::approval_collection::DeferredArgumentProof),
    Commit {
        candidate_id: CandidateId,
        /// One when exploration is off; the exploration module can replace
        /// this only among eligible read-only candidates.
        propensity: f64,
    },
    Recover {
        candidate_id: CandidateId,
        reason_code: &'static str,
    },
    RequestApproval {
        terminal_id: CandidateId,
        pending_candidate_id: CandidateId,
        op_ids: Vec<OpId>,
        reason_code: &'static str,
    },
    Abstain {
        action: CausalActionKind,
        reason_code: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SelectError {
    #[error("selection inputs disagree with the admitted candidate set")]
    CandidateMismatch,
    #[error("selection received a non-finite utility")]
    NonFinite,
    #[error("no admitted terminal candidate passed the pre-commit gates")]
    NoTerminalPath,
}

/// Facts pinned with the same catalog and turn as the gate report. The
/// selector cannot discover or refresh a capability by itself.
pub(crate) struct SelectorContext {
    pub interactive: bool,
    /// Exact direct entries in the pinned catalog generation.
    pub direct_candidates: BTreeSet<CandidateId>,
}

pub(crate) struct CausalSelector;

impl CausalSelector {
    /// Preserve the deterministic selector contract when exploration is off.
    pub(crate) fn select_with_exploration(
        admitted: &CandidateSet,
        scores: &BTreeMap<CandidateId, CandidateScore>,
        prediction: &PredictionEvidence,
        pre_commit: &GateReport,
        policy: &CausalPolicy,
        context: &SelectorContext,
        evaluation: &EvaluationConfig,
        key: &DecisionKey,
        decision_id: &str,
    ) -> Result<(Selection, BTreeMap<CandidateId, f64>), SelectError> {
        let base = Self::select(admitted, scores, prediction, pre_commit, policy, context)?;
        let Selection::Abstain {
            reason_code: "commit_threshold_or_margin",
            ..
        } = &base
        else {
            let mut distribution = BTreeMap::new();
            if let Selection::Commit { candidate_id, .. } = &base {
                distribution.insert(candidate_id.clone(), 1.0);
            }
            return Ok((base, distribution));
        };
        if evaluation.exploration != super::config::ExplorationMode::ReadOnly
            || evaluation.exploration_rate <= 0.0
            || policy.minimum_top_margin <= 0.0
        {
            return Ok((base, BTreeMap::new()));
        }
        let CandidateSet::Legal { candidates, .. } = admitted else {
            return Ok((base, BTreeMap::new()));
        };
        let mut passed: Vec<_> = candidates
            .iter()
            .filter(|candidate| {
                !is_terminal(candidate.action())
                    && matches!(
                        pre_commit.outcomes.get(candidate.candidate_id()),
                        Some(GateOutcome::Passed)
                    )
            })
            .collect();
        passed.sort_by(|left, right| {
            let left_score = scores
                .get(left.candidate_id())
                .map_or(f64::NEG_INFINITY, |s| s.execution_utility);
            let right_score = scores
                .get(right.candidate_id())
                .map_or(f64::NEG_INFINITY, |s| s.execution_utility);
            right_score
                .total_cmp(&left_score)
                .then_with(|| left.side_effect().cmp(&right.side_effect()))
                .then_with(|| estimated_cost(left).cmp(&estimated_cost(right)))
                .then_with(|| estimated_latency(left).cmp(&estimated_latency(right)))
                .then_with(|| left.candidate_id().cmp(right.candidate_id()))
        });
        let Some(top) = passed.first() else {
            return Ok((base, BTreeMap::new()));
        };
        let top_id = top.candidate_id().clone();
        let Some(top_utility) = scores.get(top.candidate_id()).map(|score| score.execution_utility) else {
            return Err(SelectError::CandidateMismatch);
        };
        let band: Vec<_> = passed
            .into_iter()
            .filter(|candidate| {
                scores
                    .get(candidate.candidate_id())
                    .is_some_and(|score| top_utility - score.execution_utility <= policy.minimum_top_margin)
            })
            .collect();
        if band.len() < 2
            || prediction.action_confidence < policy.minimum_choice_confidence
            || prediction.target_confidence < policy.minimum_choice_confidence
        {
            return Ok((base, BTreeMap::new()));
        }
        for candidate in &band {
            let Some(score) = scores.get(candidate.candidate_id()) else {
                return Err(SelectError::CandidateMismatch);
            };
            let Some(features) = prediction.candidates.get(candidate.candidate_id()) else {
                return Err(SelectError::CandidateMismatch);
            };
            let target_ok = candidate.target() == CapabilityTarget::None
                || (context.direct_candidates.contains(candidate.candidate_id())
                    && prediction
                        .target_probabilities
                        .get(&candidate.target())
                        .copied()
                        .unwrap_or(0.0)
                        >= policy.minimum_choice_confidence);
            if features.choice_probability <= 0.0
                || score.execution_utility
                    < effective_threshold(f64::from(policy.commit_threshold), candidate.side_effect())
                || (candidate.action() == CausalActionKind::Answer
                    && prediction.evidence_sufficient < EVIDENCE_THRESHOLD)
                || (candidate.action() != CausalActionKind::Retrieve
                    && prediction.needs_fresh_state > FRESHNESS_THRESHOLD)
                || !target_ok
                || !exploration::eligible(
                    &CandidateFacts {
                        candidate,
                        semantic_risk: features.semantic_risk,
                    },
                    evaluation,
                )
            {
                return Ok((base, BTreeMap::new()));
            }
        }
        let ids: Vec<_> = band.iter().map(|candidate| candidate.candidate_id().clone()).collect();
        let Some((chosen, distribution)) =
            exploration::choose(key, decision_id, &top_id, &ids, evaluation.exploration_rate)
        else {
            return Ok((base, BTreeMap::new()));
        };
        let propensity = distribution
            .get(&chosen)
            .copied()
            .ok_or(SelectError::CandidateMismatch)?;
        Ok((
            Selection::Commit {
                candidate_id: chosen,
                propensity,
            },
            distribution,
        ))
    }

    pub(crate) fn select(
        admitted: &CandidateSet,
        scores: &BTreeMap<CandidateId, CandidateScore>,
        prediction: &PredictionEvidence,
        pre_commit: &GateReport,
        policy: &CausalPolicy,
        context: &SelectorContext,
    ) -> Result<Selection, SelectError> {
        let CandidateSet::Legal {
            candidates,
            approval_scopes,
            ..
        } = admitted
        else {
            let CandidateSet::NoLegalPath {
                terminals,
                approval_only,
                report: admitted_report,
                ..
            } = admitted
            else {
                return Err(SelectError::CandidateMismatch);
            };
            let approval = approval_only
                .iter()
                .filter_map(|pending| {
                    let terminal =
                        CandidateId::new(CausalActionKind::RequestApproval, pending.candidate.target(), None);
                    (approval_verified(pending, pre_commit)
                        && terminals.iter().any(|item| item.candidate_id() == &terminal)
                        && matches!(admitted_report.outcomes.get(&terminal), Some(GateOutcome::Passed))
                        && matches!(pre_commit.outcomes.get(&terminal), Some(GateOutcome::Passed)))
                    .then(|| approval_selection(&terminal, pending))
                })
                .min_by(|left, right| approval_order(left).cmp(&approval_order(right)));
            if let Some(approval) = approval {
                return Ok(approval);
            }
            let refusal = CandidateId::new(CausalActionKind::Refuse, CapabilityTarget::None, None);
            if terminals.iter().any(|item| item.candidate_id() == &refusal)
                && matches!(admitted_report.outcomes.get(&refusal), Some(GateOutcome::Passed))
                && matches!(pre_commit.outcomes.get(&refusal), Some(GateOutcome::Passed))
            {
                return Ok(Selection::Abstain {
                    action: CausalActionKind::Refuse,
                    reason_code: "no_legal_path",
                });
            }
            return Err(SelectError::NoTerminalPath);
        };
        let mut passed = Vec::new();
        let mut working_count = 0_usize;
        for candidate in candidates {
            if is_terminal(candidate.action()) {
                continue;
            }
            working_count += 1;
            if matches!(
                pre_commit.outcomes.get(candidate.candidate_id()),
                Some(GateOutcome::Passed)
            ) {
                let Some(score) = scores.get(candidate.candidate_id()) else {
                    return Err(SelectError::CandidateMismatch);
                };
                let Some(features) = prediction.candidates.get(candidate.candidate_id()) else {
                    return Err(SelectError::CandidateMismatch);
                };
                if !score.execution_utility.is_finite() || !features.choice_probability.is_finite() {
                    return Err(SelectError::NonFinite);
                }
                passed.push((candidate, score, features.choice_probability));
            }
        }
        if passed.is_empty() || working_count == 0 {
            return Ok(
                if let Some(approval) = available_approval(candidates, approval_scopes, pre_commit) {
                    approval
                } else {
                    terminal_abstain(candidates, pre_commit, context, "all_gated")?
                },
            );
        }
        passed.sort_by(|left, right| {
            right
                .1
                .execution_utility
                .total_cmp(&left.1.execution_utility)
                .then_with(|| left.0.side_effect().cmp(&right.0.side_effect()))
                .then_with(|| estimated_cost(left.0).cmp(&estimated_cost(right.0)))
                .then_with(|| estimated_latency(left.0).cmp(&estimated_latency(right.0)))
                .then_with(|| left.0.candidate_id().cmp(right.0.candidate_id()))
        });
        let Some(&(top, top_score, top_mass)) = passed.first() else {
            return Err(SelectError::CandidateMismatch);
        };
        let retrieve = passed
            .iter()
            .find(|(candidate, _, _)| {
                candidate.action() == CausalActionKind::Retrieve
                    && context.direct_candidates.contains(candidate.candidate_id())
            })
            .map(|(candidate, _, _)| *candidate);
        let discover = passed
            .iter()
            // Discover is the local catalog search itself; the target's
            // missing direct entry is why this recovery path exists. The
            // post-rehearsal and pre-commit gates still apply to Discover.
            .find(|(candidate, _, _)| candidate.action() == CausalActionKind::Discover)
            .map(|(candidate, _, _)| *candidate);
        let insufficient = prediction.evidence_sufficient < EVIDENCE_THRESHOLD;
        let stale = prediction.needs_fresh_state > FRESHNESS_THRESHOLD;
        let no_choice_mass = passed.iter().all(|(_, _, probability)| *probability <= 0.0);
        let other_action = matches!(prediction.chosen_action.as_str(), "other" | "none");
        let other_target = prediction.chosen_target == "other"
            || (prediction.chosen_target == "none" && top.target() != CapabilityTarget::None);
        if no_choice_mass || other_action || other_target {
            return terminal_abstain(candidates, pre_commit, context, "choice_outside_legal_set");
        }
        if insufficient && retrieve.is_none() && discover.is_none() {
            return terminal_abstain(candidates, pre_commit, context, "insufficient_evidence");
        }
        if (insufficient || stale)
            && let Some(candidate) = retrieve
        {
            return Ok(Selection::Recover {
                candidate_id: candidate.candidate_id().clone(),
                reason_code: "retrieve_evidence",
            });
        }
        let target_probability = prediction
            .target_probabilities
            .get(&top.target())
            .copied()
            .unwrap_or(0.0);
        let target_missing = top.target() != CapabilityTarget::None
            && (!context.direct_candidates.contains(top.candidate_id())
                || target_probability < policy.minimum_choice_confidence);
        if (insufficient || target_missing)
            && let Some(candidate) = discover
        {
            return Ok(Selection::Recover {
                candidate_id: candidate.candidate_id().clone(),
                reason_code: "discover_capability",
            });
        }
        let runner_up = passed
            .get(1)
            .map_or(f64::NEG_INFINITY, |(_, score, _)| score.execution_utility);
        let margin = top_score.execution_utility - runner_up;
        let threshold = effective_threshold(f64::from(policy.commit_threshold), top.side_effect());
        let enough_choice_confidence = prediction.action_confidence >= policy.minimum_choice_confidence
            && prediction.target_confidence >= policy.minimum_choice_confidence;
        let evidence_ok = top.action() != CausalActionKind::Answer || !insufficient;
        let fresh_ok = top.action() == CausalActionKind::Retrieve || !stale;
        if top_mass > 0.0
            && top_score.execution_utility >= threshold
            && enough_choice_confidence
            && margin >= policy.minimum_top_margin
            && evidence_ok
            && fresh_ok
            && !target_missing
        {
            return Ok(Selection::Commit {
                candidate_id: top.candidate_id().clone(),
                propensity: 1.0,
            });
        }
        terminal_abstain(candidates, pre_commit, context, "commit_threshold_or_margin")
    }
}

const EVIDENCE_THRESHOLD: f64 = 0.5;
const FRESHNESS_THRESHOLD: f64 = 0.5;

const fn is_terminal(action: CausalActionKind) -> bool {
    matches!(
        action,
        CausalActionKind::Clarify | CausalActionKind::RequestApproval | CausalActionKind::Refuse
    )
}

const fn estimated_cost(candidate: &CausalCandidate) -> u64 {
    match candidate.estimated_cost() {
        Some(cost) => cost.micro_usd,
        None => u64::MAX,
    }
}

const fn estimated_latency(candidate: &CausalCandidate) -> u32 {
    match candidate.estimated_latency_ms() {
        Some(latency) => latency,
        None => u32::MAX,
    }
}

const fn effective_threshold(base: f64, effect: SideEffectClass) -> f64 {
    let offset = match effect {
        SideEffectClass::ReadOnly => 0.0,
        SideEffectClass::LocalWrite => 0.05,
        SideEffectClass::ProcessExec => 0.10,
        SideEffectClass::ExternalSend => 0.15,
        SideEffectClass::CredentialOrConfig => 0.20,
        SideEffectClass::Destructive => 0.25,
    };
    if base + offset > 1.0 { 1.0 } else { base + offset }
}

fn available_approval(
    candidates: &[CausalCandidate],
    scopes: &BTreeMap<CandidateId, Vec<PendingApproval>>,
    gates: &GateReport,
) -> Option<Selection> {
    candidates
        .iter()
        .filter(|candidate| {
            candidate.action() == CausalActionKind::RequestApproval
                && matches!(gates.outcomes.get(candidate.candidate_id()), Some(GateOutcome::Passed))
        })
        .flat_map(|terminal| {
            scopes
                .get(terminal.candidate_id())
                .into_iter()
                .flatten()
                .filter(move |pending| {
                    pending.candidate.target() == terminal.target() && approval_verified(pending, gates)
                })
                .map(move |pending| approval_selection(terminal.candidate_id(), pending))
        })
        .min_by(|left, right| approval_order(left).cmp(&approval_order(right)))
}

fn approval_verified(pending: &PendingApproval, gates: &GateReport) -> bool {
    !pending.op_ids.is_empty()
        && matches!(
            gates.outcomes.get(pending.candidate.candidate_id()),
            Some(GateOutcome::ApprovalOnly { op_ids }) if op_ids == &pending.op_ids
        )
}

fn approval_selection(terminal_id: &CandidateId, pending: &PendingApproval) -> Selection {
    Selection::RequestApproval {
        terminal_id: terminal_id.clone(),
        pending_candidate_id: pending.candidate.candidate_id().clone(),
        op_ids: pending.op_ids.clone(),
        reason_code: "approval_only",
    }
}

fn approval_order(selection: &Selection) -> Option<(&CandidateId, &CandidateId, &[OpId])> {
    let Selection::RequestApproval {
        terminal_id,
        pending_candidate_id,
        op_ids,
        ..
    } = selection
    else {
        return None;
    };
    Some((terminal_id, pending_candidate_id, op_ids))
}

fn terminal_abstain(
    candidates: &[CausalCandidate],
    gates: &GateReport,
    context: &SelectorContext,
    reason_code: &'static str,
) -> Result<Selection, SelectError> {
    let preferred: &[CausalActionKind] = if context.interactive {
        &[CausalActionKind::Clarify, CausalActionKind::Refuse]
    } else {
        &[CausalActionKind::Refuse]
    };
    for &action in preferred {
        if candidates.iter().any(|candidate| {
            candidate.action() == action
                && matches!(gates.outcomes.get(candidate.candidate_id()), Some(GateOutcome::Passed))
        }) {
            return Ok(Selection::Abstain { action, reason_code });
        }
    }
    Err(SelectError::NoTerminalPath)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_tree::branch::{ApprovalRequirement, CandidateSpec, CostEstimate, OpId};
    use crate::causal_tree::candidate_generator::{NoLegalReason, PendingApproval};
    use crate::causal_tree::decision_score::CandidatePrediction;
    use crate::causal_tree::state::fixtures;

    fn candidate(action: CausalActionKind, target: CapabilityTarget) -> CausalCandidate {
        CausalCandidate::new(
            action,
            target,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 1,
                }),
                estimated_latency_ms: Some(1),
            },
        )
    }

    fn fixture(
        mass: f64,
    ) -> (
        CandidateSet,
        BTreeMap<CandidateId, CandidateScore>,
        PredictionEvidence,
        GateReport,
        CausalPolicy,
        SelectorContext,
    ) {
        let answer = candidate(CausalActionKind::Answer, CapabilityTarget::None);
        let retrieve = candidate(CausalActionKind::Retrieve, CapabilityTarget::Memory);
        let direct_candidates = BTreeSet::from([answer.candidate_id().clone(), retrieve.candidate_id().clone()]);
        let clarify = candidate(CausalActionKind::Clarify, CapabilityTarget::None);
        let refusal = candidate(CausalActionKind::Refuse, CapabilityTarget::None);
        let candidates = vec![answer, retrieve, clarify, refusal];
        let mut scores = BTreeMap::new();
        let mut predictions = BTreeMap::new();
        let mut outcomes = BTreeMap::new();
        for candidate in &candidates {
            outcomes.insert(candidate.candidate_id().clone(), GateOutcome::Passed);
            if !is_terminal(candidate.action()) {
                scores.insert(
                    candidate.candidate_id().clone(),
                    CandidateScore {
                        semantic_utility: 0.9,
                        execution_utility: 0.9,
                    },
                );
                predictions.insert(
                    candidate.candidate_id().clone(),
                    CandidatePrediction {
                        choice_probability: mass,
                        applicability_probability: 1.0,
                        success_probability: 1.0,
                        expected_gain: 1.0,
                        gain_uncertainty: 0.0,
                        semantic_risk: 0.0,
                        risk_uncertainty: 0.0,
                    },
                );
            }
        }
        let set = CandidateSet::Legal {
            candidates,
            exact_mentions: BTreeSet::new(),
            truncated: Vec::new(),
            discoverable_families: Default::default(),
            approval_scopes: BTreeMap::new(),
            report: GateReport {
                outcomes: outcomes.clone(),
                gate_set_hash: [0; 32],
            },
        };
        let evidence = PredictionEvidence {
            candidates: predictions,
            action_confidence: 0.9,
            target_confidence: 0.9,
            target_probabilities: BTreeMap::from([(CapabilityTarget::None, 0.7), (CapabilityTarget::Memory, 0.3)]),
            chosen_action: "answer".to_string(),
            chosen_target: "none".to_string(),
            evidence_sufficient: 1.0,
            needs_fresh_state: 0.0,
        };
        let policy = CausalPolicy {
            commit_threshold: 0.5,
            minimum_top_margin: 0.0,
            ..CausalPolicy::default()
        };
        let context = SelectorContext {
            interactive: true,
            direct_candidates,
        };
        (
            set,
            scores,
            evidence,
            GateReport {
                outcomes,
                gate_set_hash: [0; 32],
            },
            policy,
            context,
        )
    }

    #[test]
    fn read_only_near_tie_explores_with_recorded_propensities() {
        let (set, scores, mut evidence, report, mut policy, context) = fixture(0.5);
        policy.minimum_top_margin = 0.1;
        evidence.target_probabilities.insert(CapabilityTarget::Memory, 0.9);
        let key = fixtures::key();
        let evaluation = EvaluationConfig {
            exploration: super::super::config::ExplorationMode::ReadOnly,
            exploration_rate: 0.1,
        };
        let base = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context).unwrap();
        assert!(matches!(base, Selection::Abstain { .. }));
        let first = CausalSelector::select_with_exploration(
            &set,
            &scores,
            &evidence,
            &report,
            &policy,
            &context,
            &evaluation,
            &key,
            "decision-a",
        )
        .unwrap();
        let second = CausalSelector::select_with_exploration(
            &set,
            &scores,
            &evidence,
            &report,
            &policy,
            &context,
            &evaluation,
            &key,
            "decision-a",
        )
        .unwrap();
        assert_eq!(first, second);
        assert!(matches!(first.0, Selection::Commit { .. }));
        assert_eq!(first.1.len(), 2);
        assert!((first.1.values().sum::<f64>() - 1.0).abs() < 1e-12);
        assert!(first.1.values().any(|value| (*value - 0.05).abs() < 1e-12));

        let CandidateSet::Legal { candidates, .. } = &set else {
            panic!("expected legal candidates")
        };
        let mut reversed = candidates.clone();
        reversed.reverse();
        let CandidateSet::Legal {
            exact_mentions,
            truncated,
            discoverable_families,
            approval_scopes,
            report: admission,
            ..
        } = &set
        else {
            panic!("expected legal candidates")
        };
        let permuted = CandidateSet::Legal {
            discoverable_families: *discoverable_families,
            candidates: reversed,
            exact_mentions: exact_mentions.clone(),
            truncated: truncated.clone(),
            approval_scopes: approval_scopes.clone(),
            report: admission.clone(),
        };
        assert_eq!(
            first,
            CausalSelector::select_with_exploration(
                &permuted,
                &scores,
                &evidence,
                &report,
                &policy,
                &context,
                &evaluation,
                &key,
                "decision-a",
            )
            .unwrap()
        );

        let off = EvaluationConfig::default();
        assert_eq!(
            CausalSelector::select_with_exploration(
                &set,
                &scores,
                &evidence,
                &report,
                &policy,
                &context,
                &off,
                &key,
                "decision-a",
            )
            .unwrap()
            .0,
            base,
        );
    }

    #[test]
    fn near_tie_with_a_write_candidate_keeps_abstention() {
        let (mut set, scores, mut evidence, report, mut policy, context) = fixture(0.5);
        policy.minimum_top_margin = 0.1;
        evidence.target_probabilities.insert(CapabilityTarget::Memory, 0.9);
        let CandidateSet::Legal { candidates, .. } = &mut set else {
            panic!("expected legal candidates")
        };
        let retrieve = candidates
            .iter_mut()
            .find(|candidate| candidate.action() == CausalActionKind::Retrieve)
            .unwrap();
        *retrieve = CausalCandidate::new(
            CausalActionKind::Retrieve,
            CapabilityTarget::Memory,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::LocalWrite,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 1,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let evaluation = EvaluationConfig {
            exploration: super::super::config::ExplorationMode::ReadOnly,
            exploration_rate: 0.1,
        };
        let (selection, distribution) = CausalSelector::select_with_exploration(
            &set,
            &scores,
            &evidence,
            &report,
            &policy,
            &context,
            &evaluation,
            &fixtures::key(),
            "decision-a",
        )
        .unwrap();
        assert!(matches!(selection, Selection::Abstain { .. }));
        assert!(distribution.is_empty());
    }

    #[test]
    fn zero_legal_choice_mass_abstains_even_with_high_utility() {
        let (set, scores, evidence, report, policy, context) = fixture(0.0);
        let selection = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            selection,
            Selection::Abstain {
                action: CausalActionKind::Clarify,
                reason_code: "choice_outside_legal_set",
            }
        );
    }

    #[test]
    fn equal_utility_uses_stable_candidate_id_tiebreak() {
        let (set, scores, evidence, report, policy, context) = fixture(0.5);
        let selection = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        let CandidateSet::Legal { candidates, .. } = &set else {
            panic!("expected legal candidates");
        };
        assert_eq!(
            selection,
            Selection::Commit {
                candidate_id: candidates
                    .iter()
                    .find(|candidate| candidate.action() == CausalActionKind::Answer)
                    .unwrap_or_else(|| panic!("expected answer"))
                    .candidate_id()
                    .clone(),
                propensity: 1.0,
            }
        );
        let mut reversed = set;
        if let CandidateSet::Legal { candidates, .. } = &mut reversed {
            candidates.reverse();
        }
        let permuted = CausalSelector::select(&reversed, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(selection, permuted);
    }

    #[test]
    fn low_choice_confidence_abstains_before_read_tool_commit() {
        let (set, scores, mut evidence, report, policy, context) = fixture(0.5);
        evidence.action_confidence = 0.01;
        let selection = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            selection,
            Selection::Abstain {
                action: CausalActionKind::Clarify,
                ..
            }
        ));
    }

    #[test]
    fn stale_answer_recovers_only_through_a_passed_retrieval() {
        let (set, scores, mut evidence, mut report, policy, mut context) = fixture(0.5);
        evidence.needs_fresh_state = 0.9;
        let selection = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            selection,
            Selection::Recover {
                reason_code: "retrieve_evidence",
                ..
            }
        ));
        let CandidateSet::Legal { candidates, .. } = &set else {
            panic!("expected legal candidates");
        };
        let retrieval = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::Retrieve)
            .unwrap_or_else(|| panic!("expected retrieval"));
        context.direct_candidates.remove(retrieval.candidate_id());
        let missing_direct = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(missing_direct, Selection::Abstain { .. }));
        context.direct_candidates.insert(retrieval.candidate_id().clone());
        report.outcomes.insert(
            retrieval.candidate_id().clone(),
            GateOutcome::Blocked {
                gate: 9,
                reason_code: "explicit_restriction",
            },
        );
        let blocked = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(blocked, Selection::Abstain { .. }));
    }

    #[test]
    fn missing_exact_direct_candidate_does_not_commit() {
        let (set, mut scores, mut evidence, report, policy, mut context) = fixture(0.5);
        let CandidateSet::Legal { candidates, .. } = &set else {
            panic!("expected legal candidates");
        };
        let retrieval = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::Retrieve)
            .unwrap_or_else(|| panic!("expected retrieval"));
        let answer = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::Answer)
            .unwrap_or_else(|| panic!("expected answer"));
        if let Some(score) = scores.get_mut(answer.candidate_id()) {
            score.execution_utility = 0.1;
        }
        evidence.chosen_action = "retrieve".to_string();
        evidence.chosen_target = "memory".to_string();
        evidence.target_probabilities.insert(CapabilityTarget::Memory, 0.9);
        context.direct_candidates.remove(retrieval.candidate_id());
        let selection = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(selection, Selection::Abstain { .. }));
    }

    #[test]
    fn approval_requires_current_g4_only_proof_and_passed_terminal() {
        let (mut set, scores, evidence, mut report, policy, context) = fixture(0.5);
        let pending = candidate(CausalActionKind::UseTool, CapabilityTarget::NativeTool);
        let approval = candidate(CausalActionKind::RequestApproval, CapabilityTarget::NativeTool);
        let op_ids = vec![OpId::new("prx.test")];
        if let CandidateSet::Legal {
            candidates,
            approval_scopes,
            ..
        } = &mut set
        {
            for candidate in candidates.iter().filter(|item| !is_terminal(item.action())) {
                report.outcomes.insert(
                    candidate.candidate_id().clone(),
                    GateOutcome::Blocked {
                        gate: 1,
                        reason_code: "scope",
                    },
                );
            }
            candidates.push(approval.clone());
            approval_scopes.insert(
                approval.candidate_id().clone(),
                vec![PendingApproval {
                    candidate: pending.clone(),
                    op_ids: op_ids.clone(),
                }],
            );
        }
        report
            .outcomes
            .insert(approval.candidate_id().clone(), GateOutcome::Passed);
        let blocked = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            blocked,
            Selection::Abstain {
                action: CausalActionKind::Clarify,
                ..
            }
        ));
        for gate in [1, 9] {
            report.outcomes.insert(
                pending.candidate_id().clone(),
                GateOutcome::Blocked {
                    gate,
                    reason_code: "not_approval_only",
                },
            );
            let blocked = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
                .unwrap_or_else(|error| panic!("{error}"));
            assert!(matches!(
                blocked,
                Selection::Abstain {
                    action: CausalActionKind::Clarify,
                    ..
                }
            ));
        }
        report.outcomes.insert(
            pending.candidate_id().clone(),
            GateOutcome::ApprovalOnly {
                op_ids: vec![OpId::new("prx.other")],
            },
        );
        let wrong_scope = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            wrong_scope,
            Selection::Abstain {
                action: CausalActionKind::Clarify,
                ..
            }
        ));
        report.outcomes.insert(
            pending.candidate_id().clone(),
            GateOutcome::ApprovalOnly { op_ids: op_ids.clone() },
        );
        let approved = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            approved,
            Selection::RequestApproval { terminal_id, pending_candidate_id, op_ids: selected_ops, .. }
                if terminal_id == *approval.candidate_id()
                    && pending_candidate_id == *pending.candidate_id()
                    && selected_ops == op_ids
        ));
        let CandidateSet::Legal { candidates, .. } = &set else {
            panic!("expected legal candidates");
        };
        let answer = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::Answer)
            .unwrap_or_else(|| panic!("expected answer"));
        report
            .outcomes
            .insert(answer.candidate_id().clone(), GateOutcome::Passed);
        let mut low_confidence = evidence.clone();
        low_confidence.action_confidence = 0.01;
        let unrelated = CausalSelector::select(&set, &scores, &low_confidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            unrelated,
            Selection::Abstain {
                action: CausalActionKind::Clarify,
                ..
            }
        ));
        report.outcomes.insert(
            answer.candidate_id().clone(),
            GateOutcome::Blocked {
                gate: 1,
                reason_code: "scope",
            },
        );
        report.outcomes.insert(
            approval.candidate_id().clone(),
            GateOutcome::Blocked {
                gate: 9,
                reason_code: "explicit_restriction",
            },
        );
        let terminal_blocked = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            terminal_blocked,
            Selection::Abstain {
                action: CausalActionKind::Clarify,
                ..
            }
        ));
    }

    #[test]
    fn multiple_approval_scopes_select_one_stable_verified_scope() {
        let (mut set, scores, evidence, mut report, policy, context) = fixture(0.5);
        let first = candidate(CausalActionKind::UseTool, CapabilityTarget::Mcp);
        let second = candidate(CausalActionKind::UseTool, CapabilityTarget::NativeTool);
        let first_terminal = candidate(CausalActionKind::RequestApproval, CapabilityTarget::Mcp);
        let second_terminal = candidate(CausalActionKind::RequestApproval, CapabilityTarget::NativeTool);
        let first_ops = vec![OpId::new("prx.mcp")];
        let second_ops = vec![OpId::new("prx.native")];
        let CandidateSet::Legal {
            candidates,
            approval_scopes,
            ..
        } = &mut set
        else {
            panic!("expected legal candidates");
        };
        for item in candidates.iter().filter(|item| !is_terminal(item.action())) {
            report.outcomes.insert(
                item.candidate_id().clone(),
                GateOutcome::Blocked {
                    gate: 1,
                    reason_code: "scope",
                },
            );
        }
        candidates.extend([second_terminal.clone(), first_terminal.clone()]);
        for (terminal, pending, ops) in [
            (&second_terminal, &second, &second_ops),
            (&first_terminal, &first, &first_ops),
        ] {
            approval_scopes.insert(
                terminal.candidate_id().clone(),
                vec![PendingApproval {
                    candidate: pending.clone(),
                    op_ids: ops.clone(),
                }],
            );
            report
                .outcomes
                .insert(terminal.candidate_id().clone(), GateOutcome::Passed);
            report.outcomes.insert(
                pending.candidate_id().clone(),
                GateOutcome::ApprovalOnly { op_ids: ops.clone() },
            );
        }
        let select = |set: &CandidateSet, report: &GateReport| {
            CausalSelector::select(set, &scores, &evidence, report, &policy, &context)
                .unwrap_or_else(|error| panic!("{error}"))
        };
        let chosen = select(&set, &report);
        assert_eq!(
            chosen,
            Selection::RequestApproval {
                terminal_id: first_terminal.candidate_id().clone(),
                pending_candidate_id: first.candidate_id().clone(),
                op_ids: first_ops,
                reason_code: "approval_only",
            }
        );
        if let CandidateSet::Legal { candidates, .. } = &mut set {
            candidates.reverse();
        }
        assert_eq!(select(&set, &report), chosen);
        report.outcomes.insert(
            first.candidate_id().clone(),
            GateOutcome::ApprovalOnly {
                op_ids: second_ops.clone(),
            },
        );
        assert_eq!(
            select(&set, &report),
            Selection::RequestApproval {
                terminal_id: second_terminal.candidate_id().clone(),
                pending_candidate_id: second.candidate_id().clone(),
                op_ids: second_ops,
                reason_code: "approval_only",
            }
        );
    }

    #[test]
    fn clarify_requires_an_admitted_and_passed_terminal() {
        let (set, scores, evidence, mut report, policy, context) = fixture(0.0);
        let CandidateSet::Legal { candidates, .. } = &set else {
            panic!("expected legal candidates");
        };
        let clarify = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::Clarify)
            .unwrap_or_else(|| panic!("expected clarify"));
        report.outcomes.insert(
            clarify.candidate_id().clone(),
            GateOutcome::Blocked {
                gate: 9,
                reason_code: "explicit_restriction",
            },
        );
        let selection = CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            selection,
            Selection::Abstain {
                action: CausalActionKind::Refuse,
                ..
            }
        ));
        let mut noninteractive = context;
        noninteractive.interactive = false;
        let refusal = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::Refuse)
            .unwrap_or_else(|| panic!("expected refusal"));
        report.outcomes.insert(
            refusal.candidate_id().clone(),
            GateOutcome::Blocked {
                gate: 9,
                reason_code: "explicit_restriction",
            },
        );
        assert!(matches!(
            CausalSelector::select(&set, &scores, &evidence, &report, &policy, &noninteractive),
            Err(SelectError::NoTerminalPath)
        ));
    }

    #[test]
    fn no_legal_path_needs_a_current_terminal_gate_result() {
        let (_, scores, evidence, _, policy, context) = fixture(0.0);
        let refusal = CandidateId::new(CausalActionKind::Refuse, CapabilityTarget::None, None);
        let mut admitted = CandidateSet::NoLegalPath {
            reason: NoLegalReason::NoLegalCandidate,
            terminals: vec![candidate(CausalActionKind::Refuse, CapabilityTarget::None)],
            approval_only: Vec::new(),
            report: GateReport {
                outcomes: BTreeMap::from([(refusal.clone(), GateOutcome::Passed)]),
                gate_set_hash: [0; 32],
            },
        };
        let mut pre_commit = GateReport {
            outcomes: BTreeMap::from([(refusal.clone(), GateOutcome::Passed)]),
            gate_set_hash: [0; 32],
        };
        assert!(matches!(
            CausalSelector::select(&admitted, &scores, &evidence, &pre_commit, &policy, &context),
            Ok(Selection::Abstain {
                action: CausalActionKind::Refuse,
                ..
            })
        ));
        pre_commit.outcomes.insert(
            refusal.clone(),
            GateOutcome::Blocked {
                gate: 9,
                reason_code: "explicit_restriction",
            },
        );
        assert!(matches!(
            CausalSelector::select(&admitted, &scores, &evidence, &pre_commit, &policy, &context),
            Err(SelectError::NoTerminalPath)
        ));
        pre_commit.outcomes.insert(refusal, GateOutcome::Passed);
        if let CandidateSet::NoLegalPath { terminals, .. } = &mut admitted {
            terminals.clear();
        }
        assert!(matches!(
            CausalSelector::select(&admitted, &scores, &evidence, &pre_commit, &policy, &context),
            Err(SelectError::NoTerminalPath)
        ));
    }

    #[test]
    fn no_legal_path_approval_keeps_its_verified_scope() {
        let (_, scores, evidence, _, policy, context) = fixture(0.0);
        let pending = candidate(CausalActionKind::UseTool, CapabilityTarget::NativeTool);
        let terminal = candidate(CausalActionKind::RequestApproval, CapabilityTarget::NativeTool);
        let refusal = CandidateId::new(CausalActionKind::Refuse, CapabilityTarget::None, None);
        let op_ids = vec![OpId::new("prx.native")];
        let admitted = CandidateSet::NoLegalPath {
            reason: NoLegalReason::NoLegalCandidate,
            terminals: vec![
                terminal.clone(),
                candidate(CausalActionKind::Refuse, CapabilityTarget::None),
            ],
            approval_only: vec![PendingApproval {
                candidate: pending.clone(),
                op_ids: op_ids.clone(),
            }],
            report: GateReport {
                outcomes: BTreeMap::from([
                    (terminal.candidate_id().clone(), GateOutcome::Passed),
                    (refusal.clone(), GateOutcome::Passed),
                ]),
                gate_set_hash: [0; 32],
            },
        };
        let mut current = GateReport {
            outcomes: BTreeMap::from([
                (terminal.candidate_id().clone(), GateOutcome::Passed),
                (
                    pending.candidate_id().clone(),
                    GateOutcome::ApprovalOnly { op_ids: op_ids.clone() },
                ),
                (refusal, GateOutcome::Passed),
            ]),
            gate_set_hash: [0; 32],
        };
        let selected = CausalSelector::select(&admitted, &scores, &evidence, &current, &policy, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            selected,
            Selection::RequestApproval {
                terminal_id: terminal.candidate_id().clone(),
                pending_candidate_id: pending.candidate_id().clone(),
                op_ids,
                reason_code: "approval_only",
            }
        );
        current.outcomes.insert(
            pending.candidate_id().clone(),
            GateOutcome::ApprovalOnly {
                op_ids: vec![OpId::new("prx.other")],
            },
        );
        assert!(matches!(
            CausalSelector::select(&admitted, &scores, &evidence, &current, &policy, &context),
            Ok(Selection::Abstain {
                action: CausalActionKind::Refuse,
                ..
            })
        ));
    }
}
