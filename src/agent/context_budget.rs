use crate::agent::history::{drop_orphan_tool_results_at, history_non_system_start};
use crate::providers::ChatMessage;

/// Fraction of `max_context_tokens` above which a pre-turn memory flush is triggered.
pub(crate) const PRE_TURN_FLUSH_THRESHOLD: f64 = 0.85;

/// Safety multiplier used when the cl100k proxy is unavailable and the
/// fallback char heuristic is the only available measurement.
pub(super) const TOKEN_HEURISTIC_BUDGET_MULTIPLIER: usize = 2;

/// Token-aware mid-turn trim: remove the oldest non-system messages one at a time
/// until the estimated token count drops below max_tokens.
pub(super) fn trim_history_token_aware(history: &mut Vec<ChatMessage>, max_tokens: usize) {
    let start_of_non_system = history_non_system_start(history);
    loop {
        if estimate_history_tokens(history) <= max_tokens {
            break;
        }
        let has_system = history.first().is_some_and(|m| m.role == "system");
        let start = if has_system { 1 } else { 0 };
        if history.len() <= start + 1 {
            break; // nothing left to remove
        }
        history.remove(start);
        drop_orphan_tool_results_at(history, start);
    }
    drop_orphan_tool_results_at(history, start_of_non_system);
}

