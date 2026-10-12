//! Authenticated failure before configuration load or worker execution.
use super::protocol::WorkerResult;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};

const DOMAIN: &str = "prx-worker-startup-failure-v1";
const PHASE: &str = "initial-config-source";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartupFailureProof {
    phase: String,
    run_id: String,
    manifest_digest: [u8; 32],
    seal: [u8; 32],
}

/// Existing result fields remain unchanged, including legacy JSON decoding.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkerOutput {
    #[serde(flatten)]
    pub(crate) result: WorkerResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) authenticated_startup_failure: Option<StartupFailureProof>,
}

fn validate_failure(result: &WorkerResult) -> anyhow::Result<()> {
    anyhow::ensure!(
        !result.success
            && result.output.is_empty()
            && result.error.as_deref().is_some_and(|error| !error.is_empty())
            && result.tokens_used.is_none()
            && result.completion_evidence.is_none()
            && result.ordinary_completion.is_none()
            && result.operator_completion.is_none(),
        "startup failure conflicts with execution or completion evidence"
    );
    Ok(())
}

impl StartupFailureProof {
    fn signing_bytes(&self, result: &WorkerResult) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            DOMAIN,
            &self.phase,
            &self.run_id,
            &self.manifest_digest,
            result,
        ))?)
    }
}

impl WorkerOutput {
    pub(crate) fn verify_startup(
        &self,
        key: &ring::hmac::Key,
        manifest_digest: [u8; 32],
        run_id: &str,
    ) -> anyhow::Result<()> {
        validate_failure(&self.result)?;
        let proof = self
            .authenticated_startup_failure
            .as_ref()
            .context("startup proof missing")?;
        anyhow::ensure!(
            proof.phase == PHASE
                && !run_id.is_empty()
                && proof.run_id == run_id
                && proof.manifest_digest == manifest_digest,
            "startup failure phase or launch binding differs"
        );
        ring::hmac::verify(key, &proof.signing_bytes(&self.result)?, &proof.seal)
            .map_err(|_| anyhow::anyhow!("startup failure authentication failed"))
    }
}

/// Only the worker's authenticated initial config preflight constructs this error.
#[derive(Debug)]
pub(super) struct AuthenticatedStartupFailure(WorkerOutput);

impl AuthenticatedStartupFailure {
    pub(super) fn attest(
        key: &ring::hmac::Key,
        manifest_digest: [u8; 32],
        run_id: &str,
        error: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(!run_id.is_empty(), "startup failure run identity missing");
        let result = WorkerResult {
            completion_evidence: None,
            ordinary_completion: None,
            operator_completion: None,
            success: false,
            output: String::new(),
            error: Some(error),
            tokens_used: None,
        };
        validate_failure(&result)?;
        let mut proof = StartupFailureProof {
            phase: PHASE.to_owned(),
            run_id: run_id.to_owned(),
            manifest_digest,
            seal: [0; 32],
        };
        proof.seal = ring::hmac::sign(key, &proof.signing_bytes(&result)?)
            .as_ref()
            .try_into()
            .context("startup HMAC length differs")?;
        Ok(Self(WorkerOutput {
            result,
            authenticated_startup_failure: Some(proof),
        }))
    }

    pub(super) const fn envelope(&self) -> &WorkerOutput {
        &self.0
    }
}

impl std::fmt::Display for AuthenticatedStartupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0.result.error.as_deref().unwrap_or("worker startup failed"))
    }
}

