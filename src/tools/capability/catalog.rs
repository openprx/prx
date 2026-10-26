pub(crate) mod process_snapshot;

use super::fingerprint::tool_spec_fingerprint;
use crate::acl::approval_grant::RiskLevel;
use crate::capability::{CapabilityAvailability, CapabilityAvailabilityLevel};
use crate::causal_tree::branch::{
    ApprovalRequirement, CapabilityId, CapabilityRef, CapabilityTarget, CatalogGeneration, OpId, SchemaFingerprint,
    SideEffectClass,
};
use crate::decision::key::DecisionKey;
use crate::skills::SkillMetadata;
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
            ExecutionTarget::Tool(name) => crate::causal_tree::capability_policy::plan_intercepts_tool(name),
            ExecutionTarget::Skill { root, .. } | ExecutionTarget::Node { root, .. } => {
                crate::causal_tree::capability_policy::plan_intercepts_tool(root)
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
    model_root_slots: BTreeMap<String, Vec<usize>>,
    model_layout: [u8; 32],
    generation: CatalogGeneration,
    search_index: super::search::SearchIndex,
}

pub(crate) struct CatalogView<'a> {
    surface: Option<&'a SurfacePolicy>,
    #[cfg(test)]
    entries: OnceLock<Vec<&'a CapabilityEntry>>,
    generation: CatalogGeneration,
    by_id: &'a BTreeMap<CapabilityId, usize>,
    pub(super) model_names: &'a BTreeMap<String, usize>,
    pub(super) model_root_slots: &'a BTreeMap<String, Vec<usize>>,
    pub(super) model_layout: [u8; 32],
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
        let mut model_root_slots: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut layout = ring::digest::Context::new(&ring::digest::SHA256);
        layout.update(b"model-admission-slot-layout-v1");
        for (index, entry) in entries.iter_mut().enumerate() {
            entry.generation = generation;
            by_id.insert(entry.id.clone(), index);
            by_name.insert(entry.public_name.clone(), index);
            model_root_slots.entry(entry.root.clone()).or_default().push(index);
            for name in [&entry.public_name, &entry.root] {
                layout.update(&(name.len() as u64).to_be_bytes());
                layout.update(name.as_bytes());
            }
            if entry.binding_digest.is_some() {
                binding_roots.insert(entry.root.clone());
            }
        }
        let search_index = super::search::SearchIndex::build(&entries);
        let mut model_layout = [0_u8; 32];
        model_layout.copy_from_slice(layout.finish().as_ref());
        Self {
            entries,
            by_id,
            by_name,
            binding_roots,
            model_root_slots,
            model_layout,
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
            model_names: &self.by_name,
            model_root_slots: &self.model_root_slots,
            model_layout: self.model_layout,
            all_entries: &self.entries,
            families: CapabilityTarget::ALL.map(|family| scope.families.is_empty() || scope.families.contains(&family)),
            channel: scope.channel,
            #[cfg(test)]
            entries: OnceLock::new(),
            generation: self.generation,
        }
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
        self.check_published_parity(exec, &manifest.allowed)
    }

    /// Verify an authenticated publication's exact references against the local
    /// executor. Authority and visibility are enforced by the owning service.
    pub(crate) fn check_published_parity(
        &self,
        exec: &ToolExecutionService,
        allowed: &std::collections::BTreeSet<crate::causal_tree::branch::CapabilityRef>,
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
    pub(crate) const fn model_admission_layout(&self) -> &[u8; 32] {
        &self.model_layout
    }

    pub(crate) fn model_admission_entry<'b>(
        &'b self,
        entry: &'b CapabilityEntry,
    ) -> crate::causal_tree::state::model_admission::ModelAdmissionEntry<'b> {
        crate::causal_tree::state::model_admission::ModelAdmissionEntry {
            public_name: &entry.public_name,
            root: &entry.root,
            generation: self.generation().0,
            layout: &self.model_layout,
            public_slot: self.model_names.get(entry.public_name.as_str()).copied(),
            root_slot: self.model_names.get(entry.root.as_str()).copied(),
        }
    }

    /// Narrow a view without copying entries, schemas, bindings or the search index.
    #[cfg(not(test))]
    pub(crate) const fn with_surface(mut self, surface: &'a SurfacePolicy) -> Self {
        self.surface = Some(surface);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_surface(mut self, surface: &'a SurfacePolicy) -> Self {
        self.surface = Some(surface);
        #[cfg(test)]
        {
            self.entries = OnceLock::new();
        }
        self
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
    let id = CapabilityId::new(family, &namespace, &spec.name).map_err(|_| CatalogError::Identity)?;
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

fn family_for(descriptor: &ToolDescriptor) -> CapabilityTarget {
    match descriptor.adapter {
        ToolAdapterKind::McpAlias => CapabilityTarget::Mcp,
        ToolAdapterKind::WasmPlugin => CapabilityTarget::Wasm,
        ToolAdapterKind::Skill => CapabilityTarget::Skill,
        ToolAdapterKind::Native => {
            if descriptor.public_name == "document_search" {
                CapabilityTarget::Document
            } else if descriptor.public_name == "sessions_history" {
                CapabilityTarget::Session
            } else if descriptor.categories.contains(&ToolCategory::FileSystem) {
                CapabilityTarget::Filesystem
            } else if descriptor.categories.contains(&ToolCategory::Memory) {
                CapabilityTarget::Memory
            } else if descriptor.categories.contains(&ToolCategory::WebBrowsing) {
                CapabilityTarget::Web
            } else if descriptor.categories.contains(&ToolCategory::Communication) {
                CapabilityTarget::Im
            } else if descriptor.public_name == "shell" {
                CapabilityTarget::Shell
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
            "shell" | "git_operations" => SideEffectClass::ProcessExec,
            "message_send" | "http_request" => SideEffectClass::ExternalSend,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::capability::search::CatalogSearch;
    use crate::tools::traits::ToolResult;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ProbeTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            self.description
        }

        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"query":{"type":"string"}}})
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    fn registry() -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(ProbeTool {
                name: "prx_search",
                description: "Search local evidence",
            }),
            Box::new(ProbeTool {
                name: "prx_read",
                description: "Read local evidence",
            }),
        ]
    }

    // Public aliases need not export their backend identity as another capability.
    struct AliasOnlyParityTool {
        backend: &'static str,
        state: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for AliasOnlyParityTool {
        fn name(&self) -> &str {
            self.backend
        }

        fn description(&self) -> &str {
            "PRX alias parity fixture"
        }

        fn parameters_schema(&self) -> Value {
            if self.state.load(Ordering::SeqCst) == 1 {
                json!({"type":"object","properties":{"value":{"type":"string"}}})
            } else {
                json!({"type":"object","properties":{}})
            }
        }

        fn specs(&self) -> Vec<ToolSpec> {
            if self.state.load(Ordering::SeqCst) == 2 {
                return Vec::new();
            }
            vec![ToolSpec {
                name: "prx_exact_alias".into(),
                description: self.description().into(),
                parameters: self.parameters_schema(),
            }]
        }

        fn supports_name(&self, name: &str) -> bool {
            name == "prx_exact_alias" && self.state.load(Ordering::SeqCst) != 2
        }

        fn availability(&self) -> CapabilityAvailability {
            if self.state.load(Ordering::SeqCst) == 3 {
                CapabilityAvailability {
                    level: CapabilityAvailabilityLevel::Declared,
                    reason: "PRX fixture is no longer configured".into(),
                }
            } else {
                CapabilityAvailability::ready("PRX fixture configured")
            }
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            anyhow::bail!("parity fixture does not execute effects")
        }
    }

    #[test]
    fn alias_only_tool_parity_preserves_exact_identity_and_live_contract() {
        let state = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(AliasOnlyParityTool {
            backend: "prx_alias_backend",
            state: Arc::clone(&state),
        })];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let entry = catalog.entry_for_public_name("prx_exact_alias").unwrap();
        let reference = CapabilityRef {
            id: entry.id.clone(),
            generation: entry.generation,
            fingerprint: entry.fingerprint,
        };
        let allowed = BTreeSet::from([reference.clone()]);
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.entry_for_public_name("prx_alias_backend").is_none());
        assert!(service.current_descriptor("prx_alias_backend").is_none());
        assert!(catalog.check_parity(&service).is_ok());
        assert!(catalog.check_published_parity(&service, &allowed).is_ok());

        let wrong_backend = ToolExecutionService::with_security_policy(
            vec![Box::new(AliasOnlyParityTool {
                backend: "prx_other_backend",
                state: Arc::clone(&state),
            })],
            Arc::new(crate::security::SecurityPolicy::default()),
        );
        assert!(catalog.check_parity(&wrong_backend).is_err());
        assert!(catalog.check_published_parity(&wrong_backend, &allowed).is_err());
        let mut stale = reference.clone();
        stale.generation = CatalogGeneration([0; 32]);
        assert_ne!(stale.generation, reference.generation);
        assert!(
            catalog
                .check_published_parity(&service, &BTreeSet::from([stale]))
                .is_err()
        );
        let mut changed = reference;
        changed.fingerprint = SchemaFingerprint([0; 32]);
        assert_ne!(changed.fingerprint, entry.fingerprint);
        assert!(
            catalog
                .check_published_parity(&service, &BTreeSet::from([changed]))
                .is_err()
        );
        // Schema drift, disappearance and loss of configured availability each
        // independently invalidate the original immutable publication.
        for mode in [1, 2, 3] {
            state.store(mode, Ordering::SeqCst);
            assert!(catalog.check_parity(&service).is_err());
            assert!(catalog.check_published_parity(&service, &allowed).is_err());
        }
    }

    struct BlankReasonAvailabilityTool;

    #[async_trait]
    impl Tool for BlankReasonAvailabilityTool {
        fn name(&self) -> &str {
            "prx_blank_reason"
        }

        fn description(&self) -> &str {
            "PRX availability compatibility fixture"
        }

        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }

        fn availability(&self) -> CapabilityAvailability {
            CapabilityAvailability {
                level: CapabilityAvailabilityLevel::Declared,
                reason: String::new(),
            }
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn parity_preserves_legacy_empty_availability_reason_promotion() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(BlankReasonAvailabilityTool)];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert_eq!(
            service
                .current_descriptor("prx_blank_reason")
                .unwrap()
                .availability
                .level,
            CapabilityAvailabilityLevel::Ready
        );
        assert!(catalog.check_parity(&service).is_ok());
    }

    struct BorrowedSpecTool {
        spec: ToolSpec,
        owned_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for BorrowedSpecTool {
        fn name(&self) -> &str {
            &self.spec.name
        }

        fn description(&self) -> &str {
            &self.spec.description
        }

        fn parameters_schema(&self) -> Value {
            self.spec.parameters.clone()
        }

        fn catalog_specs(&self) -> Vec<ToolSpec> {
            self.owned_calls.fetch_add(1, Ordering::SeqCst);
            vec![self.spec.clone()]
        }

        fn immutable_catalog_specs(&self) -> Option<&[ToolSpec]> {
            Some(std::slice::from_ref(&self.spec))
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            anyhow::bail!("borrowed-spec fixture must not execute")
        }
    }

    #[test]
    fn parity_borrows_immutable_specs_without_rebuilding_owned_inventory() {
        let owned_calls = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(BorrowedSpecTool {
            spec: ToolSpec {
                name: "prx_borrowed".to_string(),
                description: "PRX borrowed spec fixture".to_string(),
                parameters: json!({"type":"object","properties":{}}),
            },
            owned_calls: Arc::clone(&owned_calls),
        })];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        let before = owned_calls.load(Ordering::SeqCst);
        assert!(catalog.check_parity(&service).is_ok());
        assert_eq!(owned_calls.load(Ordering::SeqCst), before);
    }

    #[test]
    fn catalog_fingerprint_equals_compiled_tool_context_per_spec_sha() {
        let tools = registry();
        let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let compiled =
            crate::agent::turn_context::CompiledToolContext::new(true, tools.iter().map(|tool| tool.spec()).collect());
        for entry in catalog.view(&CatalogScope::default()).entries() {
            let compiled_spec = compiled
                .fingerprint()
                .tools
                .iter()
                .find(|spec| spec.name == entry.public_name)
                .unwrap();
            assert_eq!(hex::encode(entry.fingerprint.0), compiled_spec.sha256);
        }
    }

    /// The canonical ids that `required-tools` takes are the ones the catalog
    /// builds: the shell tool is a file-system tool first, and a discovered MCP
    /// tool is filed under its server with its alias as the public name.
    #[test]
    fn canonical_ids_of_the_shell_and_a_discovered_mcp_tool_are_the_documented_ones() {
        use crate::skills::lifecycle::{Bundle, BundleEntry, validate_manifest_strict};
        let workspace = tempfile::tempdir().unwrap();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::shell::ShellTool::new(
            Arc::new(crate::runtime::NativeRuntime::new()),
            workspace.path().to_path_buf(),
        ))];
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "notes".into(),
            tool: "search".into(),
            state_digest: [0; 32],
            spec: ToolSpec {
                name: "mcp__notes__search".into(),
                description: "search notes".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let catalog = CapabilityCatalog::build(&tools, &sources, &SurfacePolicy::default()).unwrap();
        let shell = catalog.entry_for_public_name("shell").unwrap().id.as_str().to_owned();
        let mcp = catalog
            .entry_for_public_name("mcp__notes__search")
            .unwrap()
            .id
            .as_str()
            .to_owned();
        assert_eq!(shell, "filesystem:native:shell");
        assert_eq!(mcp, "mcp:notes:mcp__notes__search");
        // Exactly these strings are accepted by the lifecycle manifest check.
        let skill = format!("---\nname: ids\ndescription: ids\nrequired-tools: [{shell}, {mcp}]\n---\n\nBody.\n");
        let bundle = Bundle::from_entries(vec![BundleEntry {
            path: std::path::PathBuf::from("SKILL.md"),
            bytes: skill.into_bytes(),
            executable: false,
        }])
        .unwrap();
        let info = validate_manifest_strict(&bundle).unwrap();
        assert_eq!(info.required_tools, vec![shell, mcp]);
    }

    /// Every id the real catalog builds, for the built-in tools and for both MCP
    /// forms, passes the strict write-time shape check of `required-tools`.
    #[test]
    fn every_id_the_catalog_builds_passes_the_required_tools_shape_check() {
        use crate::skills::lifecycle::{Bundle, BundleEntry, validate_manifest_strict};
        let workspace = tempfile::tempdir().unwrap();
        let runtime: Arc<dyn crate::runtime::RuntimeAdapter> = Arc::new(crate::runtime::NativeRuntime::new());
        let security = Arc::new(crate::security::SecurityPolicy::default());
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(crate::tools::shell::ShellTool::new(
                runtime,
                workspace.path().to_path_buf(),
            )),
            Box::new(crate::tools::file_read::FileReadTool::new(security, false)),
        ];
        let mut sources = CatalogSources::default();
        for server in ["notes", "my__srv"] {
            sources.mcp.push(McpCapabilitySpec {
                server: server.into(),
                tool: "search".into(),
                state_digest: [0; 32],
                spec: ToolSpec {
                    name: format!("mcp__{server}__search"),
                    description: "search".into(),
                    parameters: json!({"type":"object","properties":{}}),
                },
            });
        }
        let catalog = CapabilityCatalog::build(&tools, &sources, &SurfacePolicy::default()).unwrap();
        // An alias that was not discovered is filed under the namespace `mcp`.
        let alias = CapabilityId::new(CapabilityTarget::Mcp, "mcp", "mcp__late__search").unwrap();
        let mut ids: Vec<String> = catalog
            .view(&CatalogScope::default())
            .iter_entries()
            .map(|entry| entry.id.as_str().to_owned())
            .collect();
        ids.push(alias.as_str().to_owned());
        assert!(ids.iter().any(|id| id == "filesystem:native:shell"), "{ids:?}");
        assert!(ids.iter().any(|id| id == "filesystem:native:file_read"), "{ids:?}");
        assert!(ids.iter().any(|id| id == "mcp:my__srv:mcp__my__srv__search"), "{ids:?}");
        for id in &ids {
            let skill = format!("---\nname: ids\ndescription: ids\nrequired-tools: [{id}]\n---\n\nBody.\n");
            let bundle = Bundle::from_entries(vec![BundleEntry {
                path: std::path::PathBuf::from("SKILL.md"),
                bytes: skill.into_bytes(),
                executable: false,
            }])
            .unwrap();
            let info = validate_manifest_strict(&bundle).unwrap_or_else(|e| panic!("{id} must pass: {e}"));
            assert_eq!(info.required_tools, vec![id.clone()]);
        }
    }

    #[test]
    fn catalog_rejects_duplicate_public_names_before_execution_deduplicates() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ProbeTool {
                name: "prx_search",
                description: "first",
            }),
            Box::new(ProbeTool {
                name: "prx_search",
                description: "second",
            }),
        ];
        assert!(matches!(
            CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()),
            Err(CatalogError::Duplicate)
        ));
    }

    #[test]
    fn exact_name_precedes_ranked_search_and_exclusions() {
        let mut surface = SurfacePolicy::default();
        surface.excluded_names.insert("prx_read".to_string());
        let catalog = CapabilityCatalog::build(&registry(), &CatalogSources::default(), &surface).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let hit = CatalogSearch::find(&view, "prx_search", &[], 5);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit.first().unwrap().entry.public_name, "prx_search");
        assert!(
            CatalogSearch::find(&view, "prx_read", &[], 5)
                .iter()
                .all(|hit| hit.entry.public_name != "prx_read")
        );
        assert!(CatalogSearch::find(&view, "*", &[], 5).is_empty());
    }

    #[test]
    fn unspaced_script_query_matches_substring_of_longer_summary() {
        // Neutral code points exercise the same bigram branch as CJK without
        // embedding non-English prose in source or test fixtures.
        assert!(crate::tools::capability::search::character_is_unspaced_script(
            char::from_u32(0x4e00).unwrap()
        ));
        let description: String = (0x0e01..=0x0e08).filter_map(char::from_u32).collect();
        let query: String = description.chars().skip(2).take(3).collect();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool {
            name: "prx_remote",
            description: Box::leak(description.into_boxed_str()),
        })];
        let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let hits = CatalogSearch::find(&view, &query, &[], 5);
        assert_eq!(
            hits.first().map(|hit| hit.entry.public_name.as_str()),
            Some("prx_remote")
        );
    }

    #[test]
    fn excluding_dispatch_roots_hides_synthetic_children() {
        let temp = tempfile::tempdir().unwrap();
        let mut sources = CatalogSources::default();
        sources.node_key = Some(DecisionKey::load_or_create(temp.path()).unwrap());
        sources.skills.push(SkillMetadata {
            name: "prx_skill".into(),
            description: "Skill".into(),
            location: temp.path().join("SKILL.md"),
            frontmatter_sha: [1; 32],
        });
        sources.nodes.push(NodeSource {
            id: "prx_node".into(),
            availability: CapabilityAvailability::configured("configured"),
            binding_digest: [2; 32],
        });
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "read".into(),
            state_digest: [3; 32],
            spec: ToolSpec {
                name: "mcp__prx_server__read".into(),
                description: "MCP".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let mut surface = SurfacePolicy::default();
        surface
            .excluded_names
            .extend(["skill_read", "nodes", "mcp_call"].map(str::to_string));
        let catalog = CapabilityCatalog::build(&[], &sources, &surface).unwrap();
        assert!(catalog.view(&CatalogScope::default()).entries().is_empty());
        for entry in &catalog.entries {
            let reference = CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: *catalog.generation(),
            };
            assert!(matches!(
                catalog.resolve(&[reference]),
                Err(StaleCapability::Unavailable)
            ));
        }
    }

    #[test]
    fn changed_schema_invalidates_pinned_reference() {
        let original =
            CapabilityCatalog::build(&registry(), &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let entry = original
            .view(&CatalogScope::default())
            .entries()
            .first()
            .copied()
            .unwrap();
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: *original.generation(),
        };
        let changed: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool {
            name: "prx_search",
            description: "changed schema description",
        })];
        let changed =
            CapabilityCatalog::build(&changed, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        assert!(matches!(changed.resolve(&[reference]), Err(StaleCapability::Stale)));
    }

    #[test]
    fn unchanged_mcp_schema_with_changed_destination_invalidates_generation() {
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "read".into(),
            state_digest: [1; 32],
            spec: ToolSpec {
                name: "mcp__prx_server__read".into(),
                description: "Read remote data".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let first = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        let entry = *first.view(&CatalogScope::default()).entries().first().unwrap();
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: *first.generation(),
        };
        sources.mcp.first_mut().unwrap().state_digest = [2; 32];
        let changed = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        assert_ne!(first.generation(), changed.generation());
        assert!(matches!(changed.resolve(&[reference]), Err(StaleCapability::Stale)));
    }

    #[test]
    fn node_uses_keyed_canonical_identity_and_requires_key() {
        let mut sources = CatalogSources::default();
        sources.nodes.push(NodeSource {
            id: "Box:1".into(),
            availability: CapabilityAvailability::configured("configured"),
            binding_digest: [3; 32],
        });
        assert!(matches!(
            CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()),
            Err(CatalogError::Identity)
        ));
        let temp = tempfile::tempdir().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        let expected = CapabilityId::remote_node(&key, "Box:1").unwrap();
        sources.node_key = Some(key);
        let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        let entry = catalog.entries.first().unwrap();
        assert_eq!(entry.id, expected);
        assert_ne!(entry.public_name, "Box:1");
    }

    #[test]
    fn surface_policy_changes_invalidate_refs() {
        let tools = registry();
        let first = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let entry = *first.view(&CatalogScope::default()).entries().first().unwrap();
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: *first.generation(),
        };
        let mut changed_surface = SurfacePolicy::default();
        changed_surface.channel_allowed = true;
        let changed = CapabilityCatalog::build(&tools, &CatalogSources::default(), &changed_surface).unwrap();
        assert!(matches!(changed.resolve(&[reference]), Err(StaleCapability::Stale)));
    }

    #[test]
    fn high_risk_root_actions_are_never_local_write() {
        for name in ["nodes", "mcp_call", "skill_execute", "sessions_spawn", "delegate"] {
            let tools: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool {
                name,
                description: "Act",
            })];
            let catalog =
                CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
            let entry = catalog.entries.first().unwrap();
            assert!(entry.effect >= SideEffectClass::ProcessExec, "{name}");
            assert!(matches!(entry.approval, ApprovalRequirement::Required { .. }), "{name}");
            assert!(entry.op_ids.is_empty(), "{name} has call-scoped operations");
        }
    }

    struct DynamicAliasTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for DynamicAliasTool {
        fn name(&self) -> &str {
            "mcp_call"
        }
        fn description(&self) -> &str {
            "Dynamic tool"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        fn specs(&self) -> Vec<ToolSpec> {
            vec![self.spec()]
        }
        fn catalog_specs(&self) -> Vec<ToolSpec> {
            let mut specs = self.specs();
            if self.0.load(Ordering::SeqCst) != 0 {
                specs.push(ToolSpec {
                    name: "prx_collision".into(),
                    description: "new alias".into(),
                    parameters: self.parameters_schema(),
                });
            }
            specs
        }
        fn spec_for_name(&self, name: &str) -> Option<ToolSpec> {
            self.catalog_specs().into_iter().find(|spec| spec.name == name)
        }
        fn supports_name(&self, name: &str) -> bool {
            self.spec_for_name(name).is_some()
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn late_alias_collision_does_not_take_native_startup_owner_and_fails_parity() {
        let state = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(DynamicAliasTool(Arc::clone(&state))),
            Box::new(ProbeTool {
                name: "prx_collision",
                description: "native owner",
            }),
        ];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert_eq!(
            service.current_descriptor("prx_collision").unwrap().backend_name,
            "prx_collision"
        );
        assert!(catalog.check_parity(&service).is_ok());
        state.store(1, Ordering::SeqCst);
        assert_eq!(
            service.current_descriptor("prx_collision").unwrap().backend_name,
            "prx_collision"
        );
        assert!(catalog.check_parity(&service).is_err());
    }

    struct MutableBindingTool(Arc<AtomicUsize>, [u8; 32]);

    #[async_trait]
    impl Tool for MutableBindingTool {
        fn name(&self) -> &str {
            "skill_read"
        }
        fn description(&self) -> &str {
            "Read a selected skill"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})
        }
        fn catalog_binding_snapshot(&self) -> BTreeMap<String, [u8; 32]> {
            if self.0.load(Ordering::SeqCst) == 0 {
                BTreeMap::from([("prx_skill".into(), self.1)])
            } else {
                BTreeMap::new()
            }
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn synthetic_skill_parity_fails_when_exact_target_disappears() {
        let temp = tempfile::tempdir().unwrap();
        let skill = SkillMetadata {
            name: "prx_skill".into(),
            description: "Read it".into(),
            location: temp.path().join("SKILL.md"),
            frontmatter_sha: [4; 32],
        };
        let digest = crate::skills::catalog_skill_binding_digest(&skill);
        let mut sources = CatalogSources::default();
        sources.skills.push(skill);
        let state = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(MutableBindingTool(Arc::clone(&state), digest))];
        let catalog = CapabilityCatalog::build(&registry, &sources, &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.check_parity(&service).is_ok());
        state.store(1, Ordering::SeqCst);
        assert!(catalog.check_parity(&service).is_err());
    }

    struct CountingBindingTool {
        calls: Arc<AtomicUsize>,
        bindings: BTreeMap<String, [u8; 32]>,
    }

    #[async_trait]
    impl Tool for CountingBindingTool {
        fn name(&self) -> &str {
            "skill_read"
        }
        fn description(&self) -> &str {
            "Read selected skill"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})
        }
        fn catalog_binding_snapshot(&self) -> BTreeMap<String, [u8; 32]> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.bindings.clone()
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn parity_reads_one_binding_snapshot_for_many_skills() {
        let temp = tempfile::tempdir().unwrap();
        let mut sources = CatalogSources::default();
        let mut bindings = BTreeMap::new();
        for index in 0..32 {
            let skill = SkillMetadata {
                name: format!("prx_skill_{index}"),
                description: "Skill".into(),
                location: temp.path().join(format!("skill_{index}/SKILL.md")),
                frontmatter_sha: [index; 32],
            };
            bindings.insert(skill.name.clone(), crate::skills::catalog_skill_binding_digest(&skill));
            sources.skills.push(skill);
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(CountingBindingTool {
            calls: Arc::clone(&calls),
            bindings,
        })];
        let catalog = CapabilityCatalog::build(&registry, &sources, &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.check_parity(&service).is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct MutableMcpDestinationTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for MutableMcpDestinationTool {
        fn name(&self) -> &str {
            "mcp_call"
        }
        fn description(&self) -> &str {
            "MCP root"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        fn catalog_specs(&self) -> Vec<ToolSpec> {
            vec![
                self.spec(),
                ToolSpec {
                    name: "mcp__prx_server__read".into(),
                    description: "Read remote data".into(),
                    parameters: self.parameters_schema(),
                },
            ]
        }
        fn catalog_binding_snapshot(&self) -> BTreeMap<String, [u8; 32]> {
            let byte = u8::try_from(self.0.load(Ordering::SeqCst)).unwrap_or_default() + 1;
            BTreeMap::from([("mcp__prx_server__read".into(), [byte; 32])])
        }
        fn supports_name(&self, name: &str) -> bool {
            name == self.name() || name == "mcp__prx_server__read"
        }
        fn spec_for_name(&self, name: &str) -> Option<ToolSpec> {
            self.catalog_specs().into_iter().find(|spec| spec.name == name)
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn same_schema_mcp_destination_change_fails_live_parity() {
        let state = Arc::new(AtomicUsize::new(0));
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "read".into(),
            state_digest: [1; 32],
            spec: ToolSpec {
                name: "mcp__prx_server__read".into(),
                description: "Read remote data".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(MutableMcpDestinationTool(Arc::clone(&state)))];
        let catalog = CapabilityCatalog::build(&registry, &sources, &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.check_parity(&service).is_ok());
        state.store(1, Ordering::SeqCst);
        assert!(catalog.check_parity(&service).is_err());
        sources.mcp.first_mut().unwrap().state_digest = [2; 32];
        let rebuilt = CapabilityCatalog::build(
            &[Box::new(MutableMcpDestinationTool(state))],
            &sources,
            &SurfacePolicy::default(),
        )
        .unwrap();
        assert_ne!(catalog.generation(), rebuilt.generation());
    }

    #[test]
    fn child_parity_rejects_same_schema_mcp_destination_change() {
        use crate::tools::capability::child::{
            AuthorityEnvelope, CapabilityConstraints, ChildCapabilityManifest, VerifiedChildScope,
        };

        let state = Arc::new(AtomicUsize::new(0));
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "read".into(),
            state_digest: [1; 32],
            spec: ToolSpec {
                name: "mcp__prx_server__read".into(),
                description: "Read remote data".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(MutableMcpDestinationTool(Arc::clone(&state)))];
        let catalog = CapabilityCatalog::build(&registry, &sources, &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        let child_reference = catalog
            .entries
            .iter()
            .find(|entry| entry.public_name == "mcp__prx_server__read")
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .expect("MCP alias reference");
        let allowed: BTreeSet<_> = catalog
            .entries
            .iter()
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .collect();
        let workspace = tempfile::tempdir().unwrap();
        let key = DecisionKey::load_or_create(workspace.path()).unwrap();
        let parent = CapabilityConstraints {
            decision_id: crate::causal_tree::ids::DecisionId::legacy(&[7; 32]),
            allowed,
            authority: AuthorityEnvelope {
                owner: "prx_user".into(),
                workspace: "prx_workspace".into(),
                level: crate::security::AutonomyLevel::Supervised,
                max_cost_cents: 100,
                remaining_depth: 2,
                delegation: None,
            },
            catalog_generation: *catalog.generation(),
            key,
        };
        let manifest = ChildCapabilityManifest::derive_strict_subset(&parent, &[child_reference]).unwrap();
        let scope = VerifiedChildScope::new(manifest, &parent).unwrap();

        assert!(catalog.check_child_parity(&service, &scope).is_ok());
        state.store(1, Ordering::SeqCst);
        assert!(catalog.check_child_parity(&service, &scope).is_err());
    }

    struct MutableSchemaTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for MutableSchemaTool {
        fn name(&self) -> &str {
            "prx_mutable"
        }

        fn description(&self) -> &str {
            "Mutable schema test tool"
        }

        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"version":{"const":self.0.load(Ordering::SeqCst)}}})
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn execution_descriptor_rechecks_the_current_schema_after_startup() {
        let version = Arc::new(AtomicUsize::new(1));
        let service = ToolExecutionService::with_security_policy(
            vec![Box::new(MutableSchemaTool(Arc::clone(&version)))],
            Arc::new(crate::security::SecurityPolicy::default()),
        );
        let initial = service.current_descriptor("prx_mutable").unwrap();
        assert_eq!(initial.parameters.pointer("/properties/version/const"), Some(&json!(1)));
        version.store(2, Ordering::SeqCst);
        let current = service.current_descriptor("prx_mutable").unwrap();
        assert_eq!(current.parameters.pointer("/properties/version/const"), Some(&json!(2)));
    }

    #[test]
    fn excluded_executable_schema_change_still_fails_reverse_parity() {
        let version = Arc::new(AtomicUsize::new(1));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(MutableSchemaTool(Arc::clone(&version)))];
        let mut surface = SurfacePolicy::default();
        surface.excluded_names.insert("prx_mutable".into());
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &surface).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.check_parity(&service).is_ok());
        version.store(2, Ordering::SeqCst);
        assert!(catalog.check_parity(&service).is_err());
    }

    #[test]
    fn scoped_view_construction_allocates_nothing_for_any_inventory_size() {
        for size in [0, 1, 64, 256] {
            let sources = CatalogSources {
                mcp: (0..size)
                    .map(|index| McpCapabilitySpec {
                        server: "prx_server".into(),
                        tool: format!("read_{index}"),
                        state_digest: [0; 32],
                        spec: ToolSpec {
                            name: format!("mcp__prx_server__read_{index}"),
                            description: "PRX inventory fixture".into(),
                            parameters: json!({"type":"object","properties":{}}),
                        },
                    })
                    .collect(),
                ..CatalogSources::default()
            };
            let surface = SurfacePolicy {
                allowed_tools: None,
                channel_allowed: true,
                ..SurfacePolicy::default()
            };
            let catalog = CapabilityCatalog::build(&[], &sources, &surface).unwrap();
            for families in [BTreeSet::new(), BTreeSet::from([CapabilityTarget::Mcp])] {
                let scope = CatalogScope {
                    families,
                    channel: true,
                };
                let (view, allocations) = crate::decision::builtin::allocation_counter::count(|| catalog.view(&scope));
                assert_eq!(allocations, 0, "view allocations at inventory size {size}");
                assert_eq!(view.entries().len(), size);
            }
        }
    }

    #[test]
    fn borrowed_visibility_matches_cached_entries_and_all_search_paths() {
        let sources = CatalogSources {
            mcp: vec![McpCapabilitySpec {
                server: "prx_server".into(),
                tool: "read".into(),
                state_digest: [0; 32],
                spec: ToolSpec {
                    name: "mcp__prx_server__read".into(),
                    description: "Search local evidence".into(),
                    parameters: json!({"type":"object","properties":{}}),
                },
            }],
            ..CatalogSources::default()
        };
        let surface = SurfacePolicy {
            allowed_tools: None,
            channel_allowed: true,
            excluded_names: BTreeSet::from(["prx_search".into()]),
            ..SurfacePolicy::default()
        };
        let mut catalog = CapabilityCatalog::build(&registry(), &sources, &surface).unwrap();
        catalog
            .entries
            .iter_mut()
            .find(|entry| entry.public_name == "prx_read")
            .unwrap()
            .channel_eligible = false;
        let scopes = std::iter::once(BTreeSet::new())
            .chain(CapabilityTarget::ALL.map(|family| BTreeSet::from([family])))
            .chain(std::iter::once(BTreeSet::from([
                CapabilityTarget::NativeTool,
                CapabilityTarget::Mcp,
            ])));
        for families in scopes {
            for channel in [false, true] {
                let scope = CatalogScope {
                    families: families.clone(),
                    channel,
                };
                let expected: Vec<_> = catalog
                    .entries
                    .iter()
                    .filter(|entry| {
                        (families.is_empty() || families.contains(&entry.family))
                            && (!channel || entry.channel_eligible)
                            && !matches!(entry.exposure, Exposure::Unavailable { .. })
                    })
                    .map(|entry| &entry.id)
                    .collect();
                for cache_first in [false, true] {
                    let view = catalog.view(&scope);
                    assert_eq!(view.iter_entries().map(|entry| &entry.id).collect::<Vec<_>>(), expected);
                    assert!(
                        view.entries.get().is_none(),
                        "borrowing iteration must not initialize compatibility cache"
                    );
                    if cache_first {
                        assert_eq!(
                            view.entries().iter().map(|entry| &entry.id).collect::<Vec<_>>(),
                            expected
                        );
                    }
                    for entry in &catalog.entries {
                        let visible = expected.contains(&&entry.id);
                        assert_eq!(view.contains(&entry.id), visible);
                        assert!(
                            std::ptr::eq(view.entry_any(&entry.id).unwrap(), entry),
                            "unfiltered gate lookup"
                        );
                        for query in std::iter::once(entry.public_name.as_str())
                            .chain(std::iter::once(entry.id.as_str()))
                            .chain(entry.aliases.iter().map(String::as_str))
                        {
                            for query_families in [Vec::new(), vec![entry.family], vec![CapabilityTarget::None]] {
                                let hits = CatalogSearch::find(&view, query, &query_families, 16);
                                let allowed =
                                    visible && (query_families.is_empty() || query_families.contains(&entry.family));
                                assert_eq!(
                                    hits.iter().any(|hit| hit.entry.id == entry.id),
                                    allowed,
                                    "exact search visibility for {query}"
                                );
                                assert!(hits.iter().all(|hit| expected.contains(&&hit.entry.id)));
                            }
                        }
                    }
                    let ranked = CatalogSearch::find(&view, "local evidence", &[], 16);
                    assert_eq!(
                        ranked.iter().map(|hit| &hit.entry.id).collect::<BTreeSet<_>>(),
                        expected.iter().copied().collect()
                    );
                    assert_eq!(
                        view.entries.get().is_some(),
                        cache_first,
                        "search must not initialize compatibility cache"
                    );
                    assert_eq!(
                        view.entries().iter().map(|entry| &entry.id).collect::<Vec<_>>(),
                        expected
                    );
                    assert_eq!(view.iter_entries().map(|entry| &entry.id).collect::<Vec<_>>(), expected);
                }
            }
        }
    }

    #[test]
    fn thousands_of_mcp_aliases_stay_bounded_and_hidden_metadata_never_matches() {
        let mut sources = CatalogSources::default();
        sources.mcp = (0..4096)
            .map(|index| McpCapabilitySpec {
                server: "prx_server".to_string(),
                tool: format!("tool_{index}"),
                state_digest: [0; 32],
                spec: ToolSpec {
                    name: format!("mcp__prx_server__tool_{index}"),
                    description: if index == 4095 {
                        "\u{200b}last tool\nsecret=PRX-FAKE-SENTINEL".to_string()
                    } else {
                        "searchable remote tool".to_string()
                    },
                    parameters: json!({"type":"object","properties":{}}),
                },
            })
            .collect();
        let mut surface = SurfacePolicy::default();
        surface.excluded_names.insert("mcp__prx_server__tool_4095".to_string());
        let catalog = CapabilityCatalog::build(&[], &sources, &surface).unwrap();
        let view = catalog.view(&CatalogScope::default());
        assert_eq!(view.entries().len(), 4095);
        let hits = CatalogSearch::find(&view, "searchable remote tool", &[], 5);
        assert_eq!(hits.len(), 5);
        assert!(
            hits.iter()
                .all(|hit| hit.entry.public_name != "mcp__prx_server__tool_4095")
        );
        assert!(
            CatalogSearch::find(&view, "tool_4095", &[], 5)
                .iter()
                .all(|hit| hit.entry.public_name != "mcp__prx_server__tool_4095")
        );
        assert!(
            catalog
                .entries
                .iter()
                .all(|entry| entry.summary.len() <= 160 * 4 && !entry.summary.contains('\n'))
        );
    }
}
