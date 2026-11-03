use crate::self_system::evolution::analyzer::{CandidatePriority, EvolutionCandidate};
use crate::self_system::evolution::config::SharedEvolutionConfig;
use crate::self_system::evolution::engine::{CycleResult, EngineCycleInput, EvolutionEngine};
use crate::self_system::evolution::record::{ChangeType, DataBasis, EvolutionLayer, EvolutionLog, EvolutionResult};
use crate::self_system::evolution::safety_utils::{is_raw_debug_enabled, sha256_hex, validate_path_in_workspace};
use crate::self_system::evolution::storage::AsyncJsonlWriter;
use crate::self_system::evolution::{
    ChangeOperation, ChangeTarget, CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals,
    EvolutionValidation, RiskLevel, ValidationStatus,
};
use crate::skills::lifecycle::ProposerModel;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

const MAX_PROMPT_BYTES: usize = 65_536;
const IMMUTABLE_START: &str = "<!-- IMMUTABLE_START -->";
const IMMUTABLE_END: &str = "<!-- IMMUTABLE_END -->";

/// A cycle with no eligible managed prompt is a successful no-op.
#[derive(Debug, thiserror::Error)]
#[error("no managed prompt target is available")]
pub struct NoManagedTarget;

fn managed_skill_name(path: &str) -> Option<&str> {
    path.strip_prefix("skills/")?
        .strip_suffix("/SKILL.md")
        .filter(|name| !name.is_empty() && !name.contains('/'))
}

/// Prompt mutation category.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptMutationType {
    FineTune,
    Reorder,
    AddRemove,
    Rewrite,
}

/// L2 managed prompt proposer; adoption belongs to the skills lifecycle.
pub struct PromptEvolutionEngine {
    shared_config: SharedEvolutionConfig,
    workspace_root: PathBuf,
    debug_raw: bool,
    proposer: Option<Arc<dyn ProposerModel>>,
    lifecycle: Option<Arc<crate::skills::lifecycle::SkillsLifecycleService>>,
}

impl PromptEvolutionEngine {
    /// Construct the engine.
    ///
    /// FIX-P1-09: the audit JSONL is now written exclusively by the pipeline (single
    /// source of truth), so this engine no longer owns a writer. The `_writer` parameter
    /// is kept for call-site compatibility but is intentionally unused.
    pub fn new(
        shared_config: SharedEvolutionConfig,
        workspace_root: impl AsRef<Path>,
        _writer: Option<Arc<AsyncJsonlWriter>>,
    ) -> Self {
        Self {
            shared_config,
            workspace_root: workspace_root.as_ref().to_path_buf(),
            debug_raw: false,
            proposer: None,
            lifecycle: None,
        }
    }

    /// Create a new engine with the `evolution_debug_raw` flag from main config.
    pub const fn with_debug_raw(mut self, debug_raw: bool) -> Self {
        self.debug_raw = debug_raw;
        self
    }

    const fn select_mutation_type(candidate: Option<&EvolutionCandidate>) -> PromptMutationType {
        let Some(candidate) = candidate else {
            return PromptMutationType::FineTune;
        };
        match candidate.priority {
            CandidatePriority::High => PromptMutationType::Rewrite,
            CandidatePriority::Medium => PromptMutationType::AddRemove,
            CandidatePriority::Low => PromptMutationType::FineTune,
        }
    }

    /// Inject the runtime-owned, accounted proposer. Without one the engine
    /// refuses generation; deterministic text is never a production fallback.
    pub fn with_proposer(mut self, proposer: Arc<dyn ProposerModel>) -> Self {
        self.proposer = Some(proposer);
        self
    }

    pub fn with_lifecycle(mut self, lifecycle: Option<Arc<crate::skills::lifecycle::SkillsLifecycleService>>) -> Self {
        self.lifecycle = lifecycle;
        self
    }

