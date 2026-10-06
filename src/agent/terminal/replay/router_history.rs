//! Authenticated historical Router readmission. This projection never issues
//! a selected-read grant, model binding, or a live execution context.
use super::{DecisionKey, MessageEvent, TurnTerminalStatus};
use crate::causal_tree::store::DecisionReplayRecord;
use crate::llm::route_decision::{AttemptStatus, ExecutionStatus, ProviderExecutionOutcome};
use crate::runtime::turn_decision::{RouterReadFacts, router_provider_binding};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RouterReadReplay {
    version: u8,
    read_decision_id: String,
    read_state_fingerprint: String,
    read_digest: String,
    selected_candidate_id: String,
    capability_id: String,
    capability_fingerprint: [u8; 32],
    catalog_generation: [u8; 32],
    domain_sha256: [u8; 32],
    query_sha256: [u8; 32],
    history_snapshot_sha256: Option<[u8; 32]>,
    readmission_succeeded: bool,
    failure_stage: Option<String>,
    next_decision_id: String,
    next_state_fingerprint: String,
    next_digest: String,
    from_provider_binding: [u8; 32],
    from_model: String,
    to_provider_binding: [u8; 32],
    to_model: String,
    pmm_route_binding: Option<[u8; 32]>,
    original_budget: [u64; 3],
    remaining_budget: [u64; 3],
    source_sequence: i64,
}

