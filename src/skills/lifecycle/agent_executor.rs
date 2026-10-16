//! Executes evaluation cases through the real PRX agent entry (SL06).
//!
//! Each run gets a fresh, owner-only workspace under a scratch root, the
//! case's fixtures, and (for an arm with a skill) the arm's revision installed
//! through the same store, import and publication path production uses, so
//! `skill_read` serves it from the pinned immutable revision and records the
//! hydration and terminal receipts. The task itself is `crate::agent::run`, the
//! single-shot CLI entry: the provider, model, price and autonomy come from the
//! caller's `Config` through the existing managed provider path. This module
//! builds no provider, reads no credential and has no registry of its own.

use super::bundle::Bundle;
use super::capture::EXPERIENCES_DIR;
use super::evaluate::{
    CaseExecutor, CleanupState, EvaluationError, ExecutionRequest, ExperienceTerminalFact, HydrationFact, InstallSkill,
    InstalledSkill, RunObservation, RunTerminal,
};
use super::journal::TransactionId;
use super::publish::PublishRequest;
use super::store::SkillLifecycleStore;
use super::suite::{FixtureFile, check_relative_path};
use super::types::{LifecycleError, NewRevision, OwnerScope, SkillId, SourceKind};
use crate::config::Config;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Time the agent gets to wind a cancelled or overdue turn down.
const CANCEL_GRACE: Duration = Duration::from_secs(20);
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024;
const MAX_EXPERIENCES: usize = 16;

/// Runs cases through `crate::agent::run` with a template configuration.
pub struct AgentCaseExecutor {
    template: Config,
    scratch_root: PathBuf,
    temperature: f64,
}

impl AgentCaseExecutor {
    /// `template` selects the provider, model and policy exactly like a CLI
    /// run would; its workspace and skill directories are replaced per run.
    /// `scratch_root` is where fresh task workspaces are created.
    #[must_use]
    pub const fn new(template: Config, scratch_root: PathBuf, temperature: f64) -> Self {
        Self {
            template,
            scratch_root,
            temperature,
        }
    }

    fn environment_hash(&self) -> String {
        let config = &self.template;
        let mut include = config.tool_tiering.always_include.clone();
        include.sort();
        let mut exclude = config.tool_tiering.always_exclude.clone();
        exclude.sort();
        let binding = serde_json::json!({
            "provider": config.default_provider,
            "model": config.default_model,
            "temperature": self.temperature,
            "autonomy": format!("{:?}", config.autonomy.level),
            "tools_included": include,
            "tools_excluded": exclude,
        });
        let mut hasher = Sha256::new();
        hasher.update(b"prx-skill-eval-environment\0");
        hasher.update(binding.to_string().as_bytes());
        hex::encode(hasher.finalize())
    }

    fn run_config(&self, workspace: &Path) -> Config {
        let mut config = self.template.clone();
        config.workspace_dir = workspace.to_path_buf();
        config.config_path = workspace.join("config.toml");
        // Only the installed revision may be discoverable: no ambient skills.
        config.skills.open_skills_dir = Some(workspace.join("no-open-skills").to_string_lossy().into_owned());
        config.skills.openclaw_skills_dir = Some(workspace.join("no-openclaw-skills").to_string_lossy().into_owned());
        config
    }
}

fn io_error(path: &Path, source: std::io::Error) -> EvaluationError {
    EvaluationError::Lifecycle(LifecycleError::io(path, source))
}

fn write_fixtures(workspace: &Path, fixtures: &[FixtureFile]) -> Result<(), EvaluationError> {
    for fixture in fixtures {
        check_relative_path("fixture path", &fixture.path)?;
        let target = workspace.join(&fixture.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
        }
        fs::write(&target, fixture.content.as_bytes()).map_err(|e| io_error(&target, e))?;
    }
    Ok(())
}

