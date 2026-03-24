use crate::self_system::evolution::analyzer::{CandidatePriority, EvolutionCandidate};
use crate::self_system::evolution::config::{EvolutionMode, SharedEvolutionConfig};
use crate::self_system::evolution::engine::{CycleResult, EngineCycleInput, EvolutionEngine};
use crate::self_system::evolution::record::{ChangeType, DataBasis, EvolutionLayer, EvolutionLog, EvolutionResult};
use crate::self_system::evolution::safety_utils::{
    atomic_write, is_raw_debug_enabled, sha256_hex, validate_path_in_workspace,
};
use crate::self_system::evolution::storage::AsyncJsonlWriter;
use crate::self_system::evolution::{
    ChangeOperation, ChangeTarget, CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals,
    EvolutionValidation, FitnessTrend, RiskLevel, ValidationStatus,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use uuid::Uuid;

const IMMUTABLE_START: &str = "<!-- IMMUTABLE_START -->";
const IMMUTABLE_END: &str = "<!-- IMMUTABLE_END -->";

/// Prompt mutation category.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptMutationType {
    FineTune,
    Reorder,
    AddRemove,
    Rewrite,
}

/// L2 prompt evolution executor.
pub struct PromptEvolutionEngine {
    shared_config: SharedEvolutionConfig,
    workspace_root: PathBuf,
    writer: Option<Arc<AsyncJsonlWriter>>,
    debug_raw: bool,
}

