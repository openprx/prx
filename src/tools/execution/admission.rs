//! Decision-free publication and dispatch boundary for model turns.
use super::{ToolDescriptor, ToolExecutionContext, ToolExecutionService, ToolExecutionStatus};
use crate::causal_tree::branch::SchemaFingerprint;
use crate::config::ToolTieringConfig;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnSurface {
    registry_generation: Option<[u8; 32]>,
    published: BTreeMap<String, SchemaFingerprint>,
    exclusions: BTreeSet<String>,
    model_allowlist: Option<BTreeSet<String>>,
    channel_eligible: BTreeSet<String>,
}

impl TurnSurface {
    /// The caller supplies trusted ACL and channel eligibility from its pinned
    /// registry. Neither a decision context nor a model-provided name grants access.
    pub(crate) fn build(
        service: &ToolExecutionService,
        config: &ToolTieringConfig,
        model: &str,
        channel: bool,
        mut eligible: impl FnMut(&ToolDescriptor) -> bool,
    ) -> Self {
        let effective = if channel {
            config.for_channel_surface()
        } else {
            config.clone()
        };
        let exclusions: BTreeSet<_> = effective.always_exclude.into_iter().collect();
        let model_allowlist = config
            .model_allowlists
            .get(model)
            .map(|names| names.iter().cloned().collect());
        let mut surface = Self {
            registry_generation: registry_generation(service),
            published: BTreeMap::new(),
            exclusions,
            model_allowlist,
            channel_eligible: BTreeSet::new(),
        };
        if surface.registry_generation.is_none() {
            // Fail closed: without a generation no later call can be proven current.
            tracing::warn!("tool registry generation unavailable; publishing no tools for this turn");
            return surface;
        }
        for descriptor in service.current_descriptors() {
            let name = &descriptor.public_name;
            if surface.exclusions.contains(name)
                || surface.exclusions.contains(&descriptor.backend_name)
                || !crate::tools::capability::policy::model_allows_tool_name(model, name, &config.model_allowlists)
                || !eligible(&descriptor)
            {
                continue;
            }
            surface.channel_eligible.insert(name.clone());
            surface.published.insert(
                name.clone(),
                crate::tools::capability::fingerprint::tool_spec_fingerprint(
                    name,
                    &descriptor.description,
                    &descriptor.parameters,
                ),
            );
        }
        surface
    }

    pub(crate) fn with_tool_calls(mut self, allowed: bool) -> Self {
        if !allowed {
            self.published.clear();
        }
        self
    }

    /// Freeze the exact published subset without widening the turn ceiling.
    pub(crate) fn for_publication(&self, specs: &[super::ToolSpec]) -> Self {
        let names: BTreeSet<_> = specs.iter().map(|spec| spec.name.as_str()).collect();
        let mut surface = self.clone();
        surface.published.retain(|name, _| names.contains(name.as_str()));
        surface
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.published.contains_key(name)
            && !self.exclusions.contains(name)
            && self.model_allowlist.as_ref().is_none_or(|names| names.contains(name))
            && self.channel_eligible.contains(name)
    }
}

// Bind the complete live registry, including dynamic destination/epoch digests.
// Sorting makes backend enumeration order irrelevant to the generation.
// `None` means an entry could not be serialized; callers treat that as stale.
fn registry_generation(service: &ToolExecutionService) -> Option<[u8; 32]> {
    let mut entries = service
        .current_descriptors()
        .into_iter()
        .map(|descriptor| {
            let binding = service
                .current_descriptor_with_binding(&descriptor.public_name, true)
                .and_then(|(_, binding)| binding);
            let value = serde_json::to_value((&descriptor, &binding)).ok()?;
            let fingerprint = crate::tools::capability::fingerprint::tool_spec_fingerprint(
                &descriptor.public_name,
                &descriptor.backend_name,
                &value,
            );
            Some(fingerprint.0)
        })
        .collect::<Option<Vec<[u8; 32]>>>()?;
    entries.sort_unstable();
    let mut digest = Sha256::new();
    for entry in entries {
        digest.update(entry);
    }
    Some(digest.finalize().into())
}

pub(super) fn admit(
    service: &ToolExecutionService,
    context: &ToolExecutionContext,
    name: &str,
) -> Result<(), (ToolExecutionStatus, String)> {
    let Some(surface) = &context.turn_surface else {
        return Ok(());
    };
    if !surface.contains(name) {
        return Err((
            ToolExecutionStatus::Denied,
            format!("tool '{name}' is outside the turn surface"),
        ));
    }
    let descriptor = service.current_descriptor(name);
    let stale = descriptor.as_ref().is_none_or(|descriptor| {
        descriptor.availability.level < crate::capability::CapabilityAvailabilityLevel::Configured
            || surface.published.get(name)
                != Some(&crate::tools::capability::fingerprint::tool_spec_fingerprint(
                    name,
                    &descriptor.description,
                    &descriptor.parameters,
                ))
    });
    if stale || surface.registry_generation.is_none() || surface.registry_generation != registry_generation(service) {
        return Err((
            ToolExecutionStatus::StaleCapability,
            format!("tool '{name}' changed after turn publication"),
        ));
    }
    Ok(())
}
