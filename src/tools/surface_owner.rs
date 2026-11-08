//! Decision-free model-turn surface construction.
//!
//! Registry ∩ ACL ∩ always_exclude/channel exclusions ∩ model allowlist ∩
//! catalog `channel_eligible`, assembled here so a turn still has a surface
//! when the decision engine is off.
use crate::config::ToolTieringConfig;
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, SurfacePolicy};
use crate::tools::execution::admission::TurnSurface;
use crate::tools::execution::{ToolExecutionDecision, ToolExecutionService, decide_tool_execution};
use std::collections::BTreeSet;

pub(crate) struct SurfaceParts<'a> {
    pub execution: &'a ToolExecutionService,
    pub catalog: &'a CapabilityCatalog,
    pub publication_policy: &'a SurfacePolicy,
    pub tiering: &'a ToolTieringConfig,
    pub security: &'a SecurityPolicy,
    pub pinned_model: &'a str,
    pub channel: bool,
    pub inherited_exclusions: &'a [String],
    pub sender: &'a str,
    pub channel_name: &'a str,
    pub chat_type: &'a str,
    pub tool_calls_allowed: bool,
}

pub(crate) fn build(parts: SurfaceParts<'_>) -> TurnSurface {
    let mut config = parts.tiering.clone();
    config.always_exclude.extend(parts.inherited_exclusions.iter().cloned());
    let view = parts
        .catalog
        .view(&CatalogScope::default())
        .with_surface(parts.publication_policy);
    let mut candidates = parts.execution.current_descriptors();
    let listed: BTreeSet<&str> = candidates
        .iter()
        .map(|descriptor| descriptor.public_name.as_str())
        .collect();
    let unlisted: Vec<_> = parts
        .catalog
        .public_names()
        .filter(|name| !listed.contains(name))
        .filter_map(|name| parts.execution.current_descriptor(name))
        .collect();
    candidates.extend(unlisted);
    TurnSurface::build_from(
        parts.execution,
        candidates,
        &config,
        parts.pinned_model,
        parts.channel,
        |descriptor| {
            parts
                .catalog
                .entry_for_public_name(&descriptor.public_name)
                .is_some_and(|entry| {
                    !view.surface_excludes(entry)
                        && (!parts.channel || entry.channel_eligible)
                        && decide_tool_execution(
                            parts.security,
                            entry.policy_tool_name(),
                            parts.sender,
                            parts.channel_name,
                            parts.chat_type,
                        ) != ToolExecutionDecision::Deny
                })
        },
    )
    .with_tool_calls(parts.tool_calls_allowed)
}
