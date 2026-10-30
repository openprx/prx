//! Resolve experiment inputs from existing runtime and lifecycle authorities.
use super::{Config, Context, Result, SkillsLifecycleService, ensure};
use crate::skills::lifecycle::experiment::ExperimentPins;
use crate::skills::lifecycle::{EvalSplit, ModelBinding, SkillId};
use sha2::{Digest, Sha256};
use std::io::Read;
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn canonical_config(config: &Config) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(config)?;
    value.sort_all_objects();
    Ok(serde_json::to_vec(&value)?)
}
fn binary_digest() -> Result<String> {
    static PIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(value) = PIN.get() {
        return Ok(value.clone());
    }
    let mut file = std::fs::File::open(std::env::current_exe()?)?;
    let mut hash = Sha256::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        hash.update(chunk.get(..count).context("binary read exceeded chunk")?);
    }
    let value = hex::encode(hash.finalize());
    let _ = PIN.set(value.clone());
    Ok(value)
}
impl SkillsLifecycleService {
    /// Exact original policy input, retained only by an explicitly budgeted witness.
    pub(super) fn experiment_policy_snapshot(&self, config: &Config) -> serde_json::Value {
        serde_json::json!({"mode": self.settings.mode, "gate": self.settings.gate_policy, "limits": self.settings.limits, "autonomy": config.autonomy})
    }
    pub(super) fn experiment_policy_digest(snapshot: &serde_json::Value) -> Result<String> {
        Ok(digest(&serde_json::to_vec(snapshot)?))
    }
    pub(in crate::skills::lifecycle) fn experiment_pins(
        &self,
        skill: &SkillId,
        config: &Config,
        verification_split: EvalSplit,
        verify_holdout: bool,
    ) -> Result<ExperimentPins> {
        ensure!(
            verification_split != EvalSplit::Development,
            "verification cannot use development cases"
        );
        let registered = self.registered_suite(skill)?;
        let suite = self.store.read_suite(registered)?;
        if verify_holdout {
            ensure!(
                verification_split == EvalSplit::Validation,
                "additional holdout requires primary validation"
            );
            ensure!(
                suite.cases_of(EvalSplit::Holdout).count() >= usize::try_from(suite.suite().min_cases)?,
                "registered suite has insufficient holdout cases"
            );
            let runs = u64::try_from(suite.cases_of(EvalSplit::Holdout).count())?
                .checked_mul(2)
                .and_then(|count| count.checked_mul(u64::from(suite.suite().trials)));
            ensure!(
                runs.is_some_and(|runs| runs <= u64::from(suite.suite().budget.max_runs)),
                "holdout exceeds registered paired-run budget"
            );
        }
        ensure!(
            suite.suite().owner_scope == *self.owner_scope() && suite.suite().skill_id == *skill,
            "registered suite owner or skill mismatch"
        );
        let binding = self
            .store
            .active_binding(skill)?
            .context("experiment has no active baseline")?;
        ensure!(binding.enabled, "experiment baseline is disabled");
        self.store
            .read_revision(skill, &binding.revision_id, Some(&binding.record_digest))?;
        let model =
            crate::providers::selection::check_default(config, crate::providers::catalog::UnknownPolicy::Invocation)?;
        let key = crate::decision::DecisionKey::load_or_create(&config.workspace_dir)?;
        let configuration = hex::encode(key.hmac(
            crate::decision::KeyDomain::TerminalClaim,
            &[b"instruction-experiment-config-v1", &canonical_config(config)?],
        ));
        let policy = Self::experiment_policy_digest(&self.experiment_policy_snapshot(config))?;
        let catalog = digest(&serde_json::to_vec(
            &serde_json::json!({"tools": crate::tools::STATIC_PUBLIC_TOOL_NAMES, "tiers": config.tool_tiering}),
        )?);
        let environment = digest(&serde_json::to_vec(
            &serde_json::json!({"os": std::env::consts::OS, "arch": std::env::consts::ARCH, "runtime": config.runtime, "config": configuration}),
        )?);
        Ok(ExperimentPins {
            skill_id: skill.clone(),
            baseline_revision: binding.revision_id,
            baseline_record_digest: binding.record_digest,
            baseline_generation: binding.generation,
            binary_sha256: binary_digest()?,
            config_sha256: configuration,
            policy_sha256: policy,
            catalog_sha256: catalog,
            environment_sha256: environment,
            edit_scope_sha256: digest(b"instruction-only:SKILL.md:v1"),
            model: ModelBinding {
                provider_label: model.provider_label().into(),
                model: model.model().into(),
                temperature_milli: 100,
            },
            suite_digest: registered.into(),
            development_split_digest: suite.split_digest(EvalSplit::Development),
            verification_split,
            verification_split_digest: suite.split_digest(verification_split),
            holdout_split_digest: verify_holdout.then(|| suite.split_digest(EvalSplit::Holdout)),
            lesson_snapshot: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Result, canonical_config};
    #[test]
    fn configuration_pin_survives_hashmap_reconstruction() -> Result<()> {
        let mut config = Config::default();
        for index in 0..64 {
            config.cost.prices.insert(
                format!("provider/model-{index}"),
                serde_json::from_value(serde_json::json!({"input": index, "output": index + 1}))?,
            );
        }
        let expected = canonical_config(&config)?;
        for _ in 0..8 {
            let restored: Config = serde_json::from_slice(&serde_json::to_vec(&config)?)?;
            assert_eq!(canonical_config(&restored)?, expected);
        }
        config.cost.daily_limit_usd += 1.0;
        assert_ne!(canonical_config(&config)?, expected);
        Ok(())
    }
}
