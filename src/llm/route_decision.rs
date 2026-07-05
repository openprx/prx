use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::ModelRouteConfig;
use crate::memory::{MemoryFabric, MemoryVisibility, MessageEvent, MessageEventScope};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteDecision {
    pub decision_id: String,
    pub created_at: DateTime<Utc>,
    pub owner_id: String,
    pub task_id: Option<String>,
    pub session_key: String,
    pub source_message_event_id: Option<String>,
    pub intent: String,
    pub estimated_tokens: u32,
    pub user_hint: Option<String>,
    pub candidates: Vec<RouteCandidate>,
    pub filtered_out: Vec<RouteFilterReason>,
    pub selected: RouteSelection,
    pub fallback_policy: FallbackPolicy,
    pub constraints: RouteConstraints,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteCandidate {
    pub provider: String,
    pub model: String,
    pub score: f32,
    pub estimated_cost_usd: Option<f32>,
    pub estimated_latency_ms: Option<u32>,
    pub max_context_tokens: u32,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteFilterReason {
    pub provider: String,
    pub model: String,
    pub reason: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteSelection {
    pub provider: String,
    pub model: String,
    pub score: f32,
    pub strategy: SelectionStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStrategy {
    Greedy,
    UserHint,
    Cascade,
    FallbackDefault,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FallbackPolicy {
    pub max_attempts: u8,
    pub retry_within_provider: bool,
    pub cross_provider_fallback: bool,
    pub on_context_overflow: ContextOverflowAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextOverflowAction {
    Compact,
    SwitchModel,
    Abort,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteConstraints {
    pub require_tools: bool,
    pub require_streaming: bool,
    pub require_vision: bool,
    pub max_cost_usd: Option<f32>,
    pub min_context_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderExecutionOutcome {
    pub decision_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub attempts: Vec<ProviderAttempt>,
    pub final_provider: String,
    pub final_model: String,
    pub status: ExecutionStatus,
    pub fallback_reason: Option<String>,
    pub tokens_used: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderAttempt {
    /// 1-based monotonic attempt index within a single `chat` failover loop.
    ///
    /// FIX-P0-31/#4: `u32` (not `u8`) so the "complete attempt sequence"
    /// contract holds even when `provider_retries` (an uncapped `u32` in config)
    /// pushes the bounded failover space (model_chain × providers × retries)
    /// past 255 — a `u8` would saturate and collapse later attempts onto a
    /// single non-orderable index.
    pub seq: u32,
    pub provider: String,
    pub model: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: AttemptStatus,
    pub error_class: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Success,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Success,
    FallbackSuccess,
    AllFailed { last_error_class: String },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenUsageSource {
    Reported,
    #[default]
    Estimated,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub source: TokenUsageSource,
}

/// Per-provider-call token usage with provider/model attribution and optional
/// display cost. Shared by main chat sessions and sub-agent sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeteredTokenUsageRecord {
    pub provider: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    pub source: TokenUsageSource,
    /// Display cost computed from `[cost].prices`. `None` means the model is
    /// intentionally unknown-priced for this UI pass, not zero cost.
    pub cost_usd: Option<f64>,
}

impl MeteredTokenUsageRecord {
    #[must_use]
    pub fn from_provider_outcome(
        outcome: &ProviderExecutionOutcome,
        cost_config: &crate::config::schema::CostConfig,
    ) -> Option<Self> {
        Self::from_parts(
            &outcome.final_provider,
            &outcome.final_model,
            &outcome.tokens_used,
            cost_config,
        )
    }

    #[must_use]
    pub fn from_parts(
        provider: &str,
        model: &str,
        usage: &TokenUsage,
        cost_config: &crate::config::schema::CostConfig,
    ) -> Option<Self> {
        if !usage.has_any_tokens() {
            return None;
        }

        let prompt_tokens = usage.prompt_tokens.map_or(0, u64::from);
        let completion_tokens = usage.completion_tokens.map_or(0, u64::from);
        let cache_creation_input_tokens = usage.cache_creation_input_tokens.map_or(0, u64::from);
        let cache_read_input_tokens = usage.cache_read_input_tokens.map_or(0, u64::from);
        let total_tokens = usage
            .total_tokens
            .map_or_else(|| prompt_tokens.saturating_add(completion_tokens), u64::from);
        if total_tokens == 0 && prompt_tokens == 0 && completion_tokens == 0 {
            return None;
        }

        let cost_usd = usage_cost_usd(
            provider,
            model,
            prompt_tokens,
            completion_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            cost_config,
        );

        Some(Self {
            provider: provider.to_string(),
            model: model.to_string(),
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            source: usage.source,
            cost_usd,
        })
    }
}

fn usage_cost_usd(
    provider: &str,
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    cost_config: &crate::config::schema::CostConfig,
) -> Option<f64> {
    if provider.eq_ignore_ascii_case("ollama") {
        return None;
    }
    let pricing_key = if model.contains('/') {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    };
    let pricing = cost_config.prices.get(&pricing_key)?;
    let ordinary_prompt_tokens = prompt_tokens
        .saturating_sub(cache_creation_input_tokens)
        .saturating_sub(cache_read_input_tokens);
    let cost = (cache_read_input_tokens as f64).mul_add(
        pricing.cache_read,
        (cache_creation_input_tokens as f64).mul_add(
            pricing.cache_write,
            (completion_tokens as f64).mul_add(pricing.output, ordinary_prompt_tokens as f64 * pricing.input),
        ),
    ) / 1_000_000.0;
    Some(cost)
}

impl TokenUsage {
    #[must_use]
    pub const fn reported(
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
        total_tokens: Option<u32>,
    ) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            source: TokenUsageSource::Reported,
        }
    }

    #[must_use]
    pub const fn reported_with_cache(
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
        total_tokens: Option<u32>,
        cache_creation_input_tokens: Option<u32>,
        cache_read_input_tokens: Option<u32>,
    ) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            source: TokenUsageSource::Reported,
        }
    }

    #[must_use]
    pub const fn estimated(
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
        total_tokens: Option<u32>,
    ) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            source: TokenUsageSource::Estimated,
        }
    }

    #[must_use]
    pub const fn has_any_tokens(&self) -> bool {
        self.prompt_tokens.is_some()
            || self.completion_tokens.is_some()
            || self.total_tokens.is_some()
            || self.cache_creation_input_tokens.is_some()
            || self.cache_read_input_tokens.is_some()
    }

    #[must_use]
    pub const fn is_reported(&self) -> bool {
        matches!(self.source, TokenUsageSource::Reported)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderUsageAccumulator {
    reported: TokenUsageSums,
    estimated: TokenUsageSums,
}

impl ProviderUsageAccumulator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            reported: TokenUsageSums::new(),
            estimated: TokenUsageSums::new(),
        }
    }

    pub fn record(&mut self, usage: TokenUsage) {
        if !usage.has_any_tokens() {
            return;
        }
        match usage.source {
            TokenUsageSource::Reported => self.reported.add(&usage),
            TokenUsageSource::Estimated => self.estimated.add(&usage),
        }
    }

    pub fn record_estimated_completion_tokens(&mut self, tokens: u32) {
        if tokens == 0 {
            return;
        }
        self.record(TokenUsage::estimated(None, Some(tokens), Some(tokens)));
    }

    pub fn record_estimated_completion_chars(&mut self, chars: usize) {
        let tokens = chars.div_ceil(4);
        if let Ok(tokens) = u32::try_from(tokens) {
            self.record_estimated_completion_tokens(tokens);
        } else {
            self.record_estimated_completion_tokens(u32::MAX);
        }
    }

    #[must_use]
    pub fn finish(&self) -> TokenUsage {
        if self.reported.has_any() {
            self.reported.to_usage(TokenUsageSource::Reported)
        } else if self.estimated.has_any() {
            self.estimated.to_usage(TokenUsageSource::Estimated)
        } else {
            TokenUsage::default()
        }
    }

    #[must_use]
    pub fn finish_or_estimate_completion_chars(&self, chars: usize) -> TokenUsage {
        if self.reported.has_any() || self.estimated.has_any() || chars == 0 {
            return self.finish();
        }
        let mut fallback = Self::new();
        fallback.record_estimated_completion_chars(chars);
        fallback.finish()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct TokenUsageSums {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    has_prompt_tokens: bool,
    has_completion_tokens: bool,
    has_total_tokens: bool,
    has_cache_creation_input_tokens: bool,
    has_cache_read_input_tokens: bool,
}

impl TokenUsageSums {
    const fn new() -> Self {
        Self {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            has_prompt_tokens: false,
            has_completion_tokens: false,
            has_total_tokens: false,
            has_cache_creation_input_tokens: false,
            has_cache_read_input_tokens: false,
        }
    }

    const fn has_any(&self) -> bool {
        self.has_prompt_tokens
            || self.has_completion_tokens
            || self.has_total_tokens
            || self.has_cache_creation_input_tokens
            || self.has_cache_read_input_tokens
    }

    fn add(&mut self, usage: &TokenUsage) {
        if let Some(tokens) = usage.prompt_tokens {
            self.prompt_tokens = self.prompt_tokens.saturating_add(u64::from(tokens));
            self.has_prompt_tokens = true;
        }
        if let Some(tokens) = usage.completion_tokens {
            self.completion_tokens = self.completion_tokens.saturating_add(u64::from(tokens));
            self.has_completion_tokens = true;
        }
        if let Some(tokens) = usage.total_tokens {
            self.total_tokens = self.total_tokens.saturating_add(u64::from(tokens));
            self.has_total_tokens = true;
        }
        if let Some(tokens) = usage.cache_creation_input_tokens {
            self.cache_creation_input_tokens = self.cache_creation_input_tokens.saturating_add(u64::from(tokens));
            self.has_cache_creation_input_tokens = true;
        }
        if let Some(tokens) = usage.cache_read_input_tokens {
            self.cache_read_input_tokens = self.cache_read_input_tokens.saturating_add(u64::from(tokens));
            self.has_cache_read_input_tokens = true;
        }
    }

    fn to_usage(self, source: TokenUsageSource) -> TokenUsage {
        let prompt_tokens = self.has_prompt_tokens.then(|| clamp_u64_to_u32(self.prompt_tokens));
        let completion_tokens = self
            .has_completion_tokens
            .then(|| clamp_u64_to_u32(self.completion_tokens));
        let total_tokens = if self.has_total_tokens {
            Some(clamp_u64_to_u32(self.total_tokens))
        } else if self.has_prompt_tokens || self.has_completion_tokens {
            Some(clamp_u64_to_u32(
                self.prompt_tokens.saturating_add(self.completion_tokens),
            ))
        } else {
            None
        };
        TokenUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_creation_input_tokens: self
                .has_cache_creation_input_tokens
                .then(|| clamp_u64_to_u32(self.cache_creation_input_tokens)),
            cache_read_input_tokens: self
                .has_cache_read_input_tokens
                .then(|| clamp_u64_to_u32(self.cache_read_input_tokens)),
            source,
        }
    }
}

fn clamp_u64_to_u32(value: u64) -> u32 {
    u32::try_from(value).map_or(u32::MAX, |value| value)
}

impl Default for FallbackPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 6,
            retry_within_provider: true,
            cross_provider_fallback: true,
            on_context_overflow: ContextOverflowAction::Compact,
        }
    }
}

