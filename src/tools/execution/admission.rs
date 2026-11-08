//! Decision-free publication and dispatch boundary for model turns.
use super::{ToolDescriptor, ToolExecutionContext, ToolExecutionService, ToolExecutionStatus};
use crate::config::ToolTieringConfig;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Live G6 tokens and the candidate estimator for the called tools of one
/// model turn. The turn engine attaches it from the pinned route and the
/// decision context. Dispatch needs neither bindings nor a decision context.
#[derive(Clone)]
pub(crate) struct CallTokenBudget {
    remaining: Arc<dyn Fn() -> u64 + Send + Sync>,
    estimate: Arc<dyn Fn(usize) -> Option<u64> + Send + Sync>,
}

impl CallTokenBudget {
    pub(crate) fn new(
        remaining: impl Fn() -> u64 + Send + Sync + 'static,
        estimate: impl Fn(usize) -> Option<u64> + Send + Sync + 'static,
    ) -> Self {
        Self {
            remaining: Arc::new(remaining),
            estimate: Arc::new(estimate),
        }
    }
}

impl std::fmt::Debug for CallTokenBudget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CallTokenBudget")
    }
}

impl PartialEq for CallTokenBudget {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.remaining, &other.remaining) && Arc::ptr_eq(&self.estimate, &other.estimate)
    }
}