impl RouterReadReplay {
    pub(super) fn from_issued(facts: RouterReadFacts) -> anyhow::Result<Self> {
        let projection = Self {
            readmission_succeeded: facts.readmission_succeeded,
            failure_stage: facts.failure_stage.map(str::to_owned),
            version: facts.version,
            read_decision_id: facts.read_decision_id.to_string(),
            read_state_fingerprint: facts.read_state_fingerprint,
            read_digest: String::new(),
            selected_candidate_id: facts.selected_candidate_id,
            capability_id: facts.capability_id,
            capability_fingerprint: facts.capability_fingerprint,
            catalog_generation: facts.catalog_generation,
            domain_sha256: facts.domain_sha256,
            query_sha256: facts.query_sha256,
            history_snapshot_sha256: facts.history_snapshot_sha256,
            next_decision_id: facts.next_decision_id.to_string(),
            next_state_fingerprint: facts.next_state_fingerprint,
            next_digest: String::new(),
            from_provider_binding: facts.from_provider_binding,
            from_model: facts.from_model,
            to_provider_binding: facts.to_provider_binding,
            to_model: facts.to_model,
            pmm_route_binding: facts.pmm_route_binding,
            original_budget: facts.original_budget,
            remaining_budget: facts.remaining_budget,
            source_sequence: facts.source_sequence,
        };
        projection.validate_shape()?;
        Ok(projection)
    }
    fn validate_shape(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "unsupported Router terminal replay version");
        for value in [
            &self.selected_candidate_id,
            &self.capability_id,
            &self.from_model,
            &self.to_model,
        ] {
            anyhow::ensure!(
                !value.is_empty() && value.len() <= 512,
                "Router replay identity exceeds bounds"
            );
        }
        let read = crate::causal_tree::ids::DecisionId::parse(&self.read_decision_id)?;
        let next = crate::causal_tree::ids::DecisionId::parse(&self.next_decision_id)?;
        anyhow::ensure!(
            read != next && self.source_sequence > 0,
            "Router replay transition identity invalid"
        );
        for value in [&self.read_state_fingerprint, &self.next_state_fingerprint] {
            anyhow::ensure!(
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Router replay state fingerprint invalid"
            );
        }
        anyhow::ensure!(
            self.remaining_budget
                .iter()
                .zip(self.original_budget)
                .all(|(left, before)| *left <= before),
            "Router replay increased the turn allowance"
        );
        if self.readmission_succeeded {
            anyhow::ensure!(
                self.failure_stage.is_none()
                    && self.history_snapshot_sha256.is_some()
                    && self.pmm_route_binding.is_some(),
                "Router successful readmission lacks issued evidence"
            );
        } else {
            anyhow::ensure!(
                matches!(
                    self.failure_stage.as_deref(),
                    Some("grant" | "record" | "read" | "embedding" | "rank" | "pmm" | "admission")
                ) && self.pmm_route_binding.is_none()
                    && self.from_provider_binding == self.to_provider_binding
                    && self.from_model == self.to_model,
                "Router failed read phase changed model authority"
            );
        }
        Ok(())
    }
    pub(super) fn validate_source(&self, source: &MessageEvent) -> anyhow::Result<()> {
        self.validate_shape()?;
        let query_sha256: [u8; 32] = Sha256::digest(source.content.as_bytes()).into();
        anyhow::ensure!(
            self.source_sequence == source.id
                && source.source == crate::memory::MessageEventSource::Agent
                && source.channel.as_deref() == Some("agent_api")
                && source.sender.as_deref() == Some("embedded-caller")
                && self.query_sha256 == query_sha256,
            "Router replay input differs from its selected read"
        );
        Ok(())
    }
    fn row_kind(&self, row: &DecisionReplayRecord) -> anyhow::Result<u8> {
        let id = row.decision.decision_id.to_string();
        if id == self.read_decision_id {
            anyhow::ensure!(
                row.state_fingerprint == self.read_state_fingerprint
                    && row.catalog_generation == hex::encode(self.catalog_generation)
                    && row.decision.directive_kind == "retrieve"
                    && row.decision.selected_candidate_id.as_deref() == Some(self.selected_candidate_id.as_str()),
                "Router selected read row differs from its authenticated phase"
            );
            let mut selected = row
                .decision
                .candidates
                .iter()
                .filter(|candidate| candidate.id == self.selected_candidate_id);
            let candidate = selected.next().context("Router selected read candidate missing")?;
            anyhow::ensure!(
                selected.next().is_none() && candidate.action == "retrieve" && candidate.target == "memory",
                "Router selected read candidate differs from memory Retrieve"
            );
            Ok(1)
        } else if id == self.next_decision_id {
            anyhow::ensure!(
                row.state_fingerprint == self.next_state_fingerprint
                    && row.catalog_generation == hex::encode(self.catalog_generation)
                    && row.decision.prior_decision_id.map(|id| id.to_string()).as_deref()
                        == Some(self.read_decision_id.as_str()),
                "Router model readmission is not the actual dependent decision"
            );
            Ok(2)
        } else {
            Ok(0)
        }
    }
    /// Called only after the enclosing capture has compared the full actual
    /// row digest with the service-issued DecisionRecordBuilder result.
    pub(super) fn capture_row(&mut self, row: &DecisionReplayRecord) -> anyhow::Result<()> {
        match self.row_kind(row)? {
            1 => {
                anyhow::ensure!(self.read_digest.is_empty(), "duplicate Router read hop");
                self.read_digest = row.digest.clone();
            }
            2 => {
                anyhow::ensure!(self.next_digest.is_empty(), "duplicate Router next hop");
                self.next_digest = row.digest.clone();
            }
            _ => {}
        }
        Ok(())
    }
    pub(super) fn validate_complete(&self) -> anyhow::Result<()> {
        self.validate_shape()?;
        for value in [&self.read_digest, &self.next_digest] {
            anyhow::ensure!(
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Router replay lacks both durable decision receipts"
            );
        }
        Ok(())
    }
    /// The caller already checked owner, source, chain order and signed hop digest.
    pub(super) fn verify_row(&self, row: &DecisionReplayRecord) -> anyhow::Result<u8> {
        let kind = self.row_kind(row)?;
        anyhow::ensure!(
            match kind {
                1 => row.digest == self.read_digest,
                2 => row.digest == self.next_digest,
                _ => true,
            },
            "Router historical decision receipt changed"
        );
        Ok(kind)
    }
    pub(super) fn validate_serving(
        &self,
        key: &DecisionKey,
        outcome: Option<&ProviderExecutionOutcome>,
        status: TurnTerminalStatus,
        fixed_close: bool,
    ) -> anyhow::Result<()> {
        if !self.readmission_succeeded {
            anyhow::ensure!(
                outcome.is_none()
                    && (fixed_close || matches!(status, TurnTerminalStatus::Failed | TurnTerminalStatus::Cancelled)),
                "Router failed read phase cannot authorize a provider outcome"
            );
            return Ok(());
        }
        let Some(outcome) = outcome else {
            anyhow::ensure!(
                fixed_close || matches!(status, TurnTerminalStatus::Failed | TurnTerminalStatus::Cancelled),
                "Router replay completed without a provider outcome or fixed close"
            );
            return Ok(());
        };
        let first = outcome
            .attempts
            .iter()
            .find(|attempt| attempt.status != AttemptStatus::Skipped)
            .context("Router provider outcome lacks an actual attempt")?;
        anyhow::ensure!(
            router_provider_binding(key, &first.provider) == self.to_provider_binding && first.model == self.to_model,
            "Router first provider attempt differs from the readmitted model"
        );
        let success = outcome
            .attempts
            .iter()
            .rev()
            .find(|attempt| attempt.status == AttemptStatus::Success);
        match &outcome.status {
            ExecutionStatus::Success | ExecutionStatus::FallbackSuccess => {
                let success = success.context("Router successful outcome lacks a successful physical attempt")?;
                anyhow::ensure!(
                    router_provider_binding(key, &success.provider)
                        == router_provider_binding(key, &outcome.final_provider)
                        && success.model == outcome.final_model,
                    "Router served outcome differs from its successful attempt"
                );
            }
            ExecutionStatus::AllFailed { .. } => {
                anyhow::ensure!(
                    success.is_none() && !matches!(status, TurnTerminalStatus::Completed | TurnTerminalStatus::Silent),
                    "Router all-failed trace was relabeled as a successful terminal"
                );
                // Failed-call final labels are not evidence of a serving member.
            }
        }
        Ok(())
    }
}