    fn generate_diff(before: &str, after: &str) -> String {
        if before == after {
            return "no changes".to_string();
        }
        let mut out = String::from("--- before\n+++ after\n");
        for line in before.lines() {
            if !after.contains(line) {
                out.push_str(&format!("-{line}\n"));
            }
        }
        for line in after.lines() {
            if !before.contains(line) {
                out.push_str(&format!("+{line}\n"));
            }
        }
        out
    }

    const fn severity(kind: &PromptMutationType) -> u8 {
        match kind {
            PromptMutationType::FineTune => 1,
            PromptMutationType::Reorder => 2,
            PromptMutationType::AddRemove => 3,
            PromptMutationType::Rewrite => 4,
        }
    }

    /// Build an owner-safe summary of a proposed prompt for DraftOnly persistence.
    ///
    /// Never includes the full prompt body; only a sha256 digest plus size metadata so
    /// a proposal stored in a shared/cross-owner location cannot leak prompt contents.
    fn draft_content_summary(content: &str) -> String {
        format!(
            "draft_only:sha256={};bytes={};lines={}",
            sha256_hex(content),
            content.len(),
            content.lines().count()
        )
    }

    fn redact_evolution_content(before: &str, after: &str, diff: &str, debug_raw: bool) -> (String, String, String) {
        if is_raw_debug_enabled(debug_raw) {
            return (before.to_string(), after.to_string(), diff.to_string());
        }

        let before_lines = before.lines().count() as i64;
        let after_lines = after.lines().count() as i64;
        let delta = after_lines - before_lines;
        (
            format!("sha256={};len={}", sha256_hex(before), before.len()),
            format!("sha256={};len={}", sha256_hex(after), after.len()),
            format!("sha256={};line_delta={delta}", sha256_hex(diff)),
        )
    }

    fn immutable_sections(content: &str) -> Result<Vec<String>> {
        let mut sections = Vec::new();
        let mut rest = content;
        while let Some(start) = rest.find(IMMUTABLE_START) {
            let after_start = &rest[start + IMMUTABLE_START.len()..];
            anyhow::ensure!(!rest[..start].contains(IMMUTABLE_END), "unbalanced immutable section");
            let end = after_start
                .find(IMMUTABLE_END)
                .context("unbalanced immutable section")?;
            anyhow::ensure!(
                !after_start[..end].contains(IMMUTABLE_START),
                "nested immutable section"
            );
            sections.push(after_start[..end].to_string());
            rest = &after_start[end + IMMUTABLE_END.len()..];
        }
        anyhow::ensure!(!rest.contains(IMMUTABLE_END), "unbalanced immutable section");
        Ok(sections)
    }

    fn validate_safety(before: &str, after: &str, blocked_keywords: &[String]) -> Result<()> {
        let immutable_before = Self::immutable_sections(before)?;
        let immutable_after = Self::immutable_sections(after)?;
        if immutable_before != immutable_after {
            bail!("mutation touched immutable section");
        }
        for kw in blocked_keywords {
            let before_has = before.to_ascii_lowercase().contains(&kw.to_ascii_lowercase());
            let after_has = after.to_ascii_lowercase().contains(&kw.to_ascii_lowercase());
            if after_has && !before_has {
                bail!("mutation introduced blocked keyword: {kw}");
            }
        }
        Ok(())
    }
}

