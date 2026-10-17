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
use super::suite::{FixtureFile, SideEffects, check_relative_path};
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

/// The workspace whose cost ledger and limits the evaluation spends against.
///
/// Each run settles into its own scratch ledger, which is discarded with the
/// workspace, so on its own the operator's daily and monthly limits would be
/// checked against an empty ledger. The executor therefore takes the owner of
/// the real ledger: before a run it checks the owner's budget through the
/// existing `CostTracker` (the template's `[cost]` limits), refusing the run
/// when the limit is already exceeded, and after a run it records the run's
/// settled usage into the owner's ledger through the same tracker. There is
/// no constructor that omits it.
#[derive(Clone, Debug)]
pub struct AccountingOwner {
    workspace_dir: PathBuf,
}

impl AccountingOwner {
    #[must_use]
    pub const fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

/// Runs cases through `crate::agent::run` with a template configuration.
pub struct AgentCaseExecutor {
    template: Config,
    scratch_root: PathBuf,
    temperature: f64,
    accounting: AccountingOwner,
}

impl AgentCaseExecutor {
    /// `template` selects the provider, model and policy exactly like a CLI
    /// run would; its workspace and skill directories are replaced per run.
    /// `scratch_root` is where fresh task workspaces are created.
    #[must_use]
    pub const fn new(template: Config, scratch_root: PathBuf, temperature: f64, accounting: AccountingOwner) -> Self {
        Self {
            template,
            scratch_root,
            temperature,
            accounting,
        }
    }

    fn tracker(&self) -> Result<std::sync::Arc<crate::cost::tracker::CostTracker>, EvaluationError> {
        crate::cost::tracker::CostTracker::for_workspace(self.template.cost.clone(), &self.accounting.workspace_dir)
            .map_err(|e| EvaluationError::Executor(format!("accounting owner: {e}")))
    }

    /// Refuses a run when the owner's ledger is already over its limit.
    fn admit_spend(&self) -> Result<(), EvaluationError> {
        let check = self
            .tracker()?
            .check_budget(0.0)
            .map_err(|e| EvaluationError::Executor(format!("accounting owner: {e}")))?;
        if let crate::cost::types::BudgetCheck::Exceeded { period, .. } = check {
            return Err(EvaluationError::Executor(format!(
                "the operator's {period:?} cost limit is already exceeded; the run was not started"
            )));
        }
        Ok(())
    }

    /// Charges the run's settled usage to the owner's ledger.
    fn charge_spend(&self, usage: &LedgerUsage) -> Result<(), EvaluationError> {
        let tracker = self.tracker()?;
        for record in &usage.records {
            tracker
                .record_usage(crate::cost::types::TokenUsage {
                    model: record.model.clone(),
                    input_tokens: record.input_tokens,
                    output_tokens: record.output_tokens,
                    total_tokens: record.total_tokens,
                    cost_usd: record.cost_usd,
                    timestamp: chrono::Utc::now(),
                })
                .map_err(|e| EvaluationError::Executor(format!("accounting owner: {e}")))?;
        }
        Ok(())
    }

