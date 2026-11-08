//! Budget admission for one provider round, decided before either transport.
use crate::agent::context_budget::{estimate_history_tokens_with_source, estimate_request_tokens};
use crate::agent::turn_context::measure_native_tool_payload_for;
use crate::providers::ChatMessage;
use crate::runtime::turn_decision::{TurnDecisionContext, TurnDecisionService};
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::{ToolSchemaDialect, ToolSpec};
use anyhow::{Context, Result};

/// Refuses the send unless it fits G6 and the remaining spend. Nothing is sent
/// or debited here; decision-attempt token reservation and settlement are
/// unchanged.
///
/// Tokens reproduce the G6 predicate: the answer estimate and every published
/// tool's action estimate (`cost::estimate::candidate_tokens`) must each be at
/// most the remaining decision-slice tokens. The request size and the response
/// reserve feed only the priced estimate, so an unknown price, measurement or
/// ledger also refuses.
pub(crate) fn admit_provider_send(
    service: &TurnDecisionService,
    context: &TurnDecisionContext,
    proof: &dyn TurnSourceProof,
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
    dialect: ToolSchemaDialect,
) -> Result<()> {
    let tools = tools.unwrap_or_default();
    let remaining = context.remaining_g6_tokens();
    for tokens in std::iter::once(crate::cost::estimate::candidate_tokens(0)).chain(tools.iter().map(|spec| {
        // Serialization failure saturates, so it can only over-refuse.
        crate::cost::estimate::candidate_tokens(
            serde_json::to_vec(&spec.parameters).map_or(usize::MAX, |encoded| encoded.len()),
        )
    })) {
        anyhow::ensure!(
            tokens <= remaining,
            "provider send refused: an action estimates {tokens} tokens but {remaining} remain"
        );
    }
    let limits = service
        .send_limits(proof)
        .context("provider send budget cannot be established")?;
    let payload = measure_native_tool_payload_for(tools, dialect);
    let request = estimate_request_tokens(estimate_history_tokens_with_source(messages), payload.measurement);
    let estimate = crate::cost::estimate::estimate_request_cost(
        &limits.model,
        &limits.cost,
        u64::try_from(request).unwrap_or(u64::MAX),
        u64::try_from(limits.response_reserve).unwrap_or(u64::MAX),
    )
    .context("provider send refused: the request cost cannot be estimated")?;
    anyhow::ensure!(
        estimate.micro_usd <= limits.remaining_micro_usd,
        "provider send refused: the estimated cost of {} micro-USD exceeds the {} remaining",
        estimate.micro_usd,
        limits.remaining_micro_usd
    );
    Ok(())
}
