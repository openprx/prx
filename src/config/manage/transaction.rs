//! Validate-then-commit configuration transactions with compensating
//! restore, and operator rollback.
//!
//! [`ConfigTransactions::apply`] is the one write path of the management
//! plane:
//!
//! 1. **Plan** ([`ConfigTransactions::plan`], on a blocking worker): read the
//!    current revision, then render the change into its stored form (a
//!    structured change goes through the encrypting save path,
//!    `files::plan_structured_save`, and keeps the `config.d` layout), stage
//!    it and validate the staged tree, run the shared write-boundary checks
//!    (no introduced `***`, no introduced plaintext at an encrypted slot while
//!    encryption is on), then load it exactly as a reload would (decrypting
//!    with the live key). Nothing is written. The plan remembers the revision
//!    it was read from.
//! 2. **Gate**: a plan classified high risk (credential write, encryption
//!    off, endpoint change) needs the caller's explicit confirmation and a
//!    revision. A rollback always needs confirmation and an expected
//!    revision, plus explicit flags to re-introduce a credential the current
//!    configuration no longer holds or to turn encryption off.
//! 3. **Serialize**: one transaction at a time per configuration file in this
//!    process (an async lock), and every PRX writer in any process takes the
//!    OS-backed writer lock inside the commit.
//! 4. **Guarded commit** (`files::commit_mutation_guarded`): under the writer
//!    lock, the tree must still be at the expected revision — the caller's,
//!    or the plan's own when the caller names none — (409 otherwise, nothing
//!    written); the exact before-image is recorded (sealed) as a `pending`
//!    history record before any file changes; then the existing journaled
//!    odd/even commit writes the files (temp file, fsync, rename).
//! 5. **Activate**: `reload_from_disk` through the generation manager, whose
//!    participants prepare, commit or roll back the runtime.
//! 6. **Compensate**: if activation is refused, the before-image is
//!    committed back (guarded by the revision this transaction wrote, so a
//!    later writer is never overwritten) and re-activated. The record ends
//!    `rolled_back`; the caller learns the change was not applied and what
//!    disk holds now.
//!
//! Crash safety: a crash inside step 4 is recovered by the commit journal at
//! the next write or startup (the tree is the old one or the new one, never a
//! mix). A crash between steps 4 and 6 leaves a `pending` record whose
//! before-image an operator can restore with [`ConfigTransactions::rollback`].

use super::boundary;
use super::history::{ConfigActor, ConfigHistory, HistoryRecord, NewRecord, RecordStatus};
use super::plan::{ConfigChangeRisk, ConfigChangeSummary, RiskReason};
use super::validate;
use crate::config::files::{self, ConfigBeforeImage, ConfigMutationPlan, ConfigRevisionConflict, GuardedCommit};
use crate::config::{Config, ConfigApplyReport, ConfigReloadTrigger, DesiredConfigState, SharedConfig};
use anyhow::Context as _;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

/// What a transaction writes.
#[derive(Debug)]
pub enum ConfigChange {
    /// A complete configuration, saved through the encrypting save path in
    /// the current file layout.
    Structured(Box<Config>),
    /// One raw file (`config.toml` or a `config.d` fragment), written as
    /// given; every other file is kept byte for byte.
    RawFile {
        filename: String,
        contents: String,
        /// Explicit permission to omit or empty stored secret slots.
        /// Does not bypass mask, encryption, risk or revision checks.
        allow_secret_clear: bool,
    },
}

/// A validated change that has not been written.
pub struct ConfigPlan {
    mutation: ConfigMutationPlan,
    candidate: Arc<Config>,
    restores: Option<String>,
    summary: ConfigChangeSummary,
    /// The revision of the files the plan was read from; `None` only when
    /// there was no `config.toml`.
    planned_revision: Option<String>,
    /// The configuration the plan was compared with (`None`: nothing
    /// loadable was stored); every selection the change affects is resolved
    /// against it before anything is written (PMM-D-5).
    current: Option<Arc<Config>>,
}

impl std::fmt::Debug for ConfigPlan {
    /// Only the redacted summary: the mutation holds stored secrets.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigPlan")
            .field("summary", &self.summary)
            .field("restores", &self.restores)
            .field("planned_revision", &self.planned_revision)
            .finish_non_exhaustive()
    }
}

impl ConfigPlan {
    /// The redacted semantic plan against the configuration on disk when
    /// the plan was made.
    #[must_use]
    pub const fn summary(&self) -> &ConfigChangeSummary {
        &self.summary
    }

    /// The candidate configuration as a reload would load it (secrets
    /// decrypted). Never render it without redaction.
    #[must_use]
    pub fn candidate(&self) -> &Config {
        &self.candidate
    }

    /// The revision of the files the plan was read from.
    #[must_use]
    pub fn planned_revision(&self) -> Option<&str> {
        self.planned_revision.as_deref()
    }

    /// Whether the install this plan was made against runs at the `Full`
    /// autonomy level, evaluated on the configuration before the change (a
    /// write that raises the level to `Full` is judged at the lower level it
    /// leaves; one that lowers it from `Full` is judged at `Full`). At
    /// `Full` no high-risk confirmation is demanded; the revision guard of
    /// the commit still refuses a stale write. A configuration that could
    /// not be loaded is not `Full`.
    #[must_use]
    pub fn applies_at_full(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| current.autonomy.level == crate::security::AutonomyLevel::Full)
    }
}

/// How the caller applies a plan.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApplyOptions<'a> {
    /// The revision the change was based on (`base_revision`, or an
    /// `If-Match` value). `None` uses the plan's own revision.
    pub expected_revision: Option<&'a str>,
    /// The caller confirms a change the plan classifies high risk.
    pub confirm_high_risk: bool,
    /// The revision the caller's confirmation names: the `If-Match` of a
    /// confirmed retry, the `revision` argument of `config.patch`, a
    /// rollback's expected revision. A confirmation is bound to the plan the
    /// caller was shown (D-049 B): a high-risk plan is applied only when this
    /// equals the revision the plan is checked against, never without one.
    pub confirmed_revision: Option<&'a str>,
}

/// An operator rollback request. Every flag is an explicit confirmation.
#[derive(Debug, Clone, Copy)]
pub struct RollbackRequest<'a> {
    /// The record whose before-image is restored ("restore to before X":
    /// every later change to those files is undone too).
    pub record_id: &'a str,
    /// The revision the operator saw; required.
    pub expected_revision: &'a str,
    /// Confirms the rollback itself.
    pub confirm: bool,
    /// Allows restoring credentials the current configuration no longer
    /// holds (for example a key rotated out after a leak).
    pub restore_credentials: bool,
    /// Allows a rollback that turns `secrets.encrypt` off.
    pub disable_encryption: bool,
}

