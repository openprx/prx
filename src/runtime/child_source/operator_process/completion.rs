//! Sign the original stdout result after actual terminal and segment collection.
use super::OperatorProcessAuthority;
use crate::session_worker::protocol::{OperatorWorkerCompletionEvidence, WorkerResult, WorkerSegmentTerminal};
use sha2::{Digest, Sha256};

impl OperatorProcessAuthority {
    pub(crate) fn seal_completion(
        &self,
        result: &mut WorkerResult,
        terminal: &crate::agent::terminal::TurnTerminalReceipt,
        recorded_segments: u64,
        usage: &crate::runtime::turn_terminal::TerminalUsageCollector,
    ) -> anyhow::Result<()> {
        self.verify()?;
        anyhow::ensure!(
            result.completion_evidence.is_none()
                && result.ordinary_completion.is_none()
                && result.operator_completion.is_none(),
            "operator result already has completion evidence"
        );
        let scope = self.logical_scope();
        let event = &terminal.terminal_event;
        let recording = self.snapshots.generation.effective.memory.event_recording_config();
        let terminal_matches_run = terminal.terminal_id == self.run_id;
        anyhow::ensure!(
            terminal_matches_run
                && event.run_id == scope.run_id
                && event.workspace_id == self.fabric.workspace_id()
                && event.owner_id == scope.owner_id
                && event.session_key == scope.session_key
                && event.parent_run_id == scope.parent_run_id
                && event.subject == scope.subject
                && event.source == scope.source
                && event.event_type == "turn.finalized"
                && event.config_generation_id == scope.config_generation_id
                && event.config_source_revision == scope.config_source_revision
                && (if recording.enabled {
                    event.id > 0 && !event.event_id.is_empty()
                } else {
                    event.id == 0
                }),
            "operator completion terminal differs from actual process"
        );
        let terminal = if recording.enabled {
            Some(WorkerSegmentTerminal {
                terminal_id: self.run_id.clone(),
                event_id: event.event_id.clone(),
                event_digest: format!("{:x}", Sha256::digest(serde_json::to_vec(event)?)),
            })
        } else {
            None
        };
        let mut evidence = OperatorWorkerCompletionEvidence {
            publication_id: self.snapshots.publication.id().to_string(),
            run_id: self.run_id.clone(),
            process_id: self.id.to_string(),
            input_event_id: self.input().map(|input| input.event().event_id.clone()),
            recording_enabled: recording.enabled,
            terminal,
            recorded_segments,
            confirmed_segments: usage.confirmed_terminal_ids(),
            segments: usage.terminal_references(),
            usage: usage.snapshot(),
            seal: [0; 32],
        };
        evidence.validate_inventory(result.success)?;
        evidence.seal = self.snapshots.key.hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                b"operator-worker-completion-v1",
                self.snapshots.publication.id().to_string().as_bytes(),
                &evidence.signing_bytes(result)?,
            ],
        );
        self.verify()?;
        result.operator_completion = Some(evidence);
        Ok(())
    }
}
