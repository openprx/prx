//! Bounded source-owned Router training history. These are storage contracts,
//! not a grant to inspect user memories or an attestation of embedding cost.
use std::time::Instant;

use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};

use super::traits::RouterLearningEvidence;

pub const ROUTER_HISTORY_MAX_RECORDS: usize = 1024;
pub const ROUTER_HISTORY_MAX_DIMENSIONS: usize = 2048;
pub const ROUTER_HISTORY_MAX_INPUT_BYTES: usize = 65_536;
pub const ROUTER_HISTORY_MAX_METADATA_BYTES: usize = 32 * 1024;

/// Exact training domain. Values alone confer no read or write authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouterHistoryDomain {
    pub workspace: String,
    pub owner: String,
    pub session: String,
    pub policy_sha256: [u8; 32],
    pub embedding_model: String,
    pub dimensions: u16,
    pub embedding_version: u16,
}

impl RouterHistoryDomain {
    pub(crate) fn validate(&self) -> Result<()> {
        for value in [&self.workspace, &self.owner, &self.session, &self.embedding_model] {
            ensure!(
                !value.is_empty() && value.len() <= 512,
                "Router history domain exceeds identity bounds"
            );
        }
        ensure!(
            (1..=ROUTER_HISTORY_MAX_DIMENSIONS).contains(&usize::from(self.dimensions)) && self.embedding_version == 1,
            "unsupported Router history embedding descriptor"
        );
        Ok(())
    }

    pub(crate) fn key(&self) -> Result<String> {
        self.validate()?;
        let mut digest = Sha256::new();
        digest.update(b"prx-router-history-domain-v1");
        for value in [&self.workspace, &self.owner, &self.session, &self.embedding_model] {
            hash_field(&mut digest, value.as_bytes());
        }
        digest.update(self.policy_sha256);
        digest.update(self.dimensions.to_be_bytes());
        digest.update(self.embedding_version.to_be_bytes());
        Ok(hex::encode(digest.finalize()))
    }
}

/// Native embedded input expected by the actual selected read. Every other
/// authority field must be absent as in the existing Router learning contract.
/// The backend still compares these bytes against the live durable source.
pub(super) struct RouterHistorySource {
    pub event_id: String,
    pub run: String,
    pub generation: i64,
    pub revision: String,
    pub input: String,
    pub raw_payload: Option<String>,
}

/// Issued only from an actual purpose-bound selection in the runtime's unique
/// routing phase. Consumed by the storage call; not Clone or deserializable.
pub struct RouterHistoryReadEvidence {
    pub(super) source: RouterHistorySource,
    pub(super) domain: RouterHistoryDomain,
    pub(super) decision_id: String,
    pub(super) capability_id: String,
    pub(super) query_sha256: [u8; 32],
    pub(super) deadline: Instant,
}

impl RouterHistoryReadEvidence {
    #[cfg(feature = "llm-router")]
    pub(crate) fn from_selected(
        grant: crate::causal_tree::decision_engine::SelectedRouterReadGrant,
        source: &dyn crate::runtime::turn_source::TurnSourceProof,
        witness: &crate::agent::embedded_source::VerifiedEmbeddedInputMetadata,
        domain: RouterHistoryDomain,
        deadline: Instant,
    ) -> Result<Self> {
        ensure!(
            grant.matches_source(
                source.workspace_id(),
                source.owner_id(),
                source.session_key(),
                source.run_id(),
                source.source_event_id()
            ),
            "Router grant differs from original source"
        );
        let (decision, reference, purpose, granted_deadline) = grant.into_parts();
        let domain_digest = hex::decode(domain.key()?)?;
        ensure!(
            domain_digest.as_slice() == purpose.domain_sha256
                && domain.workspace == source.workspace_id()
                && domain.owner == source.owner_id()
                && domain.session == source.session_key(),
            "Router grant differs from history domain"
        );
        let raw = witness.verified_raw_for(source)?;
        // Bounds are checked before cloning any caller-visible source bytes.
        ensure!(
            source.routing_input().len() <= ROUTER_HISTORY_MAX_INPUT_BYTES
                && raw.is_none_or(|raw| raw.len() <= ROUTER_HISTORY_MAX_METADATA_BYTES),
            "Router selected source exceeds byte capacity"
        );
        let evidence = Self {
            source: RouterHistorySource {
                event_id: source
                    .source_event_id()
                    .ok_or_else(|| anyhow::anyhow!("Router source missing"))?
                    .to_owned(),
                run: source.run_id().to_owned(),
                generation: i64::try_from(source.config_generation_id())?,
                revision: source.config_source_revision().to_owned(),
                input: source.routing_input().to_owned(),
                raw_payload: raw.map(str::to_owned),
            },
            domain,
            decision_id: decision.to_string(),
            capability_id: reference.id.as_str().to_owned(),
            query_sha256: purpose.query_sha256,
            deadline: granted_deadline,
        };
        evidence.validate(deadline)?;
        Ok(evidence)
    }

