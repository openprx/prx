

use super::fingerprint::tool_spec_fingerprint;
use crate::acl::approval_grant::RiskLevel;
use crate::capability::{CapabilityAvailability, CapabilityAvailabilityLevel};
use crate::security::runtime_key::DecisionKey;
use crate::skills::SkillMetadata;
use crate::tools::capability::contract::{
    ApprovalRequirement, CapabilityId, CapabilityRef, CapabilityTarget, CatalogGeneration, OpId, SchemaFingerprint,
    SideEffectClass,
};
use crate::tools::execution::{ToolAdapterKind, ToolCatalog, ToolDescriptor, ToolEffect, ToolExecutionService};
use crate::tools::mcp::McpCapabilitySpec;
use crate::tools::traits::{Tool, ToolCategory, ToolSpec};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
#[cfg(test)]
use std::sync::OnceLock;

/// Model exposure is explicit and never inferred from existence in config.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Exposure {
    Direct,
    Searchable,
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum ExecutionTarget {
    Tool(String),
    Skill { name: String, root: String },
    Node { id: String, root: String },
    Hook,
}

#[derive(Debug, Clone)]
pub(crate) struct CapabilityEntry {
    pub id: CapabilityId,
    pub public_name: String,
    pub root: String,
    pub family: CapabilityTarget,
    pub namespace: String,
    pub summary: String,
    pub aliases: Vec<String>,
    pub categories: Vec<ToolCategory>,
    /// Lowercased ID, public name and aliases, frozen after all source
    /// metadata is attached and before the entry is published.
    pub exact_mention_terms: Vec<String>,
    pub effect: SideEffectClass,
    pub risk: RiskLevel,
    pub op_ids: Vec<OpId>,
    pub approval: ApprovalRequirement,
    pub channel_eligible: bool,
    pub schema: Arc<Value>,
    /// Serialized size of the pinned schema, computed with the same JSON
    /// encoder used by turn cost estimates.
    pub schema_bytes: usize,
    pub fingerprint: SchemaFingerprint,
    pub generation: CatalogGeneration,
    pub target: ExecutionTarget,
    pub binding_digest: Option<[u8; 32]>,
    pub skill_metadata: Option<Arc<SkillMetadata>>,
    pub exposure: Exposure,
    pub availability: CapabilityAvailability,
    pub process_worker_compatible: bool,
}

impl CapabilityEntry {
    /// A concrete read-only Skill uses this native loader. Keep registry target
    /// representation inside the catalog; decision ranking only needs the fact.
    pub(crate) fn loads_read_only_skill(&self, skill: &Self) -> bool {
        self.effect == SideEffectClass::ReadOnly
            && skill.effect == SideEffectClass::ReadOnly
            && matches!((&self.target, &skill.target),
                (ExecutionTarget::Tool(name), ExecutionTarget::Skill { root, .. }) if name == root)
    }

    /// Native tools whose approval operation derives completely from the
    /// proposed arguments. Collection records one exact call before review;
    /// it never executes. MCP, plugin and skill names cannot opt into this path.
    pub(crate) fn collects_exact_call(&self) -> bool {
        self.root == self.public_name
            && matches!(&self.target, ExecutionTarget::Tool(name) if name == &self.public_name)
            && match self.public_name.as_str() {
                "skills_manage" => self.family == CapabilityTarget::NativeTool,
                "shell" => matches!(self.family, CapabilityTarget::Filesystem | CapabilityTarget::Shell),
                "http_request" => self.family == CapabilityTarget::Web,
                _ => false,
            }
    }

    /// Lifecycle argument collection can follow an unnamed metadata search.
    /// Direct publication must not hide that root from discovery. This offers
    /// metadata only; collection and execution retain their own approval gates.
    pub(crate) fn supports_metadata_discovery(&self) -> bool {
        matches!(self.exposure, Exposure::Searchable)
            || (matches!(self.exposure, Exposure::Direct)
                && self.public_name == "skills_manage"
                && self.collects_exact_call())
    }

    /// The (lowercased) request names a grant-bound action of a collectable
    /// tool. Other actions need no approval collection: the model may call them
    /// in an ordinary turn, and collecting them would end in an error.
    pub(crate) fn names_collectable_action(&self, folded_text: &str) -> bool {
        self.collects_exact_call()
            && (self.public_name != "skills_manage"
                || crate::security::approval_operation::SKILLS_MANAGE_GRANT_BOUND_ACTIONS
                    .iter()
                    .any(|action| folded_text.contains(action)))
    }

    /// Name understood by the execution policy. Synthetic Skill and Node
    /// entries dispatch through their registered root and inherit that root's
    /// read/write classification; public names remain available for search,
    /// exclusion and exact binding.
    pub(crate) fn policy_tool_name(&self) -> &str {
        match &self.target {
            ExecutionTarget::Skill { root, .. } | ExecutionTarget::Node { root, .. } => root,
            ExecutionTarget::Tool(_) | ExecutionTarget::Hook => &self.public_name,
        }
    }