/// An applied transaction.
#[derive(Debug)]
pub struct ConfigTransactionOutcome {
    /// The history record; `None` when the files already held the change.
    pub record_id: Option<String>,
    /// One sentence per selection the change made that the evidence does
    /// not verify (accepted for the operator, D-034). Never a credential.
    pub warnings: Vec<String>,
    /// The redacted plan recorded for this transaction.
    pub summary: ConfigChangeSummary,
    /// The activation report of the generation manager.
    pub report: ConfigApplyReport,
}

/// What disk holds after the runtime refused a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotAppliedOutcome {
    /// The previous files are back and active again.
    Restored,
    /// The previous files are back on disk, but re-activating them failed;
    /// the process keeps its previous active configuration.
    RestoredNotReactivated,
    /// The previous files could not be put back (another writer changed
    /// them, or the restore failed); the refused files are still on disk.
    NotRestored,
    /// The files already held this configuration; nothing was written.
    NothingWritten,
}

impl NotAppliedOutcome {
    /// Whether disk holds the configuration from before the transaction.
    #[must_use]
    pub const fn disk_restored(self) -> bool {
        matches!(self, Self::Restored | Self::RestoredNotReactivated)
    }
}

/// Why a transaction did not apply.
#[derive(Debug, thiserror::Error)]
pub enum ConfigTransactionError {
    /// The change does not stage, validate, pass the write-boundary checks
    /// or load; nothing was written. The message can quote a value the
    /// caller sent: scrub it before returning it.
    #[error("invalid configuration: {0:#}")]
    Invalid(anyhow::Error),
    /// The change needs an explicit confirmation the caller did not give;
    /// nothing was written. `required` names the flag.
    #[error("{}", confirmation_message(required, reasons))]
    ConfirmationRequired {
        required: &'static str,
        reasons: Vec<RiskReason>,
        /// The revision the refused plan was computed against (the caller's
        /// or the plan's own): a confirmed retry names it as its expected
        /// revision, so a change in between is a conflict.
        revision: Option<String>,
    },
    /// The configuration changed since the expected revision; nothing was
    /// written.
    #[error(transparent)]
    Conflict(ConfigRevisionConflict),
    /// The runtime refused the change. `outcome` says what disk holds now.
    /// `reason` is redacted.
    #[error("{}", not_applied_message(reason, *outcome))]
    NotApplied {
        record_id: Option<String>,
        reason: String,
        outcome: NotAppliedOutcome,
    },
    /// Writing failed; the commit journal restored the previous files.
    #[error("configuration transaction failed: {0:#}")]
    Failed(anyhow::Error),
}

fn confirmation_message(required: &str, reasons: &[RiskReason]) -> String {
    let reasons = reasons.iter().map(RiskReason::describe).collect::<Vec<_>>();
    if required == "if_match" {
        return format!(
            "Confirmation required (if_match): a high-risk confirmation must name the revision it confirms. \
             Resend it with If-Match set to the revision this answer reports (the `revision` argument for \
             config.patch); `*` or no If-Match confirms nothing. The change {}. Nothing was written.",
            if reasons.is_empty() {
                "is high risk".to_string()
            } else {
                reasons.join("; ")
            }
        );
    }
    if reasons.is_empty() {
        format!("Confirmation required ({required}); nothing was written.")
    } else {
        format!(
            "Confirmation required ({required}): the change {}. Nothing was written.",
            reasons.join("; ")
        )
    }
}

fn not_applied_message(reason: &str, outcome: NotAppliedOutcome) -> String {
    match outcome {
        NotAppliedOutcome::Restored => format!("Not applied: {reason}. The previous configuration was restored."),
        NotAppliedOutcome::RestoredNotReactivated => format!(
            "Not applied: {reason}. The previous configuration was restored on disk, but re-activating it failed; \
             the running process keeps its previous configuration."
        ),
        NotAppliedOutcome::NotRestored => format!(
            "Not applied: {reason}. The previous configuration could not be restored; the rejected configuration \
             is still on disk and the running process keeps its previous configuration."
        ),
        NotAppliedOutcome::NothingWritten => format!(
            "Not applied: {reason}. The files already held this configuration, so nothing was written; \
             activating it failed."
        ),
    }
}

/// Where an applied change is activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activation {
    /// This process runs the configuration's runtime: activation here is
    /// the activation.
    InProcess,
    /// Another process (a running PRX) activates it through its watcher;
    /// this process only validates the load.
    Deferred,
}

/// The configuration transaction service of one process configuration.
#[derive(Clone)]
pub struct ConfigTransactions {
    shared: SharedConfig,
    activation: Activation,
    #[cfg(test)]
    after_deferred_read: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for ConfigTransactions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ConfigTransactions").finish_non_exhaustive()
    }
}

/// What a running PRX's configuration watcher saw of one content of the
/// files, for [`ConfigTransactions::settle_deferred_activation`]. Bound to
/// that content: `revision` is the fingerprint of the files the reload (or
/// the start) evaluated, so a later write is never settled with this
/// outcome (D-101 F-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatcherActivation {
    /// The revision (`ConfigSourceRevision::fingerprint_sha256`) of the
    /// files the outcome is about.
    pub revision: String,
    pub outcome: WatcherOutcome,
}

/// What became of the files a watcher evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherOutcome {
    /// They are the active generation: a reload succeeded, or the process
    /// started on them. The fields that wait for a restart.
    Active { restart_required: Vec<String> },
    /// Their reload was refused; the previous generation stays active. The
    /// reason as the watcher logged it.
    Refused { reason: String },
}

/// What [`ConfigTransactions::settle_deferred_activation`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// The record for the evaluated revision now has this status.
    Settled(RecordStatus),
    /// No deferred record names the evaluated revision yet (the CLI may not
    /// have recorded its commit): retry later.
    NotYet,
    /// Nothing to do: the history cannot be read, or the outcome no longer
    /// describes the active generation.
    Skipped,
}

/// One async lock per configuration file, shared by every surface of this
/// process, so the commit-activate-compensate sequence of one transaction
/// never interleaves with another's.
static TRANSACTION_LOCKS: LazyLock<parking_lot::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