    pub(super) fn validate(&self, deadline: Instant) -> Result<()> {
        self.domain.validate()?;
        ensure!(
            deadline <= self.deadline && Instant::now() < deadline,
            "Router history read exceeds its selected deadline"
        );
        ensure!(
            !self.source.event_id.is_empty()
                && self.source.event_id.len() <= 512
                && !self.source.run.is_empty()
                && self.source.run.len() <= 512
                && !self.source.revision.is_empty()
                && self.source.revision.len() <= 512
                && self.source.generation >= 0
                && self.source.input.len() <= ROUTER_HISTORY_MAX_INPUT_BYTES
                && self
                    .source
                    .raw_payload
                    .as_ref()
                    .is_none_or(|raw| raw.len() <= ROUTER_HISTORY_MAX_METADATA_BYTES),
            "Router history source exceeds bounds"
        );
        ensure!(
            !self.decision_id.is_empty()
                && self.decision_id.len() <= 128
                && !self.capability_id.is_empty()
                && self.capability_id.len() <= 512,
            "Router history selection identity exceeds bounds"
        );
        let actual: [u8; 32] = Sha256::digest(self.source.input.as_bytes()).into();
        ensure!(
            actual == self.query_sha256,
            "Router history query differs from its selected input"
        );
        Ok(())
    }
}

/// Fully decoded bounded history row. A malformed row is an error, not a row
/// silently omitted from KNN's threshold or nearest-neighbor population.
pub struct RouterHistoryRow {
    pub source_sequence: i64,
    pub terminal_sequence: i64,
    pub terminal_event_id: String,
    pub provider: String,
    pub model: String,
    pub success: bool,
    pub embedding: Vec<f32>,
}

pub struct RouterHistoryRead {
    pub source_sequence: i64,
    pub rows: Vec<RouterHistoryRow>,
}

impl RouterHistoryRead {
    pub(crate) fn validate(&self, domain: &RouterHistoryDomain) -> Result<()> {
        domain.validate()?;
        ensure!(
            self.source_sequence > 0 && self.rows.len() <= ROUTER_HISTORY_MAX_RECORDS,
            "Router history exceeds complete-domain capacity"
        );
        let mut terminals = std::collections::HashSet::new();
        for row in &self.rows {
            ensure!(
                row.source_sequence > 0 && row.source_sequence < row.terminal_sequence,
                "Router history includes an outcome after its admitted input"
            );
            ensure!(
                row.terminal_sequence < self.source_sequence,
                "Router history includes an outcome after its admitted input"
            );
            for value in [&row.terminal_event_id, &row.provider, &row.model] {
                ensure!(
                    !value.is_empty() && value.len() <= 512,
                    "Router history row exceeds identity bounds"
                );
            }
            ensure!(
                terminals.insert(row.terminal_event_id.as_str()),
                "Router history duplicates a terminal"
            );
            validate_vector(&row.embedding, domain.dimensions)?;
        }
        Ok(())
    }
}

/// Actual terminal evidence plus the locally retained, source-bound vector.
/// Read permission does not issue this write authority.
pub struct RouterHistoryWriteEvidence {
    pub(super) terminal: RouterLearningEvidence,
    pub(super) domain: RouterHistoryDomain,
    pub(super) embedding: Vec<f32>,
}