    /// Project the actual execution target into the domain's Plan policy.
    /// Aliases/effect labels never substitute for the registered root.
    pub(crate) fn plan_intercepted(&self) -> bool {
        match &self.target {
            ExecutionTarget::Tool(name) => crate::tools::capability::policy::plan_intercepts_tool(name),
            ExecutionTarget::Skill { root, .. } | ExecutionTarget::Node { root, .. } => {
                crate::tools::capability::policy::plan_intercepts_tool(root)
            }
            ExecutionTarget::Hook => true,
        }
    }
}

/// Inputs outside the finalized executable registry. Callers take these as
/// read-only snapshots of the same runtime generation.
#[derive(Debug, Clone, Default)]
pub(crate) struct CatalogSources {
    pub config_generation_id: String,
    pub mcp_state_epoch: u64,
    pub wasm_generation: u64,
    pub skills_dir_epoch: u64,
    pub mcp: Vec<McpCapabilitySpec>,
    pub skills: Vec<SkillMetadata>,
    pub hooks: Vec<HookSource>,
    pub nodes: Vec<NodeSource>,
    pub node_key: Option<Arc<DecisionKey>>,
}

#[derive(Debug, Clone)]
pub(crate) struct HookSource {
    pub event: String,
    pub action: String,
    pub effect: SideEffectClass,
}

#[derive(Debug, Clone)]
pub(crate) struct NodeSource {
    pub id: String,
    pub availability: CapabilityAvailability,
    pub binding_digest: [u8; 32],
}

impl CatalogSources {
    /// Snapshot auxiliary metadata without network probes, skill-body
    /// hydration or decision-key access. The turn service pins the finalized
    /// registry and installs its generation-local key before catalog build.
    pub(crate) fn from_runtime_inputs(
        config_generation_id: String,
        config: &crate::config::Config,
        mcp: Vec<McpCapabilitySpec>,
        hooks: &crate::hooks::HookDiagnostics,
        wasm_generation: u64,
    ) -> Self {
        let skills = crate::skills::catalog_skill_metadata(&config.workspace_dir, config);
        let mut mcp_hasher = Sha256::new();
        let mut mcp_ordered: Vec<_> = mcp.iter().collect();
        mcp_ordered.sort_by(|left, right| (&left.server, &left.tool).cmp(&(&right.server, &right.tool)));
        for item in mcp_ordered {
            mcp_hasher.update(item.server.len().to_be_bytes());
            mcp_hasher.update(item.server.as_bytes());
            mcp_hasher.update(item.state_digest);
        }
        let mcp_digest = mcp_hasher.finalize();
        let mut mcp_epoch_bytes = [0_u8; 8];
        mcp_epoch_bytes.copy_from_slice(mcp_digest.get(..8).unwrap_or_default());
        let mut skill_hasher = Sha256::new();
        for skill in &skills {
            skill_hasher.update(skill.name.len().to_be_bytes());
            skill_hasher.update(skill.name.as_bytes());
            skill_hasher.update(skill.frontmatter_sha);
            skill_hasher.update(skill.location.to_string_lossy().as_bytes());
        }
        let skill_digest = skill_hasher.finalize();
        let mut epoch_bytes = [0_u8; 8];
        epoch_bytes.copy_from_slice(skill_digest.get(..8).unwrap_or_default());
        let skills_dir_epoch = u64::from_be_bytes(epoch_bytes);
        let hooks = hooks
            .events
            .iter()
            .flat_map(|(event, count)| {
                (0..*count).map(move |index| HookSource {
                    event: event.clone(),
                    action: format!(
                        "action_{index}_{}",
                        hex::encode(hooks.catalog_digest.unwrap_or_default().get(..8).unwrap_or_default())
                    ),
                    effect: SideEffectClass::ProcessExec,
                })
            })
            .collect();
        let nodes = config
            .nodes
            .nodes
            .iter()
            .map(|node| NodeSource {
                id: node.id.clone(),
                availability: CapabilityAvailability::configured("remote node configured"),
                binding_digest: serde_json::to_vec(node)
                    .map(|bytes| Sha256::digest(bytes).into())
                    .unwrap_or_default(),
            })
            .collect();
        Self {
            config_generation_id,
            mcp_state_epoch: u64::from_be_bytes(mcp_epoch_bytes),
            wasm_generation,
            skills_dir_epoch,
            mcp,
            skills,
            hooks,
            nodes,
            node_key: None,
        }
    }
}

/// The entrypoint's publication boundary, applied after registry discovery.
#[derive(Clone, Default)]
pub(crate) struct SurfacePolicy {
    pub allowed_tools: Option<BTreeSet<String>>,
    pub excluded_names: BTreeSet<String>,
    pub direct_names: BTreeSet<String>,
    pub channel_allowed: bool,
}

impl std::fmt::Debug for SurfacePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut value = f.debug_struct("SurfacePolicy");
        value
            .field("excluded_names", &self.excluded_names)
            .field("direct_names", &self.direct_names)
            .field("channel_allowed", &self.channel_allowed);
        // Preserve the established generation fingerprint for unrestricted surfaces.
        if let Some(allowed) = &self.allowed_tools {
            value.field("allowed_tools", allowed);
        }
        value.finish()
    }
}

