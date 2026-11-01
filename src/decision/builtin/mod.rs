//! `BuiltinDecisionBackend`: the local, deterministic, always available
//! decision backend (CDP 4.3 item 1; SP-CDP-A 5.7; model `builtin-v4`).
//!
//! It answers every question of a sealed request from the language-neutral
//! features of the state v1 object (module `features`) and the versioned
//! constants of module `weights`:
//!
//! - every offered candidate gets a log-score: the policy prior of its action
//!   plus evidence (exact catalog naming; a URL for web targets, a path for
//!   files and code for the shell, each of which commits to its tool; a
//!   trusted record that this very candidate failed; availability;
//!   staleness; an empty first turn; retries; a pending approval; and a lean
//!   towards Discover and Answer when nothing is decisive);
//! - a Choice over actions or targets is a softmax over its offered options
//!   (an action or a target takes the best score of the candidates it
//!   names);
//! - a Noul is a logistic transform of the same features;
//! - a Score is a fixed-width distribution over the levels, centered by the
//!   features, and its expectation is the mean level index;
//! - aggregate confidence is `1 − H(p)/ln n`, capped at 0.9;
//! - a question it does not recognize (an id it does not know, a primitive
//!   other than the one the id implies, a candidate the state does not offer
//!   or lists twice) gets a uniform distribution or 0.5, with confidence 0.
//!
//! It never reads a natural-language word, performs no I/O and no network
//! call, and computes in a fixed order, so the same request always gets the
//! same bits. It only answers questions over the offered options: it is the
//! final fallback, not an alternate authority.

pub(crate) mod features;
pub(crate) mod weights;

use std::collections::BTreeMap;
use std::sync::Arc;

use self::features::{Action, CandidateFeatures, StateFeatures};
use super::error::DecisionError;
use super::protocol::{
    BackendIdentity, BackendKind, BackendLocality, CandidateQuestion, ChoiceAnswer, DecisionAnswer, DecisionQuestion,
    DecisionRequest, DecisionResponse, DecisionUsage, GlobalQuestion, NoulAnswer, OptionId, Probability, QuestionId,
    ScoreAnswer, StructuredValue,
};

/// The builtin decision backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinDecisionBackend;

/// What a question id means to the builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Meaning<'q> {
    Global(GlobalQuestion),
    Candidate(&'q str, CandidateQuestion),
    Unknown,
}

fn meaning(id: &QuestionId) -> Meaning<'_> {
    let text = id.as_str();
    if let Some(global) = GlobalQuestion::ALL.into_iter().find(|global| global.as_str() == text) {
        return Meaning::Global(global);
    }
    let per_candidate = text.strip_prefix("candidate.").and_then(|rest| rest.rsplit_once('.'));
    if let Some((key, question)) = per_candidate {
        if let Some(question) = CandidateQuestion::ALL
            .into_iter()
            .find(|candidate_question| candidate_question.as_str() == question)
        {
            return Meaning::Candidate(key, question);
        }
    }
    Meaning::Unknown
}

impl BuiltinDecisionBackend {
    /// The backend name the builtin reports.
    pub const NAME: &'static str = "builtin";

    /// The configured identity of the builtin.
    #[must_use]
    pub fn identity() -> BackendIdentity {
        BackendIdentity {
            kind: BackendKind::Builtin,
            provider: None,
            configured_model: Arc::from(weights::MODEL_VERSION),
        }
    }

    /// Where the builtin runs.
    #[must_use]
    pub const fn locality() -> BackendLocality {
        BackendLocality::Builtin
    }

    /// Answer every question of `request`.
    ///
    /// The response has no wire body (`raw_bytes = 0`), `backend = "builtin"`
    /// and the versioned `resolved_model`, and it satisfies every rule of
    /// the response validator. An error is impossible by construction (every
    /// value is a finite probability); it is still typed, never a panic.
    pub fn answer(request: &DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        let features = features::extract(request.state().as_json());
        let mut answers = BTreeMap::new();
        for (id, question) in request.questions() {
            let answer = match question {
                DecisionQuestion::Choice { criteria, .. } => {
                    DecisionAnswer::Choice(choice(&features, meaning(id), criteria)?)
                }
                DecisionQuestion::Score { levels, .. } => DecisionAnswer::Score(score(&features, meaning(id), levels)?),
                DecisionQuestion::Noul { .. } => DecisionAnswer::Noul(NoulAnswer {
                    probability_true: Probability::new(noul(&features, meaning(id)))?,
                }),
            };
            answers.insert(id.clone(), answer);
        }
        Ok(DecisionResponse {
            backend: Self::NAME.to_string(),
            resolved_model: weights::MODEL_VERSION.to_string(),
            answers,
            usage: DecisionUsage::default(),
            provider_request_id: None,
            elapsed_ms: 0,
            raw_bytes: 0,
            response_sha256: [0; 32],
        })
    }
}

// ---------------------------------------------------------------------------
// Scores
// ---------------------------------------------------------------------------

fn plain_conversation_turn(state: &StateFeatures<'_>) -> bool {
    state.text.has_text
        && state.undecided()
        && state.unsuccessful_action_steps == 0
        && state.retry_count == 0
        && !state.approval_pending
}

fn continued_plain_conversation_turn(state: &StateFeatures<'_>) -> bool {
    plain_conversation_turn(state)
        && state.has_conversation_history
        && state.successful_lookups == 0
        && state.artifact_count == 0
}

fn tool_matches_structure(state: &StateFeatures<'_>, candidate: &CandidateFeatures<'_>) -> bool {
    let text = &state.text;
    candidate.available
        && candidate.action == Some(Action::UseTool)
        && ((text.path && matches!(candidate.target, "filesystem" | "document"))
            || (text.url && candidate.target == "web")
            || (text.code
                && (candidate.target == "shell" || (candidate.target == "filesystem" && candidate.executes_process))))
}

/// A structural first hop with a matching tool and no recovery candidate.
/// Retrieve and discover stay on the existing evidence path when they are offered.
fn concrete_artifact_without_recovery(state: &StateFeatures<'_>) -> bool {
    let text = &state.text;
    text.has_text
        && text.structural()
        && state.retry_count == 0
        && state.unsuccessful_action_steps == 0
        && !state.approval_pending
        && state.fresh_artifacts == 0
        && state.successful_lookups == 0
        && !state
            .candidates
            .iter()
            .any(|candidate| matches!(candidate.action, Some(Action::Retrieve | Action::Discover)))
        && state
            .candidates
            .iter()
            .any(|candidate| tool_matches_structure(state, candidate))
}

fn plain_turn_has_no_read_recovery(state: &StateFeatures<'_>) -> bool {
    plain_conversation_turn(state)
        && !state
            .candidates
            .iter()
            .any(|candidate| matches!(candidate.action, Some(Action::Retrieve | Action::Discover)))
}

