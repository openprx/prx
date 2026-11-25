//! The state of the agent loop's own reduction points and the reporting of a
//! durable compaction the loop applied: pre-turn, at the start of a provider
//! round and after a round's tool results.

use super::preflight::PreflightCompaction;
use crate::agent::context_budget::{
    ContextBudgetSnapshot, PRE_TURN_FLUSH_THRESHOLD, estimate_history_tokens, plan_context_budget_local,
};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_context::RequestToolMeasurement;
use crate::agent::turn_contracts::{ToolLoopEvent, ToolLoopRuntimeAdapter, emit_tool_loop_event};
use crate::agent::turn_history::{CanonicalHistory, TurnHistory};
use crate::hooks::{HookEvent, HookManager};
use crate::providers::{ChatMessage, Provider};
use anyhow::Result;

/// Re-derive the provider view from the canonical history after a durable
/// compaction (the engine's projection: recalled page, skill-metadata
/// suppression, inbound middleware). The engine owns that projection, so the
/// steps hold it as a function pointer.
pub(crate) type Rederive = for<'x> fn(
    &'x mut TurnHistory,
    &'x mut Vec<ChatMessage>,
    &'x HookManager,
    &'x str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'x>>;

/// What every step of the loop needs from the turn besides the history.
#[derive(Clone, Copy)]
pub(crate) struct Services<'a> {
    pub(crate) provider: &'a dyn Provider,
    pub(crate) model: &'a str,
    pub(crate) audit: Option<&'a DocumentIngestRuntime>,
    pub(crate) runtime_adapter: &'a ToolLoopRuntimeAdapter,
    pub(crate) hooks: &'a HookManager,
    pub(crate) channel_name: &'a str,
    pub(crate) rederive: Rederive,
}

/// Before the loop opens: the one turn-level reduction point.
pub(crate) struct PreTurn<'a> {
    pub(crate) history: &'a mut Vec<ChatMessage>,
    pub(crate) turn_history: &'a mut TurnHistory,
    pub(crate) config: &'a crate::config::AgentCompactionConfig,
    pub(crate) services: Services<'a>,
}

/// At the start of a provider round, after the tool surface is measured.
pub(crate) struct Round<'a> {
    pub(crate) history: &'a mut Vec<ChatMessage>,
    pub(crate) turn_history: &'a mut TurnHistory,
    /// The configured compaction config; the round budgets with it reduced by
    /// the measured tool surface.
    pub(crate) config: &'a crate::config::AgentCompactionConfig,
    pub(crate) tool_surface: RequestToolMeasurement,
    pub(crate) services: Services<'a>,
    /// What the compaction step left for the trim step.
    pub(crate) compaction: PreflightCompaction,
    /// The budget the request was left with, set by the trim step.
    pub(crate) budget: Option<ContextBudgetSnapshot>,
}

/// After a round's tool results were appended to the history.
pub(crate) struct AfterTools<'a> {
    pub(crate) history: &'a mut Vec<ChatMessage>,
    pub(crate) turn_history: &'a mut TurnHistory,
    /// The config reserved for this turn's tool surface.
    pub(crate) reserved_config: &'a crate::config::AgentCompactionConfig,
}

/// The loop's history before a step that may compact it durably, kept only
/// while a session authority listens (`runtime_adapter.events`), so
/// [`report_durable_compaction`] can describe the step to it.
pub(crate) fn history_before_change(
    runtime_adapter: &ToolLoopRuntimeAdapter,
    canonical: &CanonicalHistory,
) -> Option<CanonicalHistory> {
    runtime_adapter.events.as_ref().map(|_| canonical.clone())
}

/// Report one durable in-turn compaction (a handoff, a summary or an evicted
/// page applied by the loop on its own: pre-turn, per-provider preflight,
/// overflow recovery) to the session authority, in order with the turn's
/// other events: one exact patch whose guarded range is the whole
/// pre-compaction canonical history `before` and whose replacement is the
/// compacted canonical history `compacted` (never the provider view, which may
/// hold this turn's earlier trims).
/// Lossy trims are not reported: they are the projection of one request, and
/// the session keeps the turns a later handoff can still recover (the run loop
/// rebuilds an inline view that diverged from the session after the turn).
/// Nothing is sent when nobody listens or nothing changed.
pub(crate) async fn report_durable_compaction(
    runtime_adapter: &ToolLoopRuntimeAdapter,
    before: Option<CanonicalHistory>,
    compacted: &CanonicalHistory,
    config: &crate::config::AgentCompactionConfig,
) -> Result<()> {
    let Some(source_history) = before else {
        return Ok(());
    };
    if source_history == *compacted {
        return Ok(());
    }
    let skipped_system = |messages: &[ChatMessage]| usize::from(messages.first().is_some_and(|m| m.role == "system"));
    crate::hooks::notify(
        HookEvent::ReductionFinished,
        serde_json::json!({
            "step": "compaction",
            "tokens_before": estimate_history_tokens(source_history.messages()),
            "tokens_after": estimate_history_tokens(compacted.messages()),
            "turns_before": source_history.len().saturating_sub(skipped_system(source_history.messages())),
            "turns_after": compacted.messages().len().saturating_sub(skipped_system(compacted.messages())),
        }),
    );
    if runtime_adapter.events.is_none() {
        return Ok(());
    }
    let guard = source_history
        .guard()
        .ok_or_else(|| anyhow::anyhow!("invalid in-turn compaction range"))?;
    let compacted_messages = compacted.messages();
    let patch = crate::agent::turn_contracts::CompactionPatch {
        replacement: compacted_messages.to_vec(),
        append_after: Vec::new(),
        guard,
    };
    let system_messages =
        |messages: &[ChatMessage]| usize::from(messages.first().is_some_and(|message| message.role == "system"));
    let used_context_tokens = plan_context_budget_local(compacted_messages, config, PRE_TURN_FLUSH_THRESHOLD).used_tokens;
    emit_tool_loop_event(
        runtime_adapter,
        ToolLoopEvent::ContextCompactionPatch {
            turns_before: source_history
                .len()
                .saturating_sub(system_messages(source_history.messages())),
            tokens_before: estimate_history_tokens(source_history.messages()),
            source_history,
            compacted_history: Box::new(compacted.clone()),
            patch,
            config: config.clone(),
            turns_after: compacted_messages
                .len()
                .saturating_sub(system_messages(compacted_messages)),
            tokens_after: estimate_history_tokens(compacted_messages),
            used_context_tokens,
        },
    )
    .await
}
