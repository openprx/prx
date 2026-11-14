//! Exact tool schemas a turn may bind for dispatch (R25 execution-spec row).
//!
//! Every function rechecks the live turn through its [`TurnSurfacePort`] and
//! the execution-owned publication rules; none reads an engine decision.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use anyhow::Context as _;

use crate::capability::CapabilityAvailabilityLevel;
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::ToolSpec;
use crate::tools::capability::catalog::{ExecutionTarget, Exposure};
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::capability::search_tool::{capability_search_spec, discovery_entries, prepare_discovered_tool};
use crate::tools::surface_owner::turn_port::{TurnSurfacePort, rules};

pub(crate) struct ConstrainedToolSelection {
    pub(crate) roots: BTreeSet<String>,
    pub(crate) specs: Vec<ToolSpec>,
}

/// The exact schemas of the turn's published surface, after rechecking the
/// turn binding and that the surface is still the one published at opening.
pub(crate) fn execution_specs(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
) -> anyhow::Result<ConstrainedToolSelection> {
    let model = port.validate_turn(proof)?;
    let view = port.catalog_view();
    let rules = rules(port, &view);
    anyhow::ensure!(
        rules.turn_scope_admits(),
        "turn scope no longer passes its execution rules"
    );
    let cost = &port.config().cost;
    let references = rules.surface(port.outgoing_child(), &|bytes| {
        crate::cost::estimate::estimate_cost(&model, cost, bytes)
    });
    anyhow::ensure!(
        references == *port.execution_refs(),
        "published surface changed after admission"
    );
    let references: Vec<_> = references.into_iter().collect();
    let entries = port.catalog().resolve(&references)?;
    let mut roots = BTreeSet::new();
    let mut specs = Vec::with_capacity(entries.len());
    for entry in entries {
        let descriptor = port
            .execution()
            .current_descriptor(&entry.public_name)
            .context("selected executable capability is missing")?;
        let spec = ToolSpec {
            name: descriptor.public_name,
            description: descriptor.description,
            parameters: descriptor.parameters,
        };
        anyhow::ensure!(
            crate::tools::capability::fingerprint::tool_spec_fingerprint(
                &spec.name,
                &spec.description,
                &spec.parameters
            ) == entry.fingerprint,
            "selected executable schema changed"
        );
        roots.insert(entry.root.clone());
        specs.push(spec);
    }
    Ok(ConstrainedToolSelection { roots, specs })
}

/// Only the loop's fixed runtime dependencies can extend a published
/// surface. They still share source, exclusion and availability rules.
pub(crate) fn runtime_dependency_spec(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
    name: &str,
) -> anyhow::Result<ToolSpec> {
    anyhow::ensure!(proof.tool_calls_allowed(), "source configuration forbids tool calls");
    anyhow::ensure!(
        matches!(name, "transcript_history_lookup" | "stay_silent" | "skill_read"),
        "unknown runtime dependency"
    );
    execution_specs(port, proof)?;
    let entry = port
        .catalog()
        .entry_for_public_name(name)
        .context("runtime dependency is absent")?;
    let trusted = port.trusted();
    // Full has no permission limits (R51): manifest membership does not
    // limit a Full child's runtime dependencies; parity and exclusion stay.
    let full = port.facts().policy.autonomy == crate::security::AutonomyLevel::Full;
    if let Some(scope) = proof.child_scope().filter(|_| !full) {
        anyhow::ensure!(
            scope.gate_inputs().0.allowed.contains(&CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            }),
            "runtime dependency is outside child authority"
        );
    }
    let view = port.catalog_view();
    anyhow::ensure!(
        entry.generation == trusted.catalog_generation
            && !view.surface_excludes(entry)
            && !trusted.excludes_entry(view.model_admission_entry(entry))
            && (trusted.channel.is_none() || entry.channel_eligible)
            && crate::tools::execution::decide_tool_execution(
                &port.facts().policy,
                entry.policy_tool_name(),
                proof.policy_sender(),
                proof.policy_channel(),
                proof.policy_chat_type(),
            ) != crate::tools::execution::ToolExecutionDecision::Deny
            && !matches!(entry.exposure, Exposure::Unavailable { .. })
            && entry.availability.level >= CapabilityAvailabilityLevel::Configured,
        "runtime dependency is excluded, unavailable or stale"
    );
    let descriptor = port
        .execution()
        .current_descriptor(name)
        .context("runtime dependency cannot execute")?;
    anyhow::ensure!(
        name != "skill_read"
            || !proof.owner_isolated()
            || crate::tools::execution::admits_owner_isolated_origin(&descriptor),
        "skill_read is unavailable to this owner-isolated source"
    );
    let spec = ToolSpec {
        name: descriptor.public_name,
        description: descriptor.description,
        parameters: descriptor.parameters,
    };
    anyhow::ensure!(
        crate::tools::capability::fingerprint::tool_spec_fingerprint(&spec.name, &spec.description, &spec.parameters)
            == entry.fingerprint,
        "runtime dependency schema changed"
    );
    Ok(spec)
}

