//! Agent incarnation lifecycle on the gateway side (MA-01b1b-2): the removal
//! barrier shared by config reload and targeted resolves, the startup latch
//! and the reload participant's retire, rollback and refresh steps.

use crate::config::{Config, DelegateAgentConfig};
use crate::memory::Memory;
use crate::memory::agent_registry::RetiredIncarnation;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Registry key of a named agent.
pub(crate) fn agent_key(name: &str) -> String {
    format!("n:{name}")
}

/// The non-secret fields that identify an agent's configuration, in the order
/// they are hashed. `api_key` is never part of it; the credential is named by
/// reference only.
#[derive(Serialize)]
struct RevisionPreimage<'a> {
    provider: &'a str,
    model: &'a str,
    system_prompt: Option<&'a str>,
    credential_ref: Option<&'a str>,
    temperature: Option<f64>,
    max_depth: u32,
    agentic: bool,
    allowed_tools: &'a [String],
    identity_dir: Option<&'a str>,
    memory_scope: Option<&'a str>,
    spawn_enabled: Option<bool>,
    enabled: bool,
    autonomy: Option<crate::security::AutonomyLevel>,
}

/// Lowercase SHA-256 over the serialized preimage.
pub(crate) fn config_revision(agent: &DelegateAgentConfig) -> anyhow::Result<String> {
    let preimage = serde_json::to_string(&RevisionPreimage {
        provider: &agent.provider,
        model: &agent.model,
        system_prompt: agent.system_prompt.as_deref(),
        credential_ref: agent.credential_ref.as_deref(),
        temperature: agent.temperature,
        max_depth: agent.max_depth,
        agentic: agent.agentic,
        allowed_tools: &agent.allowed_tools,
        identity_dir: agent.identity_dir.as_deref(),
        memory_scope: agent.memory_scope.as_deref(),
        spawn_enabled: agent.spawn_enabled,
        enabled: agent.enabled,
        autonomy: agent.autonomy,
    })?;
    Ok(crate::memory::SqliteMemory::schema_migration_checksum(&preimage))
}

fn revisions(config: &Config) -> anyhow::Result<BTreeMap<String, String>> {
    config
        .agents
        .iter()
        .map(|(name, agent)| Ok((agent_key(name), config_revision(agent)?)))
        .collect()
}

/// Retire every active row whose agent left the config and refresh the
/// revisions of the rest. At startup a failure latches every targeted resolve.
pub(crate) fn reconcile(memory: &dyn Memory, config: &Config, latch_on_failure: bool) {
    let Some(registry) = memory.agent_registry() else {
        return;
    };
    let workspace = config.workspace_dir.to_string_lossy();
    if let Err(error) =
        revisions(config).and_then(|revisions| registry.reconcile(&workspace, &revisions, latch_on_failure))
    {
        tracing::error!(%error, "agent registry reconcile failed");
    }
}

/// One reload's agent steps, carried by the prepared gateway runtime.
pub(crate) struct AgentReconcile {
    memory: Arc<dyn Memory>,
    next: Arc<Config>,
    removed: BTreeSet<String>,
    retired: Vec<RetiredIncarnation>,
}

impl AgentReconcile {
    pub(crate) fn prepare(memory: &Arc<dyn Memory>, previous: &Config, next: &Arc<Config>) -> Self {
        Self {
            memory: Arc::clone(memory),
            removed: previous
                .agents
                .keys()
                .filter(|name| !next.agents.contains_key(*name))
                .map(|name| agent_key(name))
                .collect(),
            next: Arc::clone(next),
            retired: Vec::new(),
        }
    }

    /// Block the removed keys, then retire their rows in one transaction. A
    /// failure rejects the reload, so the removal is never published.
    pub(crate) fn commit(&mut self) -> anyhow::Result<()> {
        if let Some(registry) = self.memory.agent_registry().filter(|_| !self.removed.is_empty()) {
            self.retired = registry.begin_removal(&self.next.workspace_dir.to_string_lossy(), &self.removed)?;
        }
        Ok(())
    }

    /// Lift the block, re-activating `restore`. If that fails the rows stay
    /// `retiring`, which only costs availability.
    fn end_removal(&self, restore: &[RetiredIncarnation]) {
        if let Some(Err(error)) = self
            .memory
            .agent_registry()
            .map(|registry| registry.end_removal(restore))
        {
            tracing::error!(%error, "agent registry removal barrier could not be lifted cleanly");
        }
    }

    pub(crate) fn rollback(&mut self) {
        let retired = std::mem::take(&mut self.retired);
        self.end_removal(&retired);
    }

    /// The new config is published: lift the block and refresh. While the
    /// startup latch is set, this is also the sweep that can clear it.
    pub(crate) fn finalize(&self) {
        self.end_removal(&[]);
        reconcile(self.memory.as_ref(), &self.next, false);
    }
}
