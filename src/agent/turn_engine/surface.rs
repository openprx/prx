//! Tool surface selection of one round: which tools the round may expose, the
//! budget the request is planned against, and the recovery tool publication.

use crate::providers::ChatMessage;
use crate::tools::Tool;
use anyhow::Result;

use crate::agent::context_budget::RequestToolSurfaceCounts;
use crate::agent::turn_history::TurnHistory;
use crate::context::budget::anchor::{self, RequestBudget};
use crate::context::budget::record::plan_round;
use crate::context::reduce::engine::{Round, Services};
use crate::context::reduce::normalize::repair_view;
use crate::context::reduce::preflight::PreflightCompaction;

use super::state::{RoundBudget, RoundSurface, TurnInputs, TurnState};
use super::{rederive_boxed, refresh_selected_tool};

/// Selects the tools this round exposes and measures the surface they make up.
pub(super) async fn select_round_surface<'t>(
    turn: &'t TurnInputs<'_>,
    state: &mut TurnState,
    turn_history: &mut TurnHistory,
    history: &mut Vec<ChatMessage>,
) -> Result<RoundSurface<'t>> {
    let TurnInputs {
        expose_stay_silent,
        service,
        context,
        proof,
        ref tools_registry,
        observer,
        hooks,
        ref tool_surface_reserve,
        ref mode_capabilities,
        ref inherited_exclusions,
        ..
    } = *turn;
    // Route the turn before refreshing dynamic tools. Refreshing the whole
    // registry here made an unrelated turn wait for every configured MCP
    // server (for example, a PDF-only WhatsApp request paid Playwright's
    // cold `npx` startup cost). The selected slice is also the exact slice
    // whose schemas are exposed below, so runtime discovery and the model's
    // catalog stay on one capability boundary.
    //
    // This whole block runs ahead of the context-budget preflight because
    // the schemas it produces travel to the provider in the same request as
    // the messages: a budget that cannot see them cannot keep the request
    // inside the window. Routing still precedes every refresh, and only the
    // routed slice is refreshed.
    let constrained = service.agent_execution_specs(proof, context)?;
    let search_spec = service.agent_capability_search_spec(proof, context)?;
    let appended = service.agent_appended_execution_specs(proof, context)?;
    // From the canonical history, which is never suppressed: a previous
    // iteration's suppressed view would read as "no skills advertised" and
    // let a later re-derived view restore the metadata without skill_read.
    let advertises_skills = turn_history
        .canonical(history)
        .iter()
        .any(|message| message.role == "system" && message.content.contains("<available_skills>"));
    let skill_spec = if advertises_skills && proof.tool_calls_allowed() && service.model_can_call_tools(proof) {
        // Exact selected/discovered skill readers already passed source
        // and fingerprint admission, including their bound skill name.
        let admitted = constrained
            .specs
            .iter()
            .chain(appended.specs.iter())
            .find(|spec| spec.name == "skill_read")
            .cloned();
        match admitted.map_or_else(
            || service.agent_runtime_dependency_spec(proof, context, "skill_read"),
            Ok,
        ) {
            Ok(spec) => Some(spec),
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::warn!(
                    error_len,
                    error_sha256,
                    "suppressing skill metadata: live skill_read is unavailable"
                );
                None
            }
        }
    } else {
        None
    };
    let suppress_skill_metadata = advertises_skills && skill_spec.is_none();
    turn_history.set_skill_metadata_suppressed(suppress_skill_metadata);
    if suppress_skill_metadata {
        turn_history.reshape(history, |view| {
            crate::agent::runtime_prompt::suppress_unreadable_skill_metadata(view)
        });
        tracing::warn!("skill metadata suppressed because this turn has no admitted skill_read");
    }

    let mut selected_tools: Vec<&dyn Tool> = tools_registry
        .iter()
        .filter(|tool| {
            constrained.roots.contains(tool.name())
                || appended.roots.contains(tool.name())
                || search_spec.as_ref().is_some_and(|spec| spec.name == tool.name())
                || skill_spec.as_ref().is_some_and(|spec| tool.supports_name(&spec.name))
        })
        .map(Box::as_ref)
        .collect();
    if !inherited_exclusions.is_empty() {
        // Matched through `supports_name`, not the root name: `channel_exclude`
        // names a *published* tool, and a backend that publishes several
        // (an MCP router, a WASM alias) answers to a name its root spelling
        // never equals. Comparing root names left those aliases exposed.
        selected_tools.retain(|tool| {
            !inherited_exclusions
                .iter()
                .any(|hidden| tool.name() == hidden || tool.supports_name(hidden))
        });
    }
    for tool in &selected_tools {
        refresh_selected_tool(*tool, observer, hooks).await;
    }
    // Refresh dynamic backends before snapshotting their schemas. Rebuild on
    // every iteration so MCP discovery and WASM generation swaps cannot leave
    // the provider with stale ToolSpecs for the remainder of the turn.
    let total: usize = tools_registry.len();
    tracing::debug!(total, filtered = selected_tools.len(), "tool tiering applied");
    let tool_catalog = crate::tools::ToolCatalog::from_tools(selected_tools.iter().copied());
    let mut tool_specs = service.agent_execution_specs(proof, context)?.specs;
    let appended = service.agent_appended_execution_specs(proof, context)?;
    let appended_names: std::collections::BTreeSet<_> = appended.specs.iter().map(|spec| spec.name.clone()).collect();
    tool_specs.extend(appended.specs);
    if let Some(spec) = search_spec {
        tool_specs.push(spec);
    }
    if let Some(spec) = &skill_spec {
        tool_specs.retain(|candidate| candidate.name != spec.name);
        tool_specs.push(spec.clone());
    }
    if expose_stay_silent {
        {
            tool_specs.push(service.agent_runtime_dependency_spec(proof, context, "stay_silent")?);
        }
    }
    let decision_fingerprints = {
        tool_specs
            .iter()
            .map(|spec| {
                (
                    spec.name.clone(),
                    crate::tools::capability::fingerprint::tool_spec_fingerprint(
                        &spec.name,
                        &spec.description,
                        &spec.parameters,
                    ),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    crate::tools::filter_tool_specs_for_exposure(&mut tool_specs, expose_stay_silent);
    let tool_surface = state
        .request_tool_surface_cache
        .measure(mode_capabilities.native_tool_calling, &tool_specs);
    if let Some(reserve) = tool_surface_reserve.as_ref() {
        reserve.record(tool_surface);
    }
    let tool_surface_counts =
        RequestToolSurfaceCounts::measure(tools_registry.len(), selected_tools.len(), &tool_catalog, &tool_specs);
    Ok(RoundSurface {
        selected_tools,
        tool_specs,
        appended_names,
        decision_fingerprints,
        tool_surface,
        tool_surface_counts,
    })
}

/// Plans the request budget of the round against its tool surface and repairs
/// the provider view.
pub(super) async fn plan_round_budget(
    turn: &TurnInputs<'_>,
    surface: &RoundSurface<'_>,
    turn_history: &mut TurnHistory,
    history: &mut Vec<ChatMessage>,
) -> Result<RoundBudget> {
    let TurnInputs {
        channel_name,
        compaction_config,
        provider,
        provider_name,
        model,
        hooks,
        ref runtime_adapter,
        ref document_ingest,
        ref usage_anchors,
        ..
    } = *turn;
    let tool_specs = &surface.tool_specs;
    let decision_fingerprints = &surface.decision_fingerprints;
    let tool_surface = surface.tool_surface;
    // P5: hard budget preflight before every provider call. Summary
    // compaction is preferred when enabled; token-aware trim is the last
    // local guard. Overflow retry below remains the provider-side fallback.
    //
    // The message total and the request total it planned against are
    // carried into the request event, so a difference between what was
    // budgeted and what was finally prepared is a number an operator can
    // read rather than an invariant nobody can check. `None` on the
    // entrypoints that pass no compaction config: there is no budget to
    // report there.
    let mut request_budget: Option<RequestBudget> = None;
    let budgeted_totals: Option<(usize, usize)> = if let Some(config) = compaction_config {
        // The system prompt is the leading message of the history; its
        // budgeted size feeds the window floor warning in `plan_round`. It is
        // measured only until that warning has fired.
        let system_tokens = if crate::context::budget::record::floor_warning_pending() {
            history.first().filter(|m| m.role == "system").map_or(0, |m| {
                crate::agent::context_budget::budgeted_tokens(
                    crate::agent::context_budget::estimate_history_tokens_with_source(std::slice::from_ref(m)),
                )
            })
        } else {
            0
        };
        // The durable phase, then the projection phase: the two steps of
        // the reduction ladder that run on every round, under the
        // calibration the last response's reported usage gives.
        let tool_hash = anchor::tool_surface_hash(&tool_specs, &decision_fingerprints);
        let probe = usage_anchors.probe(history, Some(tool_hash), provider_name, model);
        let mut round = Round {
            history: &mut *history,
            turn_history,
            config,
            tool_surface,
            services: Services {
                provider,
                model,
                audit: document_ingest.as_ref(),
                runtime_adapter: &runtime_adapter,
                hooks,
                channel_name,
                rederive: rederive_boxed,
            },
            compaction: PreflightCompaction::default(),
            budget: None,
        };
        let planning = probe.planning(tool_surface);
        anchor::scoped(planning, Box::pin(crate::context::api::round::next_round(&mut round))).await?;
        request_budget = round
            .budget
            .map(|budget| plan_round(&usage_anchors, probe, config, budget, tool_surface, system_tokens));
        round.budget.map(|budget| budget.local_request_totals(tool_surface))
    } else {
        None
    };
    // The end of the reduction: whatever step cut the view, the request
    // that leaves for the provider follows the tool-call protocol. Runs on
    // the entrypoints without a compaction config too, whose overflow
    // retry trims the view.
    repair_view(turn_history, history, "pre_provider_call");
    Ok(RoundBudget {
        request_budget,
        budgeted_totals,
    })
}

/// Publishes the recovery tool when the handoff needs it, and reports whether
/// the required lookup is part of the round.
pub(super) async fn publish_context_lookup<'t>(
    turn: &'t TurnInputs<'_>,
    state: &mut TurnState,
    surface: &mut RoundSurface<'t>,
    turn_history: &TurnHistory,
    history: &[ChatMessage],
) -> Result<bool> {
    let TurnInputs {
        expose_stay_silent,
        compaction_config,
        service,
        context,
        proof,
        ref tools_registry,
        observer,
        hooks,
        ref mode_capabilities,
        ..
    } = *turn;
    let RoundSurface {
        selected_tools,
        tool_specs,
        decision_fingerprints,
        tool_surface,
        tool_surface_counts,
        ..
    } = surface;
    // Evaluated after the preflight: a `switch` rollover performed there is
    // what puts the handoff into history, and the iteration that receives
    // the handoff is the one that has to publish the recovery tool the
    // handoff tells the model to call.
    let context_lookup_required = compaction_config
        .is_some_and(|config| matches!(config.mode, crate::config::AgentCompactionMode::Switch))
        && turn_history.holds_context_handoff(history);
    if context_lookup_required
        && !selected_tools
            .iter()
            .any(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
        && let Some(lookup) = tools_registry
            .iter()
            .find(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
    {
        refresh_selected_tool(lookup.as_ref(), observer, hooks).await;
        selected_tools.push(lookup.as_ref());
        let tool_catalog = crate::tools::ToolCatalog::from_tools(selected_tools.iter().copied());
        {
            let dependency = service.agent_runtime_dependency_spec(
                proof,
                context,
                crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME,
            )?;
            {
                let fingerprints = &mut *decision_fingerprints;
                fingerprints.insert(
                    dependency.name.clone(),
                    crate::tools::capability::fingerprint::tool_spec_fingerprint(
                        &dependency.name,
                        &dependency.description,
                        &dependency.parameters,
                    ),
                );
            }
            tool_specs.push(dependency);
        }
        crate::tools::filter_tool_specs_for_exposure(&mut *tool_specs, expose_stay_silent);
        // The recovery tool is published after the budget was planned, so
        // this iteration's window is short by exactly what that one spec
        // adds — its schema and its line in the capability snapshot. The
        // next iteration budgets the full set.
        *tool_surface = state
            .request_tool_surface_cache
            .measure(mode_capabilities.native_tool_calling, &tool_specs);
        *tool_surface_counts =
            RequestToolSurfaceCounts::measure(tools_registry.len(), selected_tools.len(), &tool_catalog, &tool_specs);
    }
    Ok(context_lookup_required)
}
