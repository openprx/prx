use serde::{Deserialize, Serialize};

use super::{MemoryVisibility, MessageEvent, MessageEventSource};

/// Source-owned runtime statistics authority. Fields are not caller-constructible.
/// This is not a Memory Retrieve grant and carries no user-memory read permission.
#[derive(Clone)]
pub struct RouterLearningEvidence {
    pub(in crate::memory) terminal: RouterLearningTerminal,
    pub(in crate::memory) source_event_id: String,
    pub(in crate::memory) source_input: String,
    pub(in crate::memory) source_raw_payload: Option<String>,
    pub(in crate::memory) generation: i64,
    pub(in crate::memory) revision: String,
    pub(in crate::memory) policy: String,
    pub(in crate::memory) provider: String,
    pub(in crate::memory) model: String,
    pub(in crate::memory) success: bool,
    pub(in crate::memory) latency_ms: u64,
}

#[derive(Clone)]
pub(in crate::memory) struct RouterLearningTerminal {
    pub id: i64,
    pub content_hash: Option<String>,
    pub event_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub run_id: Option<String>,
    pub channel: Option<String>,
    pub sender: Option<String>,
    pub source: MessageEventSource,
    pub content: String,
    pub raw_payload_json: Option<String>,
    pub idempotency_key: Option<String>,
    pub role: String,
    pub event_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterLearningMetrics {
    pub provider: String,
    pub model: String,
    pub elo: f32,
    pub success_rate: f32,
    pub latency_ms: u32,
    pub successes: std::collections::VecDeque<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterLearningSnapshot {
    pub version: i64,
    pub models: Vec<RouterLearningMetrics>,
}

pub struct RouterLearningRead {
    pub snapshot: Option<RouterLearningSnapshot>,
    pub replayed: bool,
}

pub enum RouterLearningCommit {
    Committed(RouterLearningSnapshot),
    Conflict,
}

impl RouterLearningSnapshot {
    pub(crate) const MAX_BYTES: usize = 262_144;
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version >= 0 && !self.models.is_empty() && self.models.len() <= 128,
            "invalid Router statistics bounds"
        );
        let mut ids = std::collections::HashSet::new();
        for model in &self.models {
            anyhow::ensure!(
                !model.provider.is_empty()
                    && model.provider.len() <= 512
                    && !model.model.is_empty()
                    && model.model.len() <= 512
                    && model.elo.is_finite()
                    && model.success_rate.is_finite()
                    && (0.0..=1.0).contains(&model.success_rate)
                    && model.successes.len() <= 100
                    && ids.insert((&model.provider, &model.model)),
                "invalid Router statistics model"
            );
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> anyhow::Result<String> {
        self.validate()?;
        let value = serde_json::to_string(self)?;
        anyhow::ensure!(value.len() <= Self::MAX_BYTES, "Router statistics exceed byte bound");
        Ok(value)
    }
    pub(crate) fn decode(value: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(value.len() <= Self::MAX_BYTES, "Router statistics exceed byte bound");
        let snapshot: Self = serde_json::from_str(value)?;
        snapshot.validate()?;
        Ok(snapshot)
    }
}

impl RouterLearningEvidence {
    /// Only the source-bound terminal consumer constructs this. A public receipt
    /// is not a seal: each backend still verifies its durable event and source.
    #[cfg(feature = "llm-router")]
    pub(crate) fn from_terminal(
        receipt: &MessageEvent,
        source_event_id: String,
        source_input: &str,
        source_raw_payload: Option<&str>,
        generation: u64,
        revision: &str,
        policy: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            source_raw_payload.is_none_or(|raw| raw.len() <= 32 * 1024),
            "Router source metadata exceeds bound"
        );
        anyhow::ensure!(
            source_input.len() <= 65_536
                && revision.len() <= 512
                && receipt.content_hash.as_ref().is_none_or(|value| value.len() <= 128),
            "Router source/receipt exceeds byte bound"
        );
        // This producer is the native embedded source, not a generic imported terminal.
        // Refuse any additional authority instead of erasing it while projecting the DTO.
        anyhow::ensure!(
            receipt.id > 0
                && receipt.visibility == MemoryVisibility::Session
                && receipt.parent_session_key.is_none()
                && receipt.parent_run_id.is_none()
                && receipt.agent_id.is_none()
                && receipt.persona_id.is_none()
                && receipt.subject.is_none()
                && receipt.goal_id.is_none()
                && receipt.correlation_id.is_none()
                && receipt.attempt_id.is_none()
                && receipt.lease_epoch.is_none()
                && receipt.recipient == receipt.session_key
                && receipt.causation_event_id.as_deref() == Some(source_event_id.as_str())
                && receipt.config_generation_id == Some(generation)
                && receipt.config_source_revision.as_deref() == Some(revision),
            "Router native terminal authority differs"
        );
        let payload = receipt
            .raw_payload_json
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("terminal payload missing"))?;
        anyhow::ensure!(
            payload.len() <= 65_536 && receipt.content.len() <= 8192,
            "Router terminal exceeds byte bound"
        );
        for value in [
            Some(receipt.event_id.as_str()),
            Some(receipt.workspace_id.as_str()),
            receipt.owner_id.as_deref(),
            receipt.session_key.as_deref(),
            receipt.run_id.as_deref(),
            receipt.channel.as_deref(),
            receipt.sender.as_deref(),
            receipt.idempotency_key.as_deref(),
            Some(receipt.source.as_str()),
            Some(receipt.role.as_str()),
            Some(receipt.event_type.as_str()),
        ] {
            anyhow::ensure!(
                value.is_none_or(|value| value.len() <= 512),
                "Router receipt identity exceeds byte bound"
            );
        }
        let terminal = RouterLearningTerminal {
            id: receipt.id,
            content_hash: receipt.content_hash.clone(),
            event_id: receipt.event_id.clone(),
            workspace_id: receipt.workspace_id.clone(),
            owner_id: receipt.owner_id.clone(),
            session_key: receipt.session_key.clone(),
            run_id: receipt.run_id.clone(),
            channel: receipt.channel.clone(),
            sender: receipt.sender.clone(),
            source: receipt.source.clone(),
            content: receipt.content.clone(),
            raw_payload_json: receipt.raw_payload_json.clone(),
            idempotency_key: receipt.idempotency_key.clone(),
            role: receipt.role.clone(),
            event_type: receipt.event_type.clone(),
        };
        let value: serde_json::Value = serde_json::from_str(payload)?;
        let (provider, model, success) = Self::actual_terminal_outcome(&value)?;
        let started = chrono::DateTime::parse_from_rfc3339(
            value
                .get("telemetry")
                .and_then(|telemetry| telemetry.get("started_at"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("terminal start missing"))?,
        )?;
        let finished = chrono::DateTime::parse_from_rfc3339(
            value
                .get("telemetry")
                .and_then(|telemetry| telemetry.get("finished_at"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("terminal finish missing"))?,
        )?;
        let latency_ms = u64::try_from(finished.signed_duration_since(started).num_milliseconds()).unwrap_or(0);
        let evidence = Self {
            terminal,
            source_event_id,
            source_input: source_input.to_owned(),
            source_raw_payload: source_raw_payload.map(str::to_owned),
            generation: i64::try_from(generation)?,
            revision: revision.to_owned(),
            policy,
            provider,
            model,
            success,
            latency_ms,
        };
        evidence.validate()?;
        Ok(evidence)
    }
    #[cfg(feature = "llm-router")]
    pub(crate) fn served(&self) -> (&str, &str) {
        (&self.provider, &self.model)
    }
    #[cfg(feature = "llm-router")]
    pub(crate) const fn outcome(&self) -> (bool, u64) {
        (self.success, self.latency_ms)
    }
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        let event = &self.terminal;
        anyhow::ensure!(
            self.source_input.len() <= 65_536
                && self
                    .source_raw_payload
                    .as_ref()
                    .is_none_or(|raw| raw.len() <= 32 * 1024)
                && self.generation >= 0
                && self.revision.len() <= 512,
            "Router learning source exceeds bounds"
        );
        for id in [
            event.workspace_id.as_str(),
            event.owner_id.as_deref().unwrap_or_default(),
            event.session_key.as_deref().unwrap_or_default(),
            event.run_id.as_deref().unwrap_or_default(),
            event.event_id.as_str(),
            self.source_event_id.as_str(),
            self.provider.as_str(),
            self.model.as_str(),
        ] {
            anyhow::ensure!(!id.is_empty() && id.len() <= 512, "invalid Router learning identity");
        }
        anyhow::ensure!(
            self.policy.len() == 64 && self.policy.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid Router learning policy"
        );
        anyhow::ensure!(
            event.role == "event"
                && event.event_type == "turn.finalized"
                && event.source == MessageEventSource::Agent
                && event.channel.as_deref() == Some("agent_api")
                && event.sender.as_deref() == Some("embedded-caller")
                && event.idempotency_key.as_deref() == Some(format!("turn:{}:final", self.terminal_id()).as_str()),
            "Router learning requires a committed terminal"
        );
        let payload = event
            .raw_payload_json
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("terminal payload missing"))?;
        anyhow::ensure!(
            payload.len() <= 65_536 && event.content.len() <= 8192,
            "Router terminal receipt exceeds byte bound"
        );
        let value: serde_json::Value = serde_json::from_str(payload)?;
        let (provider, model, success) = Self::actual_terminal_outcome(&value)?;
        anyhow::ensure!(
            value.get("terminal_id").and_then(serde_json::Value::as_str) == Some(self.terminal_id())
                && provider == self.provider
                && model == self.model
                && success == self.success,
            "Router learning actual outcome differs from terminal"
        );
        Ok(())
    }
    pub(super) fn actual_terminal_outcome(value: &serde_json::Value) -> anyhow::Result<(String, String, bool)> {
        use crate::llm::route_decision::{AttemptStatus, ExecutionStatus, ProviderExecutionOutcome};
        // Historical v1 records authenticate an auxiliary event and project
        // its serving outcome separately from main usage settlement.
        let outcome_value = if let Some(served) = value.get("served_auxiliary") {
            served
                .get("provider_outcome")
                .ok_or_else(|| anyhow::anyhow!("served auxiliary outcome missing"))?
        } else {
            &value["provider_outcome"]
        };
        let outcome: ProviderExecutionOutcome = serde_json::from_value(outcome_value.clone())?;
        let terminal_status: crate::agent::terminal::TurnTerminalStatus =
            serde_json::from_value(value["status"].clone())?;
        anyhow::ensure!(
            matches!(
                outcome.status,
                ExecutionStatus::Success | ExecutionStatus::FallbackSuccess
            ),
            "Router learning requires an actual serving outcome, not all-failed labels"
        );
        let served = outcome
            .attempts
            .iter()
            .rev()
            .find(|attempt| attempt.status != AttemptStatus::Skipped)
            .ok_or_else(|| anyhow::anyhow!("Router learning has no physical attempt"))?;
        anyhow::ensure!(
            served.status == AttemptStatus::Success
                && served.provider == outcome.final_provider
                && served.model == outcome.final_model
                && served.finished_at >= served.started_at
                && outcome.finished_at >= outcome.started_at,
            "Router actual serving attempt differs from terminal outcome"
        );
        let success = matches!(
            terminal_status,
            crate::agent::terminal::TurnTerminalStatus::Completed | crate::agent::terminal::TurnTerminalStatus::Silent
        );
        Ok((outcome.final_provider, outcome.final_model, success))
    }

    pub(crate) fn terminal_id(&self) -> &str {
        self.terminal.run_id.as_deref().unwrap_or_default()
    }
    pub(crate) fn domain(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for value in [
            &self.terminal.workspace_id,
            self.terminal.owner_id.as_ref().unwrap_or(&self.policy),
            self.terminal.session_key.as_ref().unwrap_or(&self.policy),
            &self.policy,
        ] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        hex::encode(hash.finalize())
    }
    pub(crate) fn fingerprint(&self) -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};
        self.validate()?;
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
            &self.source_event_id,
            self.domain(),
            &self.terminal.event_id,
            &self.terminal.raw_payload_json,
            &self.source_input,
            &self.source_raw_payload,
            self.generation,
            &self.revision,
            &self.provider,
            &self.model,
            self.success,
            self.latency_ms,
        ))?)))
    }
}