impl SurfacePolicy {
    pub(crate) fn permits(&self, name: &str, root: &str, target: &ExecutionTarget) -> bool {
        self.allowed_tools.as_ref().is_none_or(|allowed| {
            allowed.contains(name)
                || matches!(target, ExecutionTarget::Skill { .. } | ExecutionTarget::Node { .. })
                    && allowed.contains(root)
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CatalogError {
    #[error("duplicate capability public name or id")]
    Duplicate,
    #[error("invalid capability identity")]
    Identity,
    #[error("capability schema is invalid")]
    Schema,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum StaleCapability {
    #[error("capability generation or schema changed")]
    Stale,
    #[error("capability is unavailable")]
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
#[error("catalog and execution registry differ")]
pub(crate) struct ParityError;

#[derive(Debug, Clone)]
pub(crate) struct CapabilityCatalog {
    entries: Vec<CapabilityEntry>,
    by_id: BTreeMap<CapabilityId, usize>,
    by_name: BTreeMap<String, usize>,
    binding_roots: BTreeSet<String>,
    generation: CatalogGeneration,
    search_index: super::search::SearchIndex,
}

pub(crate) struct CatalogView<'a> {
    surface: Option<&'a SurfacePolicy>,
    #[cfg(test)]
    entries: OnceLock<Vec<&'a CapabilityEntry>>,
    generation: CatalogGeneration,
    by_id: &'a BTreeMap<CapabilityId, usize>,
    pub(super) search_index: &'a super::search::SearchIndex,
    pub(super) all_entries: &'a [CapabilityEntry],
    families: [bool; CapabilityTarget::ALL.len()],
    channel: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CatalogScope {
    pub families: BTreeSet<CapabilityTarget>,
    pub channel: bool,
}

impl CapabilityCatalog {
    pub(crate) fn build(
        registry: &[Box<dyn Tool>],
        sources: &CatalogSources,
        surface: &SurfacePolicy,
    ) -> Result<Self, CatalogError> {
        let discovered_mcp: BTreeSet<_> = sources.mcp.iter().map(|item| item.spec.name.as_str()).collect();
        let descriptors = ToolCatalog::from_boxed_registry(registry);
        let mut entries = Vec::new();
        let mut seen_names = BTreeSet::new();
        let mut seen_ids = BTreeSet::new();
        let mut raw_names = BTreeSet::new();
        for tool in registry {
            for spec in tool.specs() {
                if !raw_names.insert(spec.name.clone()) {
                    return Err(CatalogError::Duplicate);
                }
            }
        }
        for descriptor in descriptors.descriptors() {
            if descriptor.adapter == ToolAdapterKind::McpAlias
                && discovered_mcp.contains(descriptor.public_name.as_str())
            {
                continue;
            }
            let family = family_for(descriptor);
            let namespace = if descriptor.adapter == ToolAdapterKind::McpAlias {
                "mcp".to_string()
            } else {
                "native".to_string()
            };
            let effect = effect_for(descriptor);
            let entry = entry_from_spec(
                ToolSpec {
                    name: descriptor.public_name.clone(),
                    description: descriptor.description.clone(),
                    parameters: descriptor.parameters.clone(),
                },
                descriptor.backend_name.clone(),
                namespace,
                family,
                descriptor.categories.clone(),
                effect,
                descriptor.availability.clone(),
                descriptor.process_worker_compatible,
                ExecutionTarget::Tool(descriptor.public_name.clone()),
                surface,
            )?;
            insert(entry, &mut entries, &mut seen_names, &mut seen_ids)?;
        }
        for discovered in &sources.mcp {
            let mut entry = entry_from_spec(
                discovered.spec.clone(),
                "mcp_call".to_string(),
                discovered.server.clone(),
                CapabilityTarget::Mcp,
                vec![ToolCategory::Automation],
                SideEffectClass::Destructive,
                CapabilityAvailability::ready("discovered allowed MCP backend"),
                true,
                ExecutionTarget::Tool(discovered.spec.name.clone()),
                surface,
            )?;
            entry.aliases.push(discovered.tool.clone());
            entry.binding_digest = Some(discovered.state_digest);
            let op = OpId::new(&crate::security::op_id::op_id(
                "mcp_call",
                "call",
                &[&discovered.server, &discovered.tool],
            ));
            entry.op_ids = vec![op.clone()];
            entry.approval = ApprovalRequirement::Required { op_ids: vec![op] };
            insert(entry, &mut entries, &mut seen_names, &mut seen_ids)?;
        }
        for skill in &sources.skills {
            let spec = crate::tools::skills::catalog_skill_read_spec(skill);
            let mut entry = entry_from_spec(
                spec,
                "skill_read".to_string(),
                "skill".to_string(),
                CapabilityTarget::Skill,
                vec![ToolCategory::System],
                SideEffectClass::ReadOnly,
                CapabilityAvailability::configured("installed skill metadata"),
                true,
                ExecutionTarget::Skill {
                    name: skill.name.clone(),
                    root: "skill_read".to_string(),
                },
                surface,
            )?;
            entry.binding_digest = Some(crate::skills::catalog_skill_binding_digest(skill));
            entry.skill_metadata = Some(Arc::new(skill.clone()));
            if entry.public_name != skill.name {
                entry.aliases.push(skill.name.clone());
            }
            insert(entry, &mut entries, &mut seen_names, &mut seen_ids)?;
        }
        for hook in &sources.hooks {
            let name = format!("hook_{}_{}", hook.event, hook.action);
            let mut entry = entry_from_spec(
                ToolSpec {
                    name: name.clone(),
                    description: "Registered lifecycle observer".to_string(),
                    parameters: json!({"type":"object","properties":{}}),
                },
                "hooks".to_string(),
                "hook".to_string(),
                CapabilityTarget::Hook,
                vec![ToolCategory::Automation],
                hook.effect,
                CapabilityAvailability::configured("registered lifecycle hook"),
                true,
                ExecutionTarget::Hook,
                surface,
            )?;
            entry.exposure = Exposure::Unavailable {
                reason: "hooks run as observers".to_string(),
            };
            insert(entry, &mut entries, &mut seen_names, &mut seen_ids)?;
        }
        for node in &sources.nodes {
            if node.binding_digest == [0; 32] {
                return Err(CatalogError::Schema);
            }
            let key = sources.node_key.as_ref().ok_or(CatalogError::Identity)?;
            let id = CapabilityId::remote_node(key, &node.id).map_err(|_| CatalogError::Identity)?;
            let name = id
                .as_str()
                .rsplit(':')
                .next()
                .ok_or(CatalogError::Identity)?
                .to_string();
            let mut entry = entry_from_spec(
                ToolSpec {
                    name,
                    description: "Configured remote node shell".to_string(),
                    parameters: json!({"type":"object","properties":{"node":{"const":node.id}}}),
                },
                "nodes".to_string(),
                "node".to_string(),
                CapabilityTarget::Shell,
                vec![ToolCategory::DevOps],
                SideEffectClass::ProcessExec,
                node.availability.clone(),
                true,
                ExecutionTarget::Node {
                    id: node.id.clone(),
                    root: "nodes".to_string(),
                },
                surface,
            )?;
            entry.id = id;
            entry.binding_digest = Some(node.binding_digest);
            // Cancel includes a runtime task ID and cannot be represented by
            // a fixed node-level approval scope here.
            entry.op_ids = ["exec", "write"]
                .into_iter()
                .map(|action| OpId::new(&format!("nodes:{action}:{}", node.id)))
                .collect();
            entry.approval = ApprovalRequirement::Required {
                op_ids: entry.op_ids.clone(),
            };
            insert(entry, &mut entries, &mut seen_names, &mut seen_ids)?;
        }
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        let generation = generation_of(sources, surface, &entries);
        Ok(Self::from_entries(entries, generation))
    }

    fn from_entries(mut entries: Vec<CapabilityEntry>, generation: CatalogGeneration) -> Self {
        let mut by_id = BTreeMap::new();
        let mut by_name = BTreeMap::new();
        let mut binding_roots = BTreeSet::new();
        for (index, entry) in entries.iter_mut().enumerate() {
            entry.generation = generation;
            by_id.insert(entry.id.clone(), index);
            by_name.insert(entry.public_name.clone(), index);
            if entry.binding_digest.is_some() {
                binding_roots.insert(entry.root.clone());
            }
        }
        let search_index = super::search::SearchIndex::build(&entries);
        Self {
            entries,
            by_id,
            by_name,
            binding_roots,
            generation,
            search_index,
        }
    }

    pub(crate) const fn generation(&self) -> &CatalogGeneration {
        &self.generation
    }

    pub(crate) fn view(&self, scope: &CatalogScope) -> CatalogView<'_> {
        CatalogView {
            surface: None,
            search_index: &self.search_index,
            by_id: &self.by_id,
            all_entries: &self.entries,
            families: CapabilityTarget::ALL.map(|family| scope.families.is_empty() || scope.families.contains(&family)),
            channel: scope.channel,
            #[cfg(test)]
            entries: OnceLock::new(),
            generation: self.generation,
        }
    }

    /// Every pinned public name, for surface enumeration. Callers resolve each
    /// name live before treating it as a candidate.
    pub(crate) fn public_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.by_name.keys().map(String::as_str)
    }

    pub(crate) fn entry_for_public_name(&self, name: &str) -> Option<&CapabilityEntry> {
        self.by_name.get(name).and_then(|index| self.entries.get(*index))
    }

    pub(crate) fn resolve<'a>(
        &self,
        refs: impl IntoIterator<Item = &'a CapabilityRef>,
    ) -> Result<Vec<&CapabilityEntry>, StaleCapability> {
        refs.into_iter()
            .map(|reference| {
                let entry = self
                    .by_id
                    .get(&reference.id)
                    .and_then(|index| self.entries.get(*index))
                    .ok_or(StaleCapability::Stale)?;
                if reference.generation != self.generation || reference.fingerprint != entry.fingerprint {
                    return Err(StaleCapability::Stale);
                }
                if matches!(entry.exposure, Exposure::Unavailable { .. }) {
                    return Err(StaleCapability::Unavailable);
                }
                Ok(entry)
            })
            .collect()
    }

    /// Check a child's actual executor against the immutable parent catalog.
    /// Omitted parent capabilities are not part of the child executor; every
    /// granted reference and its root/dispatch binding must remain exact.
    pub(crate) fn check_child_parity(
        &self,
        exec: &ToolExecutionService,
        scope: &super::child::VerifiedChildScope,
    ) -> Result<(), ParityError> {
        let (manifest, parent) = scope.gate_inputs();
        manifest.verify(parent).map_err(|_| ParityError)?;
        if manifest.catalog_generation != self.generation {
            return Err(ParityError);
        }
        // Full children inherit the complete level surface, including parent
        // backends this child executor cannot serve. Every served reference
        // still needs exact descriptor/binding parity; below Full the entire
        // sealed subset must be executable as before.
        if manifest.authority.level == crate::security::AutonomyLevel::Full {
            self.check_published_parity(exec, &self.served_references(exec, &manifest.allowed))
        } else {
            self.check_published_parity(exec, &manifest.allowed)
        }
    }

    /// Whether `exec` registers the backend that runs `entry`. A worker's
    /// catalog is its parent's pinned one and can hold entries its own
    /// executor never registers (interactive-only tools); such an entry is
    /// unserved there, not a parity failure. A registered backend must still
    /// match exactly ([`Self::check_published_parity`]).
    pub(crate) fn served_by(exec: &ToolExecutionService, entry: &CapabilityEntry) -> bool {
        match &entry.target {
            ExecutionTarget::Tool(name) => exec.current_descriptor(name).is_some(),
            ExecutionTarget::Skill { .. } | ExecutionTarget::Node { .. } => {
                exec.current_descriptor(&entry.root).is_some()
            }
            ExecutionTarget::Hook => false,
        }
    }

    /// The references of `allowed` whose backend `exec` registers.
    pub(crate) fn served_references(
        &self,
        exec: &ToolExecutionService,
        allowed: &std::collections::BTreeSet<crate::tools::capability::contract::CapabilityRef>,
    ) -> std::collections::BTreeSet<crate::tools::capability::contract::CapabilityRef> {
        allowed
            .iter()
            .filter(|reference| {
                self.by_id
                    .get(&reference.id)
                    .and_then(|index| self.entries.get(*index))
                    .is_none_or(|entry| Self::served_by(exec, entry))
            })
            .cloned()
            .collect()
    }

    /// Verify an authenticated publication's exact references against the local
    /// executor. Authority and visibility are enforced by the owning service.
    pub(crate) fn check_published_parity(
        &self,
        exec: &ToolExecutionService,
        allowed: &std::collections::BTreeSet<crate::tools::capability::contract::CapabilityRef>,
    ) -> Result<(), ParityError> {
        for reference in allowed {
            let entry = self
                .by_id
                .get(&reference.id)
                .and_then(|index| self.entries.get(*index))
                .ok_or(ParityError)?;
            if reference.generation != self.generation
                || reference.fingerprint != entry.fingerprint
                || matches!(entry.exposure, Exposure::Unavailable { .. })
            {
                return Err(ParityError);
            }
            let current_binding = match &entry.target {
                ExecutionTarget::Tool(name) => {
                    let (exact, binding) = exec
                        .current_descriptor_with_binding(name, entry.binding_digest.is_some())
                        .ok_or(ParityError)?;
                    verify_child_descriptor(entry, &exact)?;
                    binding
                }
                ExecutionTarget::Skill { name, .. } | ExecutionTarget::Node { id: name, .. } => {
                    let root_entry = self.entry_for_public_name(&entry.root).ok_or(ParityError)?;
                    let root = exec.current_descriptor(&entry.root).ok_or(ParityError)?;
                    verify_child_descriptor(root_entry, &root)?;
                    let argument = if matches!(entry.target, ExecutionTarget::Skill { .. }) {
                        "name"
                    } else {
                        "node"
                    };
                    if root.parameters.pointer(&format!("/properties/{argument}")).is_none()
                        || entry.schema.pointer(&format!("/properties/{argument}/const"))
                            != Some(&Value::String(name.clone()))
                    {
                        return Err(ParityError);
                    }
                    exec.current_binding_digest(&entry.root, name)
                }
                ExecutionTarget::Hook => return Err(ParityError),
            };
            if let Some(expected) = entry.binding_digest {
                if current_binding != Some(expected) {
                    return Err(ParityError);
                }
            } else if !matches!(entry.target, ExecutionTarget::Tool(_)) {
                return Err(ParityError);
            }
        }
        Ok(())
    }

    pub(crate) fn check_parity(&self, exec: &ToolExecutionService) -> Result<(), ParityError> {
        let live = exec.current_specs_for_parity();
        let mut live_by_index = vec![None; self.entries.len()];
        let mut bindings = BTreeMap::new();
        for root in &self.binding_roots {
            if !live
                .iter()
                .any(|item| item.backend_name == root.as_str() && item.spec.shared_binding_digest().is_snapshot())
            {
                bindings.insert(root.as_str(), exec.current_binding_snapshot(root).ok_or(ParityError)?);
            }
        }
        for descriptor in &live {
            if !descriptor.configured {
                continue;
            }
            let index = *self.by_name.get(descriptor.spec.name.as_str()).ok_or(ParityError)?;
            let slot = live_by_index.get_mut(index).ok_or(ParityError)?;
            if slot.replace(descriptor).is_some() {
                return Err(ParityError);
            }
            let entry = self.entries.get(index).ok_or(ParityError)?;
            if entry.root != descriptor.backend_name
                || descriptor.spec.fingerprint() != entry.fingerprint
                || entry.binding_digest.is_some()
                    && descriptor.spec.shared_binding_digest().or_else_live(|| {
                        bindings
                            .get(entry.root.as_str())
                            .and_then(|snapshot| snapshot.get(descriptor.spec.name.as_str()))
                            .copied()
                    }) != entry.binding_digest
            {
                return Err(ParityError);
            }
        }
        let live_for = |name: &str| {
            self.by_name
                .get(name)
                .and_then(|index| live_by_index.get(*index))
                .copied()
                .flatten()
                .ok_or(ParityError)
        };
        for entry in &self.entries {
            if matches!(entry.exposure, Exposure::Unavailable { .. }) {
                continue;
            }
            if let ExecutionTarget::Tool(name) = &entry.target {
                let exact = live_for(name)?;
                if exact.backend_name != entry.root || exact.spec.fingerprint() != entry.fingerprint {
                    return Err(ParityError);
                }
            } else {
                let current = live_for(&entry.root)?;
                if !current.configured {
                    return Err(ParityError);
                }
                // A synthetic target must bind the exact dispatch argument
                // accepted by the root contract, not only its root name.
                let binding = match &entry.target {
                    ExecutionTarget::Skill { name, .. } => ("name", name.as_str()),
                    ExecutionTarget::Node { id, .. } => ("node", id.as_str()),
                    ExecutionTarget::Hook | ExecutionTarget::Tool(_) => return Err(ParityError),
                };
                if current
                    .spec
                    .parameters
                    .pointer(&format!("/properties/{}", binding.0))
                    .is_none()
                    || entry.schema.pointer(&format!("/properties/{}/const", binding.0))
                        != Some(&Value::String(binding.1.to_string()))
                    || current.spec.shared_named_binding(binding.1).or_else_live(|| {
                        bindings
                            .get(entry.root.as_str())
                            .and_then(|snapshot| snapshot.get(binding.1))
                            .copied()
                    }) != entry.binding_digest
                {
                    return Err(ParityError);
                }
            }
        }
        Ok(())
    }
}

fn verify_child_descriptor(entry: &CapabilityEntry, current: &ToolDescriptor) -> Result<(), ParityError> {
    let backend_matches = entry.root == current.backend_name;
    if entry.public_name != current.public_name
        || !backend_matches
        || current.availability.level < CapabilityAvailabilityLevel::Configured
        || crate::tools::schema::SchemaCleanr::validate(&current.parameters).is_err()
        || tool_spec_fingerprint(&current.public_name, &current.description, &current.parameters) != entry.fingerprint
    {
        return Err(ParityError);
    }
    Ok(())
}

impl<'a> CatalogView<'a> {
    /// Narrow a view without copying entries, schemas, bindings or the search index.
    pub(crate) const fn with_surface(&self, surface: &'a SurfacePolicy) -> Self {
        Self {
            surface: Some(surface),
            #[cfg(test)]
            entries: OnceLock::new(),
            generation: self.generation,
            by_id: self.by_id,
            search_index: self.search_index,
            all_entries: self.all_entries,
            families: self.families,
            channel: self.channel,
        }
    }

    pub(crate) fn directly_exposes(&self, entry: &CapabilityEntry) -> bool {
        matches!(entry.exposure, Exposure::Direct)
            || self
                .surface
                .is_some_and(|surface| surface.direct_names.contains(&entry.public_name))
    }

    pub(crate) fn surface_excludes(&self, entry: &CapabilityEntry) -> bool {
        self.surface.is_some_and(|surface| {
            !surface.permits(&entry.public_name, &entry.root, &entry.target)
                || surface.excluded_names.contains(&entry.public_name)
                || surface.excluded_names.contains(&entry.root)
        })
    }

    /// Compatibility slice for inspection fixtures. Production decision paths
    /// use the borrowing iterator instead of materializing this slice.
    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[&'a CapabilityEntry] {
        self.entries.get_or_init(|| self.iter_entries().collect())
    }

    pub(crate) fn iter_entries(&self) -> impl Iterator<Item = &'a CapabilityEntry> + '_ {
        self.all_entries.iter().filter(|entry| self.entry_visible(entry))
    }

    fn entry_visible(&self, entry: &CapabilityEntry) -> bool {
        self.families
            .iter()
            .zip(CapabilityTarget::ALL)
            .any(|(allowed, family)| *allowed && family == entry.family)
            && (!self.channel || entry.channel_eligible)
            && !self.surface_excludes(entry)
            && !matches!(entry.exposure, Exposure::Unavailable { .. })
    }

    pub(super) fn is_visible_index(&self, index: usize) -> bool {
        self.all_entries
            .get(index)
            .is_some_and(|entry| self.entry_visible(entry))
    }

    /// Look up a catalog entry before view filtering. Gates need to report
    /// configured exclusions before reporting availability or stale refs.
    pub(crate) fn entry_any(&self, id: &CapabilityId) -> Option<&'a CapabilityEntry> {
        self.by_id.get(id).and_then(|index| self.all_entries.get(*index))
    }

    pub(crate) fn contains(&self, id: &CapabilityId) -> bool {
        self.by_id.get(id).is_some_and(|index| self.is_visible_index(*index))
    }

    pub(crate) const fn generation(&self) -> CatalogGeneration {
        self.generation
    }
}

fn insert(
    mut entry: CapabilityEntry,
    entries: &mut Vec<CapabilityEntry>,
    names: &mut BTreeSet<String>,
    ids: &mut BTreeSet<CapabilityId>,
) -> Result<(), CatalogError> {
    if !names.insert(entry.public_name.clone()) || !ids.insert(entry.id.clone()) {
        return Err(CatalogError::Duplicate);
    }
    entry.exact_mention_terms = std::iter::once(entry.id.as_str())
        .chain(std::iter::once(entry.public_name.as_str()))
        .chain(entry.aliases.iter().map(String::as_str))
        .map(str::to_lowercase)
        .collect();
    entries.push(entry);
    Ok(())
}

// Native identities remain compatible with existing manifests while semantic
// families distinguish document/process work. Creation and authenticated
// process restoration must use this one canonical identity rule.
fn canonical_identity_family(family: CapabilityTarget, namespace: &str, name: &str) -> CapabilityTarget {
    if namespace == "native"
        && matches!(
            (family, name),
            (CapabilityTarget::Shell, "shell") | (CapabilityTarget::Document, "document_ingest" | "document_sync")
        )
    {
        CapabilityTarget::Filesystem
    } else {
        family
    }
}

fn entry_from_spec(
    spec: ToolSpec,
    root: String,
    namespace: String,
    family: CapabilityTarget,
    categories: Vec<ToolCategory>,
    effect: SideEffectClass,
    availability: CapabilityAvailability,
    process_worker_compatible: bool,
    target: ExecutionTarget,
    surface: &SurfacePolicy,
) -> Result<CapabilityEntry, CatalogError> {
    crate::tools::schema::SchemaCleanr::validate(&spec.parameters).map_err(|_| CatalogError::Schema)?;
    let identity_family = canonical_identity_family(family, &namespace, &spec.name);
    let id = CapabilityId::new(identity_family, &namespace, &spec.name).map_err(|_| CatalogError::Identity)?;
    let fingerprint = tool_spec_fingerprint(&spec.name, &spec.description, &spec.parameters);
    let exposure = if !surface.permits(&spec.name, &root, &target)
        || surface.excluded_names.contains(&spec.name)
        || surface.excluded_names.contains(&root)
        || availability.level < CapabilityAvailabilityLevel::Configured
    {
        Exposure::Unavailable {
            reason: "not available on this surface".to_string(),
        }
    } else if surface.direct_names.contains(&spec.name) {
        Exposure::Direct
    } else {
        Exposure::Searchable
    };
    // Most tools derive the exact operation id from runtime arguments (path,
    // destination, task id). Do not advertise a fabricated grant target.
    let op_ids = Vec::new();
    let approval = if effect == SideEffectClass::ReadOnly {
        ApprovalRequirement::None
    } else {
        ApprovalRequirement::Required { op_ids: Vec::new() }
    };
    let schema_bytes = serde_json::to_vec(&spec.parameters).map_or(usize::MAX, |value| value.len());
    Ok(CapabilityEntry {
        id,
        public_name: spec.name,
        root,
        family,
        namespace,
        summary: summary(&spec.description),
        aliases: Vec::new(),
        categories,
        exact_mention_terms: Vec::new(),
        effect,
        risk: risk_for_effect(effect),
        op_ids,
        approval,
        channel_eligible: surface.channel_allowed,
        schema: Arc::new(spec.parameters),
        schema_bytes,
        fingerprint,
        generation: CatalogGeneration([0; 32]),
        target,
        binding_digest: None,
        skill_metadata: None,
        exposure,
        availability,
        process_worker_compatible,
    })
}

pub(super) const fn risk_for_effect(effect: SideEffectClass) -> RiskLevel {
    match effect {
        SideEffectClass::ReadOnly => RiskLevel::Low,
        SideEffectClass::LocalWrite | SideEffectClass::ExternalSend | SideEffectClass::CredentialOrConfig => {
            RiskLevel::Medium
        }
        SideEffectClass::ProcessExec | SideEffectClass::Destructive => RiskLevel::High,
    }
}

fn summary(description: &str) -> String {
    let redacted = crate::security::redaction::redact_secrets(description);
    redacted
        .chars()
        .filter(|ch| !ch.is_control() && !matches!(ch, '\u{200b}'..='\u{200f}' | '\u{2060}' | '\u{feff}'))
        .take(160)
        .collect()
}

/// Canonical identity of a registered descriptor, shared by publication and
/// execution observations. This identifies an observed call, not permission.
pub(crate) fn descriptor_identity(descriptor: &ToolDescriptor) -> Result<CapabilityId, CatalogError> {
    let namespace = if descriptor.adapter == ToolAdapterKind::McpAlias {
        "mcp"
    } else {
        "native"
    };
    let family = canonical_identity_family(family_for(descriptor), namespace, &descriptor.public_name);
    CapabilityId::new(family, namespace, &descriptor.public_name).map_err(|_| CatalogError::Identity)
}

fn family_for(descriptor: &ToolDescriptor) -> CapabilityTarget {
    match descriptor.adapter {
        ToolAdapterKind::McpAlias => CapabilityTarget::Mcp,
        ToolAdapterKind::WasmPlugin => CapabilityTarget::Wasm,
        ToolAdapterKind::Skill => CapabilityTarget::Skill,
        ToolAdapterKind::Native => {
            if matches!(
                descriptor.public_name.as_str(),
                "document_search" | "document_ingest" | "document_sync"
            ) {
                CapabilityTarget::Document
            } else if descriptor.public_name == "sessions_history" {
                CapabilityTarget::Session
            } else if descriptor.public_name == "shell" {
                CapabilityTarget::Shell
            } else if descriptor.categories.contains(&ToolCategory::FileSystem) {
                CapabilityTarget::Filesystem
            } else if descriptor.categories.contains(&ToolCategory::Memory) {
                CapabilityTarget::Memory
            } else if descriptor.categories.contains(&ToolCategory::WebBrowsing) {
                CapabilityTarget::Web
            } else if descriptor.categories.contains(&ToolCategory::Communication) {
                CapabilityTarget::Im
            } else {
                CapabilityTarget::NativeTool
            }
        }
    }
}

fn effect_for(descriptor: &ToolDescriptor) -> SideEffectClass {
    match descriptor.effect {
        ToolEffect::Read => SideEffectClass::ReadOnly,
        ToolEffect::Act => match descriptor.public_name.as_str() {
            "shell" => SideEffectClass::ProcessExec,
            "message_send" | "http_request" => SideEffectClass::ExternalSend,
            "sessions_send" if descriptor.parameters.get("oneOf").is_some() => SideEffectClass::ExternalSend,
            "config_reload" | "proxy_config" => SideEffectClass::CredentialOrConfig,
            "nodes" | "mcp_call" | "skill_execute" => SideEffectClass::Destructive,
            "delegate" | "sessions_spawn" => SideEffectClass::ProcessExec,
            _ => SideEffectClass::LocalWrite,
        },
    }
}

fn generation_of(sources: &CatalogSources, surface: &SurfacePolicy, entries: &[CapabilityEntry]) -> CatalogGeneration {
    let mut hasher = Sha256::new();
    for value in [
        sources.config_generation_id.as_bytes(),
        &sources.mcp_state_epoch.to_be_bytes(),
        &sources.wasm_generation.to_be_bytes(),
        &sources.skills_dir_epoch.to_be_bytes(),
    ] {
        hasher.update(value.len().to_be_bytes());
        hasher.update(value);
    }
    for entry in entries {
        for value in [
            entry.id.as_str().as_bytes(),
            entry.family.as_str().as_bytes(),
            entry.fingerprint.0.as_slice(),
            entry.root.as_bytes(),
            entry.public_name.as_bytes(),
        ] {
            hasher.update(value.len().to_be_bytes());
            hasher.update(value);
        }
        let policy = format!(
            "{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}",
            entry.categories,
            entry.effect,
            entry.risk,
            entry.op_ids,
            entry.approval,
            entry.exposure,
            entry.availability,
            entry.target,
            entry.binding_digest
        );
        hasher.update(policy.len().to_be_bytes());
        hasher.update(policy.as_bytes());
    }
    let mut mcp_ordered: Vec<_> = sources.mcp.iter().collect();
    mcp_ordered.sort_by(|left, right| (&left.server, &left.tool).cmp(&(&right.server, &right.tool)));
    for item in mcp_ordered {
        hasher.update(item.server.len().to_be_bytes());
        hasher.update(item.server.as_bytes());
        hasher.update(item.state_digest);
    }
    let policy = format!("{surface:?}");
    hasher.update(policy.len().to_be_bytes());
    hasher.update(policy.as_bytes());
    CatalogGeneration(hasher.finalize().into())
}
