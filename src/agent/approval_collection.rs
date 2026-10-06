//! One main-provider proposal, with no scheduler or executable tool surface.
use crate::agent::turn_contracts::{RetainedProviderTrace, ToolLoopTrace};
use crate::providers::{ChatMessage, ChatRequest, Provider};
use crate::runtime::turn_decision::ApprovalCollectionLease;
use tokio_util::sync::CancellationToken;

pub(crate) struct CollectionProviderRequest<'a> {
    pub(crate) provider: &'a dyn Provider,
    pub(crate) messages: &'a [ChatMessage],
    pub(crate) selected_provider: &'a str,
    pub(crate) model: &'a str,
    pub(crate) temperature: f64,
    pub(crate) retained: &'a RetainedProviderTrace,
    pub(crate) cancellation: &'a CancellationToken,
}

/// Even a rejected proposal retains its actual main-provider trace. The caller
/// must settle this trace once before choosing AwaitingApproval or failure.
pub(crate) struct CollectionProviderOutcome {
    pub(crate) trace: ToolLoopTrace,
    pub(crate) pending: anyhow::Result<()>,
}

pub(crate) async fn collect_approval_arguments(
    request: CollectionProviderRequest<'_>,
    lease: ApprovalCollectionLease,
) -> anyhow::Result<CollectionProviderOutcome> {
    anyhow::ensure!(!request.cancellation.is_cancelled(), "collection cancelled");
    let specs = [lease.proposal_spec()];
    request.retained.begin();
    let result = tokio::select! {
        biased;
        () = request.cancellation.cancelled() => Err(anyhow::anyhow!("collection cancelled")),
        result = request.provider.chat_traced(
            ChatRequest { messages: request.messages, tools: Some(&specs) }, request.model, request.temperature,
        ) => result,
    };
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            request.retained.failed();
            return Err(error);
        }
    };
    let dialect = request
        .provider
        .tool_schema_dialect_of_member(
            response.served_member.as_deref().unwrap_or(&response.final_provider),
            &response.final_model,
        )
        .unwrap_or_else(|| request.provider.tool_schema_dialect_for(&response.final_model));
    let fallback = response.attempts.len() > 1
        || response.final_provider != request.selected_provider
        || response.final_model != request.model;
    let reported = response.tokens_used.is_reported() && response.tokens_used.has_any_tokens();
    let trace = ToolLoopTrace {
        final_provider: Some(response.final_provider),
        final_model: Some(response.final_model),
        attempts: response.attempts,
        any_turn_had_fallback: fallback,
        tokens_used: response.tokens_used,
        execution_trace: None,
    };
    // Do this before parsing or awaiting pending persistence, including on
    // cancellation/Drop during that persistence. No auxiliary debit is made.
    request
        .retained
        .completed(&trace, trace.tokens_used.clone(), fallback, reported);
    let mut calls = response.response.tool_calls.into_iter();
    let pending = match (calls.next(), calls.next()) {
        (Some(call), None) => lease.record(call, dialect, request.cancellation).await,
        _ => Err(anyhow::anyhow!("collection requires exactly one complete tool call")),
    };
    Ok(CollectionProviderOutcome { trace, pending })
}