impl std::error::Error for AuthenticatedStartupFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ring::hmac::Key {
        ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx-startup-proof-fixture")
    }

    fn envelope() -> WorkerOutput {
        AuthenticatedStartupFailure::attest(&key(), [7; 32], "prx-run-a", "config source unavailable".to_owned())
            .expect("fixture proof")
            .0
    }

    #[test]
    fn authenticated_failure_roundtrip_preserves_legacy_result() {
        let original = envelope();
        let bytes = serde_json::to_vec(&original).expect("serialize");
        let parsed: WorkerOutput = serde_json::from_slice(&bytes).expect("decode");
        parsed
            .verify_startup(&key(), [7; 32], "prx-run-a")
            .expect("valid proof");
        let legacy: WorkerResult = serde_json::from_slice(&bytes).expect("legacy result");
        assert!(!legacy.success);
        assert_eq!(legacy.error.as_deref(), Some("config source unavailable"));
    }

    #[test]
    fn startup_proof_rejects_cross_run_manifest_and_key() {
        let proof = envelope();
        assert!(proof.verify_startup(&key(), [7; 32], "prx-run-b").is_err());
        assert!(proof.verify_startup(&key(), [8; 32], "prx-run-a").is_err());
        let wrong = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx-other-key");
        assert!(proof.verify_startup(&wrong, [7; 32], "prx-run-a").is_err());
    }

    #[test]
    fn startup_proof_rejects_payload_success_output_and_phase_mutation() {
        let mut proof = envelope();
        proof.result.error = Some("different failure".to_owned());
        assert!(proof.verify_startup(&key(), [7; 32], "prx-run-a").is_err());
        let mut proof = envelope();
        proof.result.success = true;
        assert!(proof.verify_startup(&key(), [7; 32], "prx-run-a").is_err());
        let mut proof = envelope();
        proof.result.output = "provider output".to_owned();
        assert!(proof.verify_startup(&key(), [7; 32], "prx-run-a").is_err());
        let mut proof = envelope();
        proof.authenticated_startup_failure.as_mut().expect("proof").phase = "after-provider".to_owned();
        assert!(proof.verify_startup(&key(), [7; 32], "prx-run-a").is_err());
    }

    #[test]
    fn startup_proof_rejects_unsigned_and_duplicate_envelopes() {
        let mut unsigned = envelope();
        unsigned.authenticated_startup_failure = None;
        assert!(unsigned.verify_startup(&key(), [7; 32], "prx-run-a").is_err());
        let proof = envelope();
        let evidence = serde_json::to_string(&proof.authenticated_startup_failure).expect("proof JSON");
        let duplicate = format!(
            "{{\"success\":false,\"output\":\"\",\"error\":\"failure\",\"authenticated_startup_failure\":{evidence},\"authenticated_startup_failure\":{evidence}}}"
        );
        assert!(serde_json::from_str::<WorkerOutput>(&duplicate).is_err());
    }
    #[test]
    fn startup_proof_rejects_usage_and_competing_completion() {
        let mut proof = envelope();
        proof.result.tokens_used = Some(crate::llm::route_decision::TokenUsage::default());
        assert!(proof.verify_startup(&key(), [7; 32], "prx-run-a").is_err());
        let mut proof = envelope();
        proof.result.ordinary_completion = Some(super::super::protocol::OrdinaryWorkerCompletionEvidence {
            run_id: "prx-run-a".to_owned(),
            process_id: "prx-process".to_owned(),
            input_event_id: None,
            recording_enabled: false,
            terminal: None,
            recorded_segments: 0,
            confirmed_segments: Vec::new(),
            segments: Vec::new(),
            usage: Vec::new(),
            seal: [0; 32],
        });
        assert!(proof.verify_startup(&key(), [7; 32], "prx-run-a").is_err());
    }

    #[test]
    fn startup_proof_rejects_forged_seal_and_duplicate_result_fields() {
        let mut proof = envelope();
        proof.authenticated_startup_failure.as_mut().expect("proof").seal = [0; 32];
        assert!(proof.verify_startup(&key(), [7; 32], "prx-run-a").is_err());
        let original = envelope();
        let json = serde_json::to_string(&original).expect("envelope JSON");
        let duplicate = json.replacen('{', "{\"success\":false,", 1);
        assert!(serde_json::from_str::<WorkerOutput>(&duplicate).is_err());
    }
}
