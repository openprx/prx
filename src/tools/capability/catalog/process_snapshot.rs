//! Authenticated parent catalog transport. Does not grant executor authority.
use super::{
    ApprovalRequirement, Arc, BTreeSet, CapabilityAvailability, CapabilityCatalog, CapabilityEntry, CapabilityId,
    CapabilityTarget, CatalogGeneration, DecisionKey, ExecutionTarget, Exposure, OpId, SchemaFingerprint,
    SideEffectClass, Value, insert, risk_for_effect,
};
use crate::acl::approval_grant::RiskLevel;
use crate::decision::key::KeyDomain;
use crate::tools::traits::ToolCategory;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCatalogSnapshot {
    generation: [u8; 32],
    payload: String,
    seal: [u8; 32],
}

impl std::fmt::Debug for ProcessCatalogSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessCatalogSnapshot").finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    public_name: String,
    root: String,
    family: String,
    namespace: String,
    summary: String,
    aliases: Vec<String>,
    categories: Vec<ToolCategory>,
    effect: String,
    risk: RiskLevel,
    op_ids: Vec<String>,
    // None is no approval; Some(empty) retains required dynamic approval.
    approval_ops: Option<Vec<String>>,
    channel_eligible: bool,
    schema: Value,
    fingerprint: [u8; 32],
    target: ExecutionTarget,
    binding_digest: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skill_metadata: Option<crate::skills::SkillMetadata>,
    exposure: Exposure,
    availability: CapabilityAvailability,
}

impl CapabilityCatalog {
    pub(crate) fn bind_process(&self, key: &DecisionKey) -> anyhow::Result<ProcessCatalogSnapshot> {
        let entries = self
            .entries
            .iter()
            .map(|entry| {
                let approval_ops = match &entry.approval {
                    ApprovalRequirement::None => None,
                    ApprovalRequirement::Required { op_ids } => {
                        Some(op_ids.iter().map(|op| op.as_str().to_owned()).collect())
                    }
                    ApprovalRequirement::Satisfied { .. } => {
                        anyhow::bail!("catalog transport cannot transfer satisfied approval")
                    }
                };
                Ok(Entry {
                    id: entry.id.as_str().to_owned(),
                    public_name: entry.public_name.clone(),
                    root: entry.root.clone(),
                    family: entry.family.as_str().to_owned(),
                    namespace: entry.namespace.clone(),
                    summary: entry.summary.clone(),
                    aliases: entry.aliases.clone(),
                    categories: entry.categories.clone(),
                    effect: entry.effect.as_str().to_owned(),
                    risk: entry.risk,
                    op_ids: entry.op_ids.iter().map(|op| op.as_str().to_owned()).collect(),
                    approval_ops,
                    channel_eligible: entry.channel_eligible,
                    schema: entry.schema.as_ref().clone(),
                    fingerprint: entry.fingerprint.0,
                    target: entry.target.clone(),
                    binding_digest: entry.binding_digest,
                    skill_metadata: entry.skill_metadata.as_deref().cloned(),
                    exposure: entry.exposure.clone(),
                    availability: entry.availability.clone(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        // Keep exact payload bytes, including schema order, across processes.
        let payload = serde_json::to_string(&entries)?;
        let seal = key.hmac(
            KeyDomain::StateFingerprint,
            &[b"process-capability-catalog-v1", &self.generation.0, payload.as_bytes()],
        );
        Ok(ProcessCatalogSnapshot {
            generation: self.generation.0,
            payload,
            seal,
        })
    }

    pub(crate) fn restore_process(
        snapshot: &ProcessCatalogSnapshot,
        key: &DecisionKey,
        expected_generation: CatalogGeneration,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            snapshot.generation == expected_generation.0,
            "process catalog generation differs from parent constraints"
        );
        anyhow::ensure!(
            key.verify_hmac(
                KeyDomain::StateFingerprint,
                &[
                    b"process-capability-catalog-v1",
                    &snapshot.generation,
                    snapshot.payload.as_bytes()
                ],
                &snapshot.seal
            ),
            "process catalog signature is invalid"
        );
        let wire: Vec<Entry> = serde_json::from_str(&snapshot.payload)?;
        let mut entries = Vec::with_capacity(wire.len());
        let mut names = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for entry in wire {
            let family = CapabilityTarget::ALL
                .into_iter()
                .find(|family| family.as_str() == entry.family)
                .ok_or_else(|| anyhow::anyhow!("process catalog family is invalid"))?;
            let id = CapabilityId::new(family, &entry.namespace, &entry.public_name)?;
            anyhow::ensure!(
                id.as_str() == entry.id && !entry.root.is_empty() && entry.schema.is_object(),
                "process catalog identity or schema is invalid"
            );
            let effect = match entry.effect.as_str() {
                "read_only" => SideEffectClass::ReadOnly,
                "local_write" => SideEffectClass::LocalWrite,
                "process_exec" => SideEffectClass::ProcessExec,
                "external_send" => SideEffectClass::ExternalSend,
                "credential_or_config" => SideEffectClass::CredentialOrConfig,
                "destructive" => SideEffectClass::Destructive,
                _ => anyhow::bail!("process catalog effect is invalid"),
            };
            let approval = entry
                .approval_ops
                .map_or(ApprovalRequirement::None, |ops| ApprovalRequirement::Required {
                    op_ids: ops.iter().map(|op| OpId::new(op)).collect(),
                });
            anyhow::ensure!(entry.risk == risk_for_effect(effect), "process catalog risk is invalid");
            if let Some(metadata) = &entry.skill_metadata {
                anyhow::ensure!(
                    family == CapabilityTarget::Skill
                        && metadata.name == entry.public_name
                        && entry.binding_digest == Some(crate::skills::catalog_skill_binding_digest(metadata)),
                    "process skill metadata binding differs"
                );
            }
            let schema_bytes = serde_json::to_vec(&entry.schema)?.len();
            insert(
                CapabilityEntry {
                    id,
                    public_name: entry.public_name,
                    root: entry.root,
                    family,
                    namespace: entry.namespace,
                    summary: entry.summary,
                    aliases: entry.aliases,
                    categories: entry.categories,
                    exact_mention_terms: Vec::new(),
                    effect,
                    risk: entry.risk,
                    op_ids: entry.op_ids.iter().map(|op| OpId::new(op)).collect(),
                    approval,
                    channel_eligible: entry.channel_eligible,
                    schema: Arc::new(entry.schema),
                    schema_bytes,
                    fingerprint: SchemaFingerprint(entry.fingerprint),
                    generation: expected_generation,
                    target: entry.target,
                    binding_digest: entry.binding_digest,
                    skill_metadata: entry.skill_metadata.map(Arc::new),
                    exposure: entry.exposure,
                    availability: entry.availability,
                },
                &mut entries,
                &mut names,
                &mut ids,
            )?;
        }
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(Arc::new(Self::from_entries(entries, expected_generation)))
    }
}