impl RouterHistoryWriteEvidence {
    #[cfg(feature = "llm-router")]
    pub(crate) fn from_terminal(
        terminal: RouterLearningEvidence,
        embedding: crate::runtime::turn_decision::RouterEmbedding,
    ) -> Result<Self> {
        let (source_event_id, query_sha256, domain, embedding) = embedding.into_parts();
        ensure!(
            source_event_id == terminal.source_event_id
                && <[u8; 32]>::from(Sha256::digest(terminal.source_input.as_bytes())) == query_sha256,
            "Router training vector belongs to another actual input"
        );
        let evidence = Self {
            terminal,
            domain,
            embedding,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub(super) fn validate(&self) -> Result<()> {
        self.terminal.validate()?;
        // Provider-performance learning may retain a successful silent response,
        // but no delivered answer means no answer-training authority.
        #[derive(serde::Deserialize)]
        struct TerminalStatus {
            status: crate::agent::terminal::TurnTerminalStatus,
            execution_trace: Option<SilentObservation>,
        }
        #[derive(serde::Deserialize)]
        struct SilentObservation {
            #[serde(default)]
            authorized_silent: bool,
        }
        let status: TerminalStatus = serde_json::from_str(
            self.terminal
                .terminal
                .raw_payload_json
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Router training terminal payload missing"))?,
        )?;
        ensure!(
            status.status != crate::agent::terminal::TurnTerminalStatus::Silent
                && !status.execution_trace.is_some_and(|trace| trace.authorized_silent),
            "silent terminal cannot authorize Router answer training"
        );
        self.domain.validate()?;
        let terminal = &self.terminal.terminal;
        ensure!(
            terminal.workspace_id == self.domain.workspace
                && terminal.owner_id.as_deref() == Some(self.domain.owner.as_str())
                && terminal.session_key.as_deref() == Some(self.domain.session.as_str())
                && self.terminal.policy == hex::encode(self.domain.policy_sha256),
            "Router history terminal differs from its training domain"
        );
        validate_vector(&self.embedding, self.domain.dimensions)
    }

    pub(super) fn vector_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(self.embedding.len() * 4);
        for value in &self.embedding {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Ok(bytes)
    }

    pub(super) fn fingerprint(&self) -> Result<[u8; 32]> {
        self.validate()?;
        let mut digest = Sha256::new();
        digest.update(b"prx-router-history-record-v1");
        hash_field(&mut digest, self.domain.key()?.as_bytes());
        hash_field(&mut digest, self.terminal.fingerprint()?.as_bytes());
        for value in &self.embedding {
            digest.update(value.to_le_bytes());
        }
        Ok(digest.finalize().into())
    }
}

/// The digest read from the actual committed/idempotently replayed row.
pub struct RouterHistoryCommitReceipt {
    pub terminal_event_id: String,
    pub record_sha256: [u8; 32],
    pub replayed: bool,
}

pub(super) fn decode_vector(bytes: &[u8], dimensions: u16) -> Result<Vec<f32>> {
    let dimensions = usize::from(dimensions);
    ensure!(
        (1..=ROUTER_HISTORY_MAX_DIMENSIONS).contains(&dimensions) && bytes.len() == dimensions * 4,
        "Router history vector exceeds its fixed encoding bounds"
    );
    let mut vector = Vec::with_capacity(dimensions);
    for chunk in bytes.as_chunks::<4>().0 {
        let value = f32::from_le_bytes(*chunk);
        ensure!(value.is_finite(), "Router history vector is non-finite");
        vector.push(value);
    }
    Ok(vector)
}

fn validate_vector(vector: &[f32], dimensions: u16) -> Result<()> {
    ensure!(
        vector.len() == usize::from(dimensions)
            && (1..=ROUTER_HISTORY_MAX_DIMENSIONS).contains(&vector.len())
            && vector.iter().all(|value| value.is_finite()),
        "invalid Router history vector"
    );
    Ok(())
}

fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

#[cfg(test)]
mod codec_tests;