/// Installs the revision through the lifecycle store of the task workspace and
/// writes the catalog projection `skills/<id>/` that discovery lists.
fn install_skill(
    workspace: &Path,
    owner: &OwnerScope,
    skill_id: &SkillId,
    bundle: &Bundle,
) -> Result<InstalledSkill, EvaluationError> {
    let store = SkillLifecycleStore::open(workspace, owner.clone())?;
    let written = store.write_revision(&NewRevision::new(skill_id.clone(), SourceKind::Operator), bundle)?;
    store.publish(&PublishRequest {
        transaction_id: TransactionId::generate(),
        skill_id: skill_id.clone(),
        candidate: written.revision.revision_id.clone(),
        candidate_record_digest: written.record_digest.clone(),
        expected_generation: 0,
        enabled: true,
    })?;
    let generation = store
        .active_binding(skill_id)?
        .map(|binding| binding.generation)
        .ok_or_else(|| EvaluationError::Executor("the installed skill has no active binding".to_string()))?;
    let projection = crate::skills::skills_dir(workspace).join(skill_id.as_str());
    for file in bundle.files() {
        let target = projection.join(file.path());
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
        }
        fs::write(&target, file.bytes()).map_err(|e| io_error(&target, e))?;
    }
    Ok(InstalledSkill {
        skill_id: skill_id.clone(),
        revision_id: written.revision.revision_id,
        bundle_manifest_hash: written.revision.bundle_manifest_hash,
        generation,
    })
}

/// Hydration and terminal facts the runtime recorded in the task workspace.
fn read_experiences(
    workspace: &Path,
    owner: &OwnerScope,
) -> Result<(Vec<HydrationFact>, Option<ExperienceTerminalFact>), EvaluationError> {
    let Some(store) = SkillLifecycleStore::open_existing(workspace, owner.clone())? else {
        return Ok((Vec::new(), None));
    };
    let mut hydrations = Vec::new();
    let mut terminal = None;
    for id in store.list_experience_ids(MAX_EXPERIENCES)? {
        let Some(experience) = store.read_experience_by_id(&id)? else {
            continue;
        };
        hydrations.extend(experience.hydration_receipts.iter().map(|receipt| HydrationFact {
            skill_id: receipt.skill_id.clone(),
            revision_id: receipt.revision_id.clone(),
            generation: receipt.generation,
            resource_sha256: receipt.resource_sha256.clone(),
        }));
        if terminal.is_none() {
            terminal = experience.terminal.as_ref().map(|join| ExperienceTerminalFact {
                terminal_receipt_id: join.terminal_receipt_id.clone(),
                outcome: join.outcome,
            });
        }
    }
    Ok((hydrations, terminal))
}

/// Reads back requested artifacts. A symlink or a path that leaves the
/// workspace is never followed: it reads as absent.
fn collect_artifacts(workspace: &Path, paths: &[String]) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut artifacts = BTreeMap::new();
    for path in paths {
        let content = (|| {
            check_relative_path("artifact path", path).ok()?;
            let full = workspace.join(path);
            let meta = fs::symlink_metadata(&full).ok()?;
            if !meta.is_file() || meta.len() > MAX_ARTIFACT_BYTES {
                return None;
            }
            let canonical = full.canonicalize().ok()?;
            if !canonical.starts_with(workspace) {
                return None;
            }
            let mut bytes = Vec::new();
            fs::File::open(&canonical)
                .ok()?
                .take(MAX_ARTIFACT_BYTES)
                .read_to_end(&mut bytes)
                .ok()?;
            Some(bytes)
        })();
        artifacts.insert(path.clone(), content);
    }
    artifacts
}

#[async_trait::async_trait]
impl CaseExecutor for AgentCaseExecutor {
    fn identity(&self) -> String {
        format!("prx-agent-entry/1 env={}", self.environment_hash())
    }