/// Installed skill metadata eligible for this source. Advertising metadata
/// is separate from execution and selection receipts; this performs no
/// embedding lookup or skill-body load. The source-bound discovery
/// eligibility decides which skills are actionable on the first request.
pub(crate) fn prompt_skills(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
) -> anyhow::Result<Vec<crate::skills::Skill>> {
    execution_specs(port, proof)?;
    // The published surface's skills come first: the turn admitted their
    // documents itself, so they need no loader (the retired hop's selected
    // refs, R9). Installed metadata that is only discoverable is advertised
    // through the live skill_read loader.
    let mut refs: BTreeSet<CapabilityRef> = port.execution_refs().clone();
    if runtime_dependency_spec(port, proof, "skill_read").is_ok() {
        refs.extend(
            discovery_entries(port, proof)?
                .into_values()
                .filter(|entry| entry.searchable)
                .map(|entry| entry.reference),
        );
    } else if port.catalog_view().iter_entries().any(|entry| {
        entry.skill_metadata.is_some() && !refs.contains(&crate::tools::surface_owner::publication::entry_ref(entry))
    }) {
        tracing::warn!("discoverable skill metadata suppressed because live skill_read is unavailable");
    }
    let refs: Vec<CapabilityRef> = refs.into_iter().collect();
    let entries = port.catalog().resolve(&refs)?;
    let mut skills: Vec<crate::skills::Skill> = Vec::new();
    for entry in entries {
        // Skill adapters (including skill_execute) are execution tools,
        // not installed skill documents. Only concrete skill targets carry
        // prompt metadata and may emit a selection receipt here.
        if !matches!(entry.target, ExecutionTarget::Skill { .. }) {
            continue;
        }
        let metadata = entry
            .skill_metadata
            .as_ref()
            .context("selected skill metadata is unavailable")?;
        anyhow::ensure!(
            entry.binding_digest == Some(crate::skills::catalog_skill_binding_digest(metadata)),
            "selected skill metadata changed"
        );
        skills.push(crate::skills::Skill {
            name: metadata.name.clone(),
            description: metadata.description.clone(),
            location: Some(metadata.location.clone()),
            version: String::new(),
            author: None,
            tags: Vec::new(),
            tools: Vec::new(),
            prompts: Vec::new(),
            embedding: None,
        });
    }
    // Advertising installed metadata is not a skill selection receipt.
    let view = port.catalog_view();
    let names: Vec<&str> = skills
        .iter()
        .filter(|skill| {
            port.execution_refs().iter().any(|reference| {
                view.entry_any(&reference.id)
                    .and_then(|entry| entry.skill_metadata.as_ref())
                    .is_some_and(|metadata| metadata.name == skill.name)
            })
        })
        .map(|skill| skill.name.as_str())
        .collect();
    let config = port.config();
    crate::skills::lifecycle::schedule_selection_receipts(&crate::skills::lifecycle::SelectionRequest {
        workspace_dir: &config.workspace_dir,
        enabled: config.skills_lifecycle.enabled,
        owner_id: proof.owner_id(),
        run_id: proof.run_id(),
        task_id: proof.task_id(),
        skill_names: &names,
    });
    Ok(skills)
}

/// The dispatch-admission half of an execution binding: the turn is open,
/// the source may call tools, the execution scope is the authenticated
/// source's, and every bound schema is a published, discovered or runtime
/// dependency definition that still passes its rules.
pub(crate) fn admit_bound_specs(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
    execution_context: &crate::tools::ToolExecutionContext,
    specs: &[ToolSpec],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !port.execution_closed().load(Ordering::Acquire),
        "decision execution is already closed"
    );
    anyhow::ensure!(
        specs.is_empty() || proof.tool_calls_allowed() && port.model_can_call_tools(proof),
        "source configuration forbids tool calls"
    );
    let selected = execution_specs(port, proof)?;
    let envelope = &execution_context.envelope;
    anyhow::ensure!(
        envelope.owner_isolated == proof.owner_isolated()
            && envelope.workspace_id == proof.workspace_id()
            && envelope.resolved_owner_id() == proof.owner_id()
            && envelope.session_key == proof.session_key()
            && envelope.run_id.as_deref() == Some(proof.run_id())
            && envelope.task_id.as_deref() == proof.task_id()
            && envelope.channel.as_deref() == Some(proof.policy_channel())
            && envelope.sender.as_deref() == Some(proof.policy_sender())
            && execution_context.chat_type == proof.policy_chat_type()
            && envelope.source_message_event_id.as_deref() == proof.source_event_id()
            && envelope.config_generation_id == Some(proof.config_generation_id())
            && envelope.config_source_revision.as_deref() == Some(proof.config_source_revision()),
        "execution scope differs from the authenticated decision source"
    );
    for spec in specs {
        let discovered = port.discovery().admission(&spec.name);
        let expected = if let Some(selected) = selected.specs.iter().find(|item| item.name == spec.name) {
            selected.clone()
        } else if spec.name == "capability_search" {
            capability_search_spec(port, proof)?.context("capability search is not permitted for this turn")?
        } else if let Some(admitted) = &discovered {
            let proposal = prepare_discovered_tool(port, proof, &spec.name)?
                .context("discovery offer missing at execution binding")?;
            let pending = proposal.admission.is_pending();
            anyhow::ensure!(
                proposal.reference == admitted.reference
                    && pending == admitted.requires_approval
                    && (proposal.admission.is_passed() || pending),
                "discovered execution capability failed fresh admission"
            );
            proposal.spec
        } else {
            runtime_dependency_spec(port, proof, &spec.name)?
        };
        anyhow::ensure!(
            spec.name == expected.name
                && spec.description == expected.description
                && spec.parameters == expected.parameters,
            "execution schema differs from the admitted definition"
        );
    }
    Ok(())
}