fn model_managed_delegation_turn(state: &StateFeatures<'_>) -> bool {
    // Source-configured delegation can be planned by the serving model. Exact
    // names may be negated or contextual; a path shape cannot decide intent.
    // This opens planning only, never execution or a new capability grant.
    state.candidates.iter().any(|candidate| {
        candidate.exact_mention
            && matches!(candidate.action, Some(Action::Delegate))
            && candidate.id.rsplit_once('-').is_some_and(|(_, namespace)| {
                namespace.len() == 8 && namespace.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    })
}

/// The log-score of one candidate.
fn candidate_score(state: &StateFeatures<'_>, candidate: &CandidateFeatures<'_>) -> f64 {
    let Some(action) = candidate.action else {
        return weights::UNMATCHED_OPTION;
    };
    let text = &state.text;
    let mut score = weights::action_prior(action).ln();
    if candidate.exact_mention {
        score += weights::EXACT_MENTION;
    }
    if text.url && candidate.target == "web" {
        score += weights::URL_TO_WEB;
    }
    if text.path && matches!(candidate.target, "filesystem" | "document") {
        score += weights::PATH_TO_FILES;
    }
    if text.code && (candidate.target == "shell" || (candidate.target == "filesystem" && candidate.executes_process)) {
        score += weights::CODE_TO_SHELL;
    }
    if candidate.failed_before {
        score += weights::FAILED_BEFORE;
    }
    if !candidate.available {
        score += weights::UNAVAILABLE;
    }
    if candidate.approval_required && action == Action::UseTool {
        score += weights::APPROVAL_STILL_REQUIRED;
    }
    match action {
        Action::Retrieve if state.stale_artifacts > 0 => score += weights::STALE_TO_RETRIEVE,
        Action::Clarify => {
            if !text.has_text && !state.has_history {
                score += weights::EMPTY_TO_CLARIFY;
            }
            if state.retry_count >= weights::RETRY_THRESHOLD {
                score += weights::RETRIES_TO_CLARIFY;
            }
        }
        Action::RequestApproval if state.approval_pending => score += weights::PENDING_TO_APPROVAL,
        _ => {}
    }
    // "Nothing decisive" presumes a request; an empty turn is the Clarify
    // signal above instead.
    if text.has_text && state.undecided() {
        match action {
            Action::Discover => score += weights::UNDECIDED_TO_DISCOVER,
            Action::Answer => score += weights::UNDECIDED_TO_ANSWER,
            _ => {}
        }
    }
    if (plain_conversation_turn(state)
        && (text.request_weight <= weights::BRIEF_REQUEST_MAX_WEIGHT
            || continued_plain_conversation_turn(state)
            || plain_turn_has_no_read_recovery(state))
        || model_managed_delegation_turn(state))
        && action == Action::Answer
    {
        score += weights::BRIEF_PLAIN_TURN_TO_ANSWER;
    }
    score
}

/// The best candidate: highest score, then lower effect risk, then id.
/// Both global choices use this same preference only for exact probability
/// ties; candidate listing order cannot change it.
fn best_candidate<'f, 's>(state: &'f StateFeatures<'s>) -> Option<&'f CandidateFeatures<'s>> {
    state
        .candidates
        .iter()
        .fold(None::<(f64, &'f CandidateFeatures<'s>)>, |best, candidate| {
            let score = candidate_score(state, candidate);
            match best {
                Some((best_score, best_candidate)) => {
                    let better = score > best_score
                        || (score == best_score
                            && (candidate.side_effect_risk < best_candidate.side_effect_risk
                                || (candidate.side_effect_risk == best_candidate.side_effect_risk
                                    && candidate.id < best_candidate.id)));
                    Some(if better {
                        (score, candidate)
                    } else {
                        (best_score, best_candidate)
                    })
                }
                None => Some((score, candidate)),
            }
        })
        .map(|(_, candidate)| candidate)
}

/// The best score of the candidates `matches` selects, if any.
fn best_score(state: &StateFeatures<'_>, matches: impl Fn(&CandidateFeatures<'_>) -> bool) -> Option<f64> {
    state
        .candidates
        .iter()
        .filter(|candidate| matches(candidate))
        .map(|candidate| candidate_score(state, candidate))
        .reduce(f64::max)
}

fn action_option_score(state: &StateFeatures<'_>, option: &str) -> f64 {
    Action::from_code(option).map_or(weights::UNMATCHED_OPTION, |action| {
        best_score(state, |candidate| candidate.action == Some(action))
            .unwrap_or_else(|| weights::action_prior(action).ln() + weights::OPTION_WITHOUT_CANDIDATE)
    })
}

fn target_option_score(state: &StateFeatures<'_>, option: &str) -> f64 {
    // Targets describe the preferred action, not competing actions. In
    // particular, an answer has no external capability target. Other actions'
    // metadata must not manufacture uncertainty about that structural fact.
    let action = best_candidate(state).and_then(|candidate| candidate.action);
    best_score(state, |candidate| {
        candidate.target == option && candidate.action == action
    })
    .unwrap_or(weights::UNMATCHED_OPTION)
}

// ---------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------

fn choice(
    state: &StateFeatures<'_>,
    meaning: Meaning<'_>,
    criteria: &BTreeMap<OptionId, StructuredValue>,
) -> Result<ChoiceAnswer, DecisionError> {
    let scores: Option<Vec<f64>> = match meaning {
        Meaning::Global(GlobalQuestion::NextAction) => Some(
            criteria
                .keys()
                .map(|option| action_option_score(state, option.as_str()))
                .collect(),
        ),
        Meaning::Global(GlobalQuestion::CapabilityTarget) => Some(
            criteria
                .keys()
                .map(|option| target_option_score(state, option.as_str()))
                .collect(),
        ),
        // Every other id, and a global or per-candidate id asked as a
        // Choice, is a question the builtin does not recognize.
        Meaning::Global(_) | Meaning::Candidate(..) | Meaning::Unknown => None,
    };
    let (distribution, confidence) = scores.map_or_else(
        || uniform(criteria.len()),
        |scores| {
            let distribution = softmax(&scores);
            let confidence = confidence(&distribution);
            (distribution, confidence)
        },
    );
    let chosen_index = distribution.iter().enumerate().fold(0, |best, (index, value)| {
        if *value > distribution.get(best).copied().unwrap_or(0.0) {
            index
        } else {
            best
        }
    });
    // An exact tie must describe the same preferred candidate across action,
    // target and global risk, rather than unrelated lexical option orders.
    let preferred_index = best_candidate(state).and_then(|candidate| {
        criteria.keys().position(|option| match meaning {
            Meaning::Global(GlobalQuestion::NextAction) => {
                candidate.action.is_some() && Action::from_code(option.as_str()) == candidate.action
            }
            Meaning::Global(GlobalQuestion::CapabilityTarget) => option.as_str() == candidate.target,
            _ => false,
        })
    });
    let chosen_index = preferred_index
        .filter(|index| distribution.get(*index) == distribution.get(chosen_index))
        .unwrap_or(chosen_index);
    let mut probabilities = BTreeMap::new();
    let mut chosen = None;
    for (index, (option, value)) in criteria.keys().zip(&distribution).enumerate() {
        if index == chosen_index {
            chosen = Some(option.clone());
        }
        probabilities.insert(option.clone(), Probability::new(*value)?);
    }
    let chosen = chosen.ok_or(DecisionError::InvalidRequest(
        super::error::RequestRule::MinChoiceOptions,
    ))?;
    Ok(ChoiceAnswer {
        chosen,
        probabilities,
        confidence: Probability::new(confidence)?,
    })
}

fn noul(state: &StateFeatures<'_>, meaning: Meaning<'_>) -> f64 {
    match meaning {
        Meaning::Global(GlobalQuestion::EvidenceSufficient) => {
            // A plain conversational turn may have ordinary provider history
            // without external evidence. Treating that history as insufficient
            // forces a read/discovery hop before an admitted model answer can
            // run. Structurally or exactly targeted tool turns retain the
            // existing retrieve-first evidence path. Exact delegation instead
            // serves the model with only the sealed delegator publication.
            if concrete_artifact_without_recovery(state) {
                return weights::CONCRETE_ARTIFACT_EVIDENCE;
            }
            let plain = plain_conversation_turn(state) || model_managed_delegation_turn(state);
            let decisive = if plain {
                1.0
            } else if state.text.has_text && !state.undecided() {
                0.5
            } else {
                0.0
            };
            if plain && state.fresh_artifacts == 0 && state.successful_lookups == 0 {
                // Explicit rule: a plain turn without gathered evidence reports
                // the named plain-turn value instead of an empty logistic sum.
                return weights::PLAIN_TURN_EVIDENCE;
            }
            sigmoid(state.fresh_artifacts as f64 + state.successful_lookups as f64 + decisive - 1.0)
        }
        Meaning::Global(GlobalQuestion::NeedsFreshState) => {
            sigmoid(1.5_f64.mul_add(state.stale_artifacts as f64, -(state.fresh_artifacts as f64)) - 0.5)
        }
        Meaning::Candidate(key, CandidateQuestion::Applicable) => state.candidate(key).map_or(0.5, |candidate| {
            sigmoid(candidate_score(state, candidate) - weights::APPLICABILITY_OFFSET)
        }),
        Meaning::Candidate(key, CandidateQuestion::Success) => state.candidate(key).map_or(0.5, |candidate| {
            let mut logit = weights::SUCCESS_PER_RETRY.mul_add(state.retry_count as f64, weights::SUCCESS_BASE);
            if candidate.failed_before {
                logit += weights::FAILED_BEFORE;
            }
            if !candidate.available {
                logit += weights::FAILED_BEFORE;
            }
            if candidate.exact_mention {
                logit += weights::EXACT_MENTION / 4.0;
            }
            sigmoid(logit)
        }),
        _ => 0.5,
    }
}

fn score(
    state: &StateFeatures<'_>,
    meaning: Meaning<'_>,
    levels: &[StructuredValue],
) -> Result<ScoreAnswer, DecisionError> {
    let center = match meaning {
        Meaning::Global(GlobalQuestion::SemanticRisk) => Some(best_candidate(state).map_or(0.5, |candidate| {
            let failed = if state.unsuccessful_steps > 0 {
                weights::FAILED_RISK
            } else {
                0.0
            };
            (candidate.side_effect_risk + failed).min(1.0)
        })),
        Meaning::Global(GlobalQuestion::ExpectedGain) => {
            let base = if state.undecided() {
                weights::UNDECIDED_GAIN
            } else {
                weights::DECISIVE_GAIN
            };
            let setback = if state.unsuccessful_steps > 0 {
                weights::UNSUCCESSFUL_GAIN
            } else {
                0.0
            };
            Some((base + setback).max(0.0))
        }
        Meaning::Candidate(key, CandidateQuestion::Gain) => state
            .candidate(key)
            .map(|candidate| sigmoid(candidate_score(state, candidate) - weights::APPLICABILITY_OFFSET)),
        Meaning::Candidate(key, CandidateQuestion::Risk) => state.candidate(key).map(|candidate| {
            let failed = if candidate.failed_before {
                weights::FAILED_RISK
            } else {
                0.0
            };
            (candidate.side_effect_risk + failed).min(1.0)
        }),
        _ => None,
    };
    let count = levels.len();
    let (distribution, confidence) = center.map_or_else(
        || uniform(count),
        |center| {
            let distribution = level_distribution(center, count);
            let confidence = confidence(&distribution);
            (distribution, confidence)
        },
    );
    // The expectation is computed exactly as the validator computes the
    // mean level index, so the two agree to the bit.
    let mass: f64 = distribution.iter().sum();
    let expected = distribution
        .iter()
        .enumerate()
        .map(|(index, value)| index as f64 * value)
        .sum::<f64>()
        / mass;
    Ok(ScoreAnswer {
        distribution: distribution
            .into_iter()
            .map(Probability::new)
            .collect::<Result<Vec<_>, _>>()?,
        expected,
        legend: levels.to_vec(),
        confidence: Probability::new(confidence)?,
    })
}

// ---------------------------------------------------------------------------
// Arithmetic (f64, fixed iteration order)
// ---------------------------------------------------------------------------

/// The uniform distribution over `count` options, with confidence 0: the
/// answer to a question the builtin does not recognize.
fn uniform(count: usize) -> (Vec<f64>, f64) {
    (vec![1.0 / count as f64; count], 0.0)
}

fn sigmoid(logit: f64) -> f64 {
    1.0 / (1.0 + (-logit).exp())
}

/// `exp(s_i − max) / Σ exp(s_j − max)`, in option order.
fn softmax(scores: &[f64]) -> Vec<f64> {
    let highest = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let weights: Vec<f64> = scores.iter().map(|score| (score - highest).exp()).collect();
    let total: f64 = weights.iter().sum();
    weights.into_iter().map(|weight| weight / total).collect()
}

/// A fixed-width bell over `count` levels, centered at `fraction` of the
/// range.
fn level_distribution(fraction: f64, count: usize) -> Vec<f64> {
    let center = fraction * count.saturating_sub(1) as f64;
    let spread = 2.0 * weights::LEVEL_WIDTH * weights::LEVEL_WIDTH;
    let weights: Vec<f64> = (0..count)
        .map(|index| {
            let distance = index as f64 - center;
            (-(distance * distance) / spread).exp()
        })
        .collect();
    let total: f64 = weights.iter().sum();
    weights.into_iter().map(|weight| weight / total).collect()
}

/// `1 − H(p)/ln n`, within `[0, CONFIDENCE_CAP]`.
fn confidence(distribution: &[f64]) -> f64 {
    let count = distribution.len();
    if count < 2 {
        return 0.0;
    }
    let entropy: f64 = distribution
        .iter()
        .filter(|value| **value > 0.0)
        .map(|value| -value * value.ln())
        .sum();
    let raw = 1.0 - entropy / (count as f64).ln();
    // `H(p) ≤ ln n`, so `raw` is below 0 only by rounding error; report 0.
    if raw.is_sign_negative() {
        0.0
    } else {
        raw.min(weights::CONFIDENCE_CAP)
    }
}

#[cfg(test)]
pub(crate) mod allocation_counter {
    //! Test-only Rust allocator attempts, forwarded unchanged to `System`.
    //! Caller-poll diagnostics and isolated process measurements are separate.
    //! Process measurements include acknowledged worker allocations; neither
    //! mode counts native/C allocations or deallocations.

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};

    const PROCESS_ACTIVE: u64 = 1 << 63;
    const PROCESS_COUNT_BITS: u32 = 48;
    const PROCESS_COUNT_MASK: u64 = (1 << PROCESS_COUNT_BITS) - 1;
    const PROCESS_GENERATION_MASK: u64 = !(PROCESS_ACTIVE | PROCESS_COUNT_MASK);
    static PROCESS_WORD: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ProcessCountError {
        Busy,
        GenerationExhausted,
        Saturated,
    }

    // Capture a generation once. A recorder delayed across stop/restart must
    // never retry into the new window, even when its count is again zero.
    fn record_process_from(counter: &AtomicU64, mut observed: u64) {
        let generation = observed & PROCESS_GENERATION_MASK;
        while observed & PROCESS_ACTIVE != 0 && observed & PROCESS_GENERATION_MASK == generation {
            if observed & PROCESS_COUNT_MASK == PROCESS_COUNT_MASK {
                return;
            }
            match counter.compare_exchange_weak(observed, observed + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(current) => observed = current,
            }
        }
    }

    fn start_process_word(counter: &AtomicU64) -> Result<(), ProcessCountError> {
        let observed = counter.load(Ordering::Acquire);
        if observed & PROCESS_ACTIVE != 0 {
            return Err(ProcessCountError::Busy);
        }
        if observed & PROCESS_GENERATION_MASK == PROCESS_GENERATION_MASK {
            return Err(ProcessCountError::GenerationExhausted);
        }
        let next = ((observed & PROCESS_GENERATION_MASK) + (1 << PROCESS_COUNT_BITS)) | PROCESS_ACTIVE;
        counter
            .compare_exchange(observed, next, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| ProcessCountError::Busy)
    }

    fn finish_process_word(counter: &AtomicU64) -> Result<u64, ProcessCountError> {
        // Disable and snapshot in one operation. A stale recorder CAS fails;
        // its generation check also prevents a later restart from admitting it.
        let measured = counter.fetch_and(!PROCESS_ACTIVE, Ordering::AcqRel) & PROCESS_COUNT_MASK;
        if measured == PROCESS_COUNT_MASK {
            Err(ProcessCountError::Saturated)
        } else {
            Ok(measured)
        }
    }

    struct ProcessCountingScope {
        active: bool,
    }

    impl ProcessCountingScope {
        fn start() -> Result<Self, ProcessCountError> {
            start_process_word(&PROCESS_WORD)?;
            Ok(Self { active: true })
        }

        fn finish(mut self) -> Result<u64, ProcessCountError> {
            self.active = false;
            finish_process_word(&PROCESS_WORD)
        }
    }

    impl Drop for ProcessCountingScope {
        fn drop(&mut self) {
            if self.active {
                let _ = finish_process_word(&PROCESS_WORD);
            }
        }
    }

    /// Count all Rust allocation attempts in one isolated process window.
    /// Setup and teardown are excluded. The caller must acknowledge all
    /// attributable workers before returning and preserve measurement errors.
    pub(crate) fn count_process<T>(work: impl FnOnce() -> T) -> Result<(T, u64), ProcessCountError> {
        let scope = ProcessCountingScope::start()?;
        let result = work();
        Ok((result, scope.finish()?))
    }

    /// Keep the process window active across suspension until the prepared
    /// future and its required worker acknowledgments finish. Dropping the
    /// future restores the inactive state; no partial metric is returned.
    pub(crate) async fn count_process_future<T>(
        future: impl std::future::Future<Output = T>,
    ) -> Result<(T, u64), ProcessCountError> {
        let scope = ProcessCountingScope::start()?;
        let result = future.await;
        Ok((result, scope.finish()?))
    }

    /// A refused read-only diagnostic observation; measurement errors stay separate.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum CheckpointError {
        Inactive,
        GenerationChanged,
        Saturated,
    }

    /// Read access bound to one counter object and one allocation window.
    pub(crate) struct ProcessCheckpoint<'counter> {
        counter: &'counter AtomicU64,
        generation: u64,
    }

    impl<'counter> ProcessCheckpoint<'counter> {
        fn capture_from(counter: &'counter AtomicU64) -> Result<(Self, u64), CheckpointError> {
            let observed = counter.load(Ordering::Acquire);
            if observed & PROCESS_ACTIVE == 0 {
                return Err(CheckpointError::Inactive);
            }
            let count = observed & PROCESS_COUNT_MASK;
            if count == PROCESS_COUNT_MASK {
                return Err(CheckpointError::Saturated);
            }
            Ok((
                Self {
                    counter,
                    generation: observed & PROCESS_GENERATION_MASK,
                },
                count,
            ))
        }

        /// One atomic observation; this neither stops nor synchronizes workers.
        pub(crate) fn read(&self) -> Result<u64, CheckpointError> {
            let observed = self.counter.load(Ordering::Acquire);
            if observed & PROCESS_ACTIVE == 0 {
                return Err(CheckpointError::Inactive);
            }
            if observed & PROCESS_GENERATION_MASK != self.generation {
                return Err(CheckpointError::GenerationChanged);
            }
            let count = observed & PROCESS_COUNT_MASK;
            if count == PROCESS_COUNT_MASK {
                return Err(CheckpointError::Saturated);
            }
            Ok(count)
        }

        /// Allocation-window generation, distinct from configuration generation.
        pub(crate) const fn allocation_generation(&self) -> u64 {
            self.generation >> PROCESS_COUNT_BITS
        }
    }

    /// Capture after the existing wrapper starts its already-prepared future.
    pub(crate) fn capture_process_checkpoint() -> Result<(ProcessCheckpoint<'static>, u64), CheckpointError> {
        ProcessCheckpoint::capture_from(&PROCESS_WORD)
    }

    thread_local! {
        static COUNTING: Cell<bool> = const { Cell::new(false) };
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    pub struct Counting;

    fn record() {
        record_process_from(&PROCESS_WORD, PROCESS_WORD.load(Ordering::Relaxed));
        // `try_with`: never fail inside the allocator, even during thread
        // teardown. Both cells are const-initialized and have no destructor,
        // so reading them never allocates.
        let counting = COUNTING.try_with(Cell::get).unwrap_or(false);
        if counting {
            let _ = COUNT.try_with(|count| count.set(count.get().saturating_add(1)));
        }
    }

    // SAFETY: every method forwards its arguments unchanged to `System`,
    // which upholds the `GlobalAlloc` contract; the only extra work is
    // updating const thread-local cells and a bounded atomic word, which
    // neither allocates, locks, logs nor unwinds. Failed attempts count too.
    #[allow(unsafe_code)]
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record();
            // SAFETY: the caller's `layout` obligations pass straight through.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record();
            // SAFETY: as for `alloc`.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: `ptr` came from this allocator, which is `System`.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            record();
            // SAFETY: `ptr` came from this allocator, which is `System`; the
            // other arguments pass straight through.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    struct CountingScope {
        prior_counting: bool,
        prior_count: u64,
    }

    impl CountingScope {
        fn start() -> Self {
            Self {
                prior_counting: COUNTING.with(|counting| counting.replace(true)),
                prior_count: COUNT.with(|count| count.replace(0)),
            }
        }
    }

    impl Drop for CountingScope {
        fn drop(&mut self) {
            let measured = COUNT.with(Cell::get);
            COUNT.with(|count| {
                count.set(if self.prior_counting {
                    self.prior_count.saturating_add(measured)
                } else {
                    self.prior_count
                });
            });
            COUNTING.with(|counting| counting.set(self.prior_counting));
        }
    }

    /// Count this synchronous scope, restoring any enclosing count even on
    /// unwind. Nested measurements remain part of the enclosing measurement.
    pub fn count<T>(work: impl FnOnce() -> T) -> (T, u64) {
        let scope = CountingScope::start();
        let result = work();
        let measured = COUNT.with(Cell::get);
        drop(scope);
        (result, measured)
    }

    /// Count allocations made while polling this future on its caller thread.
    /// Setup, suspension, other tasks and writer threads are outside this
    /// diagnostic metric. This is not a process-wide allocation baseline.
    pub(crate) async fn count_future<T>(future: impl std::future::Future<Output = T>) -> (T, u64) {
        let mut pinned = Box::pin(future);
        let mut measured = 0_u64;
        let result = std::future::poll_fn(|context| {
            let (poll, allocations) = count(|| pinned.as_mut().poll(context));
            measured = measured.saturating_add(allocations);
            poll
        })
        .await;
        (result, measured)
    }

    #[cfg(test)]
    mod checkpoints;

    #[cfg(test)]
    #[allow(clippy::expect_used)] // Isolated allocator fixtures must fail loudly.
    mod tests {
        use super::*;
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        #[tokio::test(flavor = "current_thread")]
        #[ignore = "process allocation evidence requires an isolated filtered test binary"]
        async fn process_counter_includes_acknowledged_worker_allocations() {
            use std::sync::{Arc, Barrier};
            let ready = Arc::new(Barrier::new(2));
            let start = Arc::new(Barrier::new(2));
            let done = Arc::new(Barrier::new(2));
            let worker = {
                let ready = Arc::clone(&ready);
                let start = Arc::clone(&start);
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    ready.wait();
                    start.wait();
                    for _ in 0..64 {
                        std::hint::black_box(vec![7_u8; 128]);
                    }
                    done.wait();
                })
            };
            ready.wait();
            let (_, measured) = count_process_future(async {
                start.wait();
                done.wait();
            })
            .await
            .expect("valid process window");
            worker.join().expect("acknowledged allocation worker");
            assert!(
                measured >= 64,
                "the named process boundary must include all 64 worker allocations"
            );
        }

        #[test]
        fn process_word_rejects_stale_recorders_after_stop_and_restart() {
            let counter = AtomicU64::new(0);
            start_process_word(&counter).expect("first window");
            let stale = counter.load(Ordering::Acquire);
            record_process_from(&counter, stale);
            assert_eq!(finish_process_word(&counter), Ok(1));
            record_process_from(&counter, stale);
            assert_eq!(counter.load(Ordering::Acquire) & PROCESS_COUNT_MASK, 1);
            start_process_word(&counter).expect("second window");
            record_process_from(&counter, stale);
            assert_eq!(counter.load(Ordering::Acquire) & PROCESS_COUNT_MASK, 0);
            record_process_from(&counter, counter.load(Ordering::Acquire));
            assert_eq!(finish_process_word(&counter), Ok(1));
        }

        #[test]
        fn process_word_rejects_saturation_exhaustion_and_nested_reset() {
            let counter = AtomicU64::new(0);
            start_process_word(&counter).expect("outer window");
            let owned = counter.load(Ordering::Acquire);
            assert_eq!(start_process_word(&counter), Err(ProcessCountError::Busy));
            assert_eq!(
                counter.load(Ordering::Acquire),
                owned,
                "nested start preserves its owner"
            );
            counter.store(owned | (PROCESS_COUNT_MASK - 1), Ordering::Release);
            record_process_from(&counter, counter.load(Ordering::Acquire));
            record_process_from(&counter, counter.load(Ordering::Acquire));
            assert_eq!(finish_process_word(&counter), Err(ProcessCountError::Saturated));
            assert_eq!(counter.load(Ordering::Acquire) & PROCESS_ACTIVE, 0);
            start_process_word(&counter).expect("saturation does not poison the next generation");
            assert_eq!(finish_process_word(&counter), Ok(0));
            counter.store(PROCESS_GENERATION_MASK | 9, Ordering::Release);
            assert_eq!(
                start_process_word(&counter),
                Err(ProcessCountError::GenerationExhausted)
            );
            assert_eq!(counter.load(Ordering::Acquire), PROCESS_GENERATION_MASK | 9);
        }

        #[tokio::test(flavor = "current_thread")]
        #[ignore = "process allocation evidence requires an isolated filtered test binary"]
        #[allow(clippy::panic)] // Intentional unwind verifies process ownership restoration.
        async fn process_counter_restores_after_nested_attempt_pending_drop_and_unwind() {
            let (nested, allocations) = count_process(|| {
                std::hint::black_box(vec![1_u8; 32]);
                count_process(|| ())
            })
            .expect("outer measurement");
            assert_eq!(nested, Err(ProcessCountError::Busy));
            assert_eq!(allocations, 1);
            let mut context = Context::from_waker(Waker::noop());
            let mut suspended = Box::pin(count_process_future(std::future::pending::<()>()));
            assert!(suspended.as_mut().poll(&mut context).is_pending());
            assert_ne!(PROCESS_WORD.load(Ordering::Acquire) & PROCESS_ACTIVE, 0);
            std::hint::black_box(vec![2_u8; 32]);
            drop(suspended);
            assert_eq!(PROCESS_WORD.load(Ordering::Acquire) & PROCESS_ACTIVE, 0);
            let result = std::panic::catch_unwind(|| count_process(|| panic!("intentional process fixture unwind")));
            assert!(result.is_err());
            assert_eq!(PROCESS_WORD.load(Ordering::Acquire) & PROCESS_ACTIVE, 0);
            let (value, allocations) = count_process_future(async {
                std::hint::black_box(vec![3_u8; 32]);
                7
            })
            .await
            .expect("fresh valid process window");
            assert_eq!((value, allocations), (7, 1));
        }

        #[test]
        fn future_counter_excludes_suspended_work_and_restores_after_drop() {
            let mut context = Context::from_waker(Waker::noop());
            let mut polls = 0;
            let work = std::future::poll_fn(|_| {
                std::hint::black_box(vec![7_u8; 32]);
                polls += 1;
                if polls == 1 { Poll::Pending } else { Poll::Ready(9) }
            });
            let mut counted = Box::pin(count_future(work));
            assert!(counted.as_mut().poll(&mut context).is_pending());
            assert!(!COUNTING.with(Cell::get), "counting must stop before suspension");
            let before = COUNT.with(Cell::get);
            std::hint::black_box(vec![8_u8; 64]);
            assert_eq!(COUNT.with(Cell::get), before, "unrelated work is excluded");
            assert_eq!(counted.as_mut().poll(&mut context), Poll::Ready((9, 2)));
            assert!(!COUNTING.with(Cell::get));

            let mut cancelled = Box::pin(count_future(std::future::pending::<()>()));
            assert!(cancelled.as_mut().poll(&mut context).is_pending());
            drop(cancelled);
            assert!(
                !COUNTING.with(Cell::get),
                "dropping a pending future must not leave counting active"
            );
        }

        #[test]
        #[allow(clippy::panic)] // Intentional unwind verifies allocator scope restoration.
        fn counter_restores_enclosing_scope_after_nested_count_and_unwind() {
            let (_, outer) = count(|| {
                std::hint::black_box(vec![1_u8; 16]);
                let (_, inner) = count(|| {
                    std::hint::black_box(vec![2_u8; 16]);
                });
                assert_eq!(inner, 1);
                assert!(COUNTING.with(Cell::get));
                std::hint::black_box(vec![3_u8; 16]);
            });
            assert_eq!(outer, 3);
            assert!(!COUNTING.with(Cell::get));
            let result = std::panic::catch_unwind(|| {
                count(|| {
                    std::hint::black_box(vec![4_u8; 16]);
                    panic!("intentional fixture unwind");
                })
            });
            assert!(result.is_err());
            assert!(!COUNTING.with(Cell::get));
            assert_eq!(count(|| std::hint::black_box(vec![5_u8; 16])).1, 1);
            let (_, nested_unwind) = count(|| {
                let result = std::panic::catch_unwind(|| {
                    count(|| {
                        std::hint::black_box(vec![6_u8; 16]);
                        panic!("intentional nested fixture unwind");
                    })
                });
                assert!(result.is_err());
                assert!(COUNTING.with(Cell::get), "enclosing scope survives inner unwind");
                let before = COUNT.with(Cell::get);
                std::hint::black_box(vec![7_u8; 16]);
                assert_eq!(COUNT.with(Cell::get), before + 1);
            });
            assert!(nested_unwind >= 2, "enclosing count includes measured allocations");
            assert!(!COUNTING.with(Cell::get));
        }
    }
}