pub(super) fn estimate_history_tokens(history: &[ChatMessage]) -> usize {
    estimate_history_tokens_with_source(history).tokens
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenMeasurementSource {
    /// OpenAI cl100k is a stable local proxy, not the active model's tokenizer.
    Cl100kProxy,
    Heuristic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenMeasurement {
    pub tokens: usize,
    pub source: TokenMeasurementSource,
}

impl TokenMeasurement {
    /// A measurement of nothing. Attributed to the proxy tokenizer so an absent
    /// request component never drags the whole estimate onto the heuristic
    /// budget multiplier.
    pub(crate) const NONE: Self = Self {
        tokens: 0,
        source: TokenMeasurementSource::Cl100kProxy,
    };
}

/// Tokens a measurement is allowed to occupy in the budget.
///
/// The character heuristic under-counts real tokenizers, so a heuristic
/// measurement is scaled before it is compared against a context window.
pub(super) const fn budgeted_tokens(measurement: TokenMeasurement) -> usize {
    match measurement.source {
        TokenMeasurementSource::Cl100kProxy => measurement.tokens,
        TokenMeasurementSource::Heuristic => measurement.tokens.saturating_mul(TOKEN_HEURISTIC_BUDGET_MULTIPLIER),
    }
}

/// Total estimated input tokens for one provider request: the budgeted message
/// estimate plus the budgeted native tool payload.
///
/// The context-budget preflight and the compiled-request event must report the
/// same number for the same request, so both derive it here instead of each
/// folding its own total.
pub(crate) const fn estimate_request_tokens(
    messages: TokenMeasurement,
    native_tool_payload: TokenMeasurement,
) -> usize {
    budgeted_tokens(messages).saturating_add(budgeted_tokens(native_tool_payload))
}

/// Tokens `CompiledToolContext::apply_to_messages` adds beyond the sections it
/// was measured on, and which a standalone measurement of those sections cannot
/// see. Exactly one of the two branches applies to any one request: the blank
/// line that joins the sections onto an existing system message (one token,
/// two when the character heuristic is the estimator in play), or, when the
/// request carries no system message, the role and per-message framing of the
/// system message it inserts to hold them (four). Reserving the larger covers
/// both with one number.
pub(super) const APPENDED_SECTION_FRAMING_TOKENS: usize = 4;

/// Tokens the published tool surface occupies in the context window: the
/// native payload that rides beside the messages plus the prompt sections the
/// compiled snapshot appends to the system message, with the framing those
/// sections arrive in.
pub(super) const fn budgeted_request_tool_tokens(
    tool_surface: crate::agent::turn_context::RequestToolMeasurement,
) -> usize {
    let sections = budgeted_tokens(tool_surface.instructions);
    let payload = budgeted_tokens(tool_surface.native_payload.measurement);
    if sections == 0 {
        // No sections are appended, so no join and no inserted message: a
        // request that publishes nothing is budgeted exactly as before.
        return payload;
    }
    payload
        .saturating_add(sections)
        .saturating_add(APPENDED_SECTION_FRAMING_TOKENS)
}

/// Total estimated input tokens the preflight plans one request against: the
/// budgeted message estimate plus everything the published tool surface adds.
///
/// Distinct from [`estimate_request_tokens`], which totals a request that has
/// already been prepared: there the appended prompt sections are inside the
/// message estimate and must not be added a second time.
pub(super) const fn estimate_budgeted_request_tokens(
    messages: TokenMeasurement,
    tool_surface: crate::agent::turn_context::RequestToolMeasurement,
) -> usize {
    budgeted_tokens(messages).saturating_add(budgeted_request_tool_tokens(tool_surface))
}

/// How one iteration's published tool surface breaks down, for the
/// compiled-request event (SPLASH-F4 R2).
///
/// One registry root may publish many specs (an MCP router publishes its root
/// plus one alias per discovered tool, a skill runner one per skill tool), so
/// neither the registry size nor the routed selection is the number of tools on
/// the wire. Taken where the budget measures the surface, from the same
/// catalog the published specs were projected from; `tool_snapshot.count`
/// stays the final wire count, after any late addition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RequestToolSurfaceCounts {
    registry_roots: usize,
    routed_roots: usize,
    published_specs: usize,
    native: usize,
    mcp_alias: usize,
    skill: usize,
    wasm: usize,
}

impl RequestToolSurfaceCounts {
    pub(super) fn measure(
        registry_roots: usize,
        routed_roots: usize,
        catalog: &crate::tools::ToolCatalog,
        published: &[crate::tools::ToolSpec],
    ) -> Self {
        let mut counts = Self {
            registry_roots,
            routed_roots,
            published_specs: published.len(),
            ..Self::default()
        };
        for spec in published {
            // A published spec always comes from this catalog; a name it does
            // not know would be a late addition, which is native by origin.
            let adapter = catalog
                .descriptor(&spec.name)
                .map_or(crate::tools::ToolAdapterKind::Native, |descriptor| descriptor.adapter);
            let slot = match adapter {
                crate::tools::ToolAdapterKind::Native => &mut counts.native,
                crate::tools::ToolAdapterKind::McpAlias => &mut counts.mcp_alias,
                crate::tools::ToolAdapterKind::Skill => &mut counts.skill,
                crate::tools::ToolAdapterKind::WasmPlugin => &mut counts.wasm,
            };
            *slot = slot.saturating_add(1);
        }
        counts
    }

    pub(super) fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "registry_roots": self.registry_roots,
            "routed_roots": self.routed_roots,
            "published_specs": self.published_specs,
            "by_adapter": {
                "native": self.native,
                "mcp_alias": self.mcp_alias,
                "skill": self.skill,
                "wasm": self.wasm,
            },
        })
    }
}

/// Stable label for the estimator a request measurement came from.
pub(super) const fn token_source_label(source: TokenMeasurementSource) -> &'static str {
    match source {
        TokenMeasurementSource::Cl100kProxy => "cl100k_proxy",
        TokenMeasurementSource::Heuristic => "heuristic",
    }
}

