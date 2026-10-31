//! One owner-authenticated admission envelope, inside the original frozen spec.
//! Its data cannot recreate the private live entrypoint continuation.
use super::{ControlledAdmissionPin, ExperimentError, ExperimentId, ExperimentSpec, FrozenExperiment};
use crate::config::Config;
use crate::decision::{DecisionKey, KeyDomain};
use crate::skills::lifecycle::SkillLifecycleStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

const CONTROLLED_PREFIX: &str = "h2c-";
const DOMAIN: &[u8] = b"prx-h2-controlled-admission-v1\0";

/// Public verification data, never a source capability or an approval grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlledAdmissionEnvelope {
    pub pin: ControlledAdmissionPin,
    pub owner_mac: String,
}
fn refusal(reason: &str) -> ExperimentError {
    ExperimentError::InvalidEvidence(reason.into())
}
fn unsigned_digest(spec: &ExperimentSpec) -> Result<String, ExperimentError> {
    let mut unsigned = spec.clone();
    unsigned.controlled_admission = None;
    Ok(unsigned.freeze()?.digest().into())
}
impl SkillLifecycleStore {
    fn admission_workspace(&self) -> Result<&Path, ExperimentError> {
        self.root()
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| refusal("controlled admission has no owner workspace"))
    }
    fn admission_key(&self) -> Result<std::sync::Arc<DecisionKey>, ExperimentError> {
        DecisionKey::load_existing(self.admission_workspace()?)
            .map_err(|_| refusal("the existing admission owner key is unavailable"))
    }
    fn admission_mac_parts(
        &self,
        spec: &ExperimentSpec,
        pin: &ControlledAdmissionPin,
    ) -> Result<(String, Vec<u8>), ExperimentError> {
        if pin.version != 1
            || pin.source.version != 1
            || !matches!(pin.source.kind.as_str(), "terminal" | "gateway")
            || pin.source.target_owner != self.owner_scope().as_str()
            || pin.source.workspace != self.admission_workspace()?.to_string_lossy()
            || spec.owner_scope != *self.owner_scope()
        {
            return Err(refusal("controlled admission source owner/workspace differs"));
        }
        let payload = serde_json::to_vec(pin).map_err(|_| refusal("admission pin encoding failed"))?;
        Ok((unsigned_digest(spec)?, payload))
    }
    /// The only planner that stamps controlled input. The actual private live
    /// source scope supplies the pin; caller draft fields never do.
    pub(in crate::skills::lifecycle) fn freeze_experiment_admission(
        &self,
        mut spec: ExperimentSpec,
        config: &Config,
    ) -> Result<FrozenExperiment, ExperimentError> {
        if spec.controlled_admission.is_some() {
            return Err(refusal("a draft cannot supply an authenticated admission envelope"));
        }
        let pin = crate::runtime::h2_entry::current_admission_pin(config)
            .map_err(|_| refusal("controlled planning has no valid live source scope"))?;
        if let Some(pin) = pin {
            let identity = serde_json::to_vec(&("prx-h2-controlled-id-v1", spec.id.as_str(), &pin))
                .map_err(|_| refusal("controlled identity encoding failed"))?;
            let suffix: String = hex::encode(Sha256::digest(identity)).chars().take(60).collect();
            spec.id = ExperimentId::parse(&format!("{CONTROLLED_PREFIX}{suffix}"))?;
            let (unsigned, payload) = self.admission_mac_parts(&spec, &pin)?;
            let owner_mac = hex::encode(self.admission_key()?.hmac(
                KeyDomain::ApprovalState,
                &[
                    DOMAIN,
                    self.owner_scope().as_str().as_bytes(),
                    spec.id.as_str().as_bytes(),
                    unsigned.as_bytes(),
                    &payload,
                ],
            ));
            spec.controlled_admission = Some(ControlledAdmissionEnvelope { pin, owner_mac });
        } else if spec.id.as_str().starts_with(CONTROLLED_PREFIX) {
            return Err(refusal("the controlled experiment namespace requires a live source"));
        }
        let plan = spec.freeze()?;
        self.verify_experiment_admission(&plan)?;
        Ok(plan)
    }
    /// Read verification never upgrades data into live runtime authority.
    pub(in crate::skills::lifecycle) fn verify_experiment_admission(
        &self,
        plan: &FrozenExperiment,
    ) -> Result<(), ExperimentError> {
        let spec = plan.spec();
        let reserved = spec.id.as_str().starts_with(CONTROLLED_PREFIX);
        match spec.controlled_admission.as_ref() {
            None if !reserved => Ok(()),
            Some(envelope) if reserved => {
                let suffix = spec
                    .id
                    .as_str()
                    .strip_prefix(CONTROLLED_PREFIX)
                    .ok_or_else(|| refusal("missing controlled identity namespace"))?;
                if suffix.len() != 60
                    || !suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    || envelope.owner_mac.len() != 64
                    || !envelope
                        .owner_mac
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(refusal("controlled identity or owner MAC is not canonical"));
                }
                let (unsigned, payload) = self.admission_mac_parts(spec, &envelope.pin)?;
                let tag = hex::decode(&envelope.owner_mac).map_err(|_| refusal("invalid admission owner MAC"))?;
                if !self.admission_key()?.verify_hmac(
                    KeyDomain::ApprovalState,
                    &[
                        DOMAIN,
                        self.owner_scope().as_str().as_bytes(),
                        spec.id.as_str().as_bytes(),
                        unsigned.as_bytes(),
                        &payload,
                    ],
                    &tag,
                ) {
                    return Err(refusal("controlled admission owner authentication failed"));
                }
                Ok(())
            }
            _ => Err(refusal("controlled admission namespace/envelope cannot be downgraded")),
        }
    }
    /// Called under the original admission locks before either creation or
    /// idempotent admission. A copied MAC does not authorize a new live run.
    pub(in crate::skills::lifecycle) fn require_live_experiment_admission(
        &self,
        plan: &FrozenExperiment,
    ) -> Result<(), ExperimentError> {
        self.verify_experiment_admission(plan)?;
        let current = crate::runtime::h2_entry::current_run_continuation();
        match (plan.spec().controlled_admission.as_ref(), current) {
            (None, None) => Ok(()),
            (Some(envelope), Some(current)) => current
                .ensure_store_admission(
                    &envelope.pin,
                    plan,
                    self.admission_workspace()?,
                    self.owner_scope().as_str(),
                )
                .map_err(|_| refusal("controlled admission has no matching live owned continuation")),
            _ => Err(refusal("controlled admission cannot fall back to a legacy source")),
        }
    }
}