pub(crate) fn transaction_lock(config_path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    Arc::clone(
        TRANSACTION_LOCKS
            .lock()
            .entry(config_path.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

/// The reload trigger each surface reports to the generation manager.
const fn reload_trigger(actor: ConfigActor) -> ConfigReloadTrigger {
    match actor {
        ConfigActor::GatewayConfigApi | ConfigActor::GatewayCredential => ConfigReloadTrigger::Api,
        ConfigActor::GatewayConfigFile => ConfigReloadTrigger::ConfigFileApi,
        ConfigActor::AgentConfigPatch | ConfigActor::AgentProxyConfig => ConfigReloadTrigger::Tool,
        ConfigActor::Rollback | ConfigActor::Cli | ConfigActor::GatewayManagement => ConfigReloadTrigger::Management,
    }
}

/// Run blocking file work off the async runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ConfigTransactionError> + Send + 'static,
) -> Result<T, ConfigTransactionError> {
    crate::runtime::blocking::spawn_blocking(work)
        .await
        .map_err(|error| ConfigTransactionError::Failed(anyhow::anyhow!("configuration worker failed: {error}")))?
}

async fn lock_settlement(history: &ConfigHistory) -> Result<std::fs::File, ConfigTransactionError> {
    let history = history.clone();
    blocking(move || history.lock_settlement().map_err(ConfigTransactionError::Failed)).await
}

/// Resolve every selection `plan` affects under `actor`'s policy, and check
/// every credential reference it adds or re-targets, off the runtime.
/// Returns one warning per selection accepted as unverified.
async fn check_plan_selections(plan: &ConfigPlan, actor: ConfigActor) -> Result<Vec<String>, ConfigTransactionError> {
    let current = plan.current.clone();
    let candidate = Arc::clone(&plan.candidate);
    // Only a plan constructed from stored history retains existing selections.
    // Merely naming the rollback actor never broadens an ordinary write.
    let policy = if actor == ConfigActor::Rollback && plan.restores.is_some() {
        crate::providers::catalog::UnknownPolicy::Invocation
    } else {
        validate::durable_policy(actor)
    };
    blocking(move || {
        if matches!(
            actor,
            ConfigActor::GatewayManagement | ConfigActor::GatewayConfigApi | ConfigActor::GatewayConfigFile
        ) && current.as_ref().is_none_or(|current| {
            crate::providers::selection::default_provider(current)
                != crate::providers::selection::default_provider(&candidate)
                || crate::providers::selection::default_model(current)
                    != crate::providers::selection::default_model(&candidate)
        }) {
            crate::providers::catalog::admission::check_declared(
                &candidate,
                crate::providers::selection::default_provider(&candidate),
                crate::providers::selection::default_model(&candidate),
            )
            .map_err(|error| ConfigTransactionError::Invalid(error.into()))?;
        }
        let selections = validate::check_affected_selections(current.as_deref(), &candidate, policy);
        let references = validate::check_credential_references(current.as_deref(), &candidate);
        match (selections, references) {
            (Ok(warnings), Ok(())) => Ok(warnings),
            (Err(refusal), Ok(())) | (Ok(_), Err(refusal)) => {
                Err(ConfigTransactionError::Invalid(anyhow::anyhow!(refusal)))
            }
            (Err(selections), Err(references)) => Err(ConfigTransactionError::Invalid(anyhow::anyhow!(
                "{selections}; {references}"
            ))),
        }
    })
    .await
}

impl ConfigTransactions {
    #[must_use]
    pub const fn new(shared: SharedConfig) -> Self {
        Self {
            shared,
            activation: Activation::InProcess,
            #[cfg(test)]
            after_deferred_read: None,
        }
    }

    /// The same service for a process that does not run this
    /// configuration's runtime (the `prx` CLI): an applied change is loaded
    /// and validated here, and recorded `activation_deferred`, never
    /// `activated`, because a running PRX activates it through its
    /// configuration watcher and this process cannot confirm that (D-085
    /// F-4). A refusal here is still compensated as usual.
    #[must_use]
    pub const fn deferring_activation(mut self) -> Self {
        self.activation = Activation::Deferred;
        self
    }

    /// The desired configuration a change is built from.
    #[must_use]
    pub fn desired_config(&self) -> Config {
        self.shared.desired().config.as_ref().clone()
    }

    /// The desired configuration a change is built from, with the revision
    /// it was loaded at ([`Self::base_revision`]), read together.
    #[must_use]
    pub fn desired_with_revision(&self) -> (Config, Option<String>) {
        let desired = self.shared.desired();
        (desired.config.as_ref().clone(), Self::base_revision(&desired))
    }

    /// The history of this configuration.
    pub fn history(&self) -> anyhow::Result<ConfigHistory> {
        ConfigHistory::for_config(&self.shared.desired().config.config_path)
    }

    /// Remove every history record (for example after a credential leak,
    /// once the credential is rotated), under the two locks a transaction
    /// holds while it records and commits: the transaction lock of this
    /// process and the configuration writer lock. A record is therefore never
    /// removed while it is being created (PMM-D-8). A transaction in another
    /// process updates its record once more after activation, outside the
    /// writer lock; if the purge ran in between, that update fails and is
    /// logged, nothing else (D-078 T-5). Returns how many records were
    /// removed.
    ///
    /// # Errors
    ///
    /// When a lock cannot be taken or a record cannot be removed.
    pub async fn purge_history(&self) -> Result<usize, ConfigTransactionError> {
        let config_path = self.shared.desired().config.config_path.clone();
        let lock = transaction_lock(&config_path);
        let _serialized = lock.lock().await;
        let _writer = crate::self_system::evolution::safety_utils::acquire_file_lock(&config_path)
            .await
            .map_err(ConfigTransactionError::Failed)?;
        let history = ConfigHistory::for_config(&config_path).map_err(ConfigTransactionError::Failed)?;
        blocking(move || history.purge().map_err(ConfigTransactionError::Failed)).await
    }

    /// Run `work` on a blocking worker under the two locks a transaction
    /// holds while it records and commits: this process's transaction lock
    /// and the configuration writer lock (which every PRX writer in any
    /// process takes). A check `work` makes of the stored configuration
    /// therefore still holds when it acts on it, and no configuration write
    /// interleaves (D-089 11: the referrer check of a credential removal). The
    /// caller's log dispatcher is kept on the worker. `work` receives the
    /// path of `config.toml`.
    ///
    /// # Errors
    ///
    /// When a lock cannot be taken, the worker fails, or `work` fails.
    pub async fn locked<T, E>(&self, work: impl FnOnce(&Path) -> Result<T, E> + Send + 'static) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<ConfigTransactionError> + Send + 'static,
    {
        let config_path = self.shared.desired().config.config_path.clone();
        let lock = transaction_lock(&config_path);
        let _serialized = lock.lock().await;
        let _writer = crate::self_system::evolution::safety_utils::acquire_file_lock(&config_path)
            .await
            .map_err(|error| E::from(ConfigTransactionError::Failed(error)))?;
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        crate::runtime::blocking::spawn_blocking(move || {
            let _logs = tracing::dispatcher::set_default(&dispatch);
            work(&config_path)
        })
        .await
        .map_err(|error| {
            E::from(ConfigTransactionError::Failed(anyhow::anyhow!(
                "configuration worker failed: {error}"
            )))
        })?
    }

    /// The revision a change derived from `desired` is based on: the disk
    /// fingerprint it was loaded from; `None` for a configuration that did
    /// not come from disk. A caller that builds its change from `desired()`
    /// passes this as the expected revision, so a manual edit or another
    /// writer that landed since is never overwritten.
    #[must_use]
    pub fn base_revision(desired: &DesiredConfigState) -> Option<String> {
        desired
            .source_revision
            .disk_generation
            .map(|_| desired.source_revision.fingerprint_sha256.clone())
    }

    /// Resolve every model selection `plan` affects under `actor`'s policy,
    /// exactly as [`apply`](Self::apply) does before it writes anything, so a
    /// dry run reports the same refusal and the same unverified warnings as
    /// the write would. Nothing is written.
    ///
    /// # Errors
    ///
    /// [`ConfigTransactionError::Invalid`] naming every refused selection.
    pub async fn check_selections(
        &self,
        plan: &ConfigPlan,
        actor: ConfigActor,
    ) -> Result<Vec<String>, ConfigTransactionError> {
        check_plan_selections(plan, actor).await
    }

    /// Validate a change and describe it; nothing is written. Runs on a
    /// blocking worker (staging, loading and decrypting are file I/O).
    pub async fn plan(&self, change: ConfigChange) -> Result<ConfigPlan, ConfigTransactionError> {
        let this = self.clone();
        blocking(move || this.plan_blocking(change)).await
    }

    fn plan_blocking(&self, change: ConfigChange) -> Result<ConfigPlan, ConfigTransactionError> {
        let desired = self.shared.desired();
        let config_path = desired.config.config_path.clone();
        let workspace_dir = desired.config.workspace_dir.clone();
        // Read before the files: a write landing in between makes the
        // commit conflict (safe), never commit over unseen content.
        let planned_revision = planned_revision(&config_path)?;
        let (mutation, posted_file) = match change {
            ConfigChange::Structured(config) => {
                if config.config_path != config_path {
                    return Err(ConfigTransactionError::Invalid(anyhow::anyhow!(
                        "the configuration belongs to a different file"
                    )));
                }
                (files::plan_structured_save(&config), None)
            }
            ConfigChange::RawFile {
                filename,
                contents,
                allow_secret_clear,
            } => {
                files::validate_raw_config_file_name(&filename).map_err(ConfigTransactionError::Invalid)?;
                let posted = posted_file_trees(&config_path, &filename, &contents, allow_secret_clear)?;
                boundary::check_raw_secret_removal(&posted.posted, posted.current.as_ref(), allow_secret_clear)
                    .map_err(|message| ConfigTransactionError::Invalid(anyhow::anyhow!(message)))?;
                (
                    files::plan_config_file_mutation(&config_path, &workspace_dir, &filename, contents),
                    Some(posted),
                )
            }
        };
        let mutation = mutation.map_err(ConfigTransactionError::Invalid)?;
        self.plan_mutation(mutation, None, planned_revision, posted_file)
    }

    fn plan_mutation(
        &self,
        mutation: ConfigMutationPlan,
        restores: Option<String>,
        planned_revision: Option<String>,
        posted_file: Option<PostedFile>,
    ) -> Result<ConfigPlan, ConfigTransactionError> {
        let desired = self.shared.desired();
        let config_path = mutation.config_path().to_path_buf();
        let workspace_dir = desired.config.workspace_dir.clone();
        let stored = mutation
            .effective_stored_tree()
            .context("the staged configuration cannot be read")
            .map_err(ConfigTransactionError::Invalid)?;
        let current_stored = match files::read_merged_toml_with_gate(&config_path) {
            Ok(tree) => Some(tree),
            Err(error) => {
                tracing::warn!(error = %error, "The current configuration cannot be read; checking the change against an empty one");
                None
            }
        };
        let candidate = Config::from_stored_tree(stored.clone(), &config_path, workspace_dir.clone())
            .context("the staged configuration cannot be loaded")
            .map_err(ConfigTransactionError::Invalid)?;
        boundary::check_stored_candidate(&stored, current_stored.as_ref(), candidate.secrets.encrypt)
            .map_err(|message| ConfigTransactionError::Invalid(anyhow::anyhow!(message)))?;
        // The merged tree hides a file the loader skips and a section another
        // file overrides: check the posted file against its own current
        // content as well.
        if let Some(posted) = posted_file {
            boundary::check_stored_candidate(&posted.posted, posted.current.as_ref(), candidate.secrets.encrypt)
                .map_err(|message| ConfigTransactionError::Invalid(anyhow::anyhow!(message)))?;
            mutation
                .refuse_shadowed_raw_changes(&posted.filename, &posted.posted, posted.current.as_ref(), &stored)
                .map_err(ConfigTransactionError::Invalid)?;
        }
        let current = match Config::load_from_path(&config_path, workspace_dir) {
            Ok(current) => Some(current),
            Err(error) => {
                tracing::warn!(error = %error, "The current configuration cannot be loaded; planning against no configuration");
                None
            }
        };
        crate::config::mcp_launch::check_working_dirs(current.as_ref(), &candidate)
            .map_err(|message| ConfigTransactionError::Invalid(anyhow::anyhow!(message)))?;
        // A change to the embedding routes may not create or keep two routes
        // with one hint; an existing duplicate does not block unrelated
        // changes (D-104 C-1 ruling).
        let routes_changed = current.as_ref().is_none_or(|current| {
            serde_json::to_value(&current.embedding_routes).ok()
                != serde_json::to_value(&candidate.embedding_routes).ok()
        });
        if routes_changed {
            let duplicates = crate::config::schema::duplicate_embedding_route_hints(&candidate);
            if !duplicates.is_empty() {
                return Err(ConfigTransactionError::Invalid(anyhow::anyhow!(duplicates.join("; "))));
            }
        }
        let mut summary =
            ConfigChangeSummary::between(current.as_ref(), &candidate).map_err(ConfigTransactionError::Failed)?;
        // A stored configuration that exists but does not load is no baseline:
        // every rule that compares with the prior value would fail open, so
        // every change is high risk until it loads again (D-050). A first
        // write, with nothing stored, keeps the first-write rules.
        if current.is_none() && config_path.exists() {
            summary.risk = ConfigChangeRisk::High;
            summary.risk_reasons.push(RiskReason::BaselineUnavailable);
        }
        if restores.is_some() {
            let reintroduced = boundary::reintroduced_credentials(current.as_ref(), &candidate);
            if !reintroduced.is_empty() {
                summary.risk = ConfigChangeRisk::High;
                summary.risk_reasons.extend(
                    reintroduced
                        .into_iter()
                        .map(|path| RiskReason::CredentialReintroduced { path }),
                );
            }
        }
        Ok(ConfigPlan {
            mutation,
            candidate: Arc::new(candidate),
            restores,
            summary,
            planned_revision,
            current: current.map(Arc::new),
        })
    }

    /// Commit a planned change and activate it; restore the previous
    /// configuration if activation is refused.
    ///
    /// The commit is guarded by `options.expected_revision`, or by the
    /// plan's own revision when the caller names none, so it never lands on
    /// files that changed since. A high-risk plan needs
    /// `options.confirm_high_risk`.
    pub async fn apply(
        &self,
        plan: ConfigPlan,
        actor: ConfigActor,
        options: ApplyOptions<'_>,
    ) -> Result<ConfigTransactionOutcome, ConfigTransactionError> {
        self.apply_scoped(plan, actor, options, None).await
    }

    /// [`apply`](Self::apply) for an agent-management change: a plan whose
    /// every change lies inside `agents.<agent>` needs no confirmation
    /// whatever its risk class; any other plan is held to the ordinary rule.
    pub async fn apply_agent_subtree(
        &self,
        plan: ConfigPlan,
        actor: ConfigActor,
        options: ApplyOptions<'_>,
        agent: &str,
    ) -> Result<ConfigTransactionOutcome, ConfigTransactionError> {
        self.apply_scoped(plan, actor, options, Some(agent)).await
    }

    async fn apply_scoped(
        &self,
        plan: ConfigPlan,
        actor: ConfigActor,
        options: ApplyOptions<'_>,
        agent: Option<&str>,
    ) -> Result<ConfigTransactionOutcome, ConfigTransactionError> {
        self.shared
            .ensure_runtime_writable()
            .map_err(ConfigTransactionError::Failed)?;
        let expected = options
            .expected_revision
            .map(str::to_string)
            .or_else(|| plan.planned_revision.clone());
        // Every selection the change affects resolves before anything else
        // is asked of the caller: a refused model is refused, not confirmed.
        let warnings = check_plan_selections(&plan, actor).await?;
        let confined = agent.is_some_and(|agent| super::agent_scope::confines(&plan.summary, agent));
        // At an effective `Full` level nothing is confirmed: the `confirm`
        // inputs stay accepted and are not required. The commit below is
        // still guarded by the expected revision (a stale write is a 409).
        if plan.summary.risk == ConfigChangeRisk::High && !confined && !plan.applies_at_full() {
            if !options.confirm_high_risk {
                return Err(ConfigTransactionError::ConfirmationRequired {
                    required: "confirm_high_risk",
                    reasons: plan.summary.risk_reasons.clone(),
                    revision: expected,
                });
            }
            if expected.is_none() {
                return Err(ConfigTransactionError::Invalid(anyhow::anyhow!(
                    "a high-risk change needs the revision it is based on"
                )));
            }
            // The confirmation must name the revision the refusal reported;
            // `*` and an absent `If-Match` confirm nothing (D-049 B).
            let Some(confirmed) = options.confirmed_revision else {
                return Err(ConfigTransactionError::ConfirmationRequired {
                    required: "if_match",
                    reasons: plan.summary.risk_reasons.clone(),
                    revision: expected,
                });
            };
            if expected.as_deref() != Some(confirmed) {
                return Err(ConfigTransactionError::Conflict(ConfigRevisionConflict {
                    expected: confirmed.to_string(),
                    actual: expected.unwrap_or_default(),
                }));
            }
        }
        let config_path = plan.mutation.config_path().to_path_buf();
        let workspace_dir = plan.candidate.workspace_dir.clone();
        let lock = transaction_lock(&config_path);
        let _serialized = lock.lock().await;
        let history = ConfigHistory::for_config(&config_path).map_err(ConfigTransactionError::Failed)?;

        let ConfigPlan {
            mutation,
            candidate,
            restores,
            summary: planned_summary,
            planned_revision: _,
            current: _,
        } = plan;
        let slot: Arc<parking_lot::Mutex<Option<HistoryRecord>>> = Arc::new(parking_lot::Mutex::new(None));
        let hook = {
            let slot = Arc::clone(&slot);
            let history = history.clone();
            let config_path = config_path.clone();
            let workspace_dir = workspace_dir.clone();
            let base_revision = expected.clone();
            move |before: &ConfigBeforeImage| -> anyhow::Result<()> {
                let previous = load_before_image(&config_path, &workspace_dir, before);
                let summary = ConfigChangeSummary::between(previous.as_ref(), &candidate)?;
                *slot.lock() = Some(history.create_pending(
                    NewRecord {
                        actor,
                        base_revision: base_revision.as_deref(),
                        restores: restores.as_deref(),
                        summary,
                    },
                    before,
                )?);
                Ok(())
            }
        };
        let committed = files::commit_mutation_guarded(mutation, expected.as_deref(), hook).await;
        let record = slot.lock().take();

        let (before, revision) = match committed {
            Ok(GuardedCommit::Committed { before, revision }) => (before, revision),
            Ok(GuardedCommit::Unchanged { .. }) => {
                let report =
                    self.activate(reload_trigger(actor))
                        .await
                        .map_err(|error| ConfigTransactionError::NotApplied {
                            record_id: None,
                            reason: redacted_reason(&format!("{error:#}")),
                            outcome: NotAppliedOutcome::NothingWritten,
                        })?;
                return Ok(ConfigTransactionOutcome {
                    record_id: None,
                    warnings,
                    summary: planned_summary,
                    report,
                });
            }
            Err(error) => {
                if let Some(conflict) = error.downcast_ref::<ConfigRevisionConflict>() {
                    return Err(ConfigTransactionError::Conflict(conflict.clone()));
                }
                if let Some(mut record) = record {
                    record.status = RecordStatus::CommitFailed;
                    record.reason = Some(redacted_reason(&format!("{error:#}")));
                    update_record(&history, record).await;
                }
                return Err(ConfigTransactionError::Failed(error));
            }
        };
        // The hook ran, so the record exists.
        let mut record = record.ok_or_else(|| {
            ConfigTransactionError::Failed(anyhow::anyhow!("configuration history record missing after commit"))
        })?;
        record.committed_revision = Some(revision.clone());
        // The committed revision is durable before anything is activated, so
        // a running PRX's watcher can bind its settlement to it from now on:
        // a CLI write is `activation_deferred` at once (D-101 F-1).
        if self.activation == Activation::Deferred {
            record.status = RecordStatus::ActivationDeferred;
        }
        update_record(&history, record.clone()).await;

        match self.activate(reload_trigger(actor)).await {
            Ok(report) => {
                let record_id = record.id.clone();
                let summary = record.summary.clone();
                // A deferred record is settled by the running PRX's watcher
                // from here on; this process never overwrites it.
                if self.activation == Activation::InProcess {
                    record.status = RecordStatus::Activated;
                    record.active_generation = Some(report.active_generation.0);
                    record.restart_required.clone_from(&report.restart_required);
                    update_record(&history, record).await;
                }
                Ok(ConfigTransactionOutcome {
                    record_id: Some(record_id),
                    warnings,
                    summary,
                    report,
                })
            }
            Err(error) => {
                let reason = redacted_reason(&format!("{error:#}"));
                // A daemon can settle the deferred record in another process.
                // Keep this lock through the fresh read, compensation and
                // final update so an activated change is never undone.
                let _settlement_lock = if self.activation == Activation::Deferred {
                    match lock_settlement(&history).await {
                        Ok(lock) => Some(lock),
                        Err(lock_error) => {
                            return Err(ConfigTransactionError::NotApplied {
                                record_id: Some(record.id.clone()),
                                reason: format!("{reason}; settlement lock unavailable: {lock_error}"),
                                outcome: NotAppliedOutcome::NotRestored,
                            });
                        }
                    }
                } else {
                    None
                };
                // A deferred record a running PRX already settled is its
                // verdict, never overwritten or undone here (D-104 C-4).
                if self.activation == Activation::Deferred {
                    let settled = {
                        let history = history.clone();
                        let record_id = record.id.clone();
                        blocking(move || history.get(&record_id).map_err(ConfigTransactionError::Failed))
                            .await
                            .map(|stored| stored.status)
                    };
                    match settled {
                        Ok(RecordStatus::ActivationDeferred) =>
                        {
                            #[cfg(test)]
                            if let Some(hook) = &self.after_deferred_read {
                                hook();
                            }
                        }
                        Ok(status) => {
                            return Err(ConfigTransactionError::NotApplied {
                                record_id: Some(record.id.clone()),
                                reason: format!(
                                    "{reason}; a running PRX already settled the change as {}, so it is left as it is",
                                    status.as_str()
                                ),
                                outcome: NotAppliedOutcome::NotRestored,
                            });
                        }
                        Err(read_error) => {
                            return Err(ConfigTransactionError::NotApplied {
                                record_id: Some(record.id.clone()),
                                reason: format!("{reason}; settlement state unavailable: {read_error}"),
                                outcome: NotAppliedOutcome::NotRestored,
                            });
                        }
                    }
                }
                let (outcome, restore_error) = self.compensate(&config_path, &workspace_dir, before, &revision).await;
                let conflict = restore_error
                    .as_ref()
                    .is_some_and(|error| error.downcast_ref::<ConfigRevisionConflict>().is_some());
                record.status = if conflict {
                    // A later write (or a running PRX's restore) replaced the
                    // files: nothing of it is undone (D-104 C-4).
                    RecordStatus::Superseded
                } else if outcome.disk_restored() {
                    RecordStatus::RolledBack
                } else {
                    RecordStatus::NotRestored
                };
                record.reason = Some(restore_error.as_ref().map_or_else(
                    || reason.clone(),
                    |restore_error| format!("{reason}; restore: {}", redacted_reason(&format!("{restore_error:#}"))),
                ));
                record.active_generation = Some(self.shared.active_generation_id().0);
                let record_id = record.id.clone();
                tracing::warn!(
                    record = %record_id,
                    outcome = ?outcome,
                    "Configuration change was committed but not activated"
                );
                update_record(&history, record).await;
                Err(ConfigTransactionError::NotApplied {
                    record_id: Some(record_id),
                    reason,
                    outcome,
                })
            }
        }
    }

    /// The plan a [`rollback`](Self::rollback) of `record_id` commits: the
    /// record's before-image against the current configuration, so it shows
    /// everything the restore writes (every later change is undone too).
    /// Nothing is written; the Gateway shows it and derives its gate level
    /// from it before asking for the confirmation.
    ///
    /// # Errors
    ///
    /// A missing record, or a before-image that does not validate.
    pub async fn plan_rollback(&self, record_id: &str) -> Result<ConfigPlan, ConfigTransactionError> {
        let this = self.clone();
        let record_id = record_id.to_string();
        blocking(move || {
            let history = this.history().map_err(ConfigTransactionError::Failed)?;
            // A replaced credential's previous value is not kept: its record
            // cannot be undone, and its before-image (the configuration then)
            // would only revert later configuration changes (D-101 F-2).
            let record = history.get(&record_id).map_err(ConfigTransactionError::Invalid)?;
            if !record.is_restorable() {
                return Err(ConfigTransactionError::Invalid(anyhow::anyhow!(
                    "record {record_id} replaced a credential reference's value, which is not kept; it cannot be \
                     rolled back. Set the credential again with `prx providers credential set`"
                )));
            }
            let before = history
                .before_image(&record_id)
                .map_err(ConfigTransactionError::Invalid)?;
            let desired = this.shared.desired();
            let mutation =
                files::plan_before_image_restore(&desired.config.config_path, &desired.config.workspace_dir, &before)
                    .context("the recorded configuration does not validate")
                    .map_err(ConfigTransactionError::Invalid)?;
            this.plan_mutation(mutation, Some(record_id), None, None)
        })
        .await
    }

    /// Restore the before-image a history record holds ("restore to before
    /// X": every later change to those files is undone too), as a new, fully
    /// validated transaction with its own history record.
    ///
    /// Refused with nothing written unless confirmed, when the files are no
    /// longer at `expected_revision`, when the restored configuration would
    /// bring back a credential the current configuration no longer holds
    /// (without `restore_credentials`) or turn encryption off (without
    /// `disable_encryption`), when it introduces a mask or plaintext at an
    /// encrypted slot, or when its credentials can no longer be decrypted.
    /// A model selection it would restore that no longer resolves is refused
    /// before any confirmation is asked for.
    pub async fn rollback(
        &self,
        request: RollbackRequest<'_>,
    ) -> Result<ConfigTransactionOutcome, ConfigTransactionError> {
        let plan = self.plan_rollback(request.record_id).await?;
        // The selections it would restore resolve before any confirmation is
        // asked for, as in `apply`: a refused restore is refused, not
        // confirmed first (D-080 N-3).
        check_plan_selections(&plan, ConfigActor::Rollback).await?;
        let reasons = &plan.summary.risk_reasons;
        let reintroduced = reasons
            .iter()
            .filter(|reason| matches!(reason, RiskReason::CredentialReintroduced { .. }))
            .cloned()
            .collect::<Vec<_>>();
        if !reintroduced.is_empty() && !request.restore_credentials {
            return Err(ConfigTransactionError::ConfirmationRequired {
                required: "restore_credentials",
                reasons: reintroduced,
                revision: Some(request.expected_revision.to_string()),
            });
        }
        if reasons.contains(&RiskReason::EncryptionDisabled) && !request.disable_encryption {
            return Err(ConfigTransactionError::ConfirmationRequired {
                required: "disable_encryption",
                reasons: vec![RiskReason::EncryptionDisabled],
                revision: Some(request.expected_revision.to_string()),
            });
        }
        // At Full the rollback itself needs no confirmation; the revision
        // binding, `restore_credentials` and `disable_encryption` stay.
        if !request.confirm && !plan.applies_at_full() {
            return Err(ConfigTransactionError::ConfirmationRequired {
                required: "confirm",
                reasons: reasons.clone(),
                revision: Some(request.expected_revision.to_string()),
            });
        }
        self.apply(
            plan,
            ConfigActor::Rollback,
            ApplyOptions {
                expected_revision: Some(request.expected_revision),
                confirm_high_risk: true,
                confirmed_revision: Some(request.expected_revision),
            },
        )
        .await
    }

    /// Settle the `activation_deferred` record (written by the `prx` CLI,
    /// which cannot activate) whose commit is the content a running PRX's
    /// watcher evaluated, from what it saw of that content (D-085 F-4,
    /// D-101 F-1):
    ///
    /// - active: only when the active generation was loaded from exactly
    ///   that revision, the record becomes `activated`, with the generation
    ///   and the fields waiting for a restart;
    /// - refused: only while the files are still at that revision, the
    ///   record's before-image is committed back (guarded by that revision,
    ///   so a later writer is never overwritten) and re-activated, as an
    ///   in-process transaction compensates; the record becomes
    ///   `rolled_back`, or `not_restored` when the restore fails. When a
    ///   later write already replaced the refused content, nothing is
    ///   restored and the record becomes `superseded`.
    ///
    /// Every other deferred record whose commit is no longer on disk becomes
    /// `superseded` too (a later write replaced it before any PRX settled
    /// it; its before-image stays restorable). Runs under the transaction
    /// lock. [`Settlement::NotYet`] when no record names the revision yet:
    /// the caller retries.
    pub async fn settle_deferred_activation(&self, seen: WatcherActivation) -> Settlement {
        let desired = self.shared.desired();
        let config_path = desired.config.config_path.clone();
        let workspace_dir = desired.config.workspace_dir.clone();
        let lock = transaction_lock(&config_path);
        let _serialized = lock.lock().await;
        let path = config_path.clone();
        let found = blocking(move || {
            if !path.exists() {
                return Ok(None);
            }
            let history = ConfigHistory::for_config(&path).map_err(ConfigTransactionError::Failed)?;
            let deferred = history
                .list()
                .map_err(ConfigTransactionError::Failed)?
                .into_iter()
                .filter(|record| record.status == RecordStatus::ActivationDeferred)
                .collect::<Vec<_>>();
            Ok(Some((history, deferred)))
        })
        .await;
        let (history, deferred) = match found {
            Ok(Some(found)) => found,
            Ok(None) => return Settlement::Skipped,
            Err(error) => {
                tracing::warn!(error = %error, "Failed to read the configuration history to settle a deferred change");
                return Settlement::NotYet;
            }
        };
        let mut settled = Settlement::NotYet;
        for record in deferred {
            let _settlement_lock = match lock_settlement(&history).await {
                Ok(lock) => lock,
                Err(error) => {
                    tracing::warn!(error = %error, "Failed to lock deferred activation settlement");
                    return Settlement::NotYet;
                }
            };
            let record_id = record.id.clone();
            let history_for_read = history.clone();
            let reread =
                blocking(move || history_for_read.get(&record_id).map_err(ConfigTransactionError::Failed)).await;
            let mut record = match reread {
                Ok(record) if record.status == RecordStatus::ActivationDeferred => record,
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(error = %error, "Failed to reread deferred activation record");
                    return Settlement::NotYet;
                }
            };
            let path = config_path.clone();
            let revision =
                blocking(move || files::current_config_revision(&path).map_err(ConfigTransactionError::Failed)).await;
            let current = match revision {
                Ok(current) => current,
                Err(error) => {
                    tracing::warn!(error = %error, "Failed to reread configuration revision for settlement");
                    return Settlement::NotYet;
                }
            };
            let committed = record.committed_revision.clone().unwrap_or_default();
            if committed == seen.revision {
                match &seen.outcome {
                    WatcherOutcome::Active { restart_required } => {
                        // Only a generation loaded from exactly these files
                        // confirms the change.
                        if self.shared.pin().source_revision.fingerprint_sha256 != seen.revision {
                            settled = Settlement::Skipped;
                            continue;
                        }
                        record.status = RecordStatus::Activated;
                        record.active_generation = Some(self.shared.active_generation_id().0);
                        record.restart_required.clone_from(restart_required);
                        record.reason = None;
                    }
                    WatcherOutcome::Refused { reason } => {
                        let reason = redacted_reason(reason);
                        if current == seen.revision {
                            self.restore_refused(&history, &mut record, &config_path, &workspace_dir, &reason)
                                .await;
                        } else {
                            record.status = RecordStatus::Superseded;
                            record.reason = Some(format!(
                                "the running PRX refused it ({reason}), but a later write had already replaced it; \
                                 nothing was restored"
                            ));
                        }
                        record.active_generation = Some(self.shared.active_generation_id().0);
                        tracing::warn!(
                            record = %record.id,
                            status = record.status.as_str(),
                            "A running PRX refused a configuration change written by the CLI"
                        );
                    }
                }
                settled = Settlement::Settled(record.status);
            } else if committed != current {
                record.status = RecordStatus::Superseded;
                record.reason = Some(
                    "a later write replaced it before a running PRX settled it; its before-image stays restorable"
                        .to_string(),
                );
            } else {
                // The current files, but not the content this outcome is
                // about: its own watcher outcome settles it.
                continue;
            }
            update_record(&history, record).await;
        }
        settled
    }

    /// Commit the before-image of a refused deferred `record` back (guarded
    /// by its committed revision) and re-activate it; set the record's
    /// status and reason. A guard conflict (a write that landed meanwhile)
    /// is `superseded`, never `not_restored`.
    async fn restore_refused(
        &self,
        history: &ConfigHistory,
        record: &mut HistoryRecord,
        config_path: &Path,
        workspace_dir: &Path,
        reason: &str,
    ) {
        let history = history.clone();
        let record_id = record.id.clone();
        let committed = record.committed_revision.clone().unwrap_or_default();
        let before = blocking(move || history.before_image(&record_id).map_err(ConfigTransactionError::Failed)).await;
        let (status, restore_error) = match before {
            Ok(before) => {
                let (outcome, restore_error) = self.compensate(config_path, workspace_dir, before, &committed).await;
                let conflict = restore_error
                    .as_ref()
                    .is_some_and(|error| error.downcast_ref::<ConfigRevisionConflict>().is_some());
                let status = if conflict {
                    RecordStatus::Superseded
                } else if outcome.disk_restored() {
                    RecordStatus::RolledBack
                } else {
                    RecordStatus::NotRestored
                };
                (status, restore_error.map(|error| format!("{error:#}")))
            }
            Err(error) => (RecordStatus::NotRestored, Some(error.to_string())),
        };
        record.status = status;
        record.reason = Some(match (status, restore_error) {
            (RecordStatus::Superseded, _) => format!(
                "the running PRX refused it ({reason}), but a later write replaced it before the restore; nothing \
                 was restored"
            ),
            (_, None) => format!("the running PRX refused it: {reason}"),
            (_, Some(restore_error)) => format!(
                "the running PRX refused it: {reason}; restore: {}",
                redacted_reason(&restore_error)
            ),
        });
    }

    async fn activate(&self, trigger: ConfigReloadTrigger) -> anyhow::Result<ConfigApplyReport> {
        let manager = Arc::clone(&self.shared);
        crate::runtime::blocking::spawn_blocking(move || manager.reload_from_disk(trigger))
            .await
            .map_err(|error| anyhow::anyhow!("configuration activation worker failed: {error}"))?
    }

    /// Put the before-image back and re-activate it. Guarded by the revision
    /// this transaction wrote: if anything changed the files since, they
    /// are left alone. Returns what disk holds and the error, if any.
    async fn compensate(
        &self,
        config_path: &Path,
        workspace_dir: &Path,
        before: ConfigBeforeImage,
        committed_revision: &str,
    ) -> (NotAppliedOutcome, Option<anyhow::Error>) {
        let (path, workspace) = (config_path.to_path_buf(), workspace_dir.to_path_buf());
        let restore = crate::runtime::blocking::spawn_blocking(move || {
            files::plan_before_image_restore(&path, &workspace, &before)
        })
        .await
        .map_err(|error| anyhow::anyhow!("configuration restore worker failed: {error}"))
        .and_then(|plan| plan);
        let written = match restore {
            Ok(plan) => files::commit_mutation_guarded(plan, Some(committed_revision), |_| Ok(())).await,
            Err(error) => Err(error),
        };
        if let Err(error) = written {
            return (NotAppliedOutcome::NotRestored, Some(error));
        }
        match self.activate(ConfigReloadTrigger::Management).await {
            Ok(_) => (NotAppliedOutcome::Restored, None),
            Err(error) => (
                NotAppliedOutcome::RestoredNotReactivated,
                Some(error.context("the restored configuration was written but could not be re-activated")),
            ),
        }
    }
}

/// A raw file write: the posted file's own tree and the same file's current
/// tree (`None` when absent or explicitly permitted to be uninspectable).
struct PostedFile {
    filename: String,
    posted: toml::Value,
    current: Option<toml::Value>,
}

/// Parse the posted document before checking the stored file for secrets.
/// An unreadable baseline needs explicit clearing permission, because its
/// populated secret slots cannot be determined. Never quote parser input.
fn posted_file_trees(
    config_path: &Path,
    filename: &str,
    contents: &str,
    allow_secret_clear: bool,
) -> Result<PostedFile, ConfigTransactionError> {
    let posted = toml::from_str::<toml::Table>(contents)
        .map(toml::Value::Table)
        .map_err(|error| {
            ConfigTransactionError::Invalid(anyhow::anyhow!(super::render::toml_error_location(contents, &error)))
        })?;
    let target = if filename == "config.toml" {
        config_path.to_path_buf()
    } else {
        files::config_dir_path(config_path).join(filename)
    };
    let cannot_inspect = || {
        ConfigTransactionError::Invalid(anyhow::anyhow!(
            "Cannot inspect stored secrets in {filename}: the stored file cannot be read or parsed. \
             Repair it before using PUT /api/config/secrets, or explicitly permit raw-file replacement \
             with the CLI --allow-credential-clear flag or \
             PUT /api/config/files/{filename}?allow_secret_clear=true. Nothing was written."
        ))
    };
    let current = match std::fs::read_to_string(&target) {
        Ok(text) => match toml::from_str::<toml::Table>(&text) {
            Ok(table) => Some(toml::Value::Table(table)),
            Err(_) if allow_secret_clear => None,
            Err(_) => return Err(cannot_inspect()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) if allow_secret_clear => None,
        Err(_) => return Err(cannot_inspect()),
    };
    Ok(PostedFile {
        filename: filename.to_owned(),
        posted,
        current,
    })
}

/// The current revision, read before the plan reads the files; `None` when
/// there is no `config.toml` yet.
fn planned_revision(config_path: &Path) -> Result<Option<String>, ConfigTransactionError> {
    if !config_path.exists() {
        return Ok(None);
    }
    files::current_config_revision(config_path)
        .map(Some)
        .map_err(ConfigTransactionError::Failed)
}

/// The configuration a before-image holds, loaded as a reload would load
/// it; `None` when it cannot be loaded (for example, a hand-broken file).
fn load_before_image(config_path: &Path, workspace_dir: &Path, before: &ConfigBeforeImage) -> Option<Config> {
    match files::plan_before_image_restore(config_path, workspace_dir, before)
        .and_then(|plan| plan.effective_stored_tree())
        .and_then(|tree| Config::from_stored_tree(tree, config_path, workspace_dir.to_path_buf()))
    {
        Ok(config) => Some(config),
        Err(error) => {
            tracing::warn!(error = %error, "The configuration before this transaction cannot be loaded; recording the change against no configuration");
            None
        }
    }
}

/// Persist a record's new state on a blocking worker; a failure is logged,
/// never fatal (the transaction outcome is already decided).
async fn update_record(history: &ConfigHistory, mut record: HistoryRecord) {
    let history = history.clone();
    let result = crate::runtime::blocking::spawn_blocking(move || {
        let outcome = history.update(&mut record);
        (record.id, outcome)
    })
    .await;
    match result {
        Ok((_, Ok(()))) => {}
        Ok((id, Err(error))) => {
            tracing::warn!(record = %id, error = %error, "Failed to update a configuration history record");
        }
        Err(error) => tracing::warn!(error = %error, "Configuration history worker failed"),
    }
}

/// A reason as the transaction reports it: redacted by shape (secret-shaped
/// tokens, URL userinfo, query values and fragments), never by replacing
/// the candidate's secret values as substrings, which garbled any word that
/// happened to equal a short value (D-101 F-4). Reasons come from the loader,
/// the participants and the commit, which name paths and settings, not
/// values.
fn redacted_reason(message: &str) -> String {
    crate::security::redaction::redact_provider_text(&crate::providers::sanitize_api_error(message))
}

#[cfg(test)]
mod tests;