    /// The evaluation configuration, built from an allowlist. Only the managed
    /// provider path (provider, model, credentials, reliability, pricing,
    /// routes, decision backend) is taken from the caller's template; every
    /// other section starts from the default, so no hook, MCP server, channel,
    /// plugin, scheduled job, delegate, outbound network tool, embedder,
    /// exporter or caller memory can be reached from an evaluation run. The
    /// tools offered are `skill_read` plus the suite's declared tools; a
    /// declared network tool takes its settings from the template. The security
    /// policy is workspace-only at a fixed autonomy level and denies the
    /// lifecycle store, which holds the suite oracle.
    pub(super) fn run_config(
        &self,
        workspace: &Path,
        request: &ExecutionRequest<'_>,
    ) -> Result<Config, EvaluationError> {
        let template = &self.template;
        let mut config = provider_sections(template)?;
        config.workspace_dir = workspace.to_path_buf();
        config.config_path = workspace.join("config.toml");
        config.api_key.clone_from(&template.api_key);
        config.api_url.clone_from(&template.api_url);
        config.default_provider.clone_from(&template.default_provider);
        config.default_model.clone_from(&template.default_model);
        config.default_temperature = self.temperature;
        // Memory lives in the run workspace and nothing is saved from the turn.
        config.memory.backend = "sqlite".to_string();
        config.memory.auto_save = false;
        config.memory.embedding_provider = "none".to_string();
        config.memory.embedding_provider_explicit = true;
        // Only the installed revision may be discoverable: no ambient skills.
        config.skills.open_skills_dir = Some(workspace.join("no-open-skills").to_string_lossy().into_owned());
        config.skills.openclaw_skills_dir = Some(workspace.join("no-openclaw-skills").to_string_lossy().into_owned());
        config.autonomy.level = match request.side_effects {
            SideEffects::None => crate::security::AutonomyLevel::ReadOnly,
            SideEffects::WorkspaceWrite => crate::security::AutonomyLevel::Full,
        };
        config.autonomy.workspace_only = true;
        // The run may not touch its own evaluator state through the file tools:
        // the cost ledger, memory, lifecycle store, installed skill and config.
        let mut forbidden: Vec<String> = request
            .denied_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        for name in ["state", "memory", "self", "skills", "plugins", "config.toml"] {
            forbidden.push(workspace.join(name).to_string_lossy().into_owned());
        }
        config.autonomy.forbidden_paths.extend(forbidden);
        let declared = |name: &str| request.required_tools.iter().any(|tool| tool == name);
        if declared("web_search_tool") {
            config.web_search = template.web_search.clone();
        }
        if declared("web_fetch") {
            // An empty list would allow every public host.
            if template.browser.allowed_domains.is_empty() {
                return Err(EvaluationError::Executor(
                    "web_fetch is declared but the template's browser.allowed_domains is empty".to_string(),
                ));
            }
            config
                .browser
                .allowed_domains
                .clone_from(&template.browser.allowed_domains);
        }
        // A positive allowlist. The published surface is "every registered tool
        // that is not excluded", so the exclusion list is every public tool
        // except the ones this run may have.
        let mut offered = vec!["skill_read".to_string()];
        offered.extend(request.required_tools.iter().cloned());
        let allowed: std::collections::BTreeSet<&str> = offered
            .iter()
            .map(String::as_str)
            .chain(std::iter::once("capability_search"))
            .collect();
        config.tool_tiering.always_exclude = crate::tools::STATIC_PUBLIC_TOOL_NAMES
            .iter()
            .filter(|name| !allowed.contains(**name))
            .map(|name| (*name).to_string())
            .collect();
        config.tool_tiering.always_include = offered;
        Ok(config)
    }

