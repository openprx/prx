use crate::agent::compaction::{is_context_handoff_message, is_tool_roundtrip_companion};
use crate::agent::context_preflight::capture_current_turn_request;
use crate::agent::history::history_non_system_start;
use crate::agent::turn_contracts::ContextCapacityError;
use crate::agent::turn_history::ViewSlot;
use crate::providers::ChatMessage;
use crate::tokens::count::{TokenMeasurement, TokenMeasurementSource, count_tokens_with_source};

/// Fraction of `max_context_tokens` above which a pre-turn memory flush is triggered.
pub(crate) const PRE_TURN_FLUSH_THRESHOLD: f64 = 0.85;

/// Safety multiplier used when the cl100k proxy is unavailable and the
/// fallback char heuristic is the only available measurement.
pub(super) const TOKEN_HEURISTIC_BUDGET_MULTIPLIER: usize = 2;

pub(super) fn estimate_history_tokens(history: &[ChatMessage]) -> usize {
    estimate_history_tokens_with_source(history).tokens
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

/// The tool-surface reserve of a chat session's most recent request, shared
/// between the turn engine and the chat front ends.
///
/// The preflight reserves the published tool surface before it plans a
/// request. A chat front end decides `/compact`, prints compaction feedback
/// and draws the context footer outside any turn, where that surface is not
/// measured; it reads the last measured reserve from here so all of them use
/// the window the preflight uses. Zero until the first request is measured.
#[derive(Clone, Debug, Default)]
pub(crate) struct ToolSurfaceReserve(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl ToolSurfaceReserve {
    /// Store the reserve the preflight applies for `tool_surface`.
    pub(crate) fn record(&self, tool_surface: crate::agent::turn_context::RequestToolMeasurement) {
        self.0.store(
            budgeted_request_tool_tokens(tool_surface),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// The tokens the last measured tool surface occupies in the window.
    pub(crate) fn tokens(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// `config` with the last measured tool surface reserved, the budget the
    /// preflight plans against.
    pub(crate) fn reserved_config(
        &self,
        config: &crate::config::AgentCompactionConfig,
    ) -> crate::config::AgentCompactionConfig {
        crate::config::AgentCompactionConfig {
            reserve_tokens: config.reserve_tokens.saturating_add(self.tokens()),
            ..config.clone()
        }
    }

    /// Context tokens one request occupies: the history estimate plus the
    /// reserved tool surface, the figure the context footer shows.
    pub(crate) fn request_used_tokens(&self, history_used_tokens: usize) -> usize {
        history_used_tokens.saturating_add(self.tokens())
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

/// Per-message framing overhead (role marker, separators) approximated as a
/// small constant added on top of the tokenized role and content.
const PER_MESSAGE_FRAMING_TOKENS: usize = 3;

/// One message's estimate: its role and content tokens with the framing
/// overhead, and the source the estimate came from.
fn message_measurement(message: &ChatMessage) -> TokenMeasurement {
    let role = count_tokens_with_source(&message.role);
    let content = count_tokens_with_source(&message.content);
    let source = if matches!(role.source, TokenMeasurementSource::Heuristic)
        || matches!(content.source, TokenMeasurementSource::Heuristic)
    {
        TokenMeasurementSource::Heuristic
    } else {
        TokenMeasurementSource::Cl100kProxy
    };
    TokenMeasurement {
        tokens: role
            .tokens
            .saturating_add(content.tokens)
            .saturating_add(PER_MESSAGE_FRAMING_TOKENS),
        source,
    }
}

pub(super) fn estimate_history_tokens_with_source(history: &[ChatMessage]) -> TokenMeasurement {
    let mut source = TokenMeasurementSource::Cl100kProxy;
    let tokens = history.iter().fold(0usize, |total, msg| {
        let measured = message_measurement(msg);
        if matches!(measured.source, TokenMeasurementSource::Heuristic) {
            source = TokenMeasurementSource::Heuristic;
        }
        total.saturating_add(measured.tokens)
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


/// The budgeted message tokens of `history`: the figure the fit compares with the window.
pub(crate) fn budgeted_history_tokens(history: &[ChatMessage]) -> usize {
    budgeted_tokens(estimate_history_tokens_with_source(history))
}

/// The one fit (compaction design §3.3): what a request keeps when its window
/// is over budget, shared by the engine preflight, the mid-turn trim, the
/// redux rollover and the overflow recovery.
///
/// Protected, in every step: the system message(s), the newest `context_handoff`
/// note, the turn's current request, the newest two completed rounds of the
/// in-flight turn, and every incomplete round (an untracked message is never
/// a round). A safeguard summary is not protected and is dropped like any
/// other older message.
///
/// The steps run in order, each stopping as soon as the request fits:
/// - **A.** Remove unprotected messages older than the newest handoff note (or,
///   with none, older than the request), front first.
/// - **B.** Fold the results of eligible in-flight rounds, oldest first.
/// - **C.** Remove whole acknowledged messages between the newest handoff and
///   the request, oldest first (tool-shaped messages are left to the rounds).
/// - **D.** Remove whole unprotected completed rounds, oldest first, when their
///   full membership is recorded.
/// - **D2.** Still over the limit: fold the protected completed rounds, the
///   older first, then the newest. Nothing is removed in this step.
/// - **E.** Still over the limit: [`ContextCapacityError::BudgetExhausted`].
pub(crate) fn fit_request_window(
    messages: &mut Vec<ChatMessage>,
    slots: &mut Vec<ViewSlot>,
    limit: usize,
    demand: FitDemand,
    trigger: &'static str,
) -> Result<FitReport, ContextCapacityError> {
    if slots.len() != messages.len() {
        // A view whose slots were lost is never folded or removed as a round.
        *slots = vec![ViewSlot::Acknowledged; messages.len()];
    }
    let costs = messages.iter().map(message_cost).collect();
    let mut window = FitWindow {
        messages,
        slots,
        costs,
        limit,
        demand,
        report: FitReport::default(),
    };
    let fitted = window.fits()
        || window.drop_older_than_boundary()
        || window.fold_inflight_rounds()
        || window.drop_acknowledged_inside_window()
        || window.drop_unprotected_rounds()
        || window.fold_protected_rounds();
    let tokens = window.tokens();
    if !fitted && tokens > limit {
        tracing::warn!(
            trigger,
            needed_tokens = tokens,
            limit_tokens = limit,
            "the protected floor of this turn does not fit the context window"
        );
        return Err(ContextCapacityError::BudgetExhausted {
            needed: tokens,
            limit,
        });
    }
    window.report.tokens = tokens;
    window.report.strictly_smaller = match demand {
        FitDemand::Budget => true,
        FitDemand::ProviderOverflow { rejected_tokens } => tokens < rejected_tokens,
    };
    Ok(window.report)
}

/// The fit over a plain history with no in-flight rounds (the redux rollover
/// runs before the turn's loop): every message is acknowledged.
pub(crate) fn fit_plain_window(
    messages: &mut Vec<ChatMessage>,
    limit: usize,
    trigger: &'static str,
) -> Result<FitReport, ContextCapacityError> {
    let mut slots = vec![ViewSlot::Acknowledged; messages.len()];
    fit_request_window(messages, &mut slots, limit, FitDemand::Budget, trigger)
}

/// What a fit is asked to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FitDemand {
    /// The window must hold the request.
    Budget,
    /// A provider rejected a request of `rejected_tokens` budgeted message
    /// tokens although the local estimate fitted: the candidate must also be
    /// strictly smaller than that request.
    ProviderOverflow { rejected_tokens: usize },
}

/// What one fit did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FitReport {
    /// Budgeted message tokens after the fit.
    pub(crate) tokens: usize,
    /// Rounds whose results were folded (step B).
    pub(crate) folded_rounds: usize,
    /// Messages removed by steps C and D.
    pub(crate) removed_messages: usize,
    /// Whole rounds removed by step D.
    pub(crate) removed_rounds: usize,
    /// Whether any step changed the window.
    pub(crate) reduced: bool,
    /// Under `ProviderOverflow`, whether the candidate is strictly smaller than
    /// the rejected request. Always true under `Budget`.
    pub(crate) strictly_smaller: bool,
}

/// The text a folded tool result carries in place of its body (§3.4). Its
/// prefix is what makes folding idempotent.
pub(crate) const FOLDED_TOOL_RESULT_PREFIX: &str = "[tool result folded to fit the context:";

/// The degradation reason a fold of in-flight rounds records (§3.4).
pub(crate) const INFLIGHT_ROUNDS_FOLDED_REASON: &str = "inflight_tool_rounds_folded";

/// How many of the newest completed rounds the fit never touches.
const PROTECTED_COMPLETED_ROUNDS: usize = 2;

/// Closing marker of one prompt-guided result inside a `[Tool results` block.
const PROMPT_RESULT_CLOSE: &str = "\n</tool_result>";

/// Opening marker of one prompt-guided result, up to the tool name.
const PROMPT_RESULT_OPEN: &str = "<tool_result name=\"";

/// A message's own cost: its raw estimate, and whether any of it was heuristic.
#[derive(Debug, Clone, Copy)]
struct MessageCost {
    tokens: usize,
    heuristic: bool,
}

fn message_cost(message: &ChatMessage) -> MessageCost {
    let measured = message_measurement(message);
    MessageCost {
        tokens: measured.tokens,
        heuristic: matches!(measured.source, TokenMeasurementSource::Heuristic),
    }
}

/// Where the protected set starts and ends for one step.
struct FitPlan {
    start: usize,
    handoff: Option<usize>,
    request: Option<usize>,
    protected: Vec<u64>,
}

/// The fit's working state: the window it reduces and its per-message costs,
/// kept in step with the window so no step re-tokenizes the whole history.
struct FitWindow<'a> {
    messages: &'a mut Vec<ChatMessage>,
    slots: &'a mut Vec<ViewSlot>,
    costs: Vec<MessageCost>,
    limit: usize,
    demand: FitDemand,
    report: FitReport,
}

impl FitWindow<'_> {
    /// The window's budgeted message tokens, the figure `plan_context_budget` reports.
    fn tokens(&self) -> usize {
        let raw = self
            .costs
            .iter()
            .fold(0usize, |total, cost| total.saturating_add(cost.tokens));
        let source = if self.costs.iter().any(|cost| cost.heuristic) {
            TokenMeasurementSource::Heuristic
        } else {
            TokenMeasurementSource::Cl100kProxy
        };
        budgeted_tokens(TokenMeasurement { tokens: raw, source })
    }

    fn fits(&self) -> bool {
        let tokens = self.tokens();
        tokens <= self.limit
            && match self.demand {
                FitDemand::Budget => true,
                FitDemand::ProviderOverflow { rejected_tokens } => tokens < rejected_tokens,
            }
    }

    fn plan(&self) -> FitPlan {
        let request = capture_current_turn_request(self.messages)
            .and_then(|captured| captured.index_in(self.messages.len()));
        let mut completed: Vec<u64> = self
            .slots
            .iter()
            .filter_map(|slot| match slot {
                ViewSlot::Round(tag) if tag.complete => Some(tag.batch),
                _ => None,
            })
            .collect();
        completed.sort_unstable();
        completed.dedup();
        completed.reverse();
        completed.truncate(PROTECTED_COMPLETED_ROUNDS);
        FitPlan {
            start: history_non_system_start(self.messages),
            handoff: self.messages.iter().rposition(is_context_handoff_message),
            request,
            protected: completed,
        }
    }

    /// Whether the message at `index` is one of the protected completed rounds.
    fn in_protected_round(&self, index: usize, plan: &FitPlan) -> bool {
        matches!(self.slots.get(index), Some(ViewSlot::Round(tag)) if plan.protected.contains(&tag.batch))
    }

    /// Remove one message, its slot and its cost. False when out of range.
    fn remove_at(&mut self, index: usize) -> bool {
        if index >= self.messages.len() || index >= self.slots.len() || index >= self.costs.len() {
            return false;
        }
        self.messages.remove(index);
        self.slots.remove(index);
        self.costs.remove(index);
        self.report.reduced = true;
        true
    }

    /// Drop `role="tool"` results stranded at `from` by a removal. Returns how many.
    fn drop_orphans_at(&mut self, from: usize) -> usize {
        let mut dropped = 0usize;
        while self.messages.get(from).is_some_and(|message| message.role == "tool") && self.remove_at(from) {
            dropped = dropped.saturating_add(1);
        }
        dropped
    }

    /// Step A.
    fn drop_older_than_boundary(&mut self) -> bool {
        let plan = self.plan();
        let mut end = plan.handoff.or(plan.request).unwrap_or(self.messages.len());
        let mut index = plan.start;
        while index < end && index < self.messages.len() {
            let removable = self
                .messages
                .get(index)
                .is_some_and(|message| message.role != "system")
                && !self.in_protected_round(index, &plan)
                && !matches!(self.slots.get(index), Some(ViewSlot::Round(tag)) if !tag.complete);
            if !removable {
                index = index.saturating_add(1);
                continue;
            }
            if !self.remove_at(index) {
                break;
            }
            let orphans = self.drop_orphans_at(index);
            end = end.saturating_sub(1).saturating_sub(orphans);
            if self.fits() {
                return true;
            }
        }
        self.fits()
    }

    /// Step B: fold the results of eligible unprotected in-flight rounds, oldest first.
    fn fold_inflight_rounds(&mut self) -> bool {
        self.fold_rounds(false)
    }

    /// Step D2: fold the results of the protected completed rounds, oldest first.
    fn fold_protected_rounds(&mut self) -> bool {
        self.fold_rounds(true)
    }

    /// Fold the results of the completed rounds whose protection is `protected`,
    /// oldest first, until the window fits. Removes nothing.
    fn fold_rounds(&mut self, protected: bool) -> bool {
        if self.fits() {
            return true;
        }
        let plan = self.plan();
        let mut folded_batches: Vec<u64> = Vec::new();
        let mut index = 0usize;
        while index < self.messages.len() {
            let tag = match self.slots.get(index) {
                Some(ViewSlot::Round(tag))
                    if !tag.envelope && tag.complete && plan.protected.contains(&tag.batch) == protected =>
                {
                    *tag
                }
                _ => {
                    index = index.saturating_add(1);
                    continue;
                }
            };
            let Some(message) = self.messages.get(index).cloned() else {
                break;
            };
            let folded = if message.role == "tool" {
                let tool = self.tool_name_of(index, tag.batch);
                fold_native_result(&message, &tool)
            } else if message.role == "user" && message.content.starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX) {
                fold_prompt_block(&message)
            } else {
                None
            };
            let folded_cost = folded.as_ref().map(|candidate| message_cost(candidate).tokens);
            let old_cost = self.costs.get(index).map_or(usize::MAX, |cost| cost.tokens);
            if let (Some(candidate), Some(cost_after)) = (folded, folded_cost)
                && cost_after < old_cost
                && self.replace_at(index, candidate)
            {
                if !folded_batches.contains(&tag.batch) {
                    folded_batches.push(tag.batch);
                    self.report.folded_rounds = folded_batches.len();
                }
                if self.fits() {
                    return true;
                }
            }
            index = index.saturating_add(1);
        }
        self.fits()
    }

    /// Replace the message at `index`, its cost included. False when out of range.
    fn replace_at(&mut self, index: usize, message: ChatMessage) -> bool {
        let cost = message_cost(&message);
        let (Some(slot), Some(cost_slot)) = (self.messages.get_mut(index), self.costs.get_mut(index)) else {
            return false;
        };
        *slot = message;
        *cost_slot = cost;
        self.report.reduced = true;
        true
    }

    /// The tool name a native result answers, from its round's assistant envelope.
    fn tool_name_of(&self, index: usize, batch: u64) -> String {
        let envelope = self.slots.iter().position(|slot| {
            matches!(slot, ViewSlot::Round(tag) if tag.batch == batch && tag.envelope)
        });
        let id = self
            .messages
            .get(index)
            .and_then(|message| serde_json::from_str::<serde_json::Value>(&message.content).ok())
            .and_then(|value| value.get("tool_call_id").and_then(serde_json::Value::as_str).map(str::to_string));
        envelope
            .and_then(|envelope| self.messages.get(envelope))
            .zip(id)
            .and_then(|(envelope, id)| tool_name_for_call(&envelope.content, &id))
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Step C: remove whole acknowledged messages between the newest handoff and the request.
    fn drop_acknowledged_inside_window(&mut self) -> bool {
        if self.fits() {
            return true;
        }
        let plan = self.plan();
        let mut end = plan.request.unwrap_or(self.messages.len());
        let mut index = plan.handoff.map_or(plan.start, |handoff| handoff.saturating_add(1));
        while index < end && index < self.messages.len() {
            let removable = matches!(self.slots.get(index), Some(ViewSlot::Acknowledged))
                && self.messages.get(index).is_some_and(|message| {
                    message.role != "system"
                        && !is_tool_roundtrip_companion(message)
                        && !is_context_handoff_message(message)
                });
            if !removable {
                index = index.saturating_add(1);
                continue;
            }
            if !self.remove_at(index) {
                break;
            }
            self.report.removed_messages = self.report.removed_messages.saturating_add(1);
            end = end.saturating_sub(1);
            if self.fits() {
                return true;
            }
        }
        self.fits()
    }

    /// Step D: remove whole unprotected completed rounds, oldest first, when
    /// the recorded batch is complete in the window.
    fn drop_unprotected_rounds(&mut self) -> bool {
        if self.fits() {
            return true;
        }
        let plan = self.plan();
        let mut batches: Vec<u64> = self
            .slots
            .iter()
            .filter_map(|slot| match slot {
                ViewSlot::Round(tag) if tag.complete && !plan.protected.contains(&tag.batch) => Some(tag.batch),
                _ => None,
            })
            .collect();
        batches.sort_unstable();
        batches.dedup();
        for batch in batches {
            let members: Vec<usize> = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| matches!(slot, ViewSlot::Round(tag) if tag.batch == batch))
                .map(|(index, _)| index)
                .collect();
            let recorded_size = self.slots.iter().find_map(|slot| match slot {
                ViewSlot::Round(tag) if tag.batch == batch => Some(tag.size),
                _ => None,
            });
            // Full membership is proven only when every recorded message of the batch is here.
            if members.is_empty() || recorded_size != Some(members.len()) {
                continue;
            }
            for index in members.iter().rev() {
                self.remove_at(*index);
            }
            self.report.removed_messages = self.report.removed_messages.saturating_add(members.len());
            self.report.removed_rounds = self.report.removed_rounds.saturating_add(1);
            if self.fits() {
                return true;
            }
        }
        self.fits()
    }
}

/// The folded replacement of one native `role="tool"` result: only its nested
/// `content` changes, and `tool_call_id` with every other field stays.
fn fold_native_result(message: &ChatMessage, tool: &str) -> Option<ChatMessage> {
    let mut value: serde_json::Value = serde_json::from_str(&message.content).ok()?;
    value.get("tool_call_id").and_then(serde_json::Value::as_str)?;
    let content = value.get("content").and_then(serde_json::Value::as_str)?;
    if content.starts_with(FOLDED_TOOL_RESULT_PREFIX) {
        return None;
    }
    let folded = folded_result_text(tool, content.len());
    *value.get_mut("content")? = serde_json::Value::String(folded);
    Some(ChatMessage {
        role: message.role.clone(),
        content: value.to_string(),
    })
}

/// The folded replacement of a prompt-guided `[Tool results` block: each
/// result's body changes, the block header and the per-result labels stay.
/// `None` when the block has no well-formed result to fold.
fn fold_prompt_block(message: &ChatMessage) -> Option<ChatMessage> {
    let mut rest = message.content.as_str();
    let mut out = String::with_capacity(message.content.len());
    let mut results = 0usize;
    while let Some((before, after_open)) = rest.split_once(PROMPT_RESULT_OPEN) {
        let (name, after_name) = after_open.split_once('"')?;
        let body_and_tail = after_name.strip_prefix(">\n")?;
        let (body, tail) = body_and_tail.split_once(PROMPT_RESULT_CLOSE)?;
        out.push_str(before);
        out.push_str(PROMPT_RESULT_OPEN);
        out.push_str(name);
        out.push_str("\">\n");
        if body.starts_with(FOLDED_TOOL_RESULT_PREFIX) {
            out.push_str(body);
        } else {
            out.push_str(&folded_result_text(name, body.len()));
        }
        out.push_str(PROMPT_RESULT_CLOSE);
        rest = tail;
        results = results.saturating_add(1);
    }
    if results == 0 {
        return None;
    }
    out.push_str(rest);
    Some(ChatMessage {
        role: message.role.clone(),
        content: out,
    })
}

/// The replacement text, naming the tool and the UTF-8 bytes it dropped.
fn folded_result_text(tool: &str, dropped_bytes: usize) -> String {
    format!("{FOLDED_TOOL_RESULT_PREFIX} tool={tool}, {dropped_bytes} bytes dropped; re-run the tool if needed]")
}

/// The name of the tool call `id` in an assistant `tool_calls` envelope.
fn tool_name_for_call(envelope: &str, id: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(envelope).ok()?;
    let call = value
        .get("tool_calls")?
        .as_array()?
        .iter()
        .find(|call| call.get("id").and_then(serde_json::Value::as_str) == Some(id))?;
    call.get("name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            call.get("function")
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_string)
}

#[path = "turn_tests/context_budget.rs"]
mod tests;
