//! Signed per-segment evidence for partial ordinary-worker accounting.
use super::{Arc, ChildInputReceipt, Context, MemoryFabric, MessageEventScope, OrdinaryProcessAuthority};
use crate::decision::key::KeyDomain;
use crate::session_worker::protocol::WorkerSegmentTerminal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SegmentEvidence {
    pub version: u8,
    pub task_run_id: String,
    pub process_id: String,
    pub index: u64,
    pub input: Option<WorkerSegmentTerminal>,
    pub terminal: Option<WorkerSegmentTerminal>,
    pub seal: [u8; 32],
}

impl SegmentEvidence {
    pub(crate) fn signing_bytes(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            self.version,
            &self.task_run_id,
            &self.process_id,
            self.index,
            &self.input,
            &self.terminal,
        ))?)
    }
}

/// Only a live admitted process can create this signer. Its terminal-only
/// capability survives cancellation so cleanup can retain known settlements.
pub(crate) struct OrdinarySegmentJournal {
    fabric: MemoryFabric,
    scope: MessageEventScope,
    process_id: String,
    task_run_id: String,
    parent_scope_id: String,
    key: Arc<crate::decision::DecisionKey>,
    recording: bool,
}

impl OrdinarySegmentJournal {
    pub(crate) fn new(authority: &OrdinaryProcessAuthority) -> anyhow::Result<Self> {
        authority.verify()?;
        let (child, parent) = authority.snapshots.scope.gate_inputs();
        Ok(Self {
            fabric: authority.fabric.clone(),
            scope: authority.logical_scope(),
            process_id: authority.id.to_string(),
            task_run_id: authority.run_id.clone(),
            parent_scope_id: child.parent_decision_id.to_string(),
            key: Arc::clone(&parent.key),
            recording: authority
                .snapshots
                .generation
                .effective
                .memory
                .event_recording_config()
                .enabled,
        })
    }

    fn reference(event: &crate::memory::MessageEvent, terminal_id: String) -> anyhow::Result<WorkerSegmentTerminal> {
        anyhow::ensure!(
            event.id > 0 && !event.event_id.is_empty(),
            "segment evidence requires durable event"
        );
        Ok(WorkerSegmentTerminal {
            terminal_id,
            event_id: event.event_id.clone(),
            event_digest: format!("{:x}", Sha256::digest(serde_json::to_vec(event)?)),
        })
    }

    pub(crate) async fn admission(&self, index: u64, input: Option<&ChildInputReceipt>) -> anyhow::Result<()> {
        if !self.recording {
            return Ok(());
        }
        let run = format!("child-segment:{}:{index}", self.process_id);
        let input = input
            .map(|input| Self::reference(input.event(), run.clone()))
            .transpose()?;
        self.persist(index, input, None, "admission").await
    }

    pub(crate) async fn terminal(&self, receipt: &crate::agent::terminal::TurnTerminalReceipt) -> anyhow::Result<()> {
        if !self.recording {
            return Ok(());
        }
        let prefix = format!("child-segment:{}:", self.process_id);
        let suffix = receipt
            .terminal_id
            .strip_prefix(&prefix)
            .context("ordinary terminal process differs")?;
        let index: u64 = suffix.parse()?;
        anyhow::ensure!(suffix == index.to_string(), "ordinary terminal index is not canonical");
        let event = &receipt.terminal_event;
        anyhow::ensure!(
            event.workspace_id == self.fabric.workspace_id()
                && event.owner_id == self.scope.owner_id
                && event.session_key == self.scope.session_key
                && event.parent_run_id == self.scope.parent_run_id
                && event.source == self.scope.source
                && event.run_id.as_deref() == Some(receipt.terminal_id.as_str())
                && event.subject == self.scope.subject
                && event.event_type == "turn.finalized"
                && event.config_generation_id == self.scope.config_generation_id
                && event.config_source_revision == self.scope.config_source_revision,
            "ordinary terminal journal lineage differs"
        );
        self.persist(
            index,
            None,
            Some(Self::reference(event, receipt.terminal_id.clone())?),
            "terminal",
        )
        .await
    }

    async fn persist(
        &self,
        index: u64,
        input: Option<WorkerSegmentTerminal>,
        terminal: Option<WorkerSegmentTerminal>,
        kind: &str,
    ) -> anyhow::Result<()> {
        let mut evidence = SegmentEvidence {
            version: 1,
            task_run_id: self.task_run_id.clone(),
            process_id: self.process_id.clone(),
            index,
            input,
            terminal,
            seal: [0; 32],
        };
        evidence.seal = self.key.hmac(
            KeyDomain::StateFingerprint,
            &[
                b"ordinary-worker-segment-v1",
                kind.as_bytes(),
                self.parent_scope_id.as_bytes(),
                &evidence.signing_bytes()?,
            ],
        );
        let payload = serde_json::to_string(&evidence)?;
        let event_type = format!("worker.ordinary.segment.{kind}");
        let lookup = format!("worker:{}:segment:{index}:{kind}", self.task_run_id);
        let event = self
            .fabric
            .record_runtime_event_idempotent(
                self.scope.clone(),
                &event_type,
                "ordinary worker segment evidence",
                Some(payload.clone()),
                lookup.clone(),
            )
            .await?;
        anyhow::ensure!(
            event.id > 0
                && event.workspace_id == self.fabric.workspace_id()
                && event.owner_id == self.scope.owner_id
                && event.run_id == self.scope.run_id
                && event.session_key == self.scope.session_key
                && event.source == self.scope.source
                && event.event_type == event_type
                && event.idempotency_key.as_deref() == Some(lookup.as_str())
                && event.raw_payload_json.as_deref() == Some(payload.as_str()),
            "ordinary segment journal receipt differs"
        );
        let stored = self
            .fabric
            .memory()
            .find_owned_message_event(
                self.fabric.workspace_id(),
                self.scope
                    .owner_id
                    .as_deref()
                    .context("ordinary segment owner missing")?,
                &event.event_id,
            )
            .await?
            .context("ordinary segment journal is not durable")?;
        anyhow::ensure!(
            serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
            "ordinary segment journal differs from durable storage"
        );
        Ok(())
    }
}
