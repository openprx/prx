//! Terminal silence evidence: confirms the exposed silence dependency before
//! the turn's execution trace is sealed (R25 owner).
use anyhow::Context;

use crate::runtime::turn_decision::{TurnDecisionContext, TurnDecisionService};
use crate::runtime::turn_source::TurnSourceProof;

/// Confirm only the actual exposed runtime dependency, before sealing its
/// decision-owned trace. This does not mint successful tool-call evidence.
pub(crate) fn record_agent_silence(
    service: &TurnDecisionService,
    proof: &dyn TurnSourceProof,
    context: &TurnDecisionContext,
    compiled: &crate::agent::turn_context::CompiledToolContext,
) -> anyhow::Result<()> {
    let name = crate::tools::STAY_SILENT_TOOL_NAME;
    anyhow::ensure!(compiled.allows(name), "silence dependency was not exposed");
    let current = service.agent_runtime_dependency_spec(proof, context, name)?;
    let offered = compiled
        .tool_specs()
        .iter()
        .find(|spec| spec.name == name)
        .context("silence dependency schema was not offered")?;
    anyhow::ensure!(
        crate::tools::capability::fingerprint::tool_spec_fingerprint(
            &offered.name,
            &offered.description,
            &offered.parameters
        ) == crate::tools::capability::fingerprint::tool_spec_fingerprint(
            &current.name,
            &current.description,
            &current.parameters
        ),
        "silence dependency changed after provider request"
    );
    context
        .execution_trace
        .get()
        .context("silence has no actual execution trace")?
        .record_authorized_silence()
}