#[cfg(test)]
#[global_allocator]
static COUNTING_ALLOCATOR: allocation_counter::Counting = allocation_counter::Counting;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::{Duration, Instant};

    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::*;
    use crate::decision::protocol::{CandidateKey, RequestLimits, RequestParts};
    use crate::decision::{DecisionKey, DecisionResponseValidator, ModelVersionPolicy, ValidationContext};

    #[test]
    fn code_bonus_uses_the_real_filesystem_process_class_without_biasing_file_reads() {
        use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
        let workspace = tempfile::tempdir().unwrap();
        let security = std::sync::Arc::new(crate::security::SecurityPolicy::default());
        let tools: Vec<Box<dyn crate::tools::Tool>> = vec![
            Box::new(crate::tools::shell::ShellTool::new(
                std::sync::Arc::new(crate::runtime::NativeRuntime::new()),
                workspace.path().to_owned(),
            )),
            Box::new(crate::tools::file_read::FileReadTool::new(security, false)),
        ];
        let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        for name in ["shell", "file_read"] {
            let entry = view.entries().iter().find(|entry| entry.public_name == name).unwrap();
            assert_eq!(entry.id.as_str(), format!("filesystem:native:{name}"));
            let value = json!({"latest_user_turn":"`value`", "candidates":[{
                "id":"use_tool-filesystem", "action":"use_tool", "target":entry.family.as_str(),
                "side_effect":entry.effect.as_str(), "available":true,
            }]});
            let coded = features::extract(&value);
            let mut plain = coded.clone();
            plain.text.code = false;
            let candidate = coded.candidates.first().unwrap();
            let difference = candidate_score(&coded, candidate) - candidate_score(&plain, candidate);
            let expected = if name == "shell" { weights::CODE_TO_SHELL } else { 0.0 };
            assert!((difference - expected).abs() < 1e-10, "{name}: {difference}");
        }
    }

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/decision/state_v1/check_state_v1.rs"
    ));

    const ACTIONS: [&str; 8] = [
        "answer",
        "retrieve",
        "discover",
        "use_tool",
        "delegate",
        "clarify",
        "request_approval",
        "refuse",
    ];
    const TARGETS: [&str; 13] = [
        "none",
        "memory",
        "skill",
        "mcp",
        "native_tool",
        "wasm",
        "hook",
        "filesystem",
        "shell",
        "document",
        "web",
        "im",
        "session",
    ];
    const SIDE_EFFECTS: [&str; 6] = [
        "read_only",
        "local_write",
        "process_exec",
        "external_send",
        "credential_or_config",
        "destructive",
    ];

    fn key() -> std::sync::Arc<DecisionKey> {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let key = DecisionKey::load_or_create(dir.path()).expect("test: key");
        // The key lives in memory; the directory may go.
        drop(dir);
        key
    }

    fn structured(value: Value) -> StructuredValue {
        StructuredValue::try_from(value).expect("test: structured")
    }

    fn text(value: &str) -> StructuredValue {
        structured(json!(value))
    }

    /// The candidate key of a state v1 candidate id. The test builds ids
    /// from static codes and an index, as the compiler builds them from
    /// typed candidates.
    fn key_for(action: &'static str, target: &'static str, index: Option<u32>) -> CandidateKey {
        CandidateKey::from_parts(action, target, index.map(u32::to_be_bytes))
    }

    fn candidate_json(action: &str, target: &str, id: &str) -> Value {
        json!({
            "id": id, "action": action, "target": target, "side_effect": "read_only",
            "approval": "none", "cost_bucket": "b1", "latency_bucket": "b1",
            "summary": "A capability.", "exact_mention": false, "failed_before": false, "available": true
        })
    }

    fn state(turn: &str, candidates: Vec<Value>) -> Value {
        json!({
            "schema": "prx.causal.state.v1", "hop": 0, "prior_decision_id": null,
            "latest_user_turn": turn, "goal": "g", "has_conversation_history": false,
            "completed_steps": [], "artifacts": [],
            "failed_tool_fingerprints": [], "retry_count": 0, "approval_pending": false,
            "constraints": [], "budgets": {"tokens": "b2", "latency": "b2", "cost": "b1"},
            "candidates": candidates
        })
    }

    #[test]
    fn configured_delegation_planning_keeps_action_and_target_consistent() {
        let key = key();
        for turn in [
            "Use file_write on acceptance/root.txt, then file_read. Do not use shell or delegate.",
            "Use file_edit on acceptance/root.txt. Then file_read. Do not use shell or delegate.",
            "Use delegate to ask reviewer for 9 * 7. Do not use file_write or file_edit on acceptance/root.txt.",
            "Use delegate to ask kimi for 9 * 7. Do not use shell, sessions_spawn, file_read, file_write or file_edit.",
        ] {
            let mut file = candidate_json("use_tool", "filesystem", "use_tool-filesystem-00000001");
            file["exact_mention"] = json!(true);
            file["side_effect"] = json!("local_write");
            let mut shell = candidate_json("use_tool", "shell", "use_tool-shell-00000001");
            shell["exact_mention"] = json!(true);
            shell["side_effect"] = json!("process_exec");
            let mut delegate = candidate_json("delegate", "session", "delegate-session-00000001");
            delegate["exact_mention"] = json!(true);
            delegate["side_effect"] = json!("process_exec");
            let value = state(
                turn,
                vec![file, shell, delegate, candidate_json("answer", "none", "answer-none")],
            );
            let request = seal(&key, value.clone(), compiler_questions(&value));
            let response = answer_and_validate(&request);
            assert_eq!(
                chosen(&response, GlobalQuestion::NextAction),
                "answer",
                "planning: {turn}"
            );
            assert_eq!(
                chosen(&response, GlobalQuestion::CapabilityTarget),
                "none",
                "planning: {turn}"
            );
            let DecisionAnswer::Choice(target) =
                &response.answers[&QuestionId::global(GlobalQuestion::CapabilityTarget)]
            else {
                panic!("target choice")
            };
            assert!(
                target.confidence.get() >= 0.55,
                "an answer's deterministic target must not borrow uncertainty from executable alternatives"
            );
        }
    }

    /// The compiler's question set for `state`: the six global questions with
    /// the offered actions and targets, and the four per-candidate questions.
    fn compiler_questions(state: &Value) -> BTreeMap<QuestionId, DecisionQuestion> {
        let offered = state["candidates"].as_array().cloned().unwrap_or_default();
        let has = |field: &str, code: &str| offered.iter().any(|candidate| candidate[field] == code);
        let mut actions: BTreeMap<OptionId, StructuredValue> = ACTIONS
            .iter()
            .filter(|code| has("action", code))
            .map(|code| (OptionId::code(code), text(code)))
            .collect();
        actions.insert(OptionId::code("other"), text("Something else."));
        let mut targets: BTreeMap<OptionId, StructuredValue> = TARGETS
            .iter()
            .filter(|code| has("target", code))
            .map(|code| (OptionId::code(code), text(code)))
            .collect();
        targets
            .entry(OptionId::code("none"))
            .or_insert_with(|| text("No capability."));
        if targets.len() < 2 {
            targets.insert(OptionId::code("other"), text("Another capability."));
        }
        let levels = |count: usize| {
            (0..count)
                .map(|level| text(&format!("level {level}")))
                .collect::<Vec<_>>()
        };
        let noul = |question: &str| DecisionQuestion::Noul {
            instructions: text(question),
            criteria: None,
        };
        let mut questions = BTreeMap::from([
            (
                QuestionId::global(GlobalQuestion::NextAction),
                DecisionQuestion::Choice {
                    instructions: text("Next action?"),
                    criteria: actions,
                },
            ),
            (
                QuestionId::global(GlobalQuestion::CapabilityTarget),
                DecisionQuestion::Choice {
                    instructions: text("Capability?"),
                    criteria: targets,
                },
            ),
            (
                QuestionId::global(GlobalQuestion::EvidenceSufficient),
                noul("Enough evidence?"),
            ),
            (QuestionId::global(GlobalQuestion::NeedsFreshState), noul("Stale?")),
            (
                QuestionId::global(GlobalQuestion::SemanticRisk),
                DecisionQuestion::Score {
                    instructions: text("Risk?"),
                    levels: levels(5),
                },
            ),
            (
                QuestionId::global(GlobalQuestion::ExpectedGain),
                DecisionQuestion::Score {
                    instructions: text("Gain?"),
                    levels: levels(5),
                },
            ),
        ]);
        for (index, candidate) in offered.iter().enumerate() {
            let action = ACTIONS
                .iter()
                .find(|code| candidate["action"] == **code)
                .copied()
                .unwrap_or("answer");
            let target = TARGETS
                .iter()
                .find(|code| candidate["target"] == **code)
                .copied()
                .unwrap_or("none");
            // The id's namespace suffix, when it has one, is the key's ns8.
            let id = candidate["id"].as_str().unwrap_or_default();
            let namespace = id
                .strip_prefix(&format!("{action}-{target}-"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok());
            let key = key_for(action, target, namespace);
            let refer = |question: &str| {
                structured(json!({"candidate": format!("$.candidates[{index}]"), "question": question}))
            };
            questions.insert(
                QuestionId::candidate(&key, CandidateQuestion::Applicable),
                DecisionQuestion::Noul {
                    instructions: refer("Applies?"),
                    criteria: None,
                },
            );
            questions.insert(
                QuestionId::candidate(&key, CandidateQuestion::Success),
                DecisionQuestion::Noul {
                    instructions: refer("Succeeds?"),
                    criteria: None,
                },
            );
            questions.insert(
                QuestionId::candidate(&key, CandidateQuestion::Gain),
                DecisionQuestion::Score {
                    instructions: refer("Gain?"),
                    levels: levels(5),
                },
            );
            questions.insert(
                QuestionId::candidate(&key, CandidateQuestion::Risk),
                DecisionQuestion::Score {
                    instructions: refer("Risk?"),
                    levels: levels(5),
                },
            );
        }
        questions
    }

    fn seal(key: &DecisionKey, state: Value, questions: BTreeMap<QuestionId, DecisionQuestion>) -> DecisionRequest {
        DecisionRequest::seal(
            RequestParts {
                state: structured(state),
                questions,
                model: None,
                state_fingerprint: "state".to_string(),
                candidate_set_hash: "candidates".to_string(),
                policy_version: Arc::from("policy"),
                catalog_generation: "catalog".to_string(),
                omitted_questions: Vec::new(),
                approval_pending: false,
            },
            key,
            Instant::now() + Duration::from_hours(1),
            &RequestLimits::default(),
        )
        .expect("test: seal")
    }

    fn context() -> ValidationContext {
        ValidationContext {
            received_at: Instant::now(),
            max_response_bytes: 256 * 1024,
            version_policy: ModelVersionPolicy::ExactPinned(Arc::from(weights::MODEL_VERSION)),
            local_backend: BuiltinDecisionBackend::identity(),
        }
    }

    fn answer_and_validate(request: &DecisionRequest) -> DecisionResponse {
        let response = BuiltinDecisionBackend::answer(request).expect("test: builtin answers");
        let decision = DecisionResponseValidator::validate(request, response.clone(), &context())
            .unwrap_or_else(|error| panic!("builtin answer must validate: {error}"));
        assert_eq!(decision.claimed_model(), weights::MODEL_VERSION);
        assert_eq!(decision.claimed_backend(), "builtin");
        assert_eq!(decision.response_sha256(), None);
        response
    }

    fn chosen(response: &DecisionResponse, question: GlobalQuestion) -> String {
        match &response.answers[&QuestionId::global(question)] {
            DecisionAnswer::Choice(choice) => choice.chosen.as_str().to_string(),
            other => panic!("expected a choice, got {other:?}"),
        }
    }

    fn noul_probability(response: &DecisionResponse, question: GlobalQuestion) -> f64 {
        match &response.answers[&QuestionId::global(question)] {
            DecisionAnswer::Noul(noul) => noul.probability_true.get(),
            other => panic!("expected a noul, got {other:?}"),
        }
    }

    /// Candidates of a typical turn: direct answer, memory, skill discovery,
    /// web, filesystem and shell tools, delegation, clarification, refusal.
    fn typical_candidates() -> Vec<Value> {
        vec![
            candidate_json("answer", "none", "answer-none"),
            candidate_json("retrieve", "memory", "retrieve-memory"),
            candidate_json("discover", "skill", "discover-skill"),
            candidate_json("use_tool", "web", "use_tool-web-00000003"),
            candidate_json("use_tool", "filesystem", "use_tool-filesystem-00000004"),
            candidate_json("use_tool", "shell", "use_tool-shell-00000005"),
            candidate_json("delegate", "session", "delegate-session"),
            candidate_json("clarify", "none", "clarify-none"),
            candidate_json("refuse", "none", "refuse-none"),
        ]
    }

    // -----------------------------------------------------------------------
    // Every primitive, every fixture, always valid
    // -----------------------------------------------------------------------

    /// `builtin_answers_every_primitive_and_validates`: over the committed
    /// state v1 fixtures, with the compiler's full question set, every
    /// answer passes the CDP-01 validator under the exact builtin model pin.
    #[test]
    fn builtin_answers_every_primitive_and_validates() {
        let key = key();
        for name in ["minimal", "retrieve_hop", "tool_candidates"] {
            let path = format!(
                "{}/tests/fixtures/decision/state_v1/{name}.json",
                env!("CARGO_MANIFEST_DIR")
            );
            let state: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(state_v1_violations(&state), Vec::<String>::new(), "{name}");
            let questions = compiler_questions(&state);
            let request = seal(&key, state, questions);
            let response = answer_and_validate(&request);
            assert_eq!(response.answers.len(), request.questions().len());
            let primitives: std::collections::BTreeSet<_> = response
                .answers
                .values()
                .map(|answer| format!("{:?}", answer.primitive()))
                .collect();
            assert_eq!(primitives.len(), 3, "{name}: Choice, Score and Noul are all answered");
        }
    }

    /// Questions the builtin does not recognize (a global id asked as another
    /// primitive, a candidate the state does not offer) get a uniform answer
    /// or 0.5 with confidence 0, and still validate.
    #[test]
    fn unknown_questions_get_uniform_answers_with_zero_confidence() {
        let key = key();
        let questions = BTreeMap::from([
            (
                QuestionId::global(GlobalQuestion::NextAction),
                DecisionQuestion::Score {
                    instructions: text("Mismatched primitive"),
                    levels: vec![text("a"), text("b"), text("c")],
                },
            ),
            (
                QuestionId::global(GlobalQuestion::SemanticRisk),
                DecisionQuestion::Noul {
                    instructions: text("Mismatched primitive"),
                    criteria: None,
                },
            ),
            (
                QuestionId::candidate(&key_for("use_tool", "wasm", Some(99)), CandidateQuestion::Success),
                DecisionQuestion::Noul {
                    instructions: text("Not offered"),
                    criteria: None,
                },
            ),
            (
                QuestionId::global(GlobalQuestion::EvidenceSufficient),
                DecisionQuestion::Choice {
                    instructions: text("Mismatched primitive"),
                    criteria: BTreeMap::from([
                        (OptionId::code("yes"), text("yes")),
                        (OptionId::code("no"), text("no")),
                    ]),
                },
            ),
        ]);
        let request = seal(&key, state("hello", typical_candidates()), questions);
        let response = answer_and_validate(&request);
        for answer in response.answers.values() {
            match answer {
                DecisionAnswer::Choice(choice) => {
                    assert_eq!(choice.confidence.get(), 0.0);
                    assert!(choice.probabilities.values().all(|p| p.get() == 0.5));
                }
                DecisionAnswer::Score(score) => {
                    assert_eq!(score.confidence.get(), 0.0);
                    assert!(score.distribution.iter().all(|p| p.get() == 1.0 / 3.0));
                    assert_eq!(score.expected, 1.0);
                }
                DecisionAnswer::Noul(noul) => assert_eq!(noul.probability_true.get(), 0.5),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Determinism and language neutrality
    // -----------------------------------------------------------------------

    /// `builtin_is_deterministic_across_runs_and_orderings`: the same
    /// request answers the same bits every time, and listing the candidates
    /// in another order changes nothing.
    #[test]
    fn builtin_is_deterministic_across_runs_and_orderings() {
        let key = key();
        let forward = typical_candidates();
        let mut backward = forward.clone();
        backward.reverse();
        let turn = "look at https://example.com/a and /tmp/b?";
        let first = seal(
            &key,
            state(turn, forward.clone()),
            compiler_questions(&state(turn, forward)),
        );
        let second = seal(
            &key,
            state(turn, backward.clone()),
            compiler_questions(&state(turn, backward)),
        );
        let a = format!("{:?}", answer_and_validate(&first).answers);
        let b = format!("{:?}", BuiltinDecisionBackend::answer(&first).unwrap().answers);
        let c = format!("{:?}", answer_and_validate(&second).answers);
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn tied_global_choices_share_the_lower_effect_candidate_without_changing_probabilities() {
        let key = key();
        let mut read = candidate_json("use_tool", "skill", "use_tool-skill-ffffffff");
        read["exact_mention"] = json!(true);
        let mut write = candidate_json("use_tool", "filesystem", "use_tool-filesystem-00000001");
        write["exact_mention"] = json!(true);
        write["side_effect"] = json!("local_write");
        for candidates in [vec![read.clone(), write.clone()], vec![write.clone(), read.clone()]] {
            let value = state("Read the reference and create artifact.txt", candidates);
            let request = seal(&key, value.clone(), compiler_questions(&value));
            let response = answer_and_validate(&request);
            assert_eq!(chosen(&response, GlobalQuestion::NextAction), "use_tool");
            assert_eq!(chosen(&response, GlobalQuestion::CapabilityTarget), "skill");
            let DecisionAnswer::Choice(choice) =
                &response.answers[&QuestionId::global(GlobalQuestion::CapabilityTarget)]
            else {
                panic!("target choice")
            };
            assert_eq!(
                choice.probabilities[&OptionId::code("skill")],
                choice.probabilities[&OptionId::code("filesystem")]
            );
        }
        let value = state("Use the named tools on /tmp/artifact.txt", vec![read, write]);
        let request = seal(&key, value.clone(), compiler_questions(&value));
        assert_eq!(
            chosen(&answer_and_validate(&request), GlobalQuestion::CapabilityTarget),
            "filesystem",
            "a stronger structural score still wins; this is only an exact tie rule"
        );
    }

    /// Two candidates with equal scores but different side-effect risk:
    /// the global risk answer takes the lower effect, then the smaller id,
    /// whatever order the state lists them in.
    #[test]
    fn tied_candidates_are_broken_by_effect_then_id_not_by_order() {
        let key = key();
        let mut safe = candidate_json("use_tool", "mcp", "use_tool-mcp-00000001");
        safe["side_effect"] = json!("read_only");
        let mut dangerous = candidate_json("use_tool", "mcp", "use_tool-mcp-00000002");
        dangerous["side_effect"] = json!("destructive");
        let risk = |candidates: Vec<Value>| {
            let value = state("run it", candidates);
            let request = seal(&key, value.clone(), compiler_questions(&value));
            format!(
                "{:?}",
                answer_and_validate(&request).answers[&QuestionId::global(GlobalQuestion::SemanticRisk)]
            )
        };
        let forward = risk(vec![safe.clone(), dangerous.clone()]);
        let backward = risk(vec![dangerous, safe]);
        assert_eq!(forward, backward);
    }

    /// One segment of a request text: letters that may be translated, or
    /// structure that stays.
    #[derive(Clone, Copy)]
    enum Segment<'a> {
        Words(&'a str),
        Structure(&'a str),
    }

    /// One script the words are replaced with.
    #[derive(Clone, Copy, Debug)]
    struct Script {
        name: &'static str,
        /// Six letters, each one code point or a base and a combining mark
        /// (the decomposed forms); a second code point of 0 is none.
        letters: [[u32; 2]; 6],
        /// Written with spaces between words. Only the scripts written
        /// without spaces get the glued layout (D-061 amendment 4).
        spaced: bool,
    }

    /// Six single-code-point letters.
    const fn single(points: [u32; 6]) -> [[u32; 2]; 6] {
        [
            [points[0], 0],
            [points[1], 0],
            [points[2], 0],
            [points[3], 0],
            [points[4], 0],
            [points[5], 0],
        ]
    }

    /// Unicode data table (D-061 L-4 and amendment 4): six letters of each
    /// script the words are replaced with, as code points. The repository
    /// bans CJK prose in source; these are character-class samples, not
    /// text. Chinese, Japanese, Thai and Lao are written without spaces;
    /// the Latin scripts cover ASCII, accented letters in NFC and in NFD
    /// (base plus combining mark), and Vietnamese with stacked marks.
    const SCRIPTS: [Script; 15] = [
        Script {
            name: "Chinese",
            letters: single([0x8bf7, 0x641c, 0x7d22, 0x6587, 0x4ef6, 0x7f51]),
            spaced: false,
        },
        Script {
            name: "Japanese",
            letters: single([0x30ed, 0x30b0, 0x3092, 0x898b, 0x305b, 0x3066]),
            spaced: false,
        },
        Script {
            name: "Thai",
            letters: single([0x0e40, 0x0e1b, 0x0e34, 0x0e14, 0x0e01, 0x0e32]),
            spaced: false,
        },
        Script {
            name: "Lao",
            letters: single([0x0e81, 0x0e82, 0x0e84, 0x0e87, 0x0e88, 0x0e8a]),
            spaced: false,
        },
        Script {
            name: "Arabic",
            letters: single([0x0627, 0x0628, 0x062d, 0x062b, 0x0639, 0x0646]),
            spaced: true,
        },
        Script {
            name: "Russian",
            letters: single([0x043f, 0x043e, 0x0438, 0x0441, 0x043a, 0x0444]),
            spaced: true,
        },
        Script {
            name: "Georgian",
            letters: single([0x10eb, 0x10d4, 0x10d1, 0x10dc, 0x10d0, 0x10e4]),
            spaced: true,
        },
        Script {
            name: "Hindi",
            letters: single([0x0916, 0x094b, 0x091c, 0x0947, 0x0902, 0x092b]),
            spaced: true,
        },
        Script {
            name: "Latin",
            letters: single([0x0078, 0x0071, 0x007a, 0x006a, 0x0077, 0x006b]),
            spaced: true,
        },
        Script {
            name: "Latin accented",
            letters: single([0x00e9, 0x00f1, 0x00fc, 0x00f8, 0x00e7, 0x00e5]),
            spaced: true,
        },
        Script {
            name: "Latin accented NFD",
            letters: [
                [0x65, 0x301],
                [0x6e, 0x303],
                [0x75, 0x308],
                [0x61, 0x30a],
                [0x63, 0x327],
                [0x6f, 0x302],
            ],
            spaced: true,
        },
        Script {
            name: "Greek",
            letters: single([0x03b1, 0x03b2, 0x03b3, 0x03b4, 0x03b5, 0x03bb]),
            spaced: true,
        },
        Script {
            name: "Hebrew",
            letters: single([0x05d0, 0x05d1, 0x05d2, 0x05d3, 0x05d4, 0x05d5]),
            spaced: true,
        },
        Script {
            name: "Korean",
            letters: single([0xd55c, 0xad6d, 0xc5b4, 0xac00, 0xb098, 0xb2e4]),
            spaced: true,
        },
        Script {
            name: "Vietnamese",
            letters: single([0x1ec7, 0x1edf, 0x1ea1, 0x01b0, 0x0111, 0x0103]),
            spaced: true,
        },
    ];

    /// Push one letter of a script.
    fn push_letter(out: &mut String, letter: [u32; 2]) {
        for point in letter {
            if point != 0 {
                out.extend(char::from_u32(point));
            }
        }
    }

    /// How the structure sits among the words (D-061 M-1): any combination
    /// of four transformations, all off being the text as written, with
    /// spaces around the structure (D-061 amendment 3: every combination is
    /// tested, not a fixed list).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Layout {
        /// No space between the words and the structure, as scripts written
        /// without spaces put it. Only for those scripts: in every other
        /// script `word/path` is one word (D-061 amendment 4).
        glued: bool,
        /// The structure in full-width forms, as CJK input methods type it.
        full_width: bool,
        /// An invisible format character after every character of the
        /// structure, so no shape is found unless the scan skips them all.
        marked: bool,
        /// The names inside the structure in the target script as well:
        /// localized directories, an internationalized host. A URL scheme,
        /// a drive letter and the first name after a bare `/` stay ASCII,
        /// as the scan requires (D-061 amendment N-1, N-2).
        localized: bool,
    }

    impl Layout {
        const SPACED: Self = Self::from_bits(0);

        /// The layout of the four bits of `bits`.
        const fn from_bits(bits: u8) -> Self {
            Self {
                glued: bits & 1 != 0,
                full_width: bits & 2 != 0,
                marked: bits & 4 != 0,
                localized: bits & 8 != 0,
            }
        }

        /// Every combination of the four transformations.
        fn all() -> impl Iterator<Item = Self> {
            (0..16_u8).map(Self::from_bits)
        }
    }

    /// Unicode data (category Cf): left-to-right mark, right-to-left mark,
    /// zero-width space, word joiner, Mongolian vowel separator, zero-width
    /// non-joiner.
    const MARKS: [u32; 6] = [0x200E, 0x200F, 0x200B, 0x2060, 0x180E, 0x200C];

    /// `text` with each run of ASCII letters replaced by letters of
    /// `letters`, except a run right before `:` (a scheme or a drive) and
    /// the first run after a leading bare `/`.
    fn localize(text: &str, script: &Script, next: &mut impl FnMut() -> u64) -> String {
        let mut out = String::new();
        let chars: Vec<char> = text.chars().collect();
        let lead = chars.iter().position(|character| !character.is_whitespace());
        let mut index = 0;
        while index < chars.len() {
            if !chars[index].is_ascii_alphabetic() {
                out.push(chars[index]);
                index += 1;
                continue;
            }
            let end = chars[index..]
                .iter()
                .position(|character| !character.is_ascii_alphabetic())
                .map_or(chars.len(), |length| index + length);
            let before_colon = chars.get(end) == Some(&':');
            let first_after_bare_slash = index > 0 && chars[index - 1] == '/' && lead == Some(index - 1);
            if before_colon || first_after_bare_slash {
                out.extend(&chars[index..end]);
            } else {
                for _ in 0..=(next() % 6) {
                    push_letter(&mut out, script.letters[(next() % 6) as usize]);
                }
            }
            index = end;
        }
        out
    }

    fn full_width(text: &str) -> String {
        text.chars()
            .map(|character| {
                if ('!'..='~').contains(&character) {
                    char::from_u32(character as u32 + 0xFEE0).unwrap_or(character)
                } else {
                    character
                }
            })
            .collect()
    }

    /// `text` with every maximal run of letters in a `Words` segment replaced
    /// by letters of `script` (length and content drawn from `seed`), and
    /// the structure laid out as `layout` says.
    fn render(segments: &[Segment<'_>], script: Option<&Script>, seed: u64, layout: Layout) -> String {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut out = String::new();
        for segment in segments {
            match (segment, script) {
                (Segment::Structure(text), _) => {
                    let mut structure = match script {
                        Some(script) if layout.localized => localize(text, script, &mut next),
                        _ => (*text).to_string(),
                    };
                    if layout.glued {
                        structure = structure.trim().to_string();
                    }
                    if layout.full_width {
                        structure = full_width(&structure);
                    }
                    if layout.marked {
                        for (index, character) in structure.chars().enumerate() {
                            out.push(character);
                            out.extend(char::from_u32(MARKS[index % MARKS.len()]));
                        }
                    } else {
                        out.push_str(&structure);
                    }
                }
                (Segment::Words(text), None) => out.push_str(text),
                (Segment::Words(text), Some(script)) => {
                    let text = if layout.glued { text.trim() } else { text };
                    let mut in_run = false;
                    for character in text.chars() {
                        if character.is_alphabetic() {
                            if !in_run {
                                // A run of 1..=6 letters of the target script.
                                for _ in 0..=(next() % 6) {
                                    push_letter(&mut out, script.letters[(next() % 6) as usize]);
                                }
                            }
                            in_run = true;
                        } else {
                            in_run = false;
                            out.push(character);
                        }
                    }
                }
            }
        }
        out
    }

    fn requests() -> Vec<Vec<Segment<'static>>> {
        use Segment::{Structure, Words};
        vec![
            vec![
                Words("please look at"),
                Structure(" https://example.com/docs "),
                Words("and summarize it"),
                Structure("?"),
            ],
            vec![
                Words("open"),
                Structure(" /var/log/app.log "),
                Words("and show me the errors"),
            ],
            vec![Words("run"), Structure(" ~/build.sh "), Words("again")],
            vec![Words("run"), Structure(" ./build.sh "), Words("again")],
            vec![Words("run"), Structure(r" .\build.bat "), Words("again")],
            vec![Words("open"), Structure(r" ..\notes\plan.md "), Words("please")],
            vec![Words("open"), Structure(r" C:\data\report.txt "), Words("please")],
            vec![Words("hello there, how are you today"), Structure("?")],
            vec![Words("run"), Structure(" `cargo test` "), Words("for me please")],
            vec![Words("make this a little shorter")],
            vec![Words("search for the bugs in the audit review")],
            vec![Structure("   ")],
        ]
    }

    /// `builtin_is_invariant_under_translation_and_letter_scrambling`
    /// (SP-CDP-A 5.7; D-061 M-1): replacing the letters of each request with
    /// letters of another script (Chinese, Japanese, Thai, Lao, Arabic,
    /// Russian, Georgian, Hindi, accented Latin in NFC and NFD, Greek,
    /// Hebrew, Korean, Vietnamese) or with seeded random letters leaves every
    /// answer bit-identical, and so does laying the structure out in every
    /// combination of: without spaces (for the scripts written without spaces), in full-width
    /// forms, with invisible format characters inside it, and with its names
    /// in the target script (D-061 amendment 3). The companion RB-A on the legacy
    /// `DefaultTreeExpander` shows the same request in Chinese selects other
    /// branches today (`CDP06-rb-a-legacy-expander.log`).
    #[test]
    fn builtin_is_invariant_under_translation_and_letter_scrambling() {
        let key = key();
        let candidates = typical_candidates();
        for (index, segments) in requests().iter().enumerate() {
            let original = render(segments, None, 0, Layout::SPACED);
            let reference = {
                let request = seal(
                    &key,
                    state(&original, candidates.clone()),
                    compiler_questions(&state(&original, candidates.clone())),
                );
                format!("{:?}", answer_and_validate(&request).answers)
            };
            for script in &SCRIPTS {
                let name = script.name;
                for layout in Layout::all() {
                    if layout.glued && script.spaced {
                        continue;
                    }
                    for seed in 0..3_u64 {
                        let variant = render(segments, Some(script), seed * 31 + index as u64, layout);
                        let request = seal(
                            &key,
                            state(&variant, candidates.clone()),
                            compiler_questions(&state(&variant, candidates.clone())),
                        );
                        let answers = format!("{:?}", answer_and_validate(&request).answers);
                        assert_eq!(
                            answers, reference,
                            "request {index} in {name}, {layout:?} (seed {seed}): {variant:?}"
                        );
                    }
                }
            }
        }
    }

    /// The answers of the builtin to `text` over the typical candidates, as
    /// comparable text.
    fn answers_for(key: &DecisionKey, text: &str) -> String {
        let candidates = typical_candidates();
        let request = seal(
            key,
            state(text, candidates.clone()),
            compiler_questions(&state(text, candidates)),
        );
        format!("{:?}", answer_and_validate(&request).answers)
    }

    proptest! {
        /// D-061 amendment 3 (property): for any request, script, combination
        /// of layouts and letter seed, the answers are those of the English
        /// text as written.
        #[test]
        fn every_layout_combination_is_invariant(
            request in 0..requests().len(),
            script in 0..SCRIPTS.len(),
            bits in 0..16_u8,
            seed in any::<u64>(),
        ) {
            let script = SCRIPTS[script];
            let mut layout = Layout::from_bits(bits);
            // Glued only for the scripts written without spaces; drawn as
            // spaced otherwise, so no case is rejected.
            layout.glued &= !script.spaced;
            let segments = &requests()[request];
            let key = key();
            let reference = answers_for(&key, &render(segments, None, 0, Layout::SPACED));
            let variant = render(segments, Some(&script), seed, layout);
            prop_assert_eq!(
                answers_for(&key, &variant),
                reference,
                "request {} in {}, {:?}: {:?}",
                request,
                script.name,
                layout,
                variant
            );
        }

        /// D-061 amendment 4 (property): a generated path of any shape
        /// (bare, home, relative with either separator, drive with either
        /// separator; one to four names; with or without an extension), in
        /// any script and any combination of layouts, gives the answers of
        /// the English text as written. The ruled residuals of a path glued
        /// to its words keep ASCII names: its first name must be wholly
        /// ASCII, and a localized glued path needs three names or an
        /// extension.
        #[test]
        fn generated_path_shapes_read_alike_in_every_script_and_layout(
            prefix in prop::sample::select(PathPrefix::ALL.to_vec()),
            names in prop::collection::vec("[a-z]{1,8}", 1..=4),
            extension in prop::option::of("[a-z]{1,4}"),
            script in 0..SCRIPTS.len(),
            bits in 0..16_u8,
            seed in any::<u64>(),
        ) {
            let script = SCRIPTS[script];
            let mut layout = Layout::from_bits(bits);
            // Glued only for the scripts written without spaces; drawn as
            // spaced otherwise, so no case is rejected.
            layout.glued &= !script.spaced;
            // The ruled residuals are drawn with ASCII names instead.
            let residual = prefix == PathPrefix::Bare
                && layout.glued
                && layout.localized
                && (names.len() == 1 || (names.len() == 2 && extension.is_none()));
            layout.localized &= !residual;
            let path = format!(" {} ", path_text(prefix, &names, extension.as_deref()));
            let segments = [Segment::Words("open"), Segment::Structure(&path), Segment::Words("and show it")];
            let key = key();
            let reference = answers_for(&key, &render(&segments, None, 0, Layout::SPACED));
            let variant = render(&segments, Some(&script), seed, layout);
            prop_assert_eq!(
                answers_for(&key, &variant),
                reference,
                "{:?} in {}, {:?}: {:?}",
                path,
                script.name,
                layout,
                variant
            );
        }
    }

    /// How a generated path starts.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum PathPrefix {
        Bare,
        Home,
        Here,
        Up,
        HereBackslash,
        UpBackslash,
        DriveBackslash,
        DriveSlash,
    }

    impl PathPrefix {
        const ALL: [Self; 8] = [
            Self::Bare,
            Self::Home,
            Self::Here,
            Self::Up,
            Self::HereBackslash,
            Self::UpBackslash,
            Self::DriveBackslash,
            Self::DriveSlash,
        ];
    }

    /// The text of a path of `names`, starting as `prefix` says, with an
    /// optional extension on the last name.
    fn path_text(prefix: PathPrefix, names: &[String], extension: Option<&str>) -> String {
        let (start, separator) = match prefix {
            PathPrefix::Bare => ("/", "/"),
            PathPrefix::Home => ("~/", "/"),
            PathPrefix::Here => ("./", "/"),
            PathPrefix::Up => ("../", "/"),
            PathPrefix::HereBackslash => (".\\", "\\"),
            PathPrefix::UpBackslash => ("..\\", "\\"),
            PathPrefix::DriveBackslash => ("C:\\", "\\"),
            PathPrefix::DriveSlash => ("C:/", "/"),
        };
        let mut text = format!("{start}{}", names.join(separator));
        if let Some(extension) = extension {
            text.push('.');
            text.push_str(extension);
        }
        text
    }

    /// `an_exact_catalog_name_in_any_script_selects_that_capability`: the
    /// trusted `exact_mention` flag, whatever script the turn is in, moves
    /// the choice to the named candidate's action and target.
    #[test]
    fn an_exact_catalog_name_in_any_script_selects_that_capability() {
        let key = key();
        for script in &SCRIPTS {
            let mut turn = String::new();
            for letter in script.letters {
                push_letter(&mut turn, letter);
            }
            let mut candidates = typical_candidates();
            for candidate in &mut candidates {
                if candidate["id"] == "delegate-session" {
                    candidate["exact_mention"] = json!(true);
                }
            }
            let named = state(&turn, candidates);
            let request = seal(&key, named.clone(), compiler_questions(&named));
            let response = answer_and_validate(&request);
            assert_eq!(chosen(&response, GlobalQuestion::NextAction), "delegate");
            assert_eq!(chosen(&response, GlobalQuestion::CapabilityTarget), "session");
        }
    }

    /// V4.05 (builtin half): the three unrouted requests of
    /// `tool_surface_budget.rs` and a Chinese one carry no keyword the
    /// builtin could read. They answer identically, and the builtin leans to
    /// Discover (a bounded local catalog search), never to a tool.
    #[test]
    fn non_english_requests_lean_to_discovery_without_keywords() {
        let key = key();
        // Unicode data table (D-061 L-4): the Han ideographs of a Chinese
        // request of the same kind ("make this a bit shorter").
        const HAN_REQUEST: [u32; 8] = [0x8bf7, 0x628a, 0x8fd9, 0x4e2a, 0x5199, 0x77ed, 0x4e00, 0x70b9];
        let chinese: String = HAN_REQUEST.iter().filter_map(|point| char::from_u32(*point)).collect();
        let unrouted = [
            "haz que esto quede un poco m\u{e1}s breve".to_string(),
            "\u{43f}\u{43e}\u{436}\u{430}\u{43b}\u{443}\u{439}\u{441}\u{442}\u{430}, \u{441}\u{43e}\u{43a}\u{440}\u{430}\u{442}\u{438} \u{44d}\u{442}\u{43e}".to_string(),
            "could you make that sound a little friendlier please".to_string(),
            chinese,
        ];
        let mut distributions = Vec::new();
        for message in &unrouted {
            let turn = state(message, typical_candidates());
            let request = seal(&key, turn.clone(), compiler_questions(&turn));
            let response = answer_and_validate(&request);
            assert_eq!(chosen(&response, GlobalQuestion::NextAction), "discover", "{message:?}");
            assert_eq!(
                chosen(&response, GlobalQuestion::CapabilityTarget),
                "skill",
                "{message:?}"
            );
            distributions.push(format!("{:?}", response.answers));
        }
        assert!(distributions.windows(2).all(|pair| pair[0] == pair[1]));
    }

    /// D-061 M-2: structure decides without words. A URL commits to the
    /// web tool, a path to the filesystem tool and code to the shell (the
    /// chosen action and target, not just a raised probability); an empty
    /// first turn clarifies; a pending approval steers away from using a
    /// tool.
    #[test]
    fn structure_and_trusted_state_move_the_choice() {
        let key = key();
        let pick = |turn: &str, edit: &dyn Fn(&mut Value)| {
            let mut value = state(turn, typical_candidates());
            edit(&mut value);
            let request = seal(&key, value.clone(), compiler_questions(&value));
            let response = answer_and_validate(&request);
            (
                chosen(&response, GlobalQuestion::NextAction),
                chosen(&response, GlobalQuestion::CapabilityTarget),
            )
        };
        let tool = |target: &str| ("use_tool".to_string(), target.to_string());
        assert_eq!(
            pick("look at https://example.com/report and summarize it", &|_| {}),
            tool("web")
        );
        assert_eq!(
            pick("open /var/log/app.log and show the errors", &|_| {}),
            tool("filesystem")
        );
        assert_eq!(pick(r"open C:\data\report.txt", &|_| {}), tool("filesystem"));
        assert_eq!(pick("run `cargo test` for me", &|_| {}), tool("shell"));
        assert_eq!(pick("  ", &|_| {}).0, "clarify");
        let pending = pick("ok", &|value| {
            value["approval_pending"] = json!(true);
            value["candidates"].as_array_mut().unwrap().push(candidate_json(
                "request_approval",
                "shell",
                "request_approval-shell",
            ));
        });
        assert_ne!(pending.0, "use_tool");
    }

    #[test]
    fn a_plain_conversation_turn_can_reach_the_model_while_undecidable_input_still_clarifies() {
        let key = key();
        let terminal_candidates = || {
            vec![
                candidate_json("answer", "none", "answer-none"),
                candidate_json("clarify", "none", "clarify-none"),
                candidate_json("refuse", "none", "refuse-none"),
            ]
        };
        let answer = |value: Value| {
            let request = seal(&key, value.clone(), compiler_questions(&value));
            answer_and_validate(&request)
        };

        let plain = answer(state("Explain the current result", terminal_candidates()));
        assert_eq!(chosen(&plain, GlobalQuestion::NextAction), "answer");
        assert!(
            noul_probability(&plain, GlobalQuestion::EvidenceSufficient) >= 0.5,
            "a direct answer must pass the selector's evidence threshold"
        );

        let greeting = answer(state("hello", typical_candidates()));
        assert_eq!(chosen(&greeting, GlobalQuestion::NextAction), "answer");
        assert!(
            noul_probability(&greeting, GlobalQuestion::EvidenceSufficient) >= 0.5,
            "a brief greeting must reach the serving model even when generic capabilities are available"
        );

        let mut direct_only = typical_candidates();
        direct_only.retain(|candidate| {
            !matches!(
                candidate.get("action").and_then(Value::as_str),
                Some("retrieve" | "discover")
            )
        });
        let direct_only = answer(state("stream the fixture reply", direct_only));
        assert_eq!(chosen(&direct_only, GlobalQuestion::NextAction), "answer");

        let mut continued = state("thanks", typical_candidates());
        continued["completed_steps"] = json!([{"kind": "provider", "status": "ok"}]);
        let continued = answer(continued);
        assert_eq!(chosen(&continued, GlobalQuestion::NextAction), "answer");
        assert!(
            noul_probability(&continued, GlobalQuestion::EvidenceSufficient) >= 0.5,
            "ordinary conversation history must not force a clarification or external lookup"
        );

        let mut longer_continued = state("p4a second visible turn", typical_candidates());
        longer_continued["has_conversation_history"] = json!(true);
        let longer_continued = answer(longer_continued);
        assert_eq!(chosen(&longer_continued, GlobalQuestion::NextAction), "answer");

        let mut retrieved = state("p4a second visible turn", typical_candidates());
        retrieved["has_conversation_history"] = json!(true);
        retrieved["completed_steps"] = json!([
            {"kind": "provider", "status": "ok"},
            {"kind": "retrieve", "status": "ok"}
        ]);
        let retrieved = answer(retrieved);
        assert_ne!(chosen(&retrieved, GlobalQuestion::NextAction), "answer");

        let empty = answer(state("  ", terminal_candidates()));
        assert_eq!(chosen(&empty, GlobalQuestion::NextAction), "clarify");
        assert!(noul_probability(&empty, GlobalQuestion::EvidenceSufficient) < 0.5);

        let structured = answer(state("inspect /tmp/prx_fixture", terminal_candidates()));
        assert!(
            noul_probability(&structured, GlobalQuestion::EvidenceSufficient) < 0.5,
            "a structurally external request still needs external evidence"
        );

        let admitted_structured = answer(state("inspect /tmp/prx_fixture", typical_candidates()));
        assert_eq!(chosen(&admitted_structured, GlobalQuestion::NextAction), "use_tool");
        assert_eq!(
            chosen(&admitted_structured, GlobalQuestion::CapabilityTarget),
            "filesystem"
        );
        assert!(
            noul_probability(&admitted_structured, GlobalQuestion::EvidenceSufficient) < 0.5,
            "a matching external candidate must not bypass retrieve-first evidence"
        );

        let external = answer(state("please find the relevant capability", typical_candidates()));
        assert_eq!(chosen(&external, GlobalQuestion::NextAction), "discover");
        assert!(
            noul_probability(&external, GlobalQuestion::EvidenceSufficient) >= 0.5,
            "a plain first turn must not be forced into recovery before its ranked discovery can run"
        );

        let mut pending = state("ok", terminal_candidates());
        pending["approval_pending"] = json!(true);
        pending["candidates"].as_array_mut().unwrap().push(candidate_json(
            "request_approval",
            "shell",
            "request_approval-shell",
        ));
        let pending = answer(pending);
        assert!(
            noul_probability(&pending, GlobalQuestion::EvidenceSufficient) < 0.5,
            "pending approval must retain its existing evidence gate"
        );

        let mut failed = state("Explain the current result", terminal_candidates());
        failed["completed_steps"] = json!([{"kind": "tool", "status": "failed"}]);
        let failed = answer(failed);
        assert!(
            noul_probability(&failed, GlobalQuestion::EvidenceSufficient) < 0.5,
            "a failed step still requires recovery evidence"
        );

        let mut retried = state("Explain the current result", terminal_candidates());
        retried["retry_count"] = json!(weights::RETRY_THRESHOLD);
        let retried = answer(retried);
        assert!(
            noul_probability(&retried, GlobalQuestion::EvidenceSufficient) < 0.5,
            "an exhausted retry budget still requires clarification"
        );
    }

    #[test]
    fn concrete_artifact_without_recovery_is_sufficient_evidence() {
        let key = key();
        let direct_tools = || {
            vec![
                candidate_json("use_tool", "filesystem", "use_tool-filesystem"),
                candidate_json("use_tool", "shell", "use_tool-shell"),
                candidate_json("use_tool", "web", "use_tool-web"),
                candidate_json("answer", "none", "answer-none"),
                candidate_json("clarify", "none", "clarify-none"),
                candidate_json("refuse", "none", "refuse-none"),
            ]
        };
        let answer = |turn: &str, candidates: Vec<Value>| {
            let value = state(turn, candidates);
            let request = seal(&key, value.clone(), compiler_questions(&value));
            answer_and_validate(&request)
        };
        let explicit =
            "Read /tmp/prx_fixture/input.md and write /tmp/prx_fixture/reports/x.md. End the file with STATUS: A or B.";
        let direct = answer(explicit, direct_tools());
        assert_eq!(chosen(&direct, GlobalQuestion::NextAction), "use_tool");
        assert_eq!(chosen(&direct, GlobalQuestion::CapabilityTarget), "filesystem");
        assert!(
            (noul_probability(&direct, GlobalQuestion::EvidenceSufficient) - weights::CONCRETE_ARTIFACT_EVIDENCE).abs()
                < 1e-12
        );

        let with_recovery = answer(explicit, typical_candidates());
        assert!(
            noul_probability(&with_recovery, GlobalQuestion::EvidenceSufficient) < 0.5,
            "retrieve or discover still owns the first hop"
        );

        let terminal = answer(
            explicit,
            vec![
                candidate_json("answer", "none", "answer-none"),
                candidate_json("clarify", "none", "clarify-none"),
                candidate_json("refuse", "none", "refuse-none"),
            ],
        );
        assert!(noul_probability(&terminal, GlobalQuestion::EvidenceSufficient) < 0.5);

        let url = answer("look at https://example.com/report and summarize it", direct_tools());
        assert_eq!(chosen(&url, GlobalQuestion::CapabilityTarget), "web");
        assert!(
            (noul_probability(&url, GlobalQuestion::EvidenceSufficient) - weights::CONCRETE_ARTIFACT_EVIDENCE).abs()
                < 1e-12
        );
        let code = answer("run `cargo test` for me", direct_tools());
        assert_eq!(chosen(&code, GlobalQuestion::CapabilityTarget), "shell");
        assert!(
            (noul_probability(&code, GlobalQuestion::EvidenceSufficient) - weights::CONCRETE_ARTIFACT_EVIDENCE).abs()
                < 1e-12
        );

        let ambiguous = answer("make this friendlier", direct_tools());
        assert_eq!(chosen(&ambiguous, GlobalQuestion::NextAction), "answer");
        assert!(
            (noul_probability(&ambiguous, GlobalQuestion::EvidenceSufficient) - weights::PLAIN_TURN_EVIDENCE).abs()
                < 1e-12
        );
        let empty = answer("  ", direct_tools());
        assert_eq!(chosen(&empty, GlobalQuestion::NextAction), "clarify");
        assert!(noul_probability(&empty, GlobalQuestion::EvidenceSufficient) < 0.5);

        let mut failed = state(explicit, direct_tools());
        failed["completed_steps"] = json!([{"kind": "tool", "status": "failed"}]);
        let failed_request = seal(&key, failed.clone(), compiler_questions(&failed));
        let failed = answer_and_validate(&failed_request);
        assert!(noul_probability(&failed, GlobalQuestion::EvidenceSufficient) < 0.5);

        let mut retried = state(explicit, direct_tools());
        retried["retry_count"] = json!(weights::RETRY_THRESHOLD);
        let retried_request = seal(&key, retried.clone(), compiler_questions(&retried));
        let retried = answer_and_validate(&retried_request);
        assert!(noul_probability(&retried, GlobalQuestion::EvidenceSufficient) < 0.5);
    }

    /// D-061 amendment N-1..N-4: a localized path, an internationalized
    /// host, a full-width colon before a path, a yen-separated drive path and
    /// an accented file name choose the same tool as their English forms
    /// (the reviewer's choice probes, `R-CDP06-probes.log`).
    #[test]
    fn localized_structure_chooses_like_the_english_form() {
        // Unicode data: Han words (open, download, report), Cyrillic words
        // (example, the .rf domain), the full-width colon, the yen sign,
        // U+00E9 (e with acute), the Han month sign, Han "plan", Han
        // "Zhang San" and U+00FA (u with acute).
        const HAN_OPEN: [u32; 2] = [0x6253, 0x5F00];
        const HAN_DOWNLOAD: [u32; 2] = [0x4E0B, 0x8F7D];
        const HAN_REPORT: [u32; 2] = [0x62A5, 0x544A];
        const CYR_EXAMPLE: [u32; 6] = [0x043F, 0x0440, 0x0438, 0x043C, 0x0435, 0x0440];
        const CYR_RF: [u32; 2] = [0x0440, 0x0444];
        const FULL_WIDTH_COLON: [u32; 1] = [0xFF1A];
        const YEN: [u32; 1] = [0x00A5];
        const E_ACUTE: [u32; 1] = [0x00E9];
        const HAN_MONTH: [u32; 1] = [0x6708];
        const HAN_PLAN: [u32; 2] = [0x5957, 0x9910];
        const HAN_ZHANG_SAN: [u32; 2] = [0x5F20, 0x4E09];
        const U_ACUTE: [u32; 1] = [0x00FA];
        let word = |points: &[u32]| {
            points
                .iter()
                .filter_map(|point| char::from_u32(*point))
                .collect::<String>()
        };
        let key = key();
        let pick = |turn: &str| {
            let value = state(turn, typical_candidates());
            let request = seal(&key, value.clone(), compiler_questions(&value));
            let response = answer_and_validate(&request);
            (
                chosen(&response, GlobalQuestion::NextAction),
                chosen(&response, GlobalQuestion::CapabilityTarget),
            )
        };
        let yen = word(&YEN);
        let pairs = [
            (
                "open ~/Downloads/report.pdf".to_string(),
                format!("{}~/{}/{}.pdf", word(&HAN_OPEN), word(&HAN_DOWNLOAD), word(&HAN_REPORT)),
                "filesystem",
            ),
            (
                "open /home/user/docs".to_string(),
                format!("open /home/{}/{}", word(&HAN_DOWNLOAD), word(&HAN_REPORT)),
                "filesystem",
            ),
            (
                "see https://example.com/a".to_string(),
                format!("see https://{}.{}/a", word(&CYR_EXAMPLE), word(&CYR_RF)),
                "web",
            ),
            (
                "file: /etc/hosts".to_string(),
                format!("{}{}/etc/hosts", word(&HAN_OPEN), word(&FULL_WIDTH_COLON)),
                "filesystem",
            ),
            (
                r"open C:\Users\data\x.txt".to_string(),
                format!("open C:{yen}Users{yen}data{yen}x.txt"),
                "filesystem",
            ),
            (
                "open /home/zhang/report.pdf".to_string(),
                format!(
                    "{}/home/{}/{}.pdf",
                    word(&HAN_OPEN),
                    word(&HAN_ZHANG_SAN),
                    word(&HAN_REPORT)
                ),
                "filesystem",
            ),
            (
                "see /cafe.txt".to_string(),
                format!("see /caf{}.txt", word(&E_ACUTE)),
                "filesystem",
            ),
        ];
        for (english, localized, target) in pairs {
            let expected = ("use_tool".to_string(), target.to_string());
            assert_eq!(pick(&english), expected, "{english:?}");
            assert_eq!(pick(&localized), expected, "{localized:?}");
        }
        // D-061 amendment 2 N-5, N-6: a month list and a yen price list,
        // like `Jan/Feb/Mar report` and `Plan A: $99`, commit to no tool.
        let month = word(&HAN_MONTH);
        let colon = word(&FULL_WIDTH_COLON);
        for prose in [
            "Jan/Feb/Mar report".to_string(),
            format!("1{month}/2{month}/3{month}{}", word(&HAN_REPORT)),
            "Plan A: $99".to_string(),
            format!("{}A{colon}{yen}99", word(&HAN_PLAN)),
            "Plan A: $99/mo".to_string(),
            format!("{}A{colon}{yen}99/{month}", word(&HAN_PLAN)),
            "Peru/Chile/Bolivia".to_string(),
            format!("Per{}/Chile/Bolivia", word(&U_ACUTE)),
        ] {
            assert_ne!(pick(&prose).0, "use_tool", "{prose:?}");
        }
    }

    /// D-061 M-3: the trusted per-candidate `failed_before` flag penalises
    /// that candidate only. The approval and discovery candidates of the
    /// same namespace answer exactly as if nothing had failed, and the
    /// failed tool is not picked again.
    #[test]
    fn a_failure_penalises_only_the_candidate_that_failed() {
        let key = key();
        let namespace_candidates = |failed: bool| {
            let mut used = candidate_json("use_tool", "web", "use_tool-web-0000000a");
            used["failed_before"] = json!(failed);
            let mut candidates = typical_candidates();
            candidates.retain(|candidate| candidate["target"] != "web");
            candidates.push(used);
            candidates.push(candidate_json(
                "request_approval",
                "web",
                "request_approval-web-0000000a",
            ));
            candidates.push(candidate_json("discover", "web", "discover-web-0000000a"));
            candidates
        };
        let answers = |failed: bool| {
            let value = state("look at https://example.com/report", namespace_candidates(failed));
            let request = seal(&key, value.clone(), compiler_questions(&value));
            answer_and_validate(&request)
        };
        let (clean, failed) = (answers(false), answers(true));
        for action in ["request_approval", "discover"] {
            for question in CandidateQuestion::ALL {
                let id = QuestionId::candidate(&key_for(action, "web", Some(10)), question);
                assert_eq!(
                    format!("{:?}", clean.answers[&id]),
                    format!("{:?}", failed.answers[&id]),
                    "{action}-web {question:?} is not penalised"
                );
            }
        }
        let used = QuestionId::candidate(&key_for("use_tool", "web", Some(10)), CandidateQuestion::Success);
        let success = |response: &DecisionResponse| match &response.answers[&used] {
            DecisionAnswer::Noul(noul) => noul.probability_true.get(),
            other => panic!("expected a noul, got {other:?}"),
        };
        assert!(
            success(&failed) < success(&clean),
            "the failed tool itself is penalised"
        );
        assert_ne!(
            chosen(&failed, GlobalQuestion::NextAction),
            "use_tool",
            "and not picked again"
        );
    }

    // -----------------------------------------------------------------------
    // V7.01: bounded allocation
    // -----------------------------------------------------------------------

    /// V7.01 (builtin allocation): allocations per decision are identical
    /// across runs, do not depend on the length of the text (the scan never
    /// allocates) nor on how many candidates the state lists beyond the
    /// questions asked, and equal a pinned count (D-061 L-3).
    #[test]
    fn builtin_decide_allocations_bounded() {
        let key = key();
        let questions = |candidates: &[Value]| {
            let mut questions = compiler_questions(&state("x", candidates.to_vec()));
            questions.retain(|id, _| matches!(meaning(id), Meaning::Global(_)));
            questions
        };
        let few = typical_candidates();
        let many: Vec<Value> = (0..60)
            .map(|index| candidate_json("use_tool", "mcp", &format!("use_tool-mcp-{index:08x}")))
            .chain(few.iter().cloned())
            .collect();
        let short = seal(&key, state("hi?", few.clone()), questions(&few));
        let long_text = format!("{} https://example.com/x ?", "word ".repeat(700));
        let long = seal(&key, state(&long_text, few.clone()), questions(&few));
        let wide = seal(&key, state("hi?", many), questions(&few));

        let (_, first) = allocation_counter::count(|| BuiltinDecisionBackend::answer(&short).unwrap());
        let (_, again) = allocation_counter::count(|| BuiltinDecisionBackend::answer(&short).unwrap());
        let (_, with_long_text) = allocation_counter::count(|| BuiltinDecisionBackend::answer(&long).unwrap());
        let (_, with_many_candidates) = allocation_counter::count(|| BuiltinDecisionBackend::answer(&wide).unwrap());
        assert_eq!(first, again, "deterministic allocation count");
        assert_eq!(first, with_long_text, "the text scan does not allocate");
        assert_eq!(
            first, with_many_candidates,
            "the candidate list is one allocation, whatever its length"
        );

        // D-061 L-3: the count is pinned exactly, not bounded loosely, so
        // any added allocation turns this red. After an intended change, run
        // this test, take the count from the failure message, set the
        // constant, and say why in the commit message.
        const PINNED_ALLOCATIONS: u64 = 24;
        assert_eq!(
            first,
            PINNED_ALLOCATIONS,
            "allocations for the {} global questions of the V7.01 request",
            short.questions().len()
        );
    }

    // -----------------------------------------------------------------------
    // Property: every request shape
    // -----------------------------------------------------------------------

    fn candidate_strategy() -> impl Strategy<Value = (usize, usize, usize, bool, bool, bool, bool)> {
        (
            0..ACTIONS.len(),
            0..TARGETS.len(),
            0..SIDE_EFFECTS.len(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        )
    }

    fn turn_strategy() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                Just("word ".to_string()),
                Just("https://x.test/a ".to_string()),
                Just("/tmp/f ".to_string()),
                Just("`c` ".to_string()),
                Just("? ".to_string()),
                Just("~/b.sh ".to_string()),
                Just(r"C:\x\y.txt ".to_string()),
                Just(full_width("/etc/hosts ")),
                Just("\u{200E}/var/a.log ".to_string()),
                Just("\n{\"k\": 1}\n".to_string()),
                "[a-z]{1,8} ",
            ],
            0..12,
        )
        .prop_map(|parts| parts.concat())
    }

    /// One key for every property case (the key only salts wire ids).
    static PROPERTY_KEY: std::sync::LazyLock<Arc<DecisionKey>> = std::sync::LazyLock::new(key);

    proptest! {
        /// Every request shape the protocol allows, over random states,
        /// candidate sets and question sets, gets a builtin answer that
        /// passes the CDP-01 validator.
        #[test]
        fn builtin_answers_validate_for_every_request_shape(
            turn in turn_strategy(),
            candidates in prop::collection::vec(candidate_strategy(), 0..10),
            retry_count in 0_u64..4,
            stale in any::<bool>(),
            empty_state in any::<bool>(),
            wide_targets in any::<bool>(),
            action_options in prop::collection::btree_set(0..ACTIONS.len(), 1..=ACTIONS.len()),
            target_options in prop::collection::btree_set(0..TARGETS.len(), 1..=TARGETS.len()),
            level_counts in prop::collection::vec(2_usize..=10, 4),
            extra_options in 0_usize..240,
        ) {
            let key: &DecisionKey = &PROPERTY_KEY;
            let listed: Vec<Value> = candidates
                .iter()
                .enumerate()
                .map(|(index, (action, target, side_effect, required, available, mention, failed))| {
                    let mut value = candidate_json(ACTIONS[*action], TARGETS[*target], &format!("{}-{}-{index:08x}", ACTIONS[*action], TARGETS[*target]));
                    value["side_effect"] = json!(SIDE_EFFECTS[*side_effect]);
                    value["approval"] = json!(if *required { "required" } else { "none" });
                    value["available"] = json!(available);
                    value["exact_mention"] = json!(mention);
                    value["failed_before"] = json!(failed);
                    value
                })
                .collect();
            let mut value = state(&turn, listed);
            value["retry_count"] = json!(retry_count);
            if stale {
                value["artifacts"] = json!([{"kind": "memory", "age_ms": 7_200_000, "summary": "old"}]);
            }
            if empty_state {
                value = json!({});
            }
            let levels = |count: usize| (0..count).map(|level| text(&format!("level {level}"))).collect::<Vec<_>>();
            let mut actions: BTreeMap<OptionId, StructuredValue> =
                action_options.iter().map(|index| (OptionId::code(ACTIONS[*index]), text(ACTIONS[*index]))).collect();
            actions.insert(OptionId::code("other"), text("other"));
            let mut targets: BTreeMap<OptionId, StructuredValue> =
                target_options.iter().map(|index| (OptionId::code(TARGETS[*index]), text(TARGETS[*index]))).collect();
            targets.insert(OptionId::code("other"), text("other"));
            // A wide Choice over candidate options and filler options.
            let mut wide: BTreeMap<OptionId, StructuredValue> = (0..extra_options + 2)
                .map(|index| (OptionId::candidate(&key_for("use_tool", "mcp", Some(u32::try_from(index).unwrap() + 1000))), text("filler")))
                .collect();
            for (index, (action, target, ..)) in candidates.iter().enumerate() {
                wide.insert(OptionId::candidate(&key_for(ACTIONS[*action], TARGETS[*target], Some(u32::try_from(index).unwrap()))), text("candidate"));
            }
            // The wide Choice goes under a recognized id (capability_target)
            // or under a Noul id asked as a Choice (unrecognized).
            let (targets, mismatched) = if wide_targets { (wide, targets) } else { (targets, wide) };
            let mut questions = BTreeMap::from([
                (QuestionId::global(GlobalQuestion::NextAction), DecisionQuestion::Choice { instructions: text("a"), criteria: actions }),
                (QuestionId::global(GlobalQuestion::CapabilityTarget), DecisionQuestion::Choice { instructions: text("t"), criteria: targets }),
                (QuestionId::global(GlobalQuestion::EvidenceSufficient), DecisionQuestion::Noul { instructions: text("e"), criteria: None }),
                (QuestionId::global(GlobalQuestion::NeedsFreshState), DecisionQuestion::Choice { instructions: text("mismatch"), criteria: mismatched }),
                (QuestionId::global(GlobalQuestion::SemanticRisk), DecisionQuestion::Score { instructions: text("r"), levels: levels(level_counts[0]) }),
                (QuestionId::global(GlobalQuestion::ExpectedGain), DecisionQuestion::Score { instructions: text("g"), levels: levels(level_counts[1]) }),
            ]);
            for (index, (action, target, ..)) in candidates.iter().enumerate() {
                let key = key_for(ACTIONS[*action], TARGETS[*target], Some(u32::try_from(index).unwrap()));
                questions.insert(QuestionId::candidate(&key, CandidateQuestion::Applicable), DecisionQuestion::Noul { instructions: text("ap"), criteria: None });
                questions.insert(QuestionId::candidate(&key, CandidateQuestion::Success), DecisionQuestion::Noul { instructions: text("su"), criteria: None });
                questions.insert(QuestionId::candidate(&key, CandidateQuestion::Gain), DecisionQuestion::Score { instructions: text("ga"), levels: levels(level_counts[2]) });
                questions.insert(QuestionId::candidate(&key, CandidateQuestion::Risk), DecisionQuestion::Score { instructions: text("ri"), levels: levels(level_counts[3]) });
            }
            let request = seal(key, value, questions);
            let response = BuiltinDecisionBackend::answer(&request).expect("test: builtin answers");
            let again = BuiltinDecisionBackend::answer(&request).expect("test: builtin answers");
            prop_assert_eq!(format!("{:?}", response.answers), format!("{:?}", again.answers));
            prop_assert!(DecisionResponseValidator::validate(&request, response, &context()).is_ok());
        }
    }
}