/// Reserve the published tool surface out of the usable input window.
///
/// The budget compares history against `max_context_tokens - reserve_tokens`.
/// Tool schemas and the prompt sections compiled from them occupy that same
/// window before the first message does, so they are reserved rather than
/// budgeted through a second, divergent limit: every existing consumer of the
/// config (summary compaction, the token-aware trim, the warning threshold,
/// the mid-turn trim) then aims at the window the messages really have.
pub(crate) fn reserve_request_tool_surface(
    config: &crate::config::AgentCompactionConfig,
    tool_surface: crate::agent::turn_context::RequestToolMeasurement,
) -> crate::config::AgentCompactionConfig {
    crate::config::AgentCompactionConfig {
        reserve_tokens: config
            .reserve_tokens
            .saturating_add(budgeted_request_tool_tokens(tool_surface)),
        ..config.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextBudgetSnapshot {
    pub used_tokens: usize,
    pub measured_tokens: usize,
    pub max_context_tokens: usize,
    pub reserve_tokens: usize,
    pub available_input_tokens: usize,
    pub warning_threshold_tokens: usize,
    pub over_warning: bool,
    pub over_hard_limit: bool,
    pub measurement_source: TokenMeasurementSource,
}

impl ContextBudgetSnapshot {
    /// The message measurement this snapshot was planned from, so a caller can
    /// fold it into a request total without tokenizing the history again.
    pub(crate) const fn measurement(&self) -> TokenMeasurement {
        TokenMeasurement {
            tokens: self.measured_tokens,
            source: self.measurement_source,
        }
    }
}

pub(super) fn estimate_history_tokens_with_source(history: &[ChatMessage]) -> TokenMeasurement {
    // Per-message framing overhead (role marker, separators) approximated as a
    // small constant added on top of the tokenized role and content.
    const PER_MESSAGE_FRAMING_TOKENS: usize = 3;
    let mut source = TokenMeasurementSource::Cl100kProxy;
    let tokens = history.iter().fold(0usize, |total, msg| {
        let role = count_tokens_with_source(&msg.role);
        let content = count_tokens_with_source(&msg.content);
        if matches!(role.source, TokenMeasurementSource::Heuristic)
            || matches!(content.source, TokenMeasurementSource::Heuristic)
        {
            source = TokenMeasurementSource::Heuristic;
        }
        total
            .saturating_add(role.tokens)
            .saturating_add(content.tokens)
            .saturating_add(PER_MESSAGE_FRAMING_TOKENS)
    });
    TokenMeasurement { tokens, source }
}

pub(crate) fn measure_history_tokens(history: &[ChatMessage]) -> usize {
    estimate_history_tokens(history)
}

pub(crate) fn plan_context_budget(
    history: &[ChatMessage],
    config: &crate::config::AgentCompactionConfig,
    warning_ratio: f64,
) -> ContextBudgetSnapshot {
    let measured = estimate_history_tokens_with_source(history);
    let used_tokens = budgeted_tokens(measured);
    let available_input_tokens = config.max_context_tokens.saturating_sub(config.reserve_tokens);
    let warning_ratio = warning_ratio.clamp(0.0, 1.0);
    // Apply warning ratios to the usable input window. Reserve tokens belong to
    // the response and must not make a warning threshold exceed the hard input
    // limit or trigger unexpectedly early on a smaller configured window.
    let warning_threshold_tokens = ((available_input_tokens as f64) * warning_ratio) as usize;
    ContextBudgetSnapshot {
        used_tokens,
        measured_tokens: measured.tokens,
        max_context_tokens: config.max_context_tokens,
        reserve_tokens: config.reserve_tokens,
        available_input_tokens,
        warning_threshold_tokens,
        over_warning: used_tokens > warning_threshold_tokens,
        over_hard_limit: used_tokens > available_input_tokens,
        measurement_source: measured.source,
    }
}

pub(crate) fn trim_history_to_context_budget(
    history: &mut Vec<ChatMessage>,
    config: &crate::config::AgentCompactionConfig,
) -> bool {
    let before_len = history.len();
    let before_tokens = measure_history_tokens(history);
    let limit = config.max_context_tokens.saturating_sub(config.reserve_tokens);
    trim_history_token_aware(history, limit);
    history.len() != before_len || measure_history_tokens(history) != before_tokens
}

pub fn trim_history_to_context_budget_preserving_compaction_replacement(
    history: &mut Vec<ChatMessage>,
    config: &crate::config::AgentCompactionConfig,
    replacement_len: usize,
) -> bool {
    let before_len = history.len();
    let before_tokens = measure_history_tokens(history);
    let limit = config.max_context_tokens.saturating_sub(config.reserve_tokens);
    let has_system = history.first().is_some_and(|m| m.role == "system");
    let start: usize = if has_system { 1 } else { 0 };
    let preserve_until = start.saturating_add(replacement_len).min(history.len());
    loop {
        if estimate_history_tokens(history) <= limit {
            break;
        }
        let remove_index = if history.len() > preserve_until {
            preserve_until
        } else if replacement_len == 0 && history.len() > start.saturating_add(1) {
            start
        } else {
            break;
        };
        history.remove(remove_index);
        drop_orphan_tool_results_at(history, remove_index);
    }
    history.len() != before_len || measure_history_tokens(history) != before_tokens
}

pub fn trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
    history: &mut Vec<ChatMessage>,
    config: &crate::config::AgentCompactionConfig,
    replacement_len: usize,
) -> bool {
    let before_len = history.len();
    let before_tokens = measure_history_tokens(history);
    trim_history_to_context_budget_preserving_compaction_replacement(history, config, replacement_len);
    let after_preserve = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
    if after_preserve.over_hard_limit {
        trim_history_to_context_budget(history, config);
        while plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit {
            let start = history_non_system_start(history);
            if history.len() <= start {
                break;
            }
            history.remove(start);
            drop_orphan_tool_results_at(history, start);
        }
    }
    history.len() != before_len || measure_history_tokens(history) != before_tokens
}

/// Count the number of tokens in `text`.
///
/// When the `llm-router` feature is enabled this uses the `cl100k_base`
/// encoding (the tokenizer used by GPT-3.5/4 and a close proxy for most modern
/// chat models), giving an accurate count. The encoder is expensive to build,
/// so it is constructed once and cached in a [`std::sync::LazyLock`].
///
/// This compatibility helper is used by tests. Production callers measure
/// through `count_tokens_with_source`, which without `llm-router` falls
/// back to the `chars / 4` heuristic (rounded up) and reports that source.
#[cfg(test)]
pub fn count_tokens(text: &str) -> usize {
    count_tokens_with_source(text).tokens
}

#[cfg(test)]
pub(crate) mod token_timing;

pub(crate) fn count_tokens_with_source(text: &str) -> TokenMeasurement {
    #[cfg(test)]
    let _whole_timing = token_timing::Span::start(token_timing::Kind::Whole);
    #[cfg(feature = "llm-router")]
    {
        use std::sync::LazyLock;
        use tiktoken_rs::CoreBPE;

        static ENCODER: LazyLock<Option<CoreBPE>> = LazyLock::new(|| {
            #[cfg(test)]
            let _init_timing = token_timing::Span::start(token_timing::Kind::Init);
            match tiktoken_rs::cl100k_base() {
                Ok(bpe) => Some(bpe),
                Err(err) => {
                    tracing::warn!("tiktoken cl100k_base init failed, falling back to char heuristic: {err}");
                    None
                }
            }
        });

        #[cfg(test)]
        let lookup_timing = token_timing::Span::start(token_timing::Kind::Lookup);
        let encoder = ENCODER.as_ref();
        #[cfg(test)]
        drop(lookup_timing);
        if let Some(bpe) = encoder {
            return TokenMeasurement {
                tokens: {
                    #[cfg(test)]
                    let _encode_timing = token_timing::Span::start(token_timing::Kind::Encode);
                    bpe.encode_with_special_tokens(text).len()
                },
                source: TokenMeasurementSource::Cl100kProxy,
            };
        }
    }

    // Heuristic fallback: ~4 characters per token, rounding up so non-empty
    // text never reports zero tokens.
    TokenMeasurement {
        tokens: text.chars().count().div_ceil(4),
        source: TokenMeasurementSource::Heuristic,
    }
}

#[path = "turn_tests/context_budget.rs"]
mod tests;