    async fn run_case(
        &self,
        request: &ExecutionRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<RunObservation, EvaluationError> {
        let started = Instant::now();
        fs::create_dir_all(&self.scratch_root).map_err(|e| io_error(&self.scratch_root, e))?;
        let scratch = tempfile::Builder::new()
            .prefix("prx-skill-eval-")
            .tempdir_in(&self.scratch_root)
            .map_err(|e| io_error(&self.scratch_root, e))?;
        let workspace = scratch.path().canonicalize().map_err(|e| io_error(scratch.path(), e))?;
        write_fixtures(&workspace, request.fixtures)?;
        let config = self.run_config(&workspace);
        let owner_id =
            crate::runtime::envelope::RuntimeEnvelope::agent(config.workspace_dir.to_string_lossy(), request.run_id)
                .resolved_owner_id();
        let owner = OwnerScope::new(&owner_id)?;

        let installed = match request.skill {
            Some(InstallSkill { skill_id, bundle }) => {
                let (workspace, owner, skill_id, bundle) =
                    (workspace.clone(), owner.clone(), skill_id.clone(), bundle.clone());
                let installed =
                    tokio::task::spawn_blocking(move || install_skill(&workspace, &owner, &skill_id, &bundle))
                        .await
                        .map_err(|e| EvaluationError::Executor(format!("skill install task failed: {e}")))??;
                Some(installed)
            }
            None => None,
        };

        let token = cancel.child_token();
        let mut run = Box::pin(crate::agent::run(
            config,
            Some(request.prompt.to_string()),
            None,
            None,
            self.temperature,
            token.clone(),
        ));
        let (terminal, response) = match tokio::time::timeout(request.deadline, &mut run).await {
            Ok(Ok(text)) => (RunTerminal::Completed, text),
            Ok(Err(error)) => {
                // The error text can echo provider detail; only its class is kept.
                tracing::debug!(
                    run_id = request.run_id,
                    "evaluation agent run failed: {}",
                    error_class(&error)
                );
                (
                    if cancel.is_cancelled() {
                        RunTerminal::Cancelled
                    } else {
                        RunTerminal::Failed
                    },
                    String::new(),
                )
            }
            Err(_) => {
                token.cancel();
                if tokio::time::timeout(CANCEL_GRACE, &mut run).await.is_err() {
                    tracing::warn!(
                        run_id = request.run_id,
                        "evaluation agent run did not wind down after cancellation"
                    );
                }
                (RunTerminal::TimedOut, String::new())
            }
        };
        drop(run);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let (hydrations, experience_terminal) = {
            let (workspace, owner) = (workspace.clone(), owner.clone());
            tokio::task::spawn_blocking(move || read_experiences(&workspace, &owner))
                .await
                .map_err(|e| EvaluationError::Executor(format!("experience read task failed: {e}")))??
        };
        let artifacts = collect_artifacts(&workspace, request.collect_artifacts);
        let cleanup = match scratch.close() {
            Ok(()) => CleanupState::Removed,
            Err(error) => {
                tracing::warn!(run_id = request.run_id, "evaluation workspace cleanup failed: {error}");
                CleanupState::Failed
            }
        };
        Ok(RunObservation {
            run_id: request.run_id.to_string(),
            terminal,
            response,
            elapsed_ms,
            environment_hash: self.environment_hash(),
            installed,
            hydrations,
            experience_terminal,
            artifacts,
            cleanup,
        })
    }
}

fn error_class(error: &anyhow::Error) -> &'static str {
    let text = error.to_string().to_lowercase();
    if text.contains("cancel") {
        "cancelled"
    } else if text.contains("timeout") || text.contains("timed out") {
        "timeout"
    } else {
        "error"
    }
}

impl SkillLifecycleStore {
    /// Ids of the experience records in this store, at most `max`.
    fn list_experience_ids(&self, max: usize) -> Result<Vec<String>, EvaluationError> {
        let dir = self.root().join(EXPERIENCES_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error(&dir, e)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_error(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json").filter(|id| id.starts_with("exp-")) else {
                continue;
            };
            if ids.len() >= max {
                return Err(EvaluationError::Executor(format!(
                    "the task workspace holds more than {max} experience records"
                )));
            }
            ids.push(id.to_string());
        }
        ids.sort();
        Ok(ids)
    }
}