impl RouteDecision {
    #[must_use]
    pub fn single_candidate(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self::single_candidate_for_context(
            provider,
            model,
            "owner:unknown",
            "session:unknown",
            None,
            None,
            "conversation",
            0,
            false,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn single_candidate_for_context(
        provider: impl Into<String>,
        model: impl Into<String>,
        owner_id: impl Into<String>,
        session_key: impl Into<String>,
        source_message_event_id: Option<String>,
        user_hint: Option<String>,
        intent: impl Into<String>,
        estimated_tokens: u32,
        require_tools: bool,
        require_streaming: bool,
    ) -> Self {
        let provider = provider.into();
        let model = model.into();
        let strategy = if user_hint.is_some() {
            SelectionStrategy::UserHint
        } else {
            SelectionStrategy::FallbackDefault
        };
        Self {
            decision_id: Uuid::new_v4().to_string(),
            created_at: Utc::now(),
            owner_id: owner_id.into(),
            task_id: None,
            session_key: session_key.into(),
            source_message_event_id,
            intent: intent.into(),
            estimated_tokens,
            user_hint,
            candidates: vec![RouteCandidate {
                provider: provider.clone(),
                model: model.clone(),
                score: 1.0,
                estimated_cost_usd: None,
                estimated_latency_ms: None,
                max_context_tokens: 0,
                capabilities: Vec::new(),
            }],
            filtered_out: Vec::new(),
            selected: RouteSelection {
                provider,
                model,
                score: 1.0,
                strategy,
            },
            fallback_policy: FallbackPolicy::default(),
            constraints: RouteConstraints {
                require_tools,
                require_streaming,
                require_vision: false,
                max_cost_usd: None,
                min_context_tokens: 0,
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn from_model_routes_for_context(
        provider: impl Into<String>,
        model: impl Into<String>,
        model_routes: &[ModelRouteConfig],
        owner_id: impl Into<String>,
        session_key: impl Into<String>,
        source_message_event_id: Option<String>,
        intent: impl Into<String>,
        estimated_tokens: u32,
        require_tools: bool,
        require_streaming: bool,
    ) -> Self {
        let provider = provider.into();
        let model = model.into();
        if model_routes.is_empty() {
            return Self::single_candidate_for_context(
                provider,
                model,
                owner_id,
                session_key,
                source_message_event_id,
                None,
                intent,
                estimated_tokens,
                require_tools,
                require_streaming,
            );
        }

        let user_hint = model.strip_prefix("hint:").map(str::to_string);
        let selected_route = user_hint
            .as_deref()
            .and_then(|hint| model_routes.iter().find(|route| route.hint == hint));
        let selected_provider = selected_route.map_or_else(|| provider.clone(), |route| route.provider.clone());
        let selected_model = selected_route.map_or_else(|| model.clone(), |route| route.model.clone());
        let selected_strategy = if selected_route.is_some() {
            SelectionStrategy::UserHint
        } else if user_hint.is_some() {
            SelectionStrategy::FallbackDefault
        } else {
            SelectionStrategy::Greedy
        };

        let mut candidates = Vec::with_capacity(model_routes.len() + 1);
        push_unique_candidate(
            &mut candidates,
            RouteCandidate {
                provider,
                model,
                score: if selected_route.is_none() { 1.0 } else { 0.25 },
                estimated_cost_usd: None,
                estimated_latency_ms: None,
                max_context_tokens: 0,
                capabilities: vec!["default".to_string()],
            },
        );

        let mut filtered_out = Vec::new();
        for route in model_routes {
            let is_selected = selected_route.is_some_and(|selected| selected.hint == route.hint);
            let score = if is_selected {
                1.0
            } else if user_hint.is_some() {
                filtered_out.push(RouteFilterReason {
                    provider: route.provider.clone(),
                    model: route.model.clone(),
                    reason: "hint_mismatch".to_string(),
                    detail: Some(format!("route hint '{}' did not match requested hint", route.hint)),
                });
                0.0
            } else {
                0.5
            };
            if score > 0.0 {
                push_unique_candidate(
                    &mut candidates,
                    RouteCandidate {
                        provider: route.provider.clone(),
                        model: route.model.clone(),
                        score,
                        estimated_cost_usd: None,
                        estimated_latency_ms: None,
                        max_context_tokens: 0,
                        capabilities: vec![format!("route_hint:{}", route.hint)],
                    },
                );
            }
        }

        let selected_score = candidates
            .iter()
            .find(|candidate| candidate.provider == selected_provider && candidate.model == selected_model)
            .map_or(1.0, |candidate| candidate.score);

        Self {
            decision_id: Uuid::new_v4().to_string(),
            created_at: Utc::now(),
            owner_id: owner_id.into(),
            task_id: None,
            session_key: session_key.into(),
            source_message_event_id,
            intent: intent.into(),
            estimated_tokens,
            user_hint,
            candidates,
            filtered_out,
            selected: RouteSelection {
                provider: selected_provider,
                model: selected_model,
                score: selected_score,
                strategy: selected_strategy,
            },
            fallback_policy: FallbackPolicy::default(),
            constraints: RouteConstraints {
                require_tools,
                require_streaming,
                require_vision: false,
                max_cost_usd: None,
                min_context_tokens: 0,
            },
        }
    }

    #[must_use]
    pub fn effective_model(&self) -> &str {
        &self.selected.model
    }
}

fn push_unique_candidate(candidates: &mut Vec<RouteCandidate>, candidate: RouteCandidate) {
    if candidates
        .iter()
        .any(|existing| existing.provider == candidate.provider && existing.model == candidate.model)
    {
        return;
    }
    candidates.push(candidate);
}

impl ProviderExecutionOutcome {
    #[must_use]
    pub fn success_for_decision(decision: &RouteDecision, started_at: DateTime<Utc>) -> Self {
        Self::success_for_decision_with_usage(decision, started_at, TokenUsage::default())
    }

    #[must_use]
    pub fn success_for_decision_with_usage(
        decision: &RouteDecision,
        started_at: DateTime<Utc>,
        tokens_used: TokenUsage,
    ) -> Self {
        let finished_at = Utc::now();
        Self {
            decision_id: decision.decision_id.clone(),
            started_at,
            finished_at,
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: decision.selected.provider.clone(),
                model: decision.selected.model.clone(),
                started_at,
                finished_at,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: decision.selected.provider.clone(),
            final_model: decision.selected.model.clone(),
            status: ExecutionStatus::Success,
            fallback_reason: None,
            tokens_used,
        }
    }

    /// Build an outcome from a real execution trace produced by the provider
    /// layer (`ReliableProvider::chat_traced`).
    ///
    /// This replaces the synthetic single-attempt outcome of
    /// `success_for_decision` with the *actual* attempt sequence, the *actual*
    /// provider/model that served the request, and a fallback classification:
    ///
    /// * `Success` — the very first attempt of the routed provider/model
    ///   succeeded (no retry, no provider/model fallback).
    /// * `FallbackSuccess` — the request succeeded only after a retry or after
    ///   falling back to a different provider/model than the routed one. The
    ///   `fallback_reason` carries a stable classification string
    ///   (`retry_recovered` / `provider_fallback` / `model_fallback` /
    ///   `provider_model_fallback` / `earlier_turn_fallback`).
    ///
    /// `attempts` is taken verbatim from the trace; callers MUST pass a
    /// non-empty slice ending in a `Success` attempt for a successful trace.
    ///
    /// FIX #2: `prior_turns_had_fallback` lets a multi-turn agent loop signal
    /// that an *earlier* turn (not reflected in this final turn's `attempts`)
    /// recovered through a fallback. When set, the outcome is `FallbackSuccess`
    /// even if the final turn itself was clean, so the recorded status reflects
    /// the whole user turn rather than only its final iteration. Single-turn
    /// callers pass `false`.
    #[must_use]
    pub fn from_trace(
        decision: &RouteDecision,
        attempts: Vec<ProviderAttempt>,
        final_provider: String,
        final_model: String,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        prior_turns_had_fallback: bool,
    ) -> Self {
        Self::from_trace_with_usage(
            decision,
            attempts,
            final_provider,
            final_model,
            started_at,
            finished_at,
            prior_turns_had_fallback,
            TokenUsage::default(),
        )
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn from_trace_with_usage(
        decision: &RouteDecision,
        attempts: Vec<ProviderAttempt>,
        final_provider: String,
        final_model: String,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        prior_turns_had_fallback: bool,
        tokens_used: TokenUsage,
    ) -> Self {
        // A fallback occurred when more than one attempt was made (a retry or a
        // provider/model switch each add an attempt), when the serving
        // provider/model differs from what the router selected, or when an
        // earlier turn in the same user turn fell back (FIX #2).
        let provider_differs = final_provider != decision.selected.provider;
        let model_differs = final_model != decision.selected.model;
        let this_turn_fallback = attempts.len() > 1 || provider_differs || model_differs;
        let had_fallback = this_turn_fallback || prior_turns_had_fallback;

        let (status, fallback_reason) = if had_fallback {
            // Prefer the most specific classification: a combined provider+model
            // switch, then provider-only, then model-only, then a same-target
            // retry recovered the call. #5: distinguish the combined case. When
            // only an earlier turn fell back (this turn was clean), attribute it
            // to that earlier turn.
            let reason = if provider_differs && model_differs {
                "provider_model_fallback"
            } else if provider_differs {
                "provider_fallback"
            } else if model_differs {
                "model_fallback"
            } else if this_turn_fallback {
                "retry_recovered"
            } else {
                "earlier_turn_fallback"
            };
            (ExecutionStatus::FallbackSuccess, Some(reason.to_string()))
        } else {
            (ExecutionStatus::Success, None)
        };

        Self {
            decision_id: decision.decision_id.clone(),
            started_at,
            finished_at,
            attempts,
            final_provider,
            final_model,
            status,
            fallback_reason,
            tokens_used,
        }
    }

    #[must_use]
    pub fn failed_for_decision(decision: &RouteDecision, started_at: DateTime<Utc>, error: &anyhow::Error) -> Self {
        let finished_at = Utc::now();
        let error_class = classify_provider_error(error);
        Self {
            decision_id: decision.decision_id.clone(),
            started_at,
            finished_at,
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: decision.selected.provider.clone(),
                model: decision.selected.model.clone(),
                started_at,
                finished_at,
                status: AttemptStatus::Failed,
                error_class: Some(error_class.clone()),
                error_message: Some(error.to_string().chars().take(500).collect()),
            }],
            final_provider: decision.selected.provider.clone(),
            final_model: decision.selected.model.clone(),
            status: ExecutionStatus::AllFailed {
                last_error_class: error_class,
            },
            fallback_reason: Some("provider_error".to_string()),
            tokens_used: TokenUsage::default(),
        }
    }
}

pub fn validate_user_route_hint(hint: &str) -> anyhow::Result<()> {
    let trimmed = hint.trim();
    if trimmed.is_empty() {
        anyhow::bail!("route hint cannot be empty");
    }
    if trimmed.len() > 128 {
        anyhow::bail!("route hint is too long");
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>' | '{' | '}'))
    {
        anyhow::bail!("route hint contains unsafe characters");
    }
    if trimmed.contains("..") || trimmed.contains('\\') {
        anyhow::bail!("route hint contains path traversal characters");
    }
    Ok(())
}

pub async fn record_route_decision_event(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    decision: &RouteDecision,
) -> anyhow::Result<MessageEvent> {
    let payload = serde_json::to_string(decision)?;
    let content = format!(
        "decision_id={} selected={}/{} candidates={}",
        decision.decision_id,
        decision.selected.provider,
        decision.selected.model,
        decision.candidates.len()
    );
    fabric
        .record_runtime_event(scope, "router.route_decision", content, Some(payload))
        .await
}

pub async fn record_provider_outcome_events(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    outcome: &ProviderExecutionOutcome,
) -> anyhow::Result<()> {
    for attempt in &outcome.attempts {
        let event_type = match attempt.status {
            AttemptStatus::Success => "provider.attempt_succeeded",
            AttemptStatus::Failed => "provider.attempt_failed",
            AttemptStatus::Skipped => "provider.attempt_skipped",
        };
        let payload = serde_json::to_string(attempt)?;
        let content = format!(
            "decision_id={} seq={} provider={} model={}",
            outcome.decision_id, attempt.seq, attempt.provider, attempt.model
        );
        fabric
            .record_runtime_event(scope.clone(), event_type, content, Some(payload))
            .await?;
    }
    let payload = serde_json::to_string(outcome)?;
    let content = format!(
        "decision_id={} final={}/{} attempts={}",
        outcome.decision_id,
        outcome.final_provider,
        outcome.final_model,
        outcome.attempts.len()
    );
    fabric
        .record_runtime_event(scope, "provider.final_outcome", content, Some(payload))
        .await?;
    Ok(())
}

pub fn route_event_scope(
    source: &str,
    owner_id: Option<String>,
    session_key: Option<String>,
    run_id: Option<String>,
    sender: Option<String>,
    recipient: Option<String>,
) -> MessageEventScope {
    let mut scope = MessageEventScope::new(source.to_string(), MemoryVisibility::Workspace);
    scope.owner_id = owner_id;
    scope.channel = Some("runtime".to_string());
    scope.session_key = session_key;
    scope.run_id = run_id;
    scope.agent_id = Some("llm-router".to_string());
    scope.sender = sender;
    scope.recipient = recipient;
    scope
}

pub fn classify_provider_error(error: &anyhow::Error) -> String {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("context") && (message.contains("window") || message.contains("token")) {
        "context_overflow".to_string()
    } else if message.contains("rate limit") || message.contains("429") {
        "rate_limit".to_string()
    } else if message.contains("timeout") || message.contains("timed out") {
        "timeout".to_string()
    } else if message.contains("unauthorized") || message.contains("401") || message.contains("403") {
        "auth".to_string()
    } else {
        "provider_error".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_injection_rejected() {
        for bad in [
            "hint:fast\nignore",
            "hint:fast override",
            "anthropic/{model}",
            "../model",
        ] {
            assert!(validate_user_route_hint(bad).is_err(), "{bad} should be rejected");
        }
        assert!(validate_user_route_hint("hint:fast").is_ok());
        assert!(validate_user_route_hint("anthropic/claude-sonnet-4").is_ok());
    }

    #[test]
    fn route_decision_roundtrips() {
        let decision = RouteDecision::single_candidate_for_context(
            "openrouter",
            "anthropic/claude-sonnet-4",
            "owner:workspace:terminal:local-user",
            "chat:test",
            Some("event-1".to_string()),
            None,
            "conversation",
            42,
            true,
            false,
        );
        let encoded = serde_json::to_string(&decision).unwrap();
        let decoded: RouteDecision = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.selected.provider, "openrouter");
        assert_eq!(decoded.source_message_event_id.as_deref(), Some("event-1"));
    }

    #[test]
    fn streaming_fallback_parity() {
        let decision = RouteDecision::single_candidate_for_context(
            "openrouter",
            "anthropic/claude-sonnet-4",
            "owner",
            "chat:test",
            None,
            None,
            "stream",
            128,
            false,
            true,
        );
        assert!(decision.constraints.require_streaming);
        assert_eq!(
            decision.fallback_policy.on_context_overflow,
            ContextOverflowAction::Compact
        );
    }

    #[test]
    fn route_decision_from_model_routes_records_candidates_and_hint_filters() {
        let routes = vec![
            ModelRouteConfig {
                hint: "fast".to_string(),
                provider: "openrouter".to_string(),
                model: "openai/gpt-4o-mini".to_string(),
                api_key: None,
            },
            ModelRouteConfig {
                hint: "reasoning".to_string(),
                provider: "anthropic".to_string(),
                model: "claude-sonnet-4".to_string(),
                api_key: None,
            },
        ];
        let decision = RouteDecision::from_model_routes_for_context(
            "openrouter",
            "hint:reasoning",
            &routes,
            "owner",
            "chat:test",
            None,
            "chat",
            64,
            true,
            false,
        );

        assert_eq!(decision.selected.provider, "anthropic");
        assert_eq!(decision.selected.model, "claude-sonnet-4");
        assert_eq!(decision.selected.strategy, SelectionStrategy::UserHint);
        assert_eq!(decision.user_hint.as_deref(), Some("reasoning"));
        assert!(decision.candidates.iter().any(|candidate| {
            candidate.provider == "anthropic" && candidate.model == "claude-sonnet-4" && candidate.score == 1.0
        }));
        assert!(
            decision
                .filtered_out
                .iter()
                .any(|filtered| { filtered.model == "openai/gpt-4o-mini" && filtered.reason == "hint_mismatch" })
        );
    }

    fn success_attempt(provider: &str, model: &str) -> ProviderAttempt {
        let now = Utc::now();
        ProviderAttempt {
            seq: 1,
            provider: provider.to_string(),
            model: model.to_string(),
            started_at: now,
            finished_at: now,
            status: AttemptStatus::Success,
            error_class: None,
            error_message: None,
        }
    }

    #[test]
    fn from_trace_clean_single_attempt_is_success() {
        let decision = RouteDecision::single_candidate("primary", "m1");
        let now = Utc::now();
        let outcome = ProviderExecutionOutcome::from_trace(
            &decision,
            vec![success_attempt("primary", "m1")],
            "primary".to_string(),
            "m1".to_string(),
            now,
            now,
            false,
        );
        assert_eq!(outcome.status, ExecutionStatus::Success);
        assert!(outcome.fallback_reason.is_none());
    }

    #[test]
    fn from_trace_prior_turn_fallback_marks_fallback_even_when_final_turn_clean() {
        // FIX #2: the final turn is clean (single attempt, same target), but an
        // earlier turn fell back → the whole user turn is FallbackSuccess.
        let decision = RouteDecision::single_candidate("primary", "m1");
        let now = Utc::now();
        let outcome = ProviderExecutionOutcome::from_trace(
            &decision,
            vec![success_attempt("primary", "m1")],
            "primary".to_string(),
            "m1".to_string(),
            now,
            now,
            true, // an earlier turn fell back
        );
        assert_eq!(outcome.status, ExecutionStatus::FallbackSuccess);
        assert_eq!(outcome.fallback_reason.as_deref(), Some("earlier_turn_fallback"));
    }

    #[test]
    fn from_trace_provider_and_model_both_differ_is_provider_model_fallback() {
        // #5: when both provider AND model differ, classify distinctly.
        let decision = RouteDecision::single_candidate("primary", "m1");
        let now = Utc::now();
        let outcome = ProviderExecutionOutcome::from_trace(
            &decision,
            vec![
                ProviderAttempt {
                    seq: 1,
                    provider: "primary".to_string(),
                    model: "m1".to_string(),
                    started_at: now,
                    finished_at: now,
                    status: AttemptStatus::Failed,
                    error_class: Some("server_error".to_string()),
                    error_message: Some("500".to_string()),
                },
                success_attempt("secondary", "m2"),
            ],
            "secondary".to_string(),
            "m2".to_string(),
            now,
            now,
            false,
        );
        assert_eq!(outcome.status, ExecutionStatus::FallbackSuccess);
        assert_eq!(outcome.fallback_reason.as_deref(), Some("provider_model_fallback"));
    }

    #[test]
    fn from_trace_retry_same_target_is_retry_recovered() {
        let decision = RouteDecision::single_candidate("primary", "m1");
        let now = Utc::now();
        let outcome = ProviderExecutionOutcome::from_trace(
            &decision,
            vec![
                ProviderAttempt {
                    seq: 1,
                    provider: "primary".to_string(),
                    model: "m1".to_string(),
                    started_at: now,
                    finished_at: now,
                    status: AttemptStatus::Failed,
                    error_class: Some("rate_limited".to_string()),
                    error_message: Some("429".to_string()),
                },
                success_attempt("primary", "m1"),
            ],
            "primary".to_string(),
            "m1".to_string(),
            now,
            now,
            false,
        );
        assert_eq!(outcome.status, ExecutionStatus::FallbackSuccess);
        assert_eq!(outcome.fallback_reason.as_deref(), Some("retry_recovered"));
    }

    #[test]
    fn from_trace_single_attempt_deviated_provider_is_fallback() {
        // FIX #2 (alignment): a single successful attempt that landed on a
        // provider different from the routed selection is still a fallback. The
        // agent loop's per-turn sticky flag mirrors exactly this predicate so an
        // intermediate tool-call turn with attempts==1 but a deviated
        // provider/model is not silently recorded as a clean Success.
        let decision = RouteDecision::single_candidate("primary", "m1");
        let now = Utc::now();
        let outcome = ProviderExecutionOutcome::from_trace(
            &decision,
            vec![success_attempt("secondary", "m1")],
            "secondary".to_string(),
            "m1".to_string(),
            now,
            now,
            false,
        );
        assert_eq!(outcome.status, ExecutionStatus::FallbackSuccess);
        assert_eq!(outcome.fallback_reason.as_deref(), Some("provider_fallback"));
    }

    #[test]
    fn from_trace_single_attempt_deviated_model_is_fallback() {
        // Companion to the provider case: a single attempt that served a model
        // different from the routed selection is a model-level fallback.
        let decision = RouteDecision::single_candidate("primary", "m1");
        let now = Utc::now();
        let outcome = ProviderExecutionOutcome::from_trace(
            &decision,
            vec![success_attempt("primary", "m2")],
            "primary".to_string(),
            "m2".to_string(),
            now,
            now,
            false,
        );
        assert_eq!(outcome.status, ExecutionStatus::FallbackSuccess);
        assert_eq!(outcome.fallback_reason.as_deref(), Some("model_fallback"));
    }

    #[test]
    fn usage_accumulator_reported_wins_over_estimate() {
        let mut accumulator = ProviderUsageAccumulator::new();
        accumulator.record(TokenUsage::estimated(None, Some(40), Some(40)));
        accumulator.record(TokenUsage::reported(Some(10), Some(5), Some(15)));

        let usage = accumulator.finish();

        assert_eq!(usage.source, TokenUsageSource::Reported);
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(5));
        assert_eq!(usage.total_tokens, Some(15));
    }

    #[test]
    fn token_usage_source_marks_reported_and_estimated_values() {
        let reported = TokenUsage::reported(Some(4), Some(2), Some(6));
        let estimated = TokenUsage::estimated(None, Some(3), Some(3));
        let empty = TokenUsage::default();

        assert!(reported.is_reported());
        assert_eq!(reported.source, TokenUsageSource::Reported);
        assert!(!estimated.is_reported());
        assert_eq!(estimated.source, TokenUsageSource::Estimated);
        assert!(!empty.has_any_tokens());
        assert_eq!(empty.source, TokenUsageSource::Estimated);
    }

    #[test]
    fn token_usage_source_survives_json_round_trip() {
        let usage = TokenUsage::reported(Some(10), Some(5), Some(15));

        let encoded = serde_json::to_string(&usage).expect("test: serialize token usage");
        let decoded: TokenUsage = serde_json::from_str(&encoded).expect("test: deserialize token usage");

        assert_eq!(decoded, usage);
        assert_eq!(decoded.source, TokenUsageSource::Reported);
    }

    #[test]
    fn token_usage_deserializes_legacy_json_without_cache_fields() {
        let decoded: TokenUsage = serde_json::from_str(
            r#"{
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "source": "reported"
            }"#,
        )
        .expect("legacy token usage should deserialize");

        assert_eq!(decoded.prompt_tokens, Some(10));
        assert_eq!(decoded.completion_tokens, Some(5));
        assert_eq!(decoded.total_tokens, Some(15));
        assert_eq!(decoded.cache_creation_input_tokens, None);
        assert_eq!(decoded.cache_read_input_tokens, None);
        assert_eq!(decoded.source, TokenUsageSource::Reported);
    }

    #[test]
    fn metered_usage_record_deserializes_legacy_json_without_cache_fields() {
        let decoded: MeteredTokenUsageRecord = serde_json::from_str(
            r#"{
                "provider": "anthropic",
                "model": "claude-sonnet",
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120,
                "source": "reported",
                "cost_usd": 0.001
            }"#,
        )
        .expect("legacy metered usage record should deserialize");

        assert_eq!(decoded.provider, "anthropic");
        assert_eq!(decoded.model, "claude-sonnet");
        assert_eq!(decoded.prompt_tokens, 100);
        assert_eq!(decoded.completion_tokens, 20);
        assert_eq!(decoded.total_tokens, 120);
        assert_eq!(decoded.cache_creation_input_tokens, 0);
        assert_eq!(decoded.cache_read_input_tokens, 0);
        assert_eq!(decoded.source, TokenUsageSource::Reported);
        assert_eq!(decoded.cost_usd, Some(0.001));
    }

    #[test]
    fn metered_usage_cost_prices_anthropic_cache_tokens_separately() {
        let usage = TokenUsage::reported_with_cache(Some(1_500), Some(200), Some(1_700), Some(300), Some(700));
        let record = MeteredTokenUsageRecord::from_parts(
            "anthropic",
            "claude-sonnet-4-20250514",
            &usage,
            &crate::config::schema::CostConfig::default(),
        )
        .expect("cache usage should produce a record");

        assert_eq!(record.prompt_tokens, 1_500);
        assert_eq!(record.cache_creation_input_tokens, 300);
        assert_eq!(record.cache_read_input_tokens, 700);
        let cost = record.cost_usd.expect("default anthropic price should compute cost");
        let expected = 5_835.0 / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 0.000_000_1,
            "cache read/write cost should be priced separately: {cost}"
        );
    }

    #[test]
    fn usage_accumulator_missing_usage_falls_back_to_estimate() {
        let accumulator = ProviderUsageAccumulator::new();

        let usage = accumulator.finish_or_estimate_completion_chars("hello world".chars().count());

        assert_eq!(usage.source, TokenUsageSource::Estimated);
        assert_eq!(usage.prompt_tokens, None);
        assert_eq!(usage.completion_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(3));
    }

    #[test]
    fn usage_accumulator_sums_multiple_reported_calls() {
        let mut accumulator = ProviderUsageAccumulator::new();
        accumulator.record(TokenUsage::reported(Some(100), Some(20), Some(120)));
        accumulator.record(TokenUsage::reported(Some(80), Some(30), Some(110)));

        let usage = accumulator.finish();

        assert_eq!(usage.source, TokenUsageSource::Reported);
        assert_eq!(usage.prompt_tokens, Some(180));
        assert_eq!(usage.completion_tokens, Some(50));
        assert_eq!(usage.total_tokens, Some(230));
    }

    #[test]
    fn usage_accumulator_empty_failed_turn_does_not_fabricate_reported_usage() {
        let accumulator = ProviderUsageAccumulator::new();

        let usage = accumulator.finish();

        assert_eq!(usage.source, TokenUsageSource::Estimated);
        assert_eq!(usage.prompt_tokens, None);
        assert_eq!(usage.completion_tokens, None);
        assert_eq!(usage.total_tokens, None);
        assert!(!usage.is_reported());
        assert!(!usage.has_any_tokens());
    }

    #[test]
    fn usage_accumulator_cancelled_stream_without_usage_does_not_fabricate_reported_usage() {
        let mut accumulator = ProviderUsageAccumulator::new();
        accumulator.record(TokenUsage::default());

        let usage = accumulator.finish();

        assert_eq!(usage.source, TokenUsageSource::Estimated);
        assert_eq!(usage.prompt_tokens, None);
        assert_eq!(usage.completion_tokens, None);
        assert_eq!(usage.total_tokens, None);
        assert!(
            !usage.is_reported(),
            "cancelled streams without a provider usage chunk must not become real usage"
        );
    }
}
