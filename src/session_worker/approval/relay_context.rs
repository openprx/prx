//! Builds the parent-side relay from the spawning caller and seals it into the
//! worker manifest.
//!
//! The seed is built where the spawn call still holds its caller. It is armed
//! only against the finished manifest: the relay switch is set on the manifest
//! before the manifest HMAC is computed, and only when every condition holds,
//! so a worker can never turn the relay on by itself.
use super::relay::{ProcessApprovalRelay, RelayParts};
use crate::agent::live_frames::LiveTurnRef;
use crate::runtime::child_source::{ChildInputReceipt, ChildProviderBinding};
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::session_worker::frames::WorkerFrameChannels;
use crate::session_worker::mailbox_binding::BoundWorkerMailbox;
use crate::session_worker::protocol::{WorkerControlFrame, WorkerManifest};
use crate::tools::{ToolExecutionContext, ToolExecutionService};
use anyhow::Context as _;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// What the parent knows about the spawning caller, before the manifest exists.
pub(crate) struct ProcessRelaySeed {
    execution: ToolExecutionContext,
    service: Arc<ToolExecutionService>,
    live_turn: Option<LiveTurnRef>,
    level: AutonomyLevel,
    cancel: CancellationToken,
}

/// A seed checked against the sealed manifest, with the control queue it will
/// answer on.
pub(crate) struct ArmedRelay {
    seed: ProcessRelaySeed,
    controls: mpsc::Sender<WorkerControlFrame>,
    run_id: String,
    allowed_tools: Vec<String>,
}

fn policy_at_level(policy: &Arc<SecurityPolicy>, level: AutonomyLevel) -> Arc<SecurityPolicy> {
    if policy.autonomy == level {
        Arc::clone(policy)
    } else {
        Arc::new(SecurityPolicy {
            autonomy: level,
            ..policy.as_ref().clone()
        })
    }
}

/// What the spawn call hands over to build the seed.
pub(crate) struct RelaySeedInputs<'a> {
    pub(crate) caller: Option<&'a crate::tools::execution::ChildCallContext>,
    pub(crate) binding: Option<&'a ChildProviderBinding>,
    pub(crate) resolvers: Option<&'a crate::agent::turn_request::SpawnApprovalResolverFactory>,
    pub(crate) scope: &'a crate::memory::MessageEventScope,
    pub(crate) task: &'a str,
    pub(crate) receipt: Option<&'a ChildInputReceipt>,
    pub(crate) security: &'a Arc<SecurityPolicy>,
    pub(crate) level: AutonomyLevel,
    pub(crate) run_id: &'a str,
}

impl RelaySeedInputs<'_> {
    /// Build the seed of an ordinary process child below Full. `None` when there
    /// is nothing to relay for: a Full child never asks, and an operator-rooted
    /// child has no caller approval surface.
    ///
    /// The child context is rebuilt from the same inputs the launch capture
    /// uses, so it carries the child run, its sealed scope and the caller's live
    /// surface. The approval strategy is the one a task child gets: the run's
    /// chat resolver, when the root has one, behind the caller's live surface.
    pub(crate) fn seed(self) -> anyhow::Result<Option<ProcessRelaySeed>> {
        if self.level == AutonomyLevel::Full {
            return Ok(None);
        }
        let (Some(caller), Some(binding)) = (self.caller, self.binding) else {
            return Ok(None);
        };
        let publication = caller
            .context()
            .ordinary_child_publication
            .as_ref()
            .context("approval relay parent publication missing")?;
        let (_proof, mut execution) =
            binding.ordinary_execution_source(&binding.provider(), caller, self.scope, self.task, self.receipt)?;
        let live_turn = execution.live_turn.clone();
        // As an in-process child: the ledger only accompanies a live surface.
        execution.approval_ledger = live_turn
            .as_ref()
            .and_then(|_| caller.context().approval_ledger.clone());
        let resolver = self.resolvers.map(|factory| factory.resolver_for(self.run_id));
        let manager = resolver
            .as_ref()
            .map(|_| Arc::new(crate::approval::ApprovalManager::new()));
        let cancel = CancellationToken::new();
        let policy = policy_at_level(self.security, self.level);
        let strategy = crate::agent::execution_policy::run_approval_strategy(
            manager,
            resolver,
            "sessions_spawn",
            Arc::clone(&policy),
            Some(cancel.clone()),
        );
        let service = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            publication.registry(),
            Arc::new(crate::tools::SecurityEffectPolicy::new(policy)),
            strategy,
            Arc::new(crate::tools::AdapterOwnedPreparation),
            Arc::new(crate::tools::TracingToolExecutionAudit),
        ));
        Ok(Some(ProcessRelaySeed {
            execution,
            service,
            live_turn,
            level: self.level,
            cancel,
        }))
    }
}

impl ProcessRelaySeed {
    /// Seal the relay switch into `manifest`, which must still be unsigned.
    /// Returns `None`, leaving the switch off, unless the pipe is authenticated
    /// (a mailbox binding sealed the session address), the child is below Full,
    /// and the rebuilt context names the same owner, session and run the
    /// manifest does.
    pub(crate) fn arm(self, binding: Option<&BoundWorkerMailbox>, manifest: &mut WorkerManifest) -> Option<ArmedRelay> {
        let controls = binding
            .and_then(|binding| binding.frames.as_ref())
            .map(|frames| frames.control_sender.clone())?;
        let envelope = &self.execution.envelope;
        let same_child = manifest.session_addr.is_some()
            && manifest.ordinary_source.is_some()
            && self.level != AutonomyLevel::Full
            && envelope.run_id.as_deref() == Some(manifest.run_id.as_str())
            && envelope.owner_id == manifest.owner_id
            && envelope.session_key == manifest.session_scope_key;
        if !same_child {
            tracing::warn!("approval relay not armed: the child context differs from the sealed manifest");
            return None;
        }
        manifest.approval_relay = true;
        Some(ArmedRelay {
            seed: self,
            controls,
            run_id: manifest.run_id.clone(),
            allowed_tools: manifest.allowed_tools.clone(),
        })
    }
}

impl ArmedRelay {
    /// Start the relay on the worker's frames: approval frames go to it, every
    /// other frame still reaches the mailbox actor.
    pub(crate) fn attach(
        self,
        frames: &mut WorkerFrameChannels,
        progress: Arc<crate::agent::idle::ProgressBeat>,
    ) -> Arc<ProcessApprovalRelay> {
        let relay = ProcessApprovalRelay::new(RelayParts {
            run_id: self.run_id,
            allowed_tools: self.allowed_tools,
            execution: self.seed.execution,
            service: self.seed.service,
            live_turn: self.seed.live_turn,
            controls: self.controls,
            progress,
            cancel: self.seed.cancel,
        });
        super::demux::attach(&relay, frames);
        relay
    }
}
