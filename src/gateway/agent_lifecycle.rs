//! Agent incarnation lifecycle on the gateway side (MA-01b1b-2): the removal
//! barrier shared by config reload and targeted resolves, the startup latch
//! and the reload participant's retire, rollback and refresh steps.

use crate::config::{Config, DelegateAgentConfig};
use crate::memory::Memory;
use crate::memory::agent_registry::RetiredIncarnation;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Barrier and latch of one gateway owner. Clones share the state, so the
/// owner, its prepared reloads and every targeted resolve see the same one.
#[derive(Clone, Default)]
pub(crate) struct AgentLifecycle {
    /// Keys whose removal is being committed: no new incarnation until finalize.
    pending_removals: Arc<Mutex<BTreeSet<String>>>,
    /// Set when a startup sweep failed: every targeted resolve is refused.
    registry_unreconciled: Arc<AtomicBool>,
}

impl AgentLifecycle {
    /// Whether a resolve of `agent_key` must be refused right now.
    pub(crate) fn blocks(&self, agent_key: &str) -> bool {
        self.registry_unreconciled.load(Ordering::SeqCst) || self.pending_removals.lock().contains(agent_key)
    }

    /// Retire every active row whose agent left the config and refresh the
    /// revisions of the rest. Only success clears this owner's latch; a failure
    /// sets it when `latch_on_failure` (startup) and otherwise keeps it as is.
    pub(crate) fn reconcile(&self, memory: &dyn Memory, config: &Config, latch_on_failure: bool) {
        let Some(registry) = memory.agent_registry() else {
            return;
        };
        let workspace = config.workspace_dir.to_string_lossy();
        match revisions(config).and_then(|revisions| registry.reconcile(&workspace, &revisions)) {
            Ok(()) => self.registry_unreconciled.store(false, Ordering::SeqCst),
            Err(error) => {
                tracing::error!(%error, "agent registry reconcile failed");
                if latch_on_failure {
                    self.registry_unreconciled.store(true, Ordering::SeqCst);
                }
            }
        }
    }
}

/// One reload's agent steps, carried by the prepared gateway runtime.
pub(crate) struct AgentReconcile {
    lifecycle: AgentLifecycle,
    memory: Arc<dyn Memory>,
    next: Arc<Config>,
    removed: BTreeSet<String>,
    retired: Vec<RetiredIncarnation>,
}

impl AgentReconcile {
    pub(crate) fn prepare(
        lifecycle: &AgentLifecycle,
        memory: &Arc<dyn Memory>,
        previous: &Config,
        next: &Arc<Config>,
    ) -> Self {
        Self {
            lifecycle: lifecycle.clone(),
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
            self.lifecycle
                .pending_removals
                .lock()
                .extend(self.removed.iter().cloned());
            self.retired = registry.retire_removed(&self.next.workspace_dir.to_string_lossy(), &self.removed)?;
        }
        Ok(())
    }

    /// Lift this reload's block only.
    fn lift(&self) {
        self.lifecycle
            .pending_removals
            .lock()
            .retain(|key| !self.removed.contains(key));
    }

    /// Re-activate exactly the rows this commit retired. If that fails they
    /// stay `retiring`, which only costs availability.
    pub(crate) fn rollback(&mut self) {
        self.lift();
        let retired = std::mem::take(&mut self.retired);
        if let Some(Err(error)) = self
            .memory
            .agent_registry()
            .filter(|_| !retired.is_empty())
            .map(|registry| registry.reactivate(&retired))
        {
            tracing::error!(%error, "agent registry rollback compensation failed; rows stay retiring");
        }
    }

    /// The new config is published: lift the block and refresh. While the
    /// latch is set, this is also the sweep that can clear it.
    pub(crate) fn finalize(&self) {
        self.lift();
        self.lifecycle.reconcile(self.memory.as_ref(), &self.next, false);
    }
}
