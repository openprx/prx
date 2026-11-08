//! Budget admission for one provider round, decided before either transport.
use crate::agent::context_budget::{estimate_history_tokens_with_source, estimate_request_tokens};
use crate::agent::turn_context::measure_native_tool_payload_for;
use crate::providers::ChatMessage;
use crate::runtime::turn_decision::{TurnDecisionContext, TurnDecisionService};
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::execution::admission::CallTokenBudget;
use crate::tools::{ToolSchemaDialect, ToolSpec};
use anyhow::{Context, Result};
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    /// Narrows the live G6 tokens after the decision was admitted, the way a
    /// decision attempt's settlement does. One test runs on one thread.
    pub(crate) static TEST_TOKEN_CAP: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}

#[cfg(test)]
thread_local! {
    /// A spend ceiling in micro-USD that also prices the served model at one
    /// micro-USD per token, so a money boundary applies at send time without
    /// changing the already admitted decision.
    pub(crate) static TEST_MONEY_CEILING: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Restores every send-admission test hook when dropped, including on a panic.
#[cfg(test)]
pub(crate) struct HookReset;

#[cfg(test)]
impl Drop for HookReset {
    fn drop(&mut self) {
        TEST_TOKEN_CAP.set(u64::MAX);
        TEST_MONEY_CEILING.set(None);
    }
}

/// Live remaining G6 tokens of one turn. Shared by send and dispatch admission.
pub(crate) fn live_g6_tokens(context: &TurnDecisionContext) -> u64 {
    context.remaining_g6_tokens().min(send_token_cap())
}

#[cfg(test)]
fn send_token_cap() -> u64 {
    TEST_TOKEN_CAP.get()
}

#[cfg(not(test))]
const fn send_token_cap() -> u64 {
    u64::MAX
}

/// The called-tool budget for one model turn: the live G6 tokens and the
/// candidate estimator over the pinned route's model and pricing.
pub(crate) fn call_token_budget(
    service: &TurnDecisionService,
    context: &Arc<TurnDecisionContext>,
    proof: &dyn TurnSourceProof,
) -> Result<CallTokenBudget> {
    // Fail closed: if the route budget cannot be read, the model turn fails
    // here instead of dispatching tool calls without a token gate.
    let limits = service
        .send_limits(proof)
        .context("called-tool budget cannot be established")?;
    let context = Arc::clone(context);
    Ok(CallTokenBudget::new(
        move || live_g6_tokens(&context),
        move |schema_bytes| {
            crate::cost::estimate::estimate_cost(&limits.model, &limits.cost, schema_bytes)
                .map(|estimate| u64::from(estimate.tokens))
        },
    ))
}

/// Refuses the send unless it fits G6 and the remaining spend. Nothing is sent
/// or debited here; decision-attempt token reservation and settlement are
/// unchanged.
///
/// Tokens reproduce the G6 predicate for the answer this round can produce:
/// its estimate (`cost::estimate::candidate_tokens(0)`) must be at most the
/// live remaining decision-slice tokens. A published tool does not veto the
/// answer; the called tool's own estimate is the dispatch predicate (R2a-2).
/// The request size and the response reserve feed only the priced estimate, so
/// an unknown price, measurement or ledger also refuses.
pub(crate) fn admit_provider_send(
    service: &TurnDecisionService,
    context: &TurnDecisionContext,
    proof: &dyn TurnSourceProof,
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
    dialect: ToolSchemaDialect,
) -> Result<()> {
    let tools = tools.unwrap_or_default();
    let remaining = live_g6_tokens(context);
    let answer = crate::cost::estimate::candidate_tokens(0);
    anyhow::ensure!(
        answer <= remaining,
        "provider send refused: answer estimates {answer} tokens but {remaining} remain"
    );
    let limits = service
        .send_limits(proof)
        .context("provider send budget cannot be established")?;
    #[cfg(test)]
    let (cost, ceiling) = TEST_MONEY_CEILING.get().map_or_else(
        || (limits.cost.clone(), limits.remaining_micro_usd),
        |ceiling| {
            let mut cost = limits.cost.clone();
            cost.prices.insert(
                format!("{}/{}", limits.model.provider_label(), limits.model.model()),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 1.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                },
            );
            (cost, limits.remaining_micro_usd.min(ceiling))
        },
    );
    #[cfg(not(test))]
    let (cost, ceiling) = (&limits.cost, limits.remaining_micro_usd);
    let payload = measure_native_tool_payload_for(tools, dialect);
    let request = estimate_request_tokens(estimate_history_tokens_with_source(messages), payload.measurement);
    let estimate = crate::cost::estimate::estimate_request_cost(
        &limits.model,
        &cost,
        u64::try_from(request).unwrap_or(u64::MAX),
        u64::try_from(limits.response_reserve).unwrap_or(u64::MAX),
    )
    .context("provider send refused: the request cost cannot be estimated")?;
    anyhow::ensure!(
        estimate.micro_usd <= ceiling,
        "provider send refused: the estimated cost of {} micro-USD exceeds the {} remaining",
        estimate.micro_usd,
        ceiling
    );
    Ok(())
}
