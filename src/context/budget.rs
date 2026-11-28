//! The context budget: what a request weighs, what window it must fit, and the
//! numbers the reduction ladder compares against.

pub(crate) mod anchor;
pub(crate) mod record;
pub(crate) mod session_reserves;

use crate::agent::history::{ProtectedHandoff, drop_orphan_tool_results_at, history_non_system_start};
use crate::providers::ChatMessage;
use crate::tokens::count::{TokenMeasurement, TokenMeasurementSource, count_tokens_with_source};

/// Fraction of `max_context_tokens` above which a pre-turn memory flush is triggered.
pub(crate) const PRE_TURN_FLUSH_THRESHOLD: f64 = 0.85;

/// Safety multiplier used when the cl100k proxy is unavailable and the
/// fallback char heuristic is the only available measurement.
pub(crate) const TOKEN_HEURISTIC_BUDGET_MULTIPLIER: usize = 2;

/// Token-aware mid-turn trim: remove the oldest non-system messages one at a time
/// until the estimated token count drops below max_tokens. The newest validated
/// context handoff is skipped while another message older than the newest one
/// remains ([`ProtectedHandoff`]).
pub(crate) fn trim_history_token_aware(history: &mut Vec<ChatMessage>, max_tokens: usize) {
    let start_of_non_system = history_non_system_start(history);
    let mut handoff = ProtectedHandoff::of(history);
    loop {
        if planning_history_tokens(history) <= max_tokens {
            break;
        }
        let has_system = history.first().is_some_and(|m| m.role == "system");
        let start = if has_system { 1 } else { 0 };
        if history.len() <= start + 1 {
            break; // nothing left to remove
        }
        let index = handoff.removal_index(start, history.len().saturating_sub(1));
        handoff.remove_at(history, index);
    }
    drop_orphan_tool_results_at(history, start_of_non_system);
}

/// The local tokenizer's count of the tokens `history` occupies, with no
/// calibration. This is the measurement every report, hook payload and slice
/// measurement uses; only the planning comparisons use
/// [`planning_history_tokens`].
pub(crate) fn estimate_history_tokens(history: &[ChatMessage]) -> usize {
    estimate_history_tokens_with_source(history).tokens
}

/// The estimate a reduction step compares with its limit: the local count of
/// the whole request history, planned under the usage calibration when the
/// turn engine has one in force ([`anchor::current`], see
/// [`anchor::Planning::history_tokens`]), and the local count otherwise. It
/// exists apart from [`estimate_history_tokens`] so a measurement of a slice
/// or a number reported to a hook cannot pick the calibration up by accident.
pub(crate) fn planning_history_tokens(history: &[ChatMessage]) -> usize {
    anchor::current().history_tokens(estimate_history_tokens(history))
}