impl Eq for CallTokenBudget {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnSurface {
    registry_generation: Option<[u8; 32]>,
    published: BTreeSet<String>,
    exclusions: BTreeSet<String>,
    model_allowlist: Option<BTreeSet<String>>,
}

impl TurnSurface {
    /// Applies the surface predicates over an explicit candidate list. The
    /// caller supplies every name the publication source can emit, each already
    /// resolved live through `service`; unresolved names must not be passed.
    pub(crate) fn build_from(
        service: &ToolExecutionService,
        candidates: Vec<ToolDescriptor>,
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
            registry_generation: live_generation(service),
            published: BTreeSet::new(),
            exclusions,
            model_allowlist,
        };
        if surface.registry_generation.is_none() {
            // Fail closed: without a generation no later call can be proven current.
            tracing::warn!("tool registry generation unavailable; publishing no tools for this turn");
            return surface;
        }
        for descriptor in candidates {
            let name = &descriptor.public_name;
            if surface.exclusions.contains(name)
                || surface.exclusions.contains(&descriptor.backend_name)
                || !crate::tools::capability::policy::model_allows_tool_name(model, name, &config.model_allowlists)
                || !eligible(&descriptor)
            {
                continue;
            }
            surface.published.insert(name.clone());
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
        surface.published.retain(|name| names.contains(name.as_str()));
        surface
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.published.contains(name)
            && !self.exclusions.contains(name)
            && self.model_allowlist.as_ref().is_none_or(|names| names.contains(name))
    }
}

// Bind the complete live registry, including dynamic destination/epoch digests.
// It is recomputed on every dispatch on purpose: MCP, skill and other backends
// own their state and mutate it without a shared epoch, so a cached value could
// not observe their changes. One pass over the inventory, streaming each entry
// into a single hasher, keeps this O(N). Entries are sorted so backend order
// does not matter. Each entry is length-framed because JSON entries are variable.
fn registry_generation(service: &ToolExecutionService) -> Result<[u8; 32], serde_json::Error> {
    let mut entries = service.live_descriptors_with_bindings();
    entries.sort_by(|(a, _), (b, _)| {
        a.public_name
            .cmp(&b.public_name)
            .then_with(|| a.backend_name.cmp(&b.backend_name))
    });
    let mut digest = Sha256::new();
    let mut buffer = Vec::new();
    for (descriptor, binding) in &entries {
        buffer.clear();
        serde_json::to_writer(&mut buffer, &(descriptor, binding))?;
        digest.update((buffer.len() as u64).to_be_bytes());
        digest.update(&buffer);
    }
    Ok(digest.finalize().into())
}

/// Fail closed: a generation that cannot be computed is treated as no generation.
fn live_generation(service: &ToolExecutionService) -> Option<[u8; 32]> {
    registry_generation(service)
        .inspect_err(|error| tracing::warn!(%error, "tool registry generation unavailable"))
        .ok()
}

fn outside_surface(name: &str) -> (ToolExecutionStatus, String) {
    (
        ToolExecutionStatus::Denied,
        format!("tool '{name}' is outside the turn surface"),
    )
}

/// Runs after `resolve` has bound a backend to `name`. `admit` lets a name
/// through when it is unpublished and has no live descriptor, so that the
/// registry's unknown-tool diagnostic is reported. A backend can register the
/// name between those two steps, so the surface membership is checked again
/// here, before any backend is reached.
pub(super) fn recheck_resolved(
    context: &ToolExecutionContext,
    name: &str,
) -> Result<(), (ToolExecutionStatus, String)> {
    match &context.turn_surface {
        Some(surface) if !surface.contains(name) => Err(outside_surface(name)),
        _ => Ok(()),
    }
}

/// Mandatory incoming-child admission, independent of bindings and surface.
/// Child provenance without a verified scope, or a scope without its pin, is
/// stale. Authority is never inferred from absence. `last` is the pre-invocation
/// recheck, where a vanished descriptor is also stale instead of deferred to the
/// registry's unknown-tool diagnostic.
pub(super) fn admit_child(
    service: &ToolExecutionService,
    context: &ToolExecutionContext,
    name: &str,
    last: bool,
) -> Result<(), (ToolExecutionStatus, String)> {
    let stale = || {
        (
            ToolExecutionStatus::StaleCapability,
            format!("tool '{name}' lacks current child authority"),
        )
    };
    let (scope, pin) = match (&context.child_scope, &context.child_pin) {
        (None, None) => return Ok(()),
        (Some(scope), Some(pin)) => (scope, pin),
        _ => return Err(stale()),
    };
    // The owner must be the envelope's own, never a principal derived from channel or sender.
    if context.envelope.workspace_id != pin.workspace
        || context.envelope.owner_id.as_deref() != Some(pin.owner.as_str())
    {
        return Err(stale());
    }
    scope
        .validate_runtime(&pin.key, &pin.owner, &pin.workspace, *pin.catalog.generation())
        .map_err(|_| stale())?;
    let Some(descriptor) = service.current_descriptor(name) else {
        return if last { Err(stale()) } else { Ok(()) };
    };
    let entry = pin
        .catalog
        .entry_for_public_name(&descriptor.public_name)
        .filter(|entry| {
            entry.root == descriptor.backend_name
                && crate::tools::capability::fingerprint::tool_spec_fingerprint(
                    &descriptor.public_name,
                    &descriptor.description,
                    &descriptor.parameters,
                ) == entry.fingerprint
        })
        .ok_or_else(stale)?;
    let reference = crate::causal_tree::branch::CapabilityRef {
        id: entry.id.clone(),
        fingerprint: entry.fingerprint,
        generation: entry.generation,
    };
    if !scope.gate_inputs().0.allowed.contains(&reference) {
        return Err(stale());
    }
    pin.catalog.check_child_parity(service, scope).map_err(|_| stale())?;
    // Both launchers need live depth and a verified outgoing authority; the
    // launcher bodies repeat their own checks, but none may be reached without them.
    if matches!(name, "delegate" | "sessions_spawn") {
        let depth = scope.gate_inputs().0.authority.remaining_depth;
        if depth == 0 || (context.child_grant.is_none() && context.ordinary_child_publication.is_none()) {
            return Err((
                ToolExecutionStatus::Denied,
                format!("tool '{name}' has no spawn depth or parent authority for this child"),
            ));
        }
    }
    Ok(())
}

/// Combines the parent policy decision with the child manifest autonomy by
/// Deny > Ask > Allow. A child can only tighten the parent decision.
pub(super) fn child_autonomy(
    context: &ToolExecutionContext,
    descriptor: &ToolDescriptor,
    decision: super::ToolExecutionDecision,
) -> super::ToolExecutionDecision {
    use super::ToolExecutionDecision::{Allow, Ask, Deny};
    use crate::security::policy::AutonomyLevel;
    let Some(scope) = &context.child_scope else {
        return decision;
    };
    let name = if descriptor.adapter == super::ToolAdapterKind::Skill {
        &descriptor.backend_name
    } else {
        &descriptor.public_name
    };
    let child = if crate::security::policy::is_read_only_tool(name) {
        Allow
    } else {
        match scope.gate_inputs().0.authority.level {
            AutonomyLevel::Full => Allow,
            AutonomyLevel::Supervised => Ask,
            AutonomyLevel::ReadOnly => Deny,
        }
    };
    match (decision, child) {
        (Deny, _) | (_, Deny) => Deny,
        (Ask, _) | (_, Ask) => Ask,
        _ => Allow,
    }
}

/// The called tool's own G6 action estimate against the live remaining tokens,
/// with the same `<=` rule as the candidate predicate. It runs before the
/// no-surface return and again before invocation, and needs no bindings or
/// decision context. Discovery is a zero-token action. A missing estimate
/// refuses. Grouped native-file sums stay with the decision gate: the group is
/// a directive-era concept, and card 03 retires that predicate with directives.
/// Refuse-kind directives never reach dispatch, so they need no exemption here.
pub(super) fn admit_called_tokens(
    service: &ToolExecutionService,
    context: &ToolExecutionContext,
    name: &str,
) -> Result<(), (ToolExecutionStatus, String)> {
    let Some(budget) = &context.call_budget else {
        return Ok(());
    };
    // A vanished descriptor is reported by resolution or by the stale recheck.
    let Some(descriptor) = service.current_descriptor(name) else {
        return Ok(());
    };
    let tokens = if name == "capability_search" {
        0
    } else {
        let schema_bytes = serde_json::to_vec(&descriptor.parameters).map_or(usize::MAX, |value| value.len());
        (budget.estimate)(schema_bytes).ok_or_else(|| {
            (
                ToolExecutionStatus::Denied,
                format!("tool '{name}' has no current token estimate"),
            )
        })?
    };
    let remaining = (budget.remaining)();
    if tokens > remaining {
        return Err((
            ToolExecutionStatus::Denied,
            format!("tool '{name}' estimates {tokens} tokens but {remaining} remain"),
        ));
    }
    Ok(())
}

pub(super) fn admit(
    service: &ToolExecutionService,
    context: &ToolExecutionContext,
    name: &str,
) -> Result<(), (ToolExecutionStatus, String)> {
    admit_child(service, context, name, false)?;
    admit_called_tokens(service, context, name)?;
    let Some(surface) = &context.turn_surface else {
        return Ok(());
    };
    if !surface.contains(name) {
        // A name that is not in the live registry and was never published has
        // no capability to protect. Let resolution report the registry's own
        // unknown-tool diagnostic. A backend that registers the name between
        // this check and resolution is caught by `recheck_resolved`, which
        // runs after resolution and before any backend is reached.
        if !surface.published.contains(name) && service.current_descriptor(name).is_none() {
            return Ok(());
        }
        return Err(outside_surface(name));
    }
    let descriptor = service.current_descriptor(name);
    // The registry generation below binds every descriptor field, so an
    // unchanged generation already proves the published schema is current.
    let stale = descriptor.as_ref().is_none_or(|descriptor| {
        descriptor.availability.level < crate::capability::CapabilityAvailabilityLevel::Configured
    });
    if stale || surface.registry_generation.is_none() || surface.registry_generation != live_generation(service) {
        return Err((
            ToolExecutionStatus::StaleCapability,
            format!("tool '{name}' changed after turn publication"),
        ));
    }
    Ok(())
}
