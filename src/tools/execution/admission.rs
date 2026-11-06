//! Decision-free publication and dispatch boundary for model turns.
use super::{ToolDescriptor, ToolExecutionContext};
use crate::causal_tree::branch::SchemaFingerprint;
use crate::config::ToolTieringConfig;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnSurface {
    published: BTreeMap<String, SchemaFingerprint>,
    exclusions: BTreeSet<String>,
    model_allowlist: Option<BTreeSet<String>>,
    channel_eligible: BTreeSet<String>,
}

impl TurnSurface {
    /// The caller supplies trusted ACL and channel eligibility from its pinned
    /// registry. Neither a decision context nor a model-provided name grants access.
    pub(crate) fn build(
        descriptors: &[ToolDescriptor],
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
            published: BTreeMap::new(),
            exclusions,
            model_allowlist,
            channel_eligible: BTreeSet::new(),
        };
        for descriptor in descriptors {
            let name = &descriptor.public_name;
            if surface.exclusions.contains(name)
                || surface.exclusions.contains(&descriptor.backend_name)
                || !crate::tools::capability::policy::model_allows_tool_name(model, name, &config.model_allowlists)
                || !eligible(descriptor)
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

pub(super) fn admit(context: &ToolExecutionContext, name: &str) -> Result<(), String> {
    if context
        .turn_surface
        .as_ref()
        .is_some_and(|surface| !surface.contains(name))
    {
        return Err(format!("tool '{name}' is outside the turn surface"));
    }
    Ok(())
}
