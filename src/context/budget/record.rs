//! The request records of the budget: what was estimated for a request, and
//! what the provider reported for it. Sizes and counts only.

use super::anchor::{Calibration, LocalPlan, Probe, ReportedRequest, RequestBudget, UsageAnchors};
use super::{ContextBudgetSnapshot, check_request_window, request_tool_tokens};
use crate::agent::turn_context::RequestToolMeasurement;
use crate::llm::route_decision::TokenUsage;
use crate::memory::{MemoryFabric, MessageEventScope};
use crate::tokens::count::TokenMeasurementSource;

/// The event type of the per-request usage row.
pub(crate) const REPORTED_EVENT_TYPE: &str = "llm.request.usage.reported";

/// Note the request the reduction steps of one round have shaped, and set it
/// against the model window. Without an anchor the estimate checked is the
/// budgeted local total of the request. The check refuses nothing: a request
/// that is still over the window goes out and the provider-overflow path
/// recovers it.
pub(crate) fn plan_round(
    anchors: &UsageAnchors,
    probe: Probe,
    config: &crate::config::AgentCompactionConfig,
    budget: ContextBudgetSnapshot,
    tool_surface: RequestToolMeasurement,
) -> RequestBudget {
    let (_, heuristic_total_tokens) = budget.local_request_totals(tool_surface);
    let planned = anchors.note_request(
        probe,
        LocalPlan {
            history_tokens: budget.measured_tokens,
            tool_tokens: request_tool_tokens(tool_surface),
            heuristic: budget.measurement_source == TokenMeasurementSource::Heuristic
                || tool_surface.instructions.source == TokenMeasurementSource::Heuristic
                || tool_surface.native_payload.measurement.source == TokenMeasurementSource::Heuristic,
        },
    );
    let window = check_request_window(config, planned.anchored_tokens().unwrap_or(heuristic_total_tokens));
    if window.over {
        tracing::warn!(
            estimate_tokens = window.estimate_tokens,
            input_window_tokens = window.input_window_tokens,
            "request still exceeds the model window after the reduction steps; the provider overflow path recovers it"
        );
    }
    planned
}

/// Settle the response of one request: it becomes the anchor of the next
/// request when it reported input tokens and was served by the requested
/// provider and model, and its usage row is recorded.
pub(crate) async fn settle_round(
    anchors: &UsageAnchors,
    planned: &RequestBudget,
    trace: &crate::providers::traits::ChatTrace,
    (iteration, provider_name, model): (usize, &str, &str),
    sink: Option<(&MemoryFabric, MessageEventScope)>,
) {
    let reported = anchors.settle(&trace.tokens_used, &trace.final_provider, &trace.final_model, planned);
    let row = reported_json(
        iteration,
        &crate::providers::redact_endpoint_userinfo(provider_name),
        model,
        &trace.tokens_used,
        &reported,
    );
    tracing::debug!(
        target: "openprx::prompt",
        request_ordinal = reported.request_ordinal,
        reported_input_tokens = reported.reported_input_tokens,
        local_estimate_tokens = reported.local_tokens,
        anchored_estimate_tokens = reported.anchored_tokens,
        "request usage reported"
    );
    if let Some((fabric, scope)) = sink {
        emit_reported(fabric, scope, &row).await;
    }
}

impl RequestBudget {
    /// The `budget` object of the compiled-request record. `heuristic_total_tokens`
    /// is the estimate used when no anchor is in force.
    pub(crate) fn to_json(
        &self,
        config: &crate::config::AgentCompactionConfig,
        heuristic_total_tokens: Option<usize>,
    ) -> serde_json::Value {
        let window = check_request_window(
            config,
            self.anchored_tokens().or(heuristic_total_tokens).unwrap_or_default(),
        );
        budget_json(self, window)
    }
}

/// The `budget` object of the compiled-request record.
fn budget_json(budget: &RequestBudget, window: super::WindowCheck) -> serde_json::Value {
    let (anchor_request, reported_input_tokens, anchor_planned_tokens, fallback) = match budget.calibration {
        Calibration::Anchored(anchored) => (
            serde_json::json!(anchored.anchor_request),
            serde_json::json!(anchored.reported_input_tokens),
            serde_json::json!(anchored.planned_tokens),
            serde_json::Value::Null,
        ),
        Calibration::Heuristic(reason) => (
            serde_json::json!("none"),
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::json!(reason.label()),
        ),
    };
    serde_json::json!({
        "request_ordinal": budget.request_ordinal,
        "anchor_request": anchor_request,
        "reported_input_tokens": reported_input_tokens,
        "anchor_planned_tokens": anchor_planned_tokens,
        "ratio": ratio_json(budget.ratio_milli),
        "fallback": fallback,
        "local_estimate_tokens": budget.plan.total(),
        "local_history_tokens": budget.plan.history_tokens,
        "local_tool_tokens": budget.plan.tool_tokens,
        "anchored_estimate_tokens": budget.anchored_tokens(),
        "window": {
            "window_tokens": window.window_tokens,
            "input_window_tokens": window.input_window_tokens,
            "estimate_tokens": window.estimate_tokens,
            "over_window": window.over,
        },
    })
}

/// A ratio per mille as a number with three decimals.
fn ratio_json(ratio_milli: Option<u64>) -> serde_json::Value {
    ratio_milli.map_or(serde_json::Value::Null, |milli| {
        serde_json::json!(f64::from(u32::try_from(milli).unwrap_or(u32::MAX)) / 1000.0)
    })
}

/// The payload of the per-request usage row: the provider's count of this
/// request next to the estimates made for it, so the estimate error is a
/// number a real test can read.
fn reported_json(
    iteration: usize,
    provider: &str,
    model: &str,
    usage: &TokenUsage,
    reported: &ReportedRequest,
) -> serde_json::Value {
    let error = |estimate: usize| -> Option<i64> {
        reported.reported_input_tokens.map(|tokens| {
            i64::try_from(estimate)
                .unwrap_or(i64::MAX)
                .saturating_sub(i64::try_from(tokens).unwrap_or(i64::MAX))
        })
    };
    serde_json::json!({
        "schema_version": 1,
        "iteration": iteration,
        "provider": provider,
        "model": model,
        "request_ordinal": reported.request_ordinal,
        "anchor_request": reported.anchor_request,
        "reported_input_tokens": reported.reported_input_tokens,
        "ratio": ratio_json(reported.ratio_milli),
        "completion_tokens": usage.completion_tokens,
        "cache_read_input_tokens": usage.cache_read_input_tokens,
        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        "local_estimate_tokens": reported.local_tokens,
        "anchored_estimate_tokens": reported.anchored_tokens,
        "local_error_tokens": error(reported.local_tokens),
        "anchored_error_tokens": reported.anchored_tokens.and_then(error),
    })
}

/// Append the usage row of one request. A failure is logged and never fails
/// the turn: the row is a measurement.
async fn emit_reported(fabric: &MemoryFabric, scope: MessageEventScope, payload: &serde_json::Value) {
    let request_ordinal = payload
        .get("request_ordinal")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    if let Err(error) = fabric
        .record_runtime_event(
            scope,
            REPORTED_EVENT_TYPE,
            format!("request_ordinal={request_ordinal}"),
            Some(payload.to_string()),
        )
        .await
    {
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::warn!(error_len, error_sha256, "Failed to persist the request usage row");
    }
}