/// Tokens a measurement is allowed to occupy in the budget.
///
/// The character heuristic under-counts real tokenizers, so a heuristic
/// measurement is scaled before it is compared against a context window.
pub(crate) const fn budgeted_tokens(measurement: TokenMeasurement) -> usize {
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
pub(crate) const APPENDED_SECTION_FRAMING_TOKENS: usize = 4;

/// Tokens the published tool surface occupies in the context window: the
/// native payload that rides beside the messages plus the prompt sections the
/// compiled snapshot appends to the system message, with the framing those
/// sections arrive in.
pub(crate) const fn budgeted_request_tool_tokens(
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

/// The tokens the published tool surface occupies, without a safety factor:
/// [`budgeted_request_tool_tokens`] as the local tokenizer counts it.
pub(crate) const fn request_tool_tokens(tool_surface: crate::agent::turn_context::RequestToolMeasurement) -> usize {
    let sections = tool_surface.instructions.tokens;
    let payload = tool_surface.native_payload.measurement.tokens;
    if sections == 0 {
        return payload;
    }
    payload
        .saturating_add(sections)
        .saturating_add(APPENDED_SECTION_FRAMING_TOKENS)
}

/// A request estimate set against the model window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowCheck {
    pub window_tokens: usize,
    /// The window less the reply reserve.
    pub input_window_tokens: usize,
    pub estimate_tokens: usize,
    /// Whether the estimate exceeds the input window.
    pub over: bool,
}

/// Set one request's estimate (anchored when an anchor is in force) against
/// the model window. This is a measurement of the request after the
/// reduction steps ran, made in one place for every round; it refuses
/// nothing: a request that is still over the window goes out and the
/// provider-overflow path recovers it.
///
/// It agrees with the reduction steps: they plan the history as
/// `apply(H + T) - T_reserved` against `window - reply - T_reserved`
/// ([`anchor::Planning`]), so a request they accept has an anchored estimate
/// `apply(H + T)` within `window - reply`, the input window here, as long as
/// the window/2 cap on reply plus tools does not bind.
pub(crate) fn check_request_window(
    config: &crate::config::AgentCompactionConfig,
    estimate_tokens: usize,
) -> WindowCheck {
    let reply_reserve =
        crate::agent::budget_inputs::capped_reply_reserve(config.max_context_tokens, config.reserve_tokens);
    let input_window_tokens = config.max_context_tokens.saturating_sub(reply_reserve);
    WindowCheck {
        window_tokens: config.max_context_tokens,
        input_window_tokens,
        estimate_tokens,
        over: estimate_tokens > input_window_tokens,
    }
}

/// Total estimated input tokens the preflight plans one request against: the
/// budgeted message estimate plus everything the published tool surface adds.
///
/// Distinct from [`estimate_request_tokens`], which totals a request that has
/// already been prepared: there the appended prompt sections are inside the
/// message estimate and must not be added a second time.
pub(crate) const fn estimate_budgeted_request_tokens(
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
pub(crate) struct RequestToolSurfaceCounts {
    registry_roots: usize,
    routed_roots: usize,
    published_specs: usize,
    native: usize,
    mcp_alias: usize,
    skill: usize,
    wasm: usize,
}

impl RequestToolSurfaceCounts {
    pub(crate) fn measure(
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

    /// Registered tools this turn did not route: callable through
    /// `capability_search`, not published.
    pub(crate) const fn deferred_roots(&self) -> usize {
        self.registry_roots.saturating_sub(self.routed_roots)
    }

    pub(crate) fn to_json(self) -> serde_json::Value {
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
pub(crate) const fn token_source_label(source: TokenMeasurementSource) -> &'static str {
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
    crate::agent::budget_inputs::reserved_budget_config(config, budgeted_request_tool_tokens(tool_surface))
}

/// The tool-surface reserve of a chat session's most recent request, shared
/// between the turn engine and the chat front ends.
///
/// The preflight reserves the published tool surface before it plans a
/// request. A chat front end decides `/compact`, prints compaction feedback
/// and draws the context footer outside any turn, where that surface is not
/// measured; it reads the last measured reserve from here so all of them use
/// the window the preflight uses. Zero until the first request is measured.
///
/// It also carries the session's usage anchors, which outlive a turn the same
/// way: the provider's count of the last request anchors the estimate of the
/// next one, in the next turn too.
#[derive(Clone, Debug, Default)]
pub(crate) struct ToolSurfaceReserve {
    tokens: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    anchors: anchor::UsageAnchors,
}

impl ToolSurfaceReserve {
    /// Store the reserve the preflight applies for `tool_surface`.
    pub(crate) fn record(&self, tool_surface: crate::agent::turn_context::RequestToolMeasurement) {
        self.tokens.store(
            budgeted_request_tool_tokens(tool_surface),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Set the reserve from the size known at turn opening, only while no
    /// request has been measured yet; a measurement always replaces it.
    pub(crate) fn seed_unmeasured(&self, tokens: usize) {
        let _ = self.tokens.compare_exchange(
            0,
            tokens,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// The usage anchors of the session this reserve belongs to.
    pub(crate) fn usage_anchors(&self) -> anchor::UsageAnchors {
        self.anchors.clone()
    }

    /// The tokens the last measured tool surface occupies in the window.
    pub(crate) fn tokens(&self) -> usize {
        self.tokens.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// `config` with the last measured tool surface reserved, the budget the
    /// preflight plans against.
    pub(crate) fn reserved_config(
        &self,
        config: &crate::config::AgentCompactionConfig,
    ) -> crate::config::AgentCompactionConfig {
        crate::agent::budget_inputs::reserved_budget_config(config, self.tokens())
    }

    /// Context tokens one request occupies: the history estimate plus the
    /// reserved tool surface, the figure the context footer shows.
    pub(crate) fn request_used_tokens(&self, history_used_tokens: usize) -> usize {
        history_used_tokens.saturating_add(self.tokens())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextBudgetSnapshot {
    /// What the reduction steps compare with their limits: planned under the
    /// usage calibration when one is in force ([`anchor::Planning`]), the
    /// budgeted local count otherwise. Never a local number to report; use
    /// [`Self::measurement`] with [`budgeted_tokens`] for that.
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

    /// The budgeted local message total and the budgeted local request total
    /// (messages plus the published tool surface) this snapshot was planned
    /// from: the heuristic-doubled local counts, never calibrated.
    pub(crate) const fn local_request_totals(
        &self,
        tool_surface: crate::agent::turn_context::RequestToolMeasurement,
    ) -> (usize, usize) {
        (
            budgeted_tokens(self.measurement()),
            estimate_budgeted_request_tokens(self.measurement(), tool_surface),
        )
    }
}

pub(crate) fn estimate_history_tokens_with_source(history: &[ChatMessage]) -> TokenMeasurement {
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
    plan_context_budget_with(history, config, warning_ratio, anchor::current())
}

/// [`plan_context_budget`] on the local estimate alone: what is reported to a
/// hook or an event, never what a reduction step decides on.
pub(crate) fn plan_context_budget_local(
    history: &[ChatMessage],
    config: &crate::config::AgentCompactionConfig,
    warning_ratio: f64,
) -> ContextBudgetSnapshot {
    plan_context_budget_with(history, config, warning_ratio, anchor::Planning::NONE)
}

fn plan_context_budget_with(
    history: &[ChatMessage],
    config: &crate::config::AgentCompactionConfig,
    warning_ratio: f64,
    planning: anchor::Planning,
) -> ContextBudgetSnapshot {
    let measured = estimate_history_tokens_with_source(history);
    // With an anchor in force the estimate is the local count planned under
    // the ratio the provider's last count showed, and the heuristic's safety
    // factor does not apply on top of it (an anchor is never formed from a
    // heuristic measurement).
    let used_tokens = if planning.is_anchored() {
        planning.history_tokens(measured.tokens)
    } else {
        budgeted_tokens(measured)
    };
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
    let mut handoff = ProtectedHandoff::of(history);
    loop {
        if planning_history_tokens(history) <= limit {
            break;
        }
        let remove_index = if history.len() > preserve_until {
            preserve_until
        } else if replacement_len == 0 && history.len() > start.saturating_add(1) {
            start
        } else {
            break;
        };
        let remove_index = handoff.removal_index(remove_index, history.len().saturating_sub(1));
        handoff.remove_at(history, remove_index);
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
        let mut handoff = ProtectedHandoff::of(history);
        while plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit {
            let start = history_non_system_start(history);
            if history.len() <= start {
                break;
            }
            let index = handoff.removal_index(start, history.len().saturating_sub(1));
            handoff.remove_at(history, index);
        }
    }
    history.len() != before_len || measure_history_tokens(history) != before_tokens
}
