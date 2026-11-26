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
use crate::config::schema::decision_engine::{BackendKind, BackendLocality};
use crate::cost::decision_attempt::error::DecisionError;
use crate::cost::decision_attempt::{BackendIdentity, DecisionUsage};
use crate::decision_engine::prediction::protocol::{
    CandidateQuestion, ChoiceAnswer, DecisionAnswer, DecisionQuestion, DecisionRequest, DecisionResponse,
    GlobalQuestion, NoulAnswer, OptionId, Probability, QuestionId, ScoreAnswer, StructuredValue,
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
            configured_model: Arc::from(crate::tools::capability::turn_state::MODEL_VERSION),
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
            resolved_model: crate::tools::capability::turn_state::MODEL_VERSION.to_string(),
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
        && state.hop == 0
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
    state.hop == 0
        && state.retry_count == 0
        && state.unsuccessful_action_steps == 0
        && state.candidates.iter().any(|candidate| {
            !candidate.failed_before
                && candidate.exact_mention
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
        crate::cost::decision_attempt::error::RequestRule::MinChoiceOptions,
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
mod tests;