    /// Binding of what the run was configured with and what actually served
    /// it. The models come from the run's own cost ledger, so a fallback or
    /// route that served a different model changes the binding.
    pub(super) fn environment_hash(config: &Config, request: &ExecutionRequest<'_>, models_used: &[String]) -> String {
        let mut include = config.tool_tiering.always_include.clone();
        include.sort();
        let mut exclude = config.tool_tiering.always_exclude.clone();
        exclude.sort();
        let routes = serde_json::to_string(&config.model_routes).unwrap_or_default();
        // The policy names the run's own workspace; the binding must not.
        let policy = serde_json::to_string(&config.autonomy)
            .unwrap_or_default()
            .replace(config.workspace_dir.to_string_lossy().as_ref(), "<workspace>");
        let binding = serde_json::json!({
            "provider": config.default_provider,
            "model": config.default_model,
            "temperature": config.default_temperature,
            "routes": routes,
            "fallback_providers": config.reliability.fallback_providers,
            "security_policy": policy,
            "side_effects": request.side_effects,
            "tools_offered": include,
            "tools_withheld": exclude,
            "models_used": models_used,
        });
        let mut hasher = Sha256::new();
        hasher.update(b"prx-skill-eval-environment\0");
        hasher.update(binding.to_string().as_bytes());
        hex::encode(hasher.finalize())
    }
}

/// Sections of the template that make up the managed provider path. Everything
/// else in the evaluation config starts from the default.
const PROVIDER_SECTIONS: [&str; 8] = [
    "providers",
    "reliability",
    "auth",
    "cost",
    "secrets",
    "proxy",
    "model_routes",
    "causal_tree",
];

/// A default configuration carrying only the template's provider sections.
fn provider_sections(template: &Config) -> Result<Config, EvaluationError> {
    let invalid = |e: serde_json::Error| EvaluationError::Executor(format!("evaluation config: {e}"));
    let mut base = serde_json::to_value(Config::default()).map_err(invalid)?;
    let source = serde_json::to_value(template).map_err(invalid)?;
    if let Some(target) = base.as_object_mut() {
        for key in PROVIDER_SECTIONS {
            if let Some(value) = source.get(key) {
                target.insert(key.to_string(), value.clone());
            }
        }
    }
    serde_json::from_value(base).map_err(invalid)
}

/// One settled record of the run's cost ledger.
#[derive(Clone, Debug)]
pub(super) struct LedgerRecord {
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cost_usd: f64,
}

/// Usage of one run read from the run's own cost ledger.
pub(super) struct LedgerUsage {
    pub tokens: Option<u64>,
    pub cost_micros: Option<u64>,
    pub models: Vec<String>,
    records: Vec<LedgerRecord>,
}

/// Sums the run's cost ledger. Only settled records (those with a settlement
/// id) count, and a malformed, negative or non-finite value makes the whole
/// ledger unmeasured. `None` fields mean the ledger could not be read or held
/// nothing, which makes the run invalid rather than free.
pub(super) fn read_usage(workspace: &Path) -> LedgerUsage {
    let path = workspace.join("state").join("costs.jsonl");
    let unmeasured = || LedgerUsage {
        tokens: None,
        cost_micros: None,
        models: Vec::new(),
        records: Vec::new(),
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return unmeasured();
    };
    let mut records: Vec<LedgerRecord> = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return unmeasured();
        };
        // Settled rows must be exactly what the ledger derives. One that is not
        // (forged, edited, or unverifiable) voids the whole ledger rather than
        // being skipped, so the spend can neither shrink nor be copied onward.
        // A row without a settlement id is not a settlement and adds nothing.
        let Ok(row) = serde_json::from_value::<crate::cost::types::CostRecord>(value) else {
            return unmeasured();
        };
        if row.settlement_id.is_none() {
            continue;
        }
        if !crate::cost::tracker::settlement_row_is_authentic(&row) {
            return unmeasured();
        }
        let usage = &row.usage;
        if !usage.cost_usd.is_finite() || usage.cost_usd < 0.0 {
            return unmeasured();
        }
        let (input_tokens, output_tokens, total_tokens, cost_usd) = (
            usage.input_tokens,
            usage.output_tokens,
            usage.total_tokens,
            usage.cost_usd,
        );
        let model = usage.model.as_str();
        records.push(LedgerRecord {
            model: model.to_string(),
            input_tokens,
            output_tokens,
            total_tokens,
            cost_usd,
        });
    }
    if records.is_empty() {
        return unmeasured();
    }
    let tokens = records.iter().fold(0_u64, |sum, r| sum.saturating_add(r.total_tokens));
    let cost: f64 = records.iter().map(|r| r.cost_usd).sum();
    let models: std::collections::BTreeSet<String> = records.iter().map(|r| r.model.clone()).collect();
    // Micro-dollars, rounded up so a nonzero cost is never recorded as zero.
    let rounded = (cost * 1_000_000.0).ceil();
    let cost_micros = format!("{rounded:.0}").parse::<u64>().unwrap_or(u64::MAX);
    LedgerUsage {
        tokens: Some(tokens),
        cost_micros: Some(cost_micros),
        models: models.into_iter().collect(),
        records,
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
            resource_path: receipt.resource_path.clone(),
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
        "prx-agent-entry/1".to_string()
    }

    async fn run_case(
        &self,
        request: &ExecutionRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<RunObservation, EvaluationError> {
        self.admit_spend()?;
        let started = Instant::now();
        fs::create_dir_all(&self.scratch_root).map_err(|e| io_error(&self.scratch_root, e))?;
        let scratch = tempfile::Builder::new()
            .prefix("prx-skill-eval-")
            .tempdir_in(&self.scratch_root)
            .map_err(|e| io_error(&self.scratch_root, e))?;
        let workspace = scratch.path().canonicalize().map_err(|e| io_error(scratch.path(), e))?;
        write_fixtures(&workspace, request.fixtures)?;
        let run_config = self.run_config(&workspace, request)?;
        let owner_id = crate::runtime::envelope::RuntimeEnvelope::agent(
            run_config.workspace_dir.to_string_lossy(),
            request.run_id,
        )
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
            run_config.clone(),
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
        let usage = read_usage(&workspace);
        self.charge_spend(&usage)?;
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
            environment_hash: Self::environment_hash(&run_config, request, &usage.models),
            installed,
            hydrations,
            experience_terminal,
            artifacts,
            cleanup,
            tokens: usage.tokens,
            cost_micros: usage.cost_micros,
            models_used: usage.models,
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

#[cfg(test)]
impl AgentCaseExecutor {
    pub(super) fn environment_hash_for_test(
        config: &Config,
        request: &ExecutionRequest<'_>,
        models: &[String],
    ) -> String {
        Self::environment_hash(config, request, models)
    }
}

#[cfg(test)]
pub(super) fn read_usage_for_test(workspace: &Path) -> LedgerUsage {
    read_usage(workspace)
}