#[async_trait]
impl EvolutionEngine for PromptEvolutionEngine {
    fn name(&self) -> &'static str {
        "prompt_evolution_engine"
    }

    fn layer(&self) -> EvolutionLayer {
        EvolutionLayer::Prompt
    }

    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
        self.run_cycle_with_cancellation(input, &tokio_util::sync::CancellationToken::new())
            .await
    }

    async fn run_cycle_with_cancellation(
        &mut self,
        input: EngineCycleInput,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<CycleResult> {
        anyhow::ensure!(!cancel.is_cancelled(), "evolution cycle cancelled before start");
        let started_at = Utc::now().to_rfc3339();
        let fitness_trend = input.fitness_trend.clone().unwrap_or_default();
        let cycle_id = if input.cycle_id.is_empty() {
            Uuid::now_v7().to_string()
        } else {
            input.cycle_id
        };
        let cfg = self.shared_config.load_full();
        let prompt_cfg = cfg.prompt.clone();
        let mode = cfg.runtime.mode.clone();
        let candidate = input.analyzer_candidates.first().cloned();

        let relative_target = if let Some(target) = prompt_cfg
            .mutable_files
            .iter()
            .find(|target| managed_skill_name(target).is_some())
        {
            if prompt_cfg.immutable_files.contains(target) {
                bail!("target file is immutable: {target}");
            }
            target.clone()
        } else {
            let targets = match &self.lifecycle {
                Some(service) => service.managed_prompt_targets()?,
                None => return Err(NoManagedTarget.into()),
            };
            targets
                .into_iter()
                .map(|target| format!("skills/{}/SKILL.md", target.name))
                .find(|target| {
                    !prompt_cfg.immutable_files.contains(target) && self.workspace_root.join(target).is_file()
                })
                .ok_or(NoManagedTarget)?
        };

        let target_path = validate_path_in_workspace(&self.workspace_root, Path::new(&relative_target))?;
        let mut before = String::new();
        fs::File::open(&target_path)
            .await?
            .take((MAX_PROMPT_BYTES + 1) as u64)
            .read_to_string(&mut before)
            .await
            .with_context(|| format!("failed to read prompt file: {}", target_path.display()))?;
        anyhow::ensure!(
            before.len() <= MAX_PROMPT_BYTES,
            "prompt exceeds the bounded generation input"
        );
        let mutation_type = Self::select_mutation_type(candidate.as_ref());
        let proposer = self
            .proposer
            .as_ref()
            .context("no managed evolution proposer is available")?;
        let name = managed_skill_name(&relative_target).context("invalid managed prompt target")?;
        let (after, provenance, managed_target) = {
            let service = self
                .lifecycle
                .as_ref()
                .context("managed prompt evolution requires the enabled skill lifecycle")?;
            let budget = crate::skills::lifecycle::ProposerBudget::new(
                MAX_PROMPT_BYTES,
                MAX_PROMPT_BYTES,
                std::time::Duration::from_secs(180),
            )?;
            let (draft, content) = service
                .propose_prompt_revision(name, &before, cycle_id.clone(), budget, Arc::clone(proposer), cancel)
                .await?;
            if let Err(error) = Self::validate_safety(&before, &content, &prompt_cfg.blocked_keywords) {
                let service = Arc::clone(service);
                let proposal_id = draft.proposal_id.clone();
                tokio::task::spawn_blocking(move || service.cancel(&proposal_id, "prompt safety validation failed"))
                    .await??;
                return Err(error);
            }
            let attempt = draft
                .attempts
                .last()
                .context("lifecycle proposal has no model attempt")?;
            let provenance = serde_json::to_string(&serde_json::json!({
                "model": draft.author.model, "usage": attempt.usage,
                "accounting_scope_id": attempt.accounting_scope_id,
                "input_sha256": attempt.prompt_sha256, "output_sha256": sha256_hex(&content),
                "evidence_ids": draft.evidence_ids, "lifecycle_proposal_id": draft.proposal_id,
            }))?;
            let target = ChangeTarget::SkillRevision {
                proposal_id: draft.proposal_id,
                skill_id: draft.skill_id.to_string(),
                revision_id: draft
                    .candidate_revision
                    .context("lifecycle proposal has no candidate")?
                    .to_string(),
            };
            (content, provenance, target)
        };
        Self::validate_safety(&before, &after, &prompt_cfg.blocked_keywords)?;

        // A whole-document model reply is a rewrite, regardless of the requested category.
        let severity = Self::severity(&PromptMutationType::Rewrite);
        let needs_human_approval = severity > prompt_cfg.human_approval_severity;
        let diff = Self::generate_diff(&before, &after);

        // FIX-P0-05: In DraftOnly mode the proposal is persisted to a shared, potentially
        // cross-owner JSONL/store without an owner scope. Persisting the full mutated prompt
        // there would leak prompt contents across owners. Store a summary + sha256 digest
        // instead; the full content is only materialized when a mutation is actually applied.
        let operation = ChangeOperation::Write {
            content: Self::draft_content_summary(&after),
        };
        let proposal = EvolutionProposal {
            id: Uuid::now_v7().to_string(),
            summary: format!("Prompt mutation for {relative_target}"),
            rationale: format!(
                "mutation_type={:?}; severity={severity}; provenance={provenance}",
                mutation_type
            ),
            risk_level: if needs_human_approval {
                RiskLevel::High
            } else {
                RiskLevel::Medium
            },
            target: managed_target,
            operation,
        };

        let outcome = CycleOutcome::NoAction;
        let notes = "candidate staged in the skill lifecycle; source review, paired evaluation and publication authorization are required";
        let (log_before, log_after, log_diff) = Self::redact_evolution_content(&before, &after, &diff, self.debug_raw);

        let evolution_log = EvolutionLog {
            experiment_id: proposal.id.clone(),
            timestamp: Utc::now().to_rfc3339(),
            layer: EvolutionLayer::Prompt,
            change_type: ChangeType::Update,
            before_value: log_before,
            after_value: log_after,
            trigger_reason: log_diff,
            data_basis: DataBasis {
                sample_count: candidate
                    .as_ref()
                    .map(|item| item.evidence_ids.len() as u32)
                    .unwrap_or(1),
                time_range_days: candidate.as_ref().map(|item| item.backfill_after_days).unwrap_or(1),
                key_metrics: HashMap::from([("severity".to_string(), f64::from(severity))]),
                patterns_found: vec![
                    format!("mutation_type={:?}", mutation_type),
                    format!("model_provenance={provenance}"),
                ],
            },
            result: if needs_human_approval {
                Some(EvolutionResult::Rejected)
            } else {
                Some(EvolutionResult::Neutral)
            },
        };

        // FIX-P1-09 (single audit source): the engine no longer appends the evolution log
        // to JSONL here. The pipeline is the sole writer of the audit record (it stamps the
        // canonical experiment_id and final result), so writing here too produced duplicate
        // rows with diverging conclusions for the same experiment_id. The log is returned in
        // `CycleResult.evolution_log` for the pipeline to persist exactly once.

        let cycle = EvolutionCycle {
            id: cycle_id,
            started_at,
            finished_at: Utc::now().to_rfc3339(),
            signals: EvolutionSignals {
                memory_count: 0,
                health_components: 1,
                health_error_components: 0,
                cron_runs: 0,
                cron_failure_ratio: 0.0,
            },
            trend: fitness_trend.clone(),
            proposal: Some(proposal.clone()),
            validation: EvolutionValidation {
                status: ValidationStatus::Skipped,
                before_score: fitness_trend.latest_score,
                after_score: None,
                delta: None,
                notes: format!("{notes}; fitness validation pending a later closed window"),
            },
            outcome,
            alert: if needs_human_approval {
                Some("needs_human_approval".to_string())
            } else {
                None
            },
            errors: Vec::new(),
        };

        Ok(CycleResult {
            review_data: Some(crate::self_system::evolution::engine::ReviewData::new(
                serde_json::to_string(
                    &serde_json::json!({"before": before, "after": after, "provenance": provenance}),
                )?,
            )),
            layer: EvolutionLayer::Prompt,
            proposal: Some(proposal),
            cycle,
            evolution_log: Some(evolution_log),
            needs_human_approval,
            shadow_mode: mode.is_draft_like(),
            pending: None,
        })
    }
}

