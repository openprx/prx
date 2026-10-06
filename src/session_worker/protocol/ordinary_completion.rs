use super::{WorkerResult, WorkerSegmentTerminal};
use serde::{Deserialize, Serialize};

/// Signed process completion, never a durable parent-decision assertion.
/// Recording-off completion carries local segment identities and usage only.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrdinaryWorkerCompletionEvidence {
    pub run_id: String,
    pub process_id: String,
    pub input_event_id: Option<String>,
    pub recording_enabled: bool,
    pub terminal: Option<WorkerSegmentTerminal>,
    pub recorded_segments: u64,
    pub confirmed_segments: Vec<String>,
    pub segments: Vec<WorkerSegmentTerminal>,
    pub usage: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    pub seal: [u8; 32],
}

impl std::fmt::Debug for OrdinaryWorkerCompletionEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrdinaryWorkerCompletionEvidence")
            .finish_non_exhaustive()
    }
}

impl OrdinaryWorkerCompletionEvidence {
    pub(crate) fn validate_inventory(&self, success: bool) -> anyhow::Result<()> {
        let process_id = uuid::Uuid::parse_str(&self.process_id)?;
        anyhow::ensure!(
            !process_id.is_nil() && process_id.to_string() == self.process_id,
            "ordinary worker process identity is not canonical"
        );
        let confirmed = u64::try_from(self.confirmed_segments.len())?;
        anyhow::ensure!(
            !self.run_id.is_empty()
                && confirmed <= self.recorded_segments
                && (!success || (confirmed > 0 && confirmed == self.recorded_segments))
                && self.recording_enabled == self.terminal.is_some()
                && (if self.recording_enabled {
                    self.segments.len() == self.confirmed_segments.len()
                } else {
                    self.segments.is_empty() && self.input_event_id.is_none()
                }),
            "ordinary worker completion inventory is inconsistent"
        );
        anyhow::ensure!(
            self.input_event_id.as_deref().is_none_or(|id| !id.is_empty()),
            "ordinary worker input identity is empty"
        );
        let prefix = format!("child-segment:{}:", self.process_id);
        let mut confirmed_ids = std::collections::BTreeSet::new();
        for id in &self.confirmed_segments {
            let index = id
                .strip_prefix(&prefix)
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("ordinary worker segment belongs to another process"))?;
            anyhow::ensure!(
                index < self.recorded_segments
                    && id == &format!("{prefix}{index}")
                    && confirmed_ids.insert(id.as_str()),
                "ordinary worker segment identity is duplicate or invalid"
            );
        }
        let mut events = std::collections::BTreeSet::new();
        if let Some(terminal) = &self.terminal {
            anyhow::ensure!(
                terminal.terminal_id == self.run_id,
                "ordinary worker terminal belongs to another run"
            );
            validate_reference(terminal)?;
            events.insert(terminal.event_id.as_str());
        }
        let mut referenced = std::collections::BTreeSet::new();
        for segment in &self.segments {
            validate_reference(segment)?;
            anyhow::ensure!(
                confirmed_ids.contains(segment.terminal_id.as_str())
                    && referenced.insert(segment.terminal_id.as_str())
                    && events.insert(segment.event_id.as_str()),
                "ordinary worker durable inventory is duplicate or unconfirmed"
            );
        }
        let mut settlements = std::collections::BTreeSet::new();
        for usage in &self.usage {
            let id = usage
                .settlement_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("ordinary worker usage has no settlement"))?;
            anyhow::ensure!(
                confirmed_ids.contains(id) && settlements.insert(id),
                "ordinary worker usage is duplicate or unconfirmed"
            );
        }
        Ok(())
    }

    pub(crate) fn signing_bytes(&self, result: &WorkerResult) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            (
                &self.run_id,
                &self.process_id,
                &self.input_event_id,
                self.recording_enabled,
                &self.terminal,
                self.recorded_segments,
                &self.confirmed_segments,
                &self.segments,
                &self.usage,
            ),
            (result.success, &result.output, &result.error, &result.tokens_used),
        ))?)
    }
}

fn validate_reference(reference: &WorkerSegmentTerminal) -> anyhow::Result<()> {
    anyhow::ensure!(
        !reference.event_id.is_empty()
            && reference.event_digest.len() == 64
            && reference.event_digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "ordinary worker durable reference is invalid"
    );
    Ok(())
}
