//! Reference discovery under the existing exclusive capture-pruner owner.
//! Unknown evidence aborts deletion; no adoption/evaluation GC or recovery.
use super::context::LessonLimits;
use super::lookup::LookupBudget;
use super::record::{AdoptionRecord, RECORD_CAP, read_bounded_bytes, refusal};
use crate::skills::lifecycle::experiment::{ExperimentEvent, ExperimentSpec, StageState};
use crate::skills::lifecycle::generation::StoreLock;
use crate::skills::lifecycle::lesson_delivery::DeliveryRetentionLimits;
use crate::skills::lifecycle::{LifecycleError, SkillLifecycleStore};
use std::collections::BTreeSet;
use std::fs;
use std::time::{Duration, Instant};

impl SkillLifecycleStore {
    /// All returned identities are retention references, never eligibility or
    /// publication grants. No expiry can release an unknown delivery pin.
    pub(in crate::skills::lifecycle) fn lesson_retention_runs_locked(
        &self,
        owner: &StoreLock,
    ) -> Result<BTreeSet<String>, LifecycleError> {
        let limits = LessonLimits::default();
        let mut budget = LookupBudget::new(limits)?;
        let mut runs = BTreeSet::new();
        let delivery = self
            .delivery_retention_refs_locked(
                owner,
                DeliveryRetentionLimits {
                    max_records: usize::try_from(limits.examined).unwrap_or(64),
                    max_decoded_bytes: limits.decoded_bytes,
                    deadline: Instant::now()
                        .checked_add(Duration::from_millis(limits.elapsed_ms))
                        .ok_or_else(|| refusal("retention deadline overflow"))?,
                },
            )
            .map_err(|_| refusal("delivery references are unknown; experience pruning refused"))?;
        for _ in 0..delivery.examined_records {
            budget.examine()?;
        }
        budget.charge(delivery.decoded_bytes)?;
        for reference in delivery.refs {
            let path = self.adoption_record_path(&reference.adoption_id)?;
            budget.examine()?;
            let bytes = read_bounded_bytes(&path, RECORD_CAP)?;
            budget.charge(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
            let record = self.decode_adoption(&bytes, &reference)?;
            let barrier = read_bounded_bytes(&self.adoption_retention_barrier_path(&record)?, RECORD_CAP)?;
            budget.charge(u64::try_from(barrier.len()).unwrap_or(u64::MAX))?;
            self.check_adoption_retention_barrier(&record, &barrier)?;
        }
        let record_dir = self.adoption_directory().join("records");
        match fs::read_dir(&record_dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LifecycleError::io(&record_dir, error)),
            Ok(entries) => {
                for entry in entries {
                    budget.examine()?;
                    let path = entry.map_err(|error| LifecycleError::io(&record_dir, error))?.path();
                    let bytes = read_bounded_bytes(&path, RECORD_CAP)?;
                    budget.charge(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
                    let (record, _): (AdoptionRecord, _) =
                        self.read_adoption_sealed(&bytes, b"prx-adopted-history-v1")?;
                    if self.adoption_record_path(&record.adoption_id)? != path {
                        return Err(refusal("unknown adoption record path; pruning refused"));
                    }
                    self.decode_adoption(&bytes, &record.reference()?)?;
                    let barrier = read_bounded_bytes(&self.adoption_retention_barrier_path(&record)?, RECORD_CAP)?;
                    budget.charge(u64::try_from(barrier.len()).unwrap_or(u64::MAX))?;
                    self.check_adoption_retention_barrier(&record, &barrier)?;
                    // This also retains orphans after an uncertain status-index
                    // write. They are not silently recreated or made eligible.
                    self.read_proposal(&record.proposal_id)?
                        .ok_or_else(|| refusal("retained adoption proposal missing"))?;
                    budget.check()?;
                }
            }
        }
        let directory = self.root().join("experiments");
        match fs::read_dir(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LifecycleError::io(&directory, error)),
            Ok(entries) => {
                for entry in entries {
                    budget.examine()?;
                    let path = entry.map_err(|error| LifecycleError::io(&directory, error))?.path();
                    let bytes = read_bounded_bytes(&path.join("spec.json"), limits.index_bytes)?;
                    budget.charge(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
                    let spec: ExperimentSpec =
                        serde_json::from_slice(&bytes).map_err(|_| refusal("retained experiment spec invalid"))?;
                    let plan = spec.freeze().map_err(|_| refusal("retained experiment spec invalid"))?;
                    if path.file_name().and_then(|name| name.to_str()) != Some(plan.spec().id.as_str())
                        || plan.spec().owner_scope != *self.owner_scope()
                    {
                        return Err(refusal("retained experiment owner changed"));
                    }
                    // Bound each journal input before replay. This is an existing
                    // journal owner read, with no transition, repair or dispatch.
                    for item in fs::read_dir(&path).map_err(|error| LifecycleError::io(&path, error))? {
                        let item = item.map_err(|error| LifecycleError::io(&path, error))?;
                        budget.check()?;
                        if item.file_name() == "spec.json" {
                            continue;
                        }
                        let bytes = read_bounded_bytes(&item.path(), RECORD_CAP)?;
                        budget.charge(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
                    }
                    let journal = self
                        .read_experiment(&plan.spec().id, plan.digest())
                        .map_err(|_| refusal("retained experiment history invalid"))?;
                    if journal.state.stages.values().any(|state| {
                        matches!(
                            state,
                            StageState::Reserved
                                | StageState::Dispatched
                                | StageState::DispatchUnknown
                                | StageState::Settled {
                                    cleanup_confirmed: false,
                                    ..
                                }
                        )
                    }) {
                        return Err(refusal(
                            "active or unresolved experiment protects unknown experience references",
                        ));
                    }
                    for entry in &journal.entries {
                        if let ExperimentEvent::Settled {
                            stage,
                            reference: Some(reference),
                            evidence_sha256,
                            ..
                        } = &entry.event
                        {
                            if reference == &format!("probe:{stage}") {
                                let probe_path = self
                                    .root()
                                    .join("experiment-evidence")
                                    .join(plan.spec().id.as_str())
                                    .join(format!("{stage}.json"));
                                let bytes = read_bounded_bytes(&probe_path, limits.index_bytes)?;
                                budget.charge(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
                                let probe = self
                                    .read_development_probe(&plan.spec().id, stage, evidence_sha256)
                                    .map_err(|_| refusal("retained probe history invalid"))?;
                                if probe.binding.experiment_sha256 != plan.digest() {
                                    return Err(refusal("retained probe belongs to another experiment"));
                                }
                                runs.extend(probe.cases.into_iter().map(|case| case.run.run_id));
                            }
                        }
                    }
                    budget.check()?;
                }
            }
        }
        budget.check()?;
        Ok(runs)
    }
}