#[cfg(test)]
pub(crate) fn test_proposer() -> Arc<dyn ProposerModel> {
    crate::skills::lifecycle::test_support::ManagedPromptModel::new("Verify the result before reporting completion.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::config::{EvolutionConfig, EvolutionMode, new_shared_evolution_config};
    use crate::skills::lifecycle::test_support::{ManagedPromptModel, managed_prompt_fixture};
    use std::sync::atomic::Ordering;
    use tempfile::tempdir;

    fn config() -> EvolutionConfig {
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Auto;
        cfg.prompt.human_approval_severity = 4;
        cfg
    }

    fn engine(
        workspace: &Path,
        cfg: EvolutionConfig,
        service: Arc<crate::skills::lifecycle::SkillsLifecycleService>,
        model: Arc<ManagedPromptModel>,
    ) -> PromptEvolutionEngine {
        PromptEvolutionEngine::new(new_shared_evolution_config(cfg), workspace, None)
            .with_lifecycle(Some(service))
            .with_proposer(model)
    }

    fn target(result: &CycleResult) -> &str {
        match &result.proposal.as_ref().unwrap().target {
            ChangeTarget::SkillRevision { skill_id, .. } => skill_id,
            other => panic!("expected lifecycle target: {other:?}"),
        }
    }

    #[tokio::test]
    async fn managed_target_stages_one_lifecycle_proposal_and_writes_nothing() {
        for mode in [EvolutionMode::Auto, EvolutionMode::Shadow, EvolutionMode::DraftOnly] {
            let dir = tempdir().unwrap();
            let service = managed_prompt_fixture(dir.path(), &[("a", true)]);
            let model = ManagedPromptModel::new("Verify the result before reporting completion.");
            let mut cfg = config();
            cfg.runtime.mode = mode.clone();
            cfg.prompt.human_approval_severity = 3;
            cfg.prompt.mutable_files = vec!["skills/a/SKILL.md".into()];
            let path = dir.path().join("skills/a/SKILL.md");
            let original = fs::read_to_string(&path).await.unwrap();
            let mut engine = engine(dir.path(), cfg, service.clone(), model.clone());
            let result = engine.run_cycle(EngineCycleInput::default()).await.unwrap();
            assert_eq!(target(&result), "a");
            assert!(result.pending.is_none());
            assert!(result.needs_human_approval);
            assert_eq!(result.shadow_mode, mode.is_draft_like());
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
            assert_eq!(service.list_proposals(None).unwrap().len(), 1);
            assert_eq!(fs::read_to_string(&path).await.unwrap(), original);
            let proposal = result.proposal.as_ref().unwrap();
            let ChangeOperation::Write { content } = &proposal.operation else {
                panic!("expected digest")
            };
            assert!(content.starts_with("draft_only:sha256="));
            assert!(!content.contains(&original));
            assert!(!format!("{:?}", result.review_data).contains(&original));
            assert!(proposal.rationale.contains("input_sha256"));
            assert!(proposal.rationale.contains("physical_requests"));
            assert!(!proposal.rationale.contains(&original));
        }
    }

    #[tokio::test]
    async fn non_skill_entries_are_never_written() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[("a", true)]);
        fs::write(dir.path().join("AGENTS.md"), "Original instructions.")
            .await
            .unwrap();
        let mut cfg = config();
        cfg.prompt.mutable_files = vec!["AGENTS.md".into(), "skills/a/SKILL.md".into()];
        let model = ManagedPromptModel::new("Verify the result.");
        let mut engine = engine(dir.path(), cfg, service.clone(), model.clone());
        let (writer, analyzer) = super::super::scheduler::tests::seeded_analysis(dir.path()).await;
        let judge = Arc::new(super::super::scheduler::tests::CountingJudge::default());
        let pipeline = super::super::pipeline::EvolutionPipeline::with_judge_model(
            engine.shared_config.clone(),
            analyzer,
            writer,
            dir.path(),
            judge.clone(),
        );
        let report = pipeline
            .run_for_layer(
                super::super::pipeline::EvolutionTrigger::Manual,
                EvolutionLayer::Prompt,
                &mut engine,
                Utc::now(),
            )
            .await
            .unwrap();
        assert_eq!(report.judge_outcome, super::super::pipeline::JudgeOutcome::Scored);
        assert_eq!(judge.calls.load(Ordering::SeqCst), 1);
        assert_eq!(service.list_proposals(None).unwrap().first().unwrap().skill_id, "a");
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("AGENTS.md")).await.unwrap(),
            "Original instructions."
        );
    }

    #[tokio::test]
    async fn active_lifecycle_skill_is_selected_when_mutable_files_is_empty() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[("a", true), ("b", true)]);
        let before_a = fs::read(dir.path().join("skills/a/SKILL.md")).await.unwrap();
        let before_b = fs::read(dir.path().join("skills/b/SKILL.md")).await.unwrap();
        let model = ManagedPromptModel::new("Verify the result.");
        let engine = engine(dir.path(), config(), service.clone(), model.clone());
        let mut scheduler = super::super::scheduler::tests::prompt_scheduler(
            dir.path(),
            engine.shared_config.clone(),
            Box::new(engine),
        )
        .await;
        let mut now = Utc::now();
        for (index, expected) in ["a", "b"].into_iter().enumerate() {
            let report = super::super::scheduler::tests::due_cycle(&mut scheduler, dir.path(), now).await;
            assert_eq!(report.layer_reports.len(), 1);
            assert!(report.layer_reports.first().unwrap().selected_candidate.is_some());
            assert!(
                report.layer_reports.first().unwrap().errors.is_empty(),
                "{:?}",
                report.layer_reports.first().unwrap().errors
            );
            let proposals = service.list_proposals(None).unwrap();
            assert_eq!(proposals.len(), index + 1);
            assert!(proposals.iter().any(|p| p.skill_id == expected));
            assert_eq!(model.calls.load(Ordering::SeqCst), index + 1);
            now = super::super::scheduler::tests::next_due_time(dir.path()).await;
        }
        assert_eq!(fs::read(dir.path().join("skills/a/SKILL.md")).await.unwrap(), before_a);
        assert_eq!(fs::read(dir.path().join("skills/b/SKILL.md")).await.unwrap(), before_b);
    }

    #[tokio::test]
    async fn explicit_immutable_managed_entry_is_refused() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[("a", true), ("b", true)]);
        let model = ManagedPromptModel::new("Verify the result.");
        let mut cfg = config();
        cfg.prompt.mutable_files = vec!["skills/a/SKILL.md".into(), "skills/b/SKILL.md".into()];
        cfg.prompt.immutable_files = vec!["skills/a/SKILL.md".into()];
        let original = fs::read(dir.path().join("skills/a/SKILL.md")).await.unwrap();
        let original_b = fs::read(dir.path().join("skills/b/SKILL.md")).await.unwrap();
        let mut engine = engine(dir.path(), cfg, service.clone(), model.clone());
        let err = engine.run_cycle(EngineCycleInput::default()).await.unwrap_err();
        assert!(err.to_string().contains("immutable"));
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        assert_eq!(service.list_proposals(None).unwrap().len(), 0);
        assert_eq!(fs::read(dir.path().join("skills/a/SKILL.md")).await.unwrap(), original);
        assert_eq!(
            fs::read(dir.path().join("skills/b/SKILL.md")).await.unwrap(),
            original_b
        );
    }

    #[tokio::test]
    async fn prompt_engine_rejects_immutable_file() {
        // The original refusal now uses an explicit managed target.
        explicit_immutable_managed_entry_is_refused_impl().await;
    }

    async fn explicit_immutable_managed_entry_is_refused_impl() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[("a", true)]);
        let mut cfg = config();
        cfg.prompt.mutable_files = vec!["skills/a/SKILL.md".into()];
        cfg.prompt.immutable_files = cfg.prompt.mutable_files.clone();
        let mut engine = engine(dir.path(), cfg, service, ManagedPromptModel::new("Verify."));
        assert!(
            engine
                .run_cycle(EngineCycleInput::default())
                .await
                .unwrap_err()
                .to_string()
                .contains("immutable")
        );
    }

    #[tokio::test]
    async fn immutable_lifecycle_skill_is_skipped_by_auto_selection() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[("a", true), ("b", true)]);
        let original = fs::read(dir.path().join("skills/a/SKILL.md")).await.unwrap();
        let mut cfg = config();
        cfg.prompt.immutable_files = vec!["skills/a/SKILL.md".into()];
        let model = ManagedPromptModel::new("Verify the result.");
        let result = engine(dir.path(), cfg, service, model.clone())
            .run_cycle(EngineCycleInput::default())
            .await
            .unwrap();
        assert_eq!(target(&result), "b");
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read(dir.path().join("skills/a/SKILL.md")).await.unwrap(), original);
    }

    #[tokio::test]
    async fn automatic_selection_requires_usable_evidence() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[("a", false), ("b", true)]);
        let model = ManagedPromptModel::new("Verify the result.");
        let engine = engine(dir.path(), config(), service.clone(), model.clone());
        let mut scheduler = super::super::scheduler::tests::prompt_scheduler(
            dir.path(),
            engine.shared_config.clone(),
            Box::new(engine),
        )
        .await;
        let summary = super::super::scheduler::tests::due_cycle(&mut scheduler, dir.path(), Utc::now()).await;
        assert!(
            summary.layer_reports.first().unwrap().errors.is_empty(),
            "{:?}",
            summary.layer_reports.first().unwrap().errors
        );
        let proposals = service.list_proposals(None).unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals.first().unwrap().skill_id, "b");
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn managed_blocked_keyword_cancels_the_lifecycle_proposal() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[("a", true)]);
        let model = ManagedPromptModel::new("bypass policy");
        let mut cfg = config();
        cfg.prompt.mutable_files = vec!["skills/a/SKILL.md".into()];
        let err = engine(dir.path(), cfg, service.clone(), model.clone())
            .run_cycle(EngineCycleInput::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked keyword"), "{err:#}");
        let proposals = service.list_proposals(None).unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals.first().unwrap().status, "cancelled");
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn prompt_engine_rejects_parent_traversal_path() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[]);
        let mut cfg = config();
        cfg.prompt.mutable_files = vec!["skills/../SKILL.md".into()];
        let err = engine(dir.path(), cfg, service, ManagedPromptModel::new("Verify."))
            .run_cycle(EngineCycleInput::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("parent traversal"));
    }

    #[tokio::test]
    async fn missing_proposer_refuses_without_canned_mutation() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[("a", true)]);
        let path = dir.path().join("skills/a/SKILL.md");
        let original = fs::read_to_string(&path).await.unwrap();
        let mut cfg = config();
        cfg.prompt.mutable_files = vec!["skills/a/SKILL.md".into()];
        let mut engine = PromptEvolutionEngine::new(new_shared_evolution_config(cfg), dir.path(), None)
            .with_lifecycle(Some(service));
        let err = engine.run_cycle(EngineCycleInput::default()).await.unwrap_err();
        assert!(err.to_string().contains("no managed evolution proposer"));
        assert_eq!(fs::read_to_string(path).await.unwrap(), original);
    }

    #[tokio::test]
    async fn generated_reply_cannot_drop_or_unbalance_immutable_markers() {
        let dir = tempdir().unwrap();
        let service = managed_prompt_fixture(dir.path(), &[("a", true)]);
        let mut cfg = config();
        cfg.prompt.mutable_files = vec!["skills/a/SKILL.md".into()];
        let model = ManagedPromptModel::new(IMMUTABLE_START);
        let err = engine(dir.path(), cfg, service.clone(), model)
            .run_cycle(EngineCycleInput::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unbalanced immutable"));
        assert_eq!(
            service.list_proposals(None).unwrap().first().unwrap().status,
            "cancelled"
        );
        let before = "prefix <!-- IMMUTABLE_START -->keep<!-- IMMUTABLE_END --> suffix";
        for after in [
            "prefix <!-- IMMUTABLE_START -->keep suffix",
            "prefix keep<!-- IMMUTABLE_END --> suffix",
            "prefix <!-- IMMUTABLE_START -->changed<!-- IMMUTABLE_END --> suffix",
            "prefix <!-- IMMUTABLE_START --><!-- IMMUTABLE_START -->keep<!-- IMMUTABLE_END --> suffix",
        ] {
            assert!(PromptEvolutionEngine::validate_safety(before, after, &[]).is_err());
        }
        assert!(PromptEvolutionEngine::validate_safety(before, &format!("{before}\nClarification."), &[]).is_ok());
    }
}