impl PromptEvolutionEngine {
    pub fn new(
        shared_config: SharedEvolutionConfig,
        workspace_root: impl AsRef<Path>,
        writer: Option<Arc<AsyncJsonlWriter>>,
    ) -> Self {
        Self {
            shared_config,
            workspace_root: workspace_root.as_ref().to_path_buf(),
            writer,
            debug_raw: false,
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

    fn mock_generate_mutation(
        &self,
        original: &str,
        kind: PromptMutationType,
        candidate: Option<&EvolutionCandidate>,
    ) -> String {
        let marker = candidate
            .map(|item| item.suggested_value.as_str())
            .unwrap_or("improve prompt robustness");
        match kind {
            PromptMutationType::FineTune => {
                format!("{original}\n# fine-tune: {marker}\n")
            }
            PromptMutationType::Reorder => {
                let mut lines = original.lines().collect::<Vec<_>>();
                if lines.len() >= 2 {
                    lines.swap(0, 1);
                }
                format!("{}\n", lines.join("\n"))
            }
            PromptMutationType::AddRemove => {
                format!("{original}\n- Safety reminder: enforce policy checks before execution.\n")
            }
            PromptMutationType::Rewrite => {
                format!("{}\n# rewrite summary: {}\n", original.trim_end(), marker)
            }
        }
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

    fn immutable_sections(content: &str) -> Vec<String> {
        let mut sections = Vec::new();
        let mut rest = content;
        while let Some(start) = rest.find(IMMUTABLE_START) {
            let after_start = &rest[start + IMMUTABLE_START.len()..];
            let Some(end) = after_start.find(IMMUTABLE_END) else {
                break;
            };
            sections.push(after_start[..end].to_string());
            rest = &after_start[end + IMMUTABLE_END.len()..];
        }
        sections
    }

    fn validate_safety(before: &str, after: &str, blocked_keywords: &[String]) -> Result<()> {
        let immutable_before = Self::immutable_sections(before);
        let immutable_after = Self::immutable_sections(after);
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

    async fn backup_version(&self, target: &Path, content: &str, max_versions: usize) -> Result<()> {
        let rel = target
            .strip_prefix(&self.workspace_root)
            .unwrap_or(target)
            .to_string_lossy()
            .replace('/', "__");
        let versions_dir = self.workspace_root.join(".evolution/prompt_versions").join(rel);
        fs::create_dir_all(&versions_dir).await?;
        let file = versions_dir.join(format!("{}.bak", Uuid::now_v7()));
        atomic_write(&self.workspace_root, &file, content.as_bytes()).await?;

        let mut entries = Vec::new();
        let mut rd = fs::read_dir(&versions_dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            if entry.file_type().await?.is_file() {
                entries.push(entry);
            }
        }
        entries.sort_by_key(|entry| entry.file_name());
        if entries.len() > max_versions.max(1) {
            let stale_count = entries.len() - max_versions.max(1);
            for entry in entries.into_iter().take(stale_count) {
                let stale_path = entry.path();
                if let Err(err) = fs::remove_file(&stale_path).await {
                    tracing::warn!(
                        error = %err,
                        path = %stale_path.display(),
                        "failed to prune stale prompt backup version"
                    );
                }
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
        let started_at = Utc::now().to_rfc3339();
        let cycle_id = if input.cycle_id.is_empty() {
            Uuid::now_v7().to_string()
        } else {
            input.cycle_id
        };
        let cfg = self.shared_config.load_full();
        let prompt_cfg = cfg.prompt.clone();
        let mode = cfg.runtime.mode.clone();
        let candidate = input.analyzer_candidates.first().cloned();

        let Some(relative_target) = prompt_cfg.mutable_files.first() else {
            bail!("prompt.mutable_files is empty");
        };
        if prompt_cfg.immutable_files.iter().any(|item| item == relative_target) {
            bail!("target file is immutable: {relative_target}");
        }

        let target_path = validate_path_in_workspace(&self.workspace_root, Path::new(relative_target))?;
        let before = fs::read_to_string(&target_path)
            .await
            .with_context(|| format!("failed to read prompt file: {}", target_path.display()))?;
        let mutation_type = Self::select_mutation_type(candidate.as_ref());
        let after = self.mock_generate_mutation(&before, mutation_type.clone(), candidate.as_ref());
        Self::validate_safety(&before, &after, &prompt_cfg.blocked_keywords)?;

        let severity = Self::severity(&mutation_type);
        let needs_human_approval = severity > prompt_cfg.human_approval_severity;
        let diff = Self::generate_diff(&before, &after);

        let proposal = EvolutionProposal {
            id: Uuid::now_v7().to_string(),
            summary: format!("Prompt mutation for {relative_target}"),
            rationale: format!("mutation_type={:?}; severity={severity}", mutation_type),
            risk_level: if needs_human_approval {
                RiskLevel::High
            } else {
                RiskLevel::Medium
            },
            target: ChangeTarget::WorkspaceFile {
                path: relative_target.clone(),
            },
            operation: ChangeOperation::Write { content: after.clone() },
        };

        let mut outcome = CycleOutcome::NoAction;
        let mut validation_status = ValidationStatus::Skipped;
        let mut notes = "shadow mode: prompt mutation recorded only".to_string();

        if !matches!(mode, EvolutionMode::Shadow) {
            if needs_human_approval {
                outcome = CycleOutcome::ApprovalRequired;
                notes = "severity exceeded threshold".to_string();
            } else {
                self.backup_version(&target_path, &before, prompt_cfg.max_rollback_versions)
                    .await?;
                atomic_write(&self.workspace_root, &target_path, after.as_bytes()).await?;
                outcome = CycleOutcome::Applied;
                validation_status = ValidationStatus::Improved;
                notes = "prompt mutation applied".to_string();
            }
        }
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
                patterns_found: vec![format!("mutation_type={:?}", mutation_type)],
            },
            result: Some(if needs_human_approval {
                EvolutionResult::Rejected
            } else if matches!(outcome, CycleOutcome::Applied) {
                EvolutionResult::Improved
            } else {
                EvolutionResult::Neutral
            }),
        };

        if let Some(writer) = &self.writer {
            writer.append_evolution(&evolution_log).await?;
        }

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
            trend: FitnessTrend {
                window: 1,
                previous_average: 0.5,
                latest_score: if matches!(outcome, CycleOutcome::Applied) {
                    0.6
                } else {
                    0.5
                },
                is_declining: false,
            },
            proposal: Some(proposal.clone()),
            validation: EvolutionValidation {
                status: validation_status,
                before_score: 0.5,
                after_score: if matches!(outcome, CycleOutcome::Applied) {
                    0.6
                } else {
                    0.5
                },
                delta: if matches!(outcome, CycleOutcome::Applied) {
                    0.1
                } else {
                    0.0
                },
                notes,
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
            layer: EvolutionLayer::Prompt,
            proposal: Some(proposal),
            cycle,
            evolution_log: Some(evolution_log),
            needs_human_approval,
            shadow_mode: matches!(mode, EvolutionMode::Shadow),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::config::{EvolutionConfig, EvolutionMode, new_shared_evolution_config};
    use tempfile::tempdir;

    #[tokio::test]
    async fn prompt_engine_rejects_immutable_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.md");
        fs::write(&path, "a").await.unwrap();

        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Auto;
        cfg.prompt.mutable_files = vec!["p.md".to_string()];
        cfg.prompt.immutable_files = vec!["p.md".to_string()];

        let shared = new_shared_evolution_config(cfg);
        let mut engine = PromptEvolutionEngine::new(shared, dir.path(), None);
        let err = engine
            .run_cycle(EngineCycleInput {
                cycle_id: "x".to_string(),
                analyzer_candidates: Vec::new(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("immutable"));
    }

    #[tokio::test]
    async fn prompt_engine_shadow_mode_does_not_modify_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.md");
        fs::write(&path, "hello").await.unwrap();

        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Shadow;
        cfg.prompt.mutable_files = vec!["p.md".to_string()];

        let shared = new_shared_evolution_config(cfg);
        let mut engine = PromptEvolutionEngine::new(shared, dir.path(), None);
        let result = engine
            .run_cycle(EngineCycleInput {
                cycle_id: "x".to_string(),
                analyzer_candidates: Vec::new(),
            })
            .await
            .unwrap();
        assert!(result.shadow_mode);
        let content = fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn prompt_engine_rejects_parent_traversal_path() {
        let dir = tempdir().unwrap();
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Auto;
        cfg.prompt.mutable_files = vec!["../escape.md".to_string()];

        let shared = new_shared_evolution_config(cfg);
        let mut engine = PromptEvolutionEngine::new(shared, dir.path(), None);
        let err = engine
            .run_cycle(EngineCycleInput {
                cycle_id: "x".to_string(),
                analyzer_candidates: Vec::new(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("parent traversal"));
    }
}
