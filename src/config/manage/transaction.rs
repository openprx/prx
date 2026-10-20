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
    RawFile { filename: String, contents: String },
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
            ConfigChange::RawFile { filename, contents } => {
                let posted = posted_file_trees(&config_path, &filename, &contents);
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
        let expected = options
            .expected_revision
            .map(str::to_string)
            .or_else(|| plan.planned_revision.clone());
        // Every selection the change affects resolves before anything else
        // is asked of the caller: a refused model is refused, not confirmed.
        let warnings = check_plan_selections(&plan, actor).await?;
        if plan.summary.risk == ConfigChangeRisk::High {
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
        if !request.confirm {
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
/// tree (`None` when it does not exist or does not parse).
struct PostedFile {
    posted: toml::Value,
    current: Option<toml::Value>,
}

/// Parse the posted raw file and the file it replaces. A posted file that
/// does not parse is refused later by staging; it is checked as empty here.
fn posted_file_trees(config_path: &Path, filename: &str, contents: &str) -> PostedFile {
    let parse = |text: &str| toml::from_str::<toml::Table>(text).ok().map(toml::Value::Table);
    if filename.contains(['/', '\\']) || filename.contains("..") {
        return PostedFile {
            posted: parse(contents).unwrap_or_else(|| toml::Value::Table(toml::Table::new())),
            current: None,
        };
    }
    let target = if filename == "config.toml" {
        config_path.to_path_buf()
    } else {
        files::config_dir_path(config_path).join(filename)
    };
    let current = match std::fs::read_to_string(&target) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(error = %error, "The file a raw write replaces cannot be read; checking it against an empty file");
            None
        }
    };
    PostedFile {
        posted: parse(contents).unwrap_or_else(|| toml::Value::Table(toml::Table::new())),
        current,
    }
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
mod tests {
    use super::*;
    use crate::config::generation::{ConfigGeneration, ConfigGenerationParticipant, PreparedConfigGeneration};
    use crate::config::secret_paths::fixture;
    use std::collections::BTreeMap;

    /// A runtime participant that refuses any generation whose default
    /// temperature is above 1.5, as a provider rebuild refuses a bad default.
    struct RefusesHotTemperature;

    struct Accepted;

    impl PreparedConfigGeneration for Accepted {
        fn commit(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn rollback(&mut self) {}
    }

    impl ConfigGenerationParticipant for RefusesHotTemperature {
        fn name(&self) -> &'static str {
            "test_refuses_hot_temperature"
        }

        fn prepares_for_field(&self, field: &str) -> bool {
            field == "default_temperature"
        }

        fn prepare(
            &self,
            generation: Arc<ConfigGeneration>,
            _changed_fields: &[String],
        ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
            if generation.effective.default_temperature > 1.5 {
                anyhow::bail!("test participant refuses a temperature above 1.5");
            }
            Ok(Box::new(Accepted))
        }
    }

    /// A participant of the CLI's own validation reload that, while the CLI
    /// activates, plays a running PRX settling the deferred record as
    /// `activated`, then refuses the CLI's activation.
    struct SettlesThenRefuses {
        config_path: PathBuf,
    }

    struct RefusesCliActivation;

    impl ConfigGenerationParticipant for RefusesCliActivation {
        fn name(&self) -> &'static str {
            "test_refuses_cli_activation"
        }

        fn prepares_for_field(&self, field: &str) -> bool {
            field == "default_temperature"
        }

        fn prepare(
            &self,
            _generation: Arc<ConfigGeneration>,
            _changed_fields: &[String],
        ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
            anyhow::bail!("test CLI activation refused")
        }
    }

    impl ConfigGenerationParticipant for SettlesThenRefuses {
        fn name(&self) -> &'static str {
            "test_settles_then_refuses"
        }

        fn prepares_for_field(&self, field: &str) -> bool {
            field == "default_temperature"
        }

        fn prepare(
            &self,
            _generation: Arc<ConfigGeneration>,
            _changed_fields: &[String],
        ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
            let history = ConfigHistory::for_config(&self.config_path)?;
            for mut record in history.list()? {
                if record.status == RecordStatus::ActivationDeferred {
                    record.status = RecordStatus::Activated;
                    history.update(&mut record)?;
                }
            }
            anyhow::bail!("test participant refuses after the daemon settled the change")
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        shared: SharedConfig,
        _participant: Arc<dyn ConfigGenerationParticipant>,
    }

    impl Fixture {
        fn config_path(&self) -> PathBuf {
            self.dir.path().join("config.toml")
        }

        fn tree(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            let mut out = BTreeMap::new();
            out.insert(
                self.config_path(),
                std::fs::read(self.config_path()).expect("test: read config.toml"),
            );
            if let Ok(entries) = std::fs::read_dir(self.dir.path().join("config.d")) {
                for entry in entries {
                    let path = entry.expect("test: entry").path();
                    out.insert(path.clone(), std::fs::read(&path).expect("test: read fragment"));
                }
            }
            out
        }

        fn service(&self) -> ConfigTransactions {
            ConfigTransactions::new(Arc::clone(&self.shared))
        }

        fn desired(&self) -> Config {
            self.shared.desired().config.as_ref().clone()
        }
    }

    /// The secret fixture saved in the split layout and loaded through the
    /// real load path, with the refusing participant registered.
    async fn fixture() -> Fixture {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = fixture::config_in(dir.path());
        crate::config::files::write_split_config(&config, false)
            .await
            .expect("test: write split fixture");
        let loaded =
            Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load fixture");
        let shared = crate::config::new_shared(loaded);
        let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(RefusesHotTemperature);
        shared.register_participant(&participant);
        Fixture {
            dir,
            shared,
            _participant: participant,
        }
    }

    async fn apply_structured(
        fixture: &Fixture,
        change: impl FnOnce(&mut Config),
        expected: Option<&str>,
    ) -> Result<ConfigTransactionOutcome, ConfigTransactionError> {
        let service = fixture.service();
        let mut config = fixture.desired();
        change(&mut config);
        let plan = service.plan(ConfigChange::Structured(Box::new(config))).await?;
        // The confirmation names the revision the change is checked against.
        let planned = plan.planned_revision().map(str::to_string);
        service
            .apply(
                plan,
                ConfigActor::GatewayConfigApi,
                ApplyOptions {
                    expected_revision: expected,
                    confirm_high_risk: true,
                    confirmed_revision: expected.or(planned.as_deref()),
                },
            )
            .await
    }

    #[tokio::test]
    async fn an_activated_change_is_recorded_with_its_redacted_plan() {
        let fixture = fixture().await;
        let generation = fixture.shared.active_generation_id();

        let outcome = apply_structured(&fixture, |config| config.default_temperature = 0.4, None)
            .await
            .expect("test: apply");

        assert_ne!(fixture.shared.active_generation_id(), generation);
        assert!((fixture.shared.load_full().default_temperature - 0.4).abs() < f64::EPSILON);
        let record_id = outcome.record_id.expect("test: recorded");
        let record = fixture
            .service()
            .history()
            .expect("test: history")
            .get(&record_id)
            .expect("test: record");
        assert_eq!(record.status, RecordStatus::Activated);
        assert_eq!(
            record
                .summary
                .changes
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>(),
            vec!["default_temperature"]
        );
        assert_eq!(
            record.committed_revision,
            ConfigTransactions::base_revision(&fixture.shared.desired())
        );
    }

    /// Apply `change` as the `prx` CLI does: a process without the daemon's
    /// participants, recording `activation_deferred`.
    async fn apply_deferred(fixture: &Fixture, change: impl FnOnce(&mut Config)) -> String {
        let cli = ConfigTransactions::new(crate::config::new_shared(fixture.desired())).deferring_activation();
        let mut config = fixture.desired();
        change(&mut config);
        let plan = cli
            .plan(ConfigChange::Structured(Box::new(config)))
            .await
            .expect("test: plan");
        let outcome = cli
            .apply(plan, ConfigActor::Cli, ApplyOptions::default())
            .await
            .expect("test: the CLI writes it");
        let record = outcome.record_id.expect("test: recorded");
        assert_eq!(
            fixture
                .service()
                .history()
                .expect("test: history")
                .get(&record)
                .expect("test: record")
                .status,
            RecordStatus::ActivationDeferred
        );
        record
    }

    /// What the watcher saw of a successful reload.
    fn watcher_active(report: &ConfigApplyReport) -> WatcherActivation {
        WatcherActivation {
            revision: report.desired_source_revision.fingerprint_sha256.clone(),
            outcome: WatcherOutcome::Active {
                restart_required: report.restart_required.clone(),
            },
        }
    }

    /// What the watcher saw of a refused reload: bound to the revision the
    /// reload evaluated.
    fn watcher_refused(fixture: &Fixture, error: &anyhow::Error) -> WatcherActivation {
        WatcherActivation {
            revision: fixture
                .shared
                .status()
                .last_failure
                .and_then(|failure| failure.desired_source_revision)
                .map(|revision| revision.fingerprint_sha256)
                .expect("test: the refused reload names its revision"),
            outcome: WatcherOutcome::Refused {
                reason: format!("{error:#}"),
            },
        }
    }

    fn record_of(fixture: &Fixture, id: &str) -> HistoryRecord {
        fixture
            .service()
            .history()
            .expect("test: history")
            .get(id)
            .expect("test: record")
    }

    /// RED before PMM-H (D-085 F-4): a change the CLI wrote stayed
    /// `activation_deferred` whatever the running PRX did with it. The
    /// daemon's watcher now settles it: `activated` when the reload took it.
    #[tokio::test]
    async fn the_watcher_marks_a_deferred_change_it_activated() {
        let fixture = fixture().await;
        let record = apply_deferred(&fixture, |config| config.default_temperature = 0.4).await;

        let report = fixture
            .shared
            .reload_from_disk(ConfigReloadTrigger::FileWatcher)
            .expect("test: the daemon takes it");
        let settled = fixture
            .service()
            .settle_deferred_activation(watcher_active(&report))
            .await;

        assert_eq!(settled, Settlement::Settled(RecordStatus::Activated));
        let stored = record_of(&fixture, &record);
        assert_eq!(stored.status, RecordStatus::Activated);
        assert_eq!(stored.active_generation, Some(fixture.shared.active_generation_id().0));
        // Settled once: nothing names the revision any more.
        assert_eq!(
            fixture
                .service()
                .settle_deferred_activation(watcher_active(&report))
                .await,
            Settlement::NotYet
        );
    }

    /// RED before PMM-H (D-085 F-4): a CLI change the running PRX refused
    /// stayed on disk (desired and active disagreed) and the record said
    /// `activation_deferred`. The watcher now restores the before-image,
    /// re-activates it, and records `rolled_back` with the reason.
    #[tokio::test]
    async fn the_watcher_restores_a_deferred_change_it_refused() {
        let fixture = fixture().await;
        let before = fixture.tree();
        let generation = fixture.shared.active_generation_id();
        let record = apply_deferred(&fixture, |config| config.default_temperature = 1.9).await;
        assert_ne!(fixture.tree(), before, "test precondition: the CLI wrote the change");

        let refused = fixture
            .shared
            .reload_from_disk(ConfigReloadTrigger::FileWatcher)
            .expect_err("test: the daemon's participant refuses it");
        let settled = fixture
            .service()
            .settle_deferred_activation(watcher_refused(&fixture, &refused))
            .await;

        assert_eq!(settled, Settlement::Settled(RecordStatus::RolledBack));
        assert_eq!(fixture.tree(), before, "the refused change is still on disk");
        assert!((fixture.shared.load_full().default_temperature - 1.9).abs() > f64::EPSILON);
        let stored = record_of(&fixture, &record);
        assert_eq!(stored.status, RecordStatus::RolledBack);
        let reason = stored.reason.unwrap_or_default();
        assert!(reason.contains("refuses a temperature above 1.5"), "{reason}");
        assert!(fixture.shared.active_generation_id().0 >= generation.0);
    }

    /// RED before PMMH-f (D-101 F-1, R-PMMH race A): a second CLI write that
    /// lands between the watcher's refused reload of the first and its
    /// settlement was compensated with the first write's refusal (the valid
    /// second change rolled back, the refused content written back). The
    /// settlement is bound to the refused revision now: nothing is restored
    /// over the later write, the refused record becomes `superseded`, and
    /// the second record is left for its own outcome.
    #[tokio::test]
    async fn a_refusal_never_restores_over_a_later_write() {
        let fixture = fixture().await;
        let first = apply_deferred(&fixture, |config| config.default_temperature = 1.9).await;
        let refused = fixture
            .shared
            .reload_from_disk(ConfigReloadTrigger::FileWatcher)
            .expect_err("test: the daemon's participant refuses it");
        let seen = watcher_refused(&fixture, &refused);
        // The second write lands before the settlement.
        let second = apply_deferred(&fixture, |config| config.default_temperature = 0.4).await;
        let after_second = fixture.tree();

        let settled = fixture.service().settle_deferred_activation(seen).await;

        assert_eq!(settled, Settlement::Settled(RecordStatus::Superseded));
        assert_eq!(fixture.tree(), after_second, "the later write was reverted");
        assert_eq!(record_of(&fixture, &first).status, RecordStatus::Superseded);
        assert_eq!(record_of(&fixture, &second).status, RecordStatus::ActivationDeferred);
        // The second write's own outcome settles it.
        let report = fixture
            .shared
            .reload_from_disk(ConfigReloadTrigger::FileWatcher)
            .expect("test: the daemon takes the second write");
        assert_eq!(
            fixture
                .service()
                .settle_deferred_activation(watcher_active(&report))
                .await,
            Settlement::Settled(RecordStatus::Activated)
        );
        assert_eq!(record_of(&fixture, &second).status, RecordStatus::Activated);
    }

    #[tokio::test]
    async fn a_stale_refusal_after_return_to_baseline_does_not_restore_over_a_newer_write() {
        let fixture = fixture().await;
        let baseline = fixture.tree();
        let baseline_revision =
            crate::config::files::current_config_revision(&fixture.config_path()).expect("test: baseline revision");
        let first = apply_deferred(&fixture, |config| config.default_temperature = 1.9).await;
        let refused = fixture
            .shared
            .reload_from_disk(ConfigReloadTrigger::FileWatcher)
            .expect_err("test: watcher refuses R1 before settlement");
        let seen = watcher_refused(&fixture, &refused);

        // Stage the exact R0 files while the captured R1 refusal is still
        // pending. The CLI's structured rendering is not byte-identical.
        for (path, bytes) in baseline {
            std::fs::write(path, bytes).expect("test: restore exact R0 file");
        }
        assert_eq!(
            crate::config::files::current_config_revision(&fixture.config_path()).expect("test: R0 revision"),
            baseline_revision,
            "test: R0 returned to byte-identical files"
        );
        let second = apply_deferred(&fixture, |config| config.default_temperature = 0.4).await;
        let after_second = fixture.tree();

        assert_eq!(
            fixture.service().settle_deferred_activation(seen).await,
            Settlement::Settled(RecordStatus::Superseded)
        );
        assert_eq!(fixture.tree(), after_second, "stale R1 outcome restored over R2");
        assert_eq!(record_of(&fixture, &first).status, RecordStatus::Superseded);
        assert_eq!(record_of(&fixture, &second).status, RecordStatus::ActivationDeferred);

        let report = fixture
            .shared
            .reload_from_disk(ConfigReloadTrigger::FileWatcher)
            .expect("test: watcher loads R2");
        assert_eq!(
            fixture
                .service()
                .settle_deferred_activation(watcher_active(&report))
                .await,
            Settlement::Settled(RecordStatus::Activated)
        );
        assert_eq!(record_of(&fixture, &second).status, RecordStatus::Activated);
    }

    /// RED before PMMH-f (D-101 F-1, R-PMMH race B): the CLI recorded its
    /// committed revision only after its own validation reload, so a
    /// watcher reload that ran first found no record and nothing retried. The
    /// revision is recorded right after the commit now, and an outcome that
    /// finds no record yet reports `NotYet`, so the watcher retries and
    /// settles it once the record names the revision.
    #[tokio::test]
    async fn an_outcome_that_finds_no_record_yet_is_settled_on_retry() {
        let fixture = fixture().await;
        let record = apply_deferred(&fixture, |config| config.default_temperature = 0.4).await;
        let recorded = record_of(&fixture, &record);
        let committed = recorded
            .committed_revision
            .clone()
            .expect("test: recorded right after the commit");
        assert_eq!(
            Some(committed.as_str()),
            crate::config::files::current_config_revision(&fixture.config_path())
                .ok()
                .as_deref()
        );
        // The window before the CLI recorded its commit.
        let history = fixture.service().history().expect("test: history");
        let mut hidden = recorded.clone();
        hidden.status = RecordStatus::Pending;
        hidden.committed_revision = None;
        history.update(&mut hidden).expect("test: hide the record");
        let report = fixture
            .shared
            .reload_from_disk(ConfigReloadTrigger::FileWatcher)
            .expect("test: the daemon takes it");
        assert_eq!(
            fixture
                .service()
                .settle_deferred_activation(watcher_active(&report))
                .await,
            Settlement::NotYet
        );
        // The CLI records it; the retry settles it.
        let mut shown = recorded;
        history.update(&mut shown).expect("test: record it");
        assert_eq!(
            fixture
                .service()
                .settle_deferred_activation(watcher_active(&report))
                .await,
            Settlement::Settled(RecordStatus::Activated)
        );
    }

    /// RED before PMMH-f (D-101 F-4): the transaction scrubbed its reasons by
    /// replacing every secret value of the candidate as a substring, so a
    /// short value garbled unrelated words of a refusal ("a *** above
    /// 1.5"). Reasons are redacted by shape now: the refusal reads as
    /// written, and a secret-shaped token is still masked.
    #[tokio::test]
    async fn a_refusal_reason_is_not_garbled_by_a_secret_value() {
        let fixture = fixture().await;
        let error = apply_structured(
            &fixture,
            |config| {
                config.api_key = Some("temperature".into());
                config.default_temperature = 1.9;
            },
            None,
        )
        .await
        .expect_err("test: the participant refuses");
        let ConfigTransactionError::NotApplied { reason, record_id, .. } = error else {
            panic!("expected NotApplied, got {error}");
        };
        assert!(reason.contains("refuses a temperature above 1.5"), "{reason}");
        let stored = record_of(&fixture, &record_id.expect("test: recorded"));
        assert!(
            stored
                .reason
                .unwrap_or_default()
                .contains("refuses a temperature above 1.5"),
            "the history reason was garbled"
        );
        assert!(
            !redacted_reason("provider said: invalid key sk-proj-abcdefghijklmnopqrstuvwxyz0123")
                .contains("abcdefghijklmnop"),
            "a secret-shaped token must stay masked"
        );
    }

    /// RED before PMMH-g (D-104 C-4): when the CLI's own validation
    /// reload failed after a running PRX had already settled its change,
    /// the CLI compensated anyway and overwrote the settled record. A
    /// settled record is left as it is now, and so are the files.
    #[tokio::test]
    async fn the_cli_never_overwrites_a_settled_record() {
        let fixture = fixture().await;
        let cli_shared = crate::config::new_shared(fixture.desired());
        let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(SettlesThenRefuses {
            config_path: fixture.config_path(),
        });
        cli_shared.register_participant(&participant);
        let cli = ConfigTransactions::new(cli_shared).deferring_activation();
        let mut config = fixture.desired();
        config.default_temperature = 0.4;
        let plan = cli
            .plan(ConfigChange::Structured(Box::new(config)))
            .await
            .expect("test: plan");

        let error = cli
            .apply(plan, ConfigActor::Cli, ApplyOptions::default())
            .await
            .expect_err("test: the CLI's validation is refused");
        let ConfigTransactionError::NotApplied { record_id, reason, .. } = error else {
            panic!("expected NotApplied, got {error}");
        };
        let record = record_of(&fixture, &record_id.expect("test: recorded"));

        assert_eq!(
            record.status,
            RecordStatus::Activated,
            "the settled record was overwritten"
        );
        assert!(reason.contains("already settled"), "{reason}");
        let on_disk =
            Config::load_from_path(&fixture.config_path(), fixture.desired().workspace_dir).expect("test: load");
        assert!(
            (on_disk.default_temperature - 0.4).abs() < f64::EPSILON,
            "the settled change was undone"
        );
    }

    /// The other process starts settlement after the CLI read its deferred
    /// record. It must wait for the CLI's compensation and then see the
    /// final status, rather than marking a restored change as activated.
    #[tokio::test]
    async fn a_settlement_after_the_cli_read_cannot_race_compensation() {
        use fs2::FileExt as _;

        let fixture = fixture().await;
        let cli_shared = crate::config::new_shared(fixture.desired());
        let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(RefusesCliActivation);
        cli_shared.register_participant(&participant);
        let mut cli = ConfigTransactions::new(cli_shared).deferring_activation();
        let history = cli.history().expect("test: history");
        let lock_path = fixture.dir.path().join(".config-history.settlement.lock");
        let contender = Arc::new(parking_lot::Mutex::new(None));
        let contender_for_hook = Arc::clone(&contender);
        cli.after_deferred_read = Some(Arc::new(move || {
            let second = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .expect("test: second lock handle");
            assert!(
                second.try_lock_exclusive().is_err(),
                "the CLI holds the cross-process lock after its status read"
            );
            let record = history
                .list()
                .expect("test: history")
                .into_iter()
                .find(|record| record.status == RecordStatus::ActivationDeferred)
                .expect("test: deferred record");
            let record_id = record.id;
            let history = history.clone();
            let (ready, started) = std::sync::mpsc::channel();
            let handle = std::thread::spawn(move || {
                ready.send(()).expect("test: contender ready");
                let _lock = history.lock_settlement().expect("test: contender lock");
                let mut stored = history.get(&record_id).expect("test: settled record");
                if stored.status == RecordStatus::ActivationDeferred {
                    stored.status = RecordStatus::Activated;
                    history.update(&mut stored).expect("test: settle");
                }
                stored.status
            });
            started.recv().expect("test: contender started");
            *contender_for_hook.lock() = Some(handle);
        }));
        let mut config = fixture.desired();
        config.default_temperature = 0.4;
        let plan = cli
            .plan(ConfigChange::Structured(Box::new(config)))
            .await
            .expect("test: plan");
        let error = cli
            .apply(plan, ConfigActor::Cli, ApplyOptions::default())
            .await
            .expect_err("test: CLI activation refused");
        let ConfigTransactionError::NotApplied { record_id, .. } = error else {
            panic!("expected NotApplied, got {error}");
        };
        let observed = contender
            .lock()
            .take()
            .expect("test: contender")
            .join()
            .expect("test: contender joined");
        assert_eq!(observed, RecordStatus::RolledBack);
        assert_eq!(
            record_of(&fixture, &record_id.expect("test: record id")).status,
            RecordStatus::RolledBack
        );
        let on_disk = Config::load_from_path(&fixture.config_path(), fixture.desired().workspace_dir)
            .expect("test: restored config");
        assert_ne!(on_disk.default_temperature, 0.4);
    }

    /// RED before PMMH-g (D-104 C-5): a credential record that never reached
    /// `credential_replaced` (`pending` after a crash, `commit_failed`) was
    /// still offered for rollback. Restorability follows what the record
    /// changed, not only its status.
    #[tokio::test]
    async fn an_unfinished_credential_record_is_not_rolled_back() {
        let fixture = fixture().await;
        let history = fixture.service().history().expect("test: history");
        let before = crate::config::files::current_before_image(&fixture.config_path()).expect("test: image");
        for status in [RecordStatus::Pending, RecordStatus::CommitFailed] {
            let mut record = history
                .create_pending(
                    NewRecord {
                        actor: ConfigActor::Cli,
                        base_revision: None,
                        restores: None,
                        summary: ConfigChangeSummary::credential_reference_replaced("cred-x"),
                    },
                    &before,
                )
                .expect("test: record");
            record.status = status;
            history.update(&mut record).expect("test: update");
            assert!(!record.is_restorable());
            let error = fixture
                .service()
                .plan_rollback(&record.id)
                .await
                .expect_err("test: not restorable");
            assert!(
                error.to_string().contains("prx providers credential set"),
                "{status:?}: {error}"
            );
        }
    }

    /// D-104 C-1 ruling: a configuration with two embedding routes sharing
    /// a hint keeps loading and accepts unrelated changes, but a change to
    /// the embedding routes that creates or keeps a duplicate is refused,
    /// with nothing written.
    #[tokio::test]
    async fn a_change_to_embedding_routes_may_not_keep_a_duplicate_hint() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let route = |model: &str| crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: model.into(),
            dimensions: None,
            api_key: None,
            credential_ref: None,
        };
        let mut config = fixture::config_in(dir.path());
        config.embedding_routes = vec![route("text-embedding-3-small"), route("text-embedding-3-large")];
        crate::config::files::write_split_config(&config, false)
            .await
            .expect("test: write a stored duplicate");
        let loaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: an existing duplicate still loads");
        let service = ConfigTransactions::new(crate::config::new_shared(loaded.clone()));

        let mut unrelated = loaded.clone();
        unrelated.default_temperature = 0.3;
        service
            .plan(ConfigChange::Structured(Box::new(unrelated)))
            .await
            .expect("test: an unrelated change is planned");

        let mut kept = loaded.clone();
        if let Some(route) = kept.embedding_routes.last_mut() {
            route.model = "text-embedding-ada-002".into();
        }
        let error = service
            .plan(ConfigChange::Structured(Box::new(kept)))
            .await
            .expect_err("test: a change that keeps the duplicate is refused");
        assert!(error.to_string().contains("both use hint 'semantic'"), "{error}");

        let mut fixed = loaded;
        fixed.embedding_routes.truncate(1);
        service
            .plan(ConfigChange::Structured(Box::new(fixed)))
            .await
            .expect("test: removing the duplicate is planned");
    }

    /// D-101 F-2: a replaced credential's record is not restorable; a
    /// rollback of it is refused with the way to set the credential again.
    #[tokio::test]
    async fn a_replaced_credential_record_is_not_rolled_back() {
        let fixture = fixture().await;
        let history = fixture.service().history().expect("test: history");
        let before = crate::config::files::current_before_image(&fixture.config_path()).expect("test: image");
        let mut record = history
            .create_pending(
                NewRecord {
                    actor: ConfigActor::Cli,
                    base_revision: None,
                    restores: None,
                    summary: ConfigChangeSummary::credential_reference_replaced("cred-x"),
                },
                &before,
            )
            .expect("test: record");
        record.status = RecordStatus::CredentialReplaced;
        history.update(&mut record).expect("test: update");
        let tree = fixture.tree();

        let error = fixture
            .service()
            .plan_rollback(&record.id)
            .await
            .expect_err("test: not restorable");
        assert!(error.to_string().contains("prx providers credential set"), "{error}");
        assert_eq!(fixture.tree(), tree);
    }

    /// RED before D-3 (the raw PUT and POST paths saved, then activated,
    /// and left a refused candidate on disk): a refused activation restores
    /// the exact before-image, keeps the active generation, re-aligns the
    /// desired configuration, and records `rolled_back` without a secret.
    #[tokio::test]
    async fn a_refused_activation_restores_the_before_image() {
        let fixture = fixture().await;
        let before = fixture.tree();
        let generation = fixture.shared.active_generation_id();
        let revision = ConfigTransactions::base_revision(&fixture.shared.desired());

        let error = apply_structured(&fixture, |config| config.default_temperature = 1.9, None)
            .await
            .expect_err("test: the participant refuses");

        let ConfigTransactionError::NotApplied {
            record_id,
            reason,
            outcome,
        } = &error
        else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(*outcome, NotAppliedOutcome::Restored, "{error}");
        assert!(reason.contains("refuses a temperature"), "{reason}");
        assert!(
            error.to_string().contains("previous configuration was restored"),
            "{error}"
        );
        assert_eq!(fixture.tree(), before, "disk must hold the before-image again");
        assert_eq!(fixture.shared.active_generation_id(), generation);
        assert!(fixture.shared.desired().config.default_temperature < 1.5);
        assert_eq!(ConfigTransactions::base_revision(&fixture.shared.desired()), revision);

        let history = fixture.service().history().expect("test: history");
        let record = history
            .get(record_id.as_deref().expect("test: recorded"))
            .expect("test: record");
        assert_eq!(record.status, RecordStatus::RolledBack);
        let json = serde_json::to_string(&record).expect("test: json");
        assert!(!json.contains(fixture::SENTINEL_PREFIX), "{json}");
    }

    #[tokio::test]
    async fn a_stale_base_revision_is_a_conflict_with_nothing_written() {
        let fixture = fixture().await;
        let base = ConfigTransactions::base_revision(&fixture.shared.desired()).expect("test: disk revision");
        // Another writer lands first.
        let manual = fixture.dir.path().join("config.d/memory.toml");
        let mut edited = std::fs::read_to_string(&manual).expect("test: read");
        edited.push_str("\n# another writer\n");
        std::fs::write(&manual, edited).expect("test: write");
        let on_disk = fixture.tree();
        let history_dir = fixture.dir.path().join(crate::config::manage::history::HISTORY_DIR);

        let error = apply_structured(&fixture, |config| config.default_temperature = 0.2, Some(&base))
            .await
            .expect_err("test: conflict");

        assert!(matches!(error, ConfigTransactionError::Conflict(_)), "{error}");
        assert_eq!(fixture.tree(), on_disk);
        assert!(!history_dir.exists(), "a refused commit records nothing");
    }

    #[tokio::test]
    async fn an_injected_commit_failure_leaves_the_before_image_and_records_it() {
        let fixture = fixture().await;
        let before = fixture.tree();
        *crate::config::files::mutation_failure_hook()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(crate::config::files::MutationFailureHook {
            config_path: fixture.config_path(),
            writes_remaining: 1,
        });

        let error = apply_structured(&fixture, |config| config.default_temperature = 0.3, None)
            .await
            .expect_err("test: injected failure");

        assert!(matches!(error, ConfigTransactionError::Failed(_)), "{error}");
        assert_eq!(fixture.tree(), before, "the journal restores a partial write");
        let records = fixture
            .service()
            .history()
            .expect("test: history")
            .list()
            .expect("test: list");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records.first().map(|record| record.status),
            Some(RecordStatus::CommitFailed)
        );
    }

    #[tokio::test]
    async fn concurrent_transactions_based_on_one_revision_never_lose_an_update() {
        let fixture = fixture().await;
        let base = ConfigTransactions::base_revision(&fixture.shared.desired()).expect("test: revision");

        let first = apply_structured(&fixture, |config| config.default_temperature = 0.6, Some(&base));
        let second = apply_structured(
            &fixture,
            |config| config.default_model = Some("second-writer".into()),
            Some(&base),
        );
        let (first, second) = tokio::join!(first, second);

        let conflicts = [&first, &second]
            .iter()
            .filter(|result| matches!(result, Err(ConfigTransactionError::Conflict(_))))
            .count();
        assert_eq!(conflicts, 1, "exactly one writer wins: {first:?} / {second:?}");
    }

    #[tokio::test]
    async fn an_invalid_raw_file_is_refused_before_anything_is_written() {
        let fixture = fixture().await;
        let before = fixture.tree();
        let service = fixture.service();

        let error = service
            .plan(ConfigChange::RawFile {
                filename: "memory.toml".into(),
                contents: "[memory]\nbackend = 7\n".into(),
            })
            .await
            .expect_err("test: invalid");

        assert!(matches!(error, ConfigTransactionError::Invalid(_)), "{error}");
        assert_eq!(fixture.tree(), before);
    }

    /// A raw write of a well-formed but undecryptable ciphertext is refused
    /// at plan time: the candidate is loaded with the live key first.
    #[tokio::test]
    async fn an_undecryptable_credential_is_refused_at_plan_time() {
        let fixture = fixture().await;
        let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
        let bogus = format!("api_key = \"enc2:{}\"\n{}", "00".repeat(40), strip_root_api_key(&main));

        let error = fixture
            .service()
            .plan(ConfigChange::RawFile {
                filename: "config.toml".into(),
                contents: bogus,
            })
            .await
            .expect_err("test: undecryptable");

        assert!(matches!(error, ConfigTransactionError::Invalid(_)), "{error}");
    }

    fn strip_root_api_key(main: &str) -> String {
        main.lines()
            .filter(|line| !line.starts_with("api_key"))
            .map(|line| format!("{line}\n"))
            .collect()
    }

    // ── Rollback (D-4) ────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_ordinary_plan_cannot_select_a_retired_model_by_naming_the_rollback_actor() {
        let fixture = fixture().await;
        let before = fixture.tree();
        let service = fixture.service();
        let mut candidate = fixture.desired();
        candidate.default_provider = Some("openai".into());
        candidate.default_model = Some("gpt-4o".into());
        let plan = service
            .plan(ConfigChange::Structured(Box::new(candidate)))
            .await
            .expect("test: ordinary structured plan");
        assert!(plan.restores.is_none());
        let error = service
            .apply(plan, ConfigActor::Rollback, ApplyOptions::default())
            .await
            .expect_err("test: actor does not grant a lifecycle exception");
        assert!(error.to_string().contains("retired"), "{error}");
        assert_eq!(fixture.tree(), before);
    }

    /// Apply A, apply B, roll back B's record: the stored files are byte
    /// for byte those after A (ciphertext included, so no mask and no
    /// re-encryption), and the rollback has its own record.
    #[tokio::test]
    async fn rollback_restores_the_recorded_stored_bytes() {
        let fixture = fixture().await;
        apply_structured(&fixture, |config| config.default_temperature = 0.5, None)
            .await
            .expect("test: apply A");
        let after_a = fixture.tree();
        let b = apply_structured(
            &fixture,
            |config| {
                config.default_temperature = 0.9;
                config.api_key = Some("sentinel-rotated-root".into());
            },
            None,
        )
        .await
        .expect("test: apply B");
        assert_eq!(
            fixture.shared.desired().config.api_key.as_deref(),
            Some("sentinel-rotated-root")
        );

        let b_id = b.record_id.expect("test: B recorded");
        let outcome = fixture
            .service()
            .rollback(RollbackRequest {
                record_id: &b_id,
                expected_revision: &current_revision(&fixture),
                confirm: true,
                restore_credentials: true,
                disable_encryption: false,
            })
            .await
            .expect("test: rollback");

        assert_eq!(fixture.tree(), after_a, "rollback must restore the exact stored bytes");
        assert_eq!(
            fixture.shared.desired().config.api_key.as_deref(),
            Some("sentinel-root")
        );
        assert!((fixture.shared.load_full().default_temperature - 0.5).abs() < f64::EPSILON);
        let record = fixture
            .service()
            .history()
            .expect("test: history")
            .get(&outcome.record_id.expect("test: rollback recorded"))
            .expect("test: record");
        assert_eq!(record.actor, ConfigActor::Rollback);
        assert_eq!(record.restores.as_deref(), Some(b_id.as_str()));
        assert_eq!(record.status, RecordStatus::Activated);
    }

    /// A before-image whose credentials the current key cannot decrypt (the
    /// key was replaced) is refused with nothing written.
    #[tokio::test]
    async fn rollback_to_an_undecryptable_record_is_refused() {
        let fixture = fixture().await;
        let a = apply_structured(&fixture, |config| config.default_temperature = 0.5, None)
            .await
            .expect("test: apply A");
        let key_path = fixture.dir.path().join(".secret_key");
        std::fs::write(&key_path, "11".repeat(32)).expect("test: replace key");
        let on_disk = fixture.tree();

        let error = fixture
            .service()
            .rollback(confirmed(
                &a.record_id.expect("test: recorded"),
                &current_revision(&fixture),
            ))
            .await
            .expect_err("test: refused");

        assert!(matches!(error, ConfigTransactionError::Invalid(_)), "{error}");
        assert!(!error.to_string().contains(fixture::SENTINEL_PREFIX), "{error}");
        assert_eq!(fixture.tree(), on_disk);
    }

    #[tokio::test]
    async fn rollback_of_an_unknown_record_is_refused() {
        let fixture = fixture().await;
        for id in ["../../etc", "not-a-record", "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"] {
            let error = fixture
                .service()
                .rollback(confirmed(id, &current_revision(&fixture)))
                .await
                .expect_err("test: refused");
            assert!(matches!(error, ConfigTransactionError::Invalid(_)), "{id}: {error}");
        }
    }

    fn current_revision(fixture: &Fixture) -> String {
        crate::config::files::current_config_revision(&fixture.config_path()).expect("test: revision")
    }

    fn confirmed<'a>(record_id: &'a str, expected_revision: &'a str) -> RollbackRequest<'a> {
        RollbackRequest {
            record_id,
            expected_revision,
            confirm: true,
            restore_credentials: false,
            disable_encryption: false,
        }
    }

    // ── PMMD-f (R-PMMD) ──────────────────────────────────────────────────

    /// RED before PMMD-f (R-PMMD probe 1, F-1): a raw write planned without
    /// an expected revision reverted another writer's edit to a different
    /// fragment that landed between plan and commit. After: the plan's own
    /// revision guards the commit; 409 and the edit survives.
    #[tokio::test]
    async fn a_raw_write_never_reverts_an_edit_made_after_its_plan() {
        let fixture = fixture().await;
        let service = fixture.service();
        let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
        let plan = service
            .plan(ConfigChange::RawFile {
                filename: "config.toml".into(),
                contents: main.replacen("default_temperature = 0.7", "default_temperature = 0.6", 1),
            })
            .await
            .expect("test: plan");
        assert!(plan.planned_revision().is_some());
        let memory = fixture.dir.path().join("config.d/memory.toml");
        let mut edited = std::fs::read_to_string(&memory).expect("test: read");
        edited.push_str("\n# FOREIGN-WRITER-EDIT\n");
        std::fs::write(&memory, &edited).expect("test: foreign edit");

        let result = service
            .apply(plan, ConfigActor::GatewayConfigFile, ApplyOptions::default())
            .await;

        assert!(matches!(result, Err(ConfigTransactionError::Conflict(_))), "{result:?}");
        assert_eq!(std::fs::read_to_string(&memory).expect("test: read"), edited);
    }

    /// RED before PMMD-f (R-PMMD probe 2, F-2): one rollback call turned
    /// encryption off and wrote the root key in plaintext. After: it needs
    /// `confirm`, and then `disable_encryption`; only with both is it done.
    #[tokio::test]
    async fn a_rollback_that_turns_encryption_off_needs_every_confirmation() {
        let fixture = fixture().await;
        apply_structured(&fixture, |config| config.secrets.encrypt = false, None)
            .await
            .expect("test: apply A");
        let b = apply_structured(&fixture, |config| config.secrets.encrypt = true, None)
            .await
            .expect("test: apply B");
        let b_id = b.record_id.expect("test: recorded");
        let main = || std::fs::read_to_string(fixture.config_path()).expect("test: read");
        assert!(!main().contains("\"sentinel-root\""), "test precondition: encrypted");
        let on_disk = fixture.tree();

        let mut request = confirmed(&b_id, "");
        let revision = current_revision(&fixture);
        request.expected_revision = &revision;
        request.confirm = false;
        for (confirm, disable, required) in [
            (false, false, "disable_encryption"),
            (true, false, "disable_encryption"),
            (false, true, "confirm"),
        ] {
            request.confirm = confirm;
            request.disable_encryption = disable;
            let error = fixture.service().rollback(request).await.expect_err("test: refused");
            assert!(
                matches!(&error, ConfigTransactionError::ConfirmationRequired { required: flag, .. } if *flag == required),
                "{error}"
            );
            assert_eq!(fixture.tree(), on_disk, "nothing may be written");
            assert!(fixture.shared.desired().config.secrets.encrypt);
        }

        request.confirm = true;
        request.disable_encryption = true;
        fixture
            .service()
            .rollback(request)
            .await
            .expect("test: confirmed rollback");
        assert!(!fixture.shared.desired().config.secrets.encrypt);
    }

    /// A rollback that would bring back a credential the current
    /// configuration no longer holds needs `restore_credentials`.
    #[tokio::test]
    async fn a_rollback_that_restores_a_rotated_credential_needs_restore_credentials() {
        let fixture = fixture().await;
        let rotated = apply_structured(
            &fixture,
            |config| config.api_key = Some("sentinel-rotated-root".into()),
            None,
        )
        .await
        .expect("test: rotate");
        let on_disk = fixture.tree();
        let revision = current_revision(&fixture);

        let error = fixture
            .service()
            .rollback(confirmed(&rotated.record_id.expect("test: recorded"), &revision))
            .await
            .expect_err("test: refused");

        let ConfigTransactionError::ConfirmationRequired { required, reasons, .. } = &error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(*required, "restore_credentials");
        assert!(
            reasons.contains(&RiskReason::CredentialReintroduced { path: "api_key".into() }),
            "{reasons:?}"
        );
        assert!(!error.to_string().contains(fixture::SENTINEL_PREFIX), "{error}");
        assert_eq!(fixture.tree(), on_disk);
    }

    #[tokio::test]
    async fn a_rollback_needs_the_current_revision() {
        let fixture = fixture().await;
        let a = apply_structured(&fixture, |config| config.default_temperature = 0.5, None)
            .await
            .expect("test: apply A");
        let error = fixture
            .service()
            .rollback(confirmed(&a.record_id.expect("test: recorded"), "00"))
            .await
            .expect_err("test: stale");
        assert!(matches!(error, ConfigTransactionError::Conflict(_)), "{error}");
    }

    /// A high-risk plan is refused without confirmation, with nothing
    /// written and the reasons named by path only.
    #[tokio::test]
    async fn a_high_risk_plan_needs_confirmation() {
        let fixture = fixture().await;
        let service = fixture.service();
        let on_disk = fixture.tree();
        let mut config = fixture.desired();
        config.api_key = Some("sentinel-new-root".into());
        let plan = service
            .plan(ConfigChange::Structured(Box::new(config)))
            .await
            .expect("test: plan");
        assert_eq!(plan.summary().risk, ConfigChangeRisk::High);

        let error = service
            .apply(plan, ConfigActor::GatewayConfigApi, ApplyOptions::default())
            .await
            .expect_err("test: unconfirmed");

        assert!(
            matches!(
                &error,
                ConfigTransactionError::ConfirmationRequired {
                    required: "confirm_high_risk",
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("api_key"), "{error}");
        assert!(!error.to_string().contains(fixture::SENTINEL_PREFIX), "{error}");
        assert_eq!(fixture.tree(), on_disk);
    }

    /// The shared plan step refuses an introduced mask for every surface.
    #[tokio::test]
    async fn an_introduced_mask_is_refused_at_plan_time() {
        let fixture = fixture().await;
        let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
        let masked = format!("api_key = \"***\"\n{}", strip_root_api_key(&main));
        let error = fixture
            .service()
            .plan(ConfigChange::RawFile {
                filename: "config.toml".into(),
                contents: masked,
            })
            .await
            .expect_err("test: masked");
        assert!(error.to_string().contains("redaction mask"), "{error}");
    }

    /// RED before PMMD-f (F-4): the restore was written but re-activating
    /// it failed, and the message claimed the rejected configuration was
    /// still on disk.
    #[tokio::test]
    async fn a_written_restore_is_not_reported_as_still_on_disk() {
        let fixture = fixture().await;
        let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(RefusesAndReplacesKey {
            key_path: fixture.dir.path().join(".secret_key"),
        });
        fixture.shared.register_participant(&participant);
        let before = fixture.tree();

        let error = apply_structured(&fixture, |config| config.default_temperature = 1.3, None)
            .await
            .expect_err("test: refused");

        assert_eq!(fixture.tree(), before, "the restore was written");
        assert!(
            matches!(
                &error,
                ConfigTransactionError::NotApplied {
                    outcome: NotAppliedOutcome::RestoredNotReactivated,
                    ..
                }
            ),
            "{error}"
        );
        assert!(!error.to_string().contains("still on disk"), "{error}");
        assert!(error.to_string().contains("restored on disk"), "{error}");
    }

    /// Refuses a warm temperature and replaces the secret key, so the
    /// restored (encrypted) tree cannot be re-activated.
    struct RefusesAndReplacesKey {
        key_path: PathBuf,
    }

    impl ConfigGenerationParticipant for RefusesAndReplacesKey {
        fn name(&self) -> &'static str {
            "test_refuses_and_replaces_key"
        }

        fn prepares_for_field(&self, field: &str) -> bool {
            field == "default_temperature"
        }

        fn prepare(
            &self,
            generation: Arc<ConfigGeneration>,
            _changed_fields: &[String],
        ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
            if generation.effective.default_temperature > 1.2 {
                std::fs::write(&self.key_path, "22".repeat(32))?;
                anyhow::bail!("test participant refuses and replaces the key");
            }
            Ok(Box::new(Accepted))
        }
    }

    /// When the files already hold the refused configuration (an unreloaded
    /// manual edit), nothing is written and the message says so.
    #[tokio::test]
    async fn activation_of_unchanged_files_is_reported_as_nothing_written() {
        let fixture = fixture().await;
        let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
        let hot = main.replacen("default_temperature = 0.7", "default_temperature = 1.9", 1);
        assert_ne!(hot, main, "test precondition: the edit applies");
        std::fs::write(fixture.config_path(), &hot).expect("test: manual edit");
        let service = fixture.service();
        let plan = service
            .plan(ConfigChange::RawFile {
                filename: "config.toml".into(),
                contents: hot,
            })
            .await
            .expect("test: plan");

        let error = service
            .apply(plan, ConfigActor::GatewayConfigFile, ApplyOptions::default())
            .await
            .expect_err("test: refused");

        assert!(
            matches!(
                &error,
                ConfigTransactionError::NotApplied {
                    outcome: NotAppliedOutcome::NothingWritten,
                    ..
                }
            ),
            "{error}"
        );
        assert!(!error.to_string().contains("could not be restored"), "{error}");
    }

    // ── PMMD-g (R-PMMDf) ─────────────────────────────────────────────────

    async fn apply_raw(
        fixture: &Fixture,
        filename: &str,
        contents: String,
        confirm_high_risk: bool,
    ) -> Result<ConfigTransactionOutcome, ConfigTransactionError> {
        let service = fixture.service();
        let plan = service
            .plan(ConfigChange::RawFile {
                filename: filename.into(),
                contents,
            })
            .await?;
        let planned = plan.planned_revision().map(str::to_string);
        service
            .apply(
                plan,
                ConfigActor::GatewayConfigFile,
                ApplyOptions {
                    expected_revision: None,
                    confirm_high_risk,
                    confirmed_revision: planned.as_deref(),
                },
            )
            .await
    }

    /// RED before PMMD-g (N-1): plaintext written to a user-owned fragment
    /// the loader skips was accepted (risk low) and stored with encryption on.
    #[tokio::test]
    async fn plaintext_in_an_unloaded_fragment_is_refused() {
        let fixture = fixture().await;
        let target = fixture.dir.path().join("config.d/zz-local.toml");
        for value in ["sentinel-probe-plain", "***"] {
            let result = apply_raw(&fixture, "zz-local.toml", format!("api_key = \"{value}\"\n"), true).await;
            assert!(
                matches!(result, Err(ConfigTransactionError::Invalid(_))),
                "{value}: {result:?}"
            );
            assert!(!target.exists(), "{value}: nothing may be written");
        }
    }

    /// RED before PMMD-g (N-1): a `config.toml` section a fragment overrides
    /// took a plaintext key (risk low) and stored it with encryption on.
    #[tokio::test]
    async fn plaintext_in_a_shadowed_section_is_refused() {
        let fixture = fixture().await;
        let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
        let shadowed = format!(
            "{main}\n[[model_routes]]\nhint = \"shadow\"\nprovider = \"openrouter\"\nmodel = \"m\"\napi_key = \"sentinel-probe-shadow\"\n"
        );

        let result = apply_raw(&fixture, "config.toml", shadowed, true).await;

        assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
        assert!(
            !std::fs::read_to_string(fixture.config_path())
                .expect("test: read")
                .contains("sentinel-probe-shadow")
        );
    }

    const USERINFO_PW: &str = "userinfo-sentinel-pw";

    fn userinfo_provider() -> String {
        format!("custom:http://alice:{USERINFO_PW}@127.0.0.1:1/v1")
    }

    /// RED before PMMD-g (N-2): a raw write that puts C2BU's `[redacted]@`
    /// placeholder where real URL userinfo is stored was accepted with the
    /// flag, and the real userinfo was gone.
    #[tokio::test]
    async fn a_redacted_userinfo_placeholder_never_replaces_real_userinfo() {
        let fixture = fixture().await;
        apply_structured(
            &fixture,
            |config| config.default_provider = Some(userinfo_provider()),
            None,
        )
        .await
        .expect("test: store userinfo");
        let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
        assert!(main.contains(USERINFO_PW), "test precondition: userinfo on disk");
        let placeholder = main.replace(&format!("alice:{USERINFO_PW}@"), "[redacted]@");

        let result = apply_raw(&fixture, "config.toml", placeholder, true).await;

        assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
        assert!(
            std::fs::read_to_string(fixture.config_path())
                .expect("test: read")
                .contains(USERINFO_PW)
        );
    }

    /// RED before PMM-H (D-099 7c): a secret the redactor found in a URL
    /// path was shown as `[REDACTED]`, which the write boundary did not know
    /// as a mask, so sending a shown provider back stored the literal mask.
    /// Both path masks are refused now, with nothing written.
    #[tokio::test]
    async fn a_url_path_mask_is_never_stored() {
        let fixture = fixture().await;
        let before = fixture.tree();
        for provider in [
            "custom:https://llm.example/v1/[REDACTED]/chat",
            "custom:https://llm.example/v1/[redacted]/chat",
        ] {
            let result = apply_structured(
                &fixture,
                |config| config.default_provider = Some(provider.to_string()),
                None,
            )
            .await;
            assert!(
                matches!(result, Err(ConfigTransactionError::Invalid(_))),
                "{provider}: {result:?}"
            );
            assert_eq!(fixture.tree(), before, "{provider} was written");
        }
    }

    fn price_key() -> String {
        format!("{}/model-a", userinfo_provider())
    }

    /// RED before PMMD-g (N-2, N-3): a credentialed `cost.prices` key needed
    /// no confirmation, and its userinfo appeared in the plan paths.
    #[tokio::test]
    async fn a_credentialed_price_key_is_high_risk_and_redacted_in_the_plan() {
        let fixture = fixture().await;
        let mut config = fixture.desired();
        config.cost.prices.insert(
            price_key(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 2.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let plan = fixture
            .service()
            .plan(ConfigChange::Structured(Box::new(config)))
            .await
            .expect("test: plan");

        let rendered = serde_json::to_string(plan.summary()).expect("test: json");
        assert!(!rendered.contains(USERINFO_PW), "the plan leaked userinfo: {rendered}");
        assert_eq!(plan.summary().risk, ConfigChangeRisk::High, "{rendered}");
        let error = fixture
            .service()
            .apply(plan, ConfigActor::GatewayConfigApi, ApplyOptions::default())
            .await
            .expect_err("test: needs confirmation");
        assert!(
            matches!(error, ConfigTransactionError::ConfirmationRequired { .. }),
            "{error}"
        );
        assert!(!error.to_string().contains(USERINFO_PW), "{error}");
    }

    /// RED before PMMD-g (N-7): a `custom:` endpoint in these provider
    /// fields classified low.
    #[test]
    fn custom_endpoints_in_every_provider_field_are_high_risk() {
        let before = Config::default();
        for change in [
            (|config: &mut Config| {
                config.reliability.fallback_providers = vec!["custom:https://attacker.example/v1".into()]
            }) as fn(&mut Config),
            |config: &mut Config| config.memory.embedding_provider = "custom:https://attacker.example/v1".into(),
            |config: &mut Config| config.smart_group.classifier_provider = "custom:https://attacker.example/v1".into(),
        ] {
            let mut after = before.clone();
            change(&mut after);
            let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
            assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
            assert!(
                summary
                    .risk_reasons
                    .iter()
                    .any(|reason| matches!(reason, RiskReason::EndpointChanged { .. })),
                "{summary:?}"
            );
        }
    }

    // ── PMMD-h (R-PMMDg) ─────────────────────────────────────────────────

    /// RED before PMMD-h (G-1, R-PMMDg probe): on a case-insensitive
    /// filesystem a raw write of `Security.toml` was planned as a user-owned
    /// fragment (candidate unchanged, risk low) but overwrote the managed
    /// `security.toml`, turning encryption off with no confirmation.
    #[tokio::test]
    async fn a_case_variant_of_a_managed_file_name_is_refused() {
        let fixture = fixture().await;
        let security = fixture.dir.path().join("config.d/security.toml");
        let stored = std::fs::read_to_string(&security).expect("test: read security.toml");
        assert!(stored.contains("encrypt = true"), "test precondition: {stored}");
        let off = stored.replacen("encrypt = true", "encrypt = false", 1);

        for name in ["Security.toml", "SECURITY.TOML", "Config.toml"] {
            let result = apply_raw(&fixture, name, off.clone(), false).await;
            assert!(
                matches!(result, Err(ConfigTransactionError::Invalid(_))),
                "{name}: {result:?}"
            );
            assert_eq!(
                std::fs::read_to_string(&security).expect("test: read"),
                stored,
                "{name}: the managed file must not change"
            );
        }
        assert!(fixture.shared.desired().config.secrets.encrypt);
    }

    // ── PMMD-i (R-PMMDh) ─────────────────────────────────────────────────

    /// RED before PMMD-i (H-1, R-PMMDh probe): APFS folds Unicode case, so
    /// `ſecurity.toml` (U+017F) and `networK.toml` (U+212A Kelvin) open the
    /// managed file; the write was planned as a user-owned fragment (risk
    /// low, no confirmation) and turned encryption off. Only plain lowercase
    /// ASCII names are accepted now.
    #[tokio::test]
    async fn a_unicode_fold_variant_of_a_managed_file_name_is_refused() {
        let fixture = fixture().await;
        let security = fixture.dir.path().join("config.d/security.toml");
        let network = fixture.dir.path().join("config.d/network.toml");
        let stored_security = std::fs::read_to_string(&security).expect("test: read security.toml");
        let stored_network = std::fs::read_to_string(&network).expect("test: read network.toml");
        let off = stored_security.replacen("encrypt = true", "encrypt = false", 1);
        assert_ne!(off, stored_security, "test precondition");

        for (name, contents) in [
            ("\u{17f}ecurity.toml", off.clone()),
            ("networ\u{212a}.toml", stored_network.clone()),
            ("co\u{fb06}.toml", "[cost]\n".to_string()),
            ("caf\u{e9}.toml", "[cafe]\n".to_string()),
        ] {
            let result = apply_raw(&fixture, name, contents, false).await;
            assert!(
                matches!(result, Err(ConfigTransactionError::Invalid(_))),
                "{name}: {result:?}"
            );
        }
        assert_eq!(std::fs::read_to_string(&security).expect("test: read"), stored_security);
        assert_eq!(std::fs::read_to_string(&network).expect("test: read"), stored_network);
        assert!(fixture.shared.desired().config.secrets.encrypt);
    }

    /// D-049 B, RED before SECRETS-h: `confirm_high_risk` alone applied a
    /// high-risk plan. After: the confirmation must name the revision the
    /// plan is checked against; without one it is refused as `if_match`
    /// with that revision, with another one it is a conflict, and with the
    /// right one it applies. Nothing is written on a refusal.
    #[tokio::test]
    async fn a_high_risk_confirmation_is_bound_to_the_planned_revision() {
        let fixture = fixture().await;
        let on_disk = fixture.tree();
        let high_risk = || {
            let mut config = fixture.desired();
            config.api_key = Some("sentinel-rotated-root".into());
            ConfigChange::Structured(Box::new(config))
        };
        let revision = current_revision(&fixture);

        let plan = fixture.service().plan(high_risk()).await.expect("test: plan");
        let error = fixture
            .service()
            .apply(
                plan,
                ConfigActor::GatewayConfigApi,
                ApplyOptions {
                    expected_revision: Some(&revision),
                    confirm_high_risk: true,
                    confirmed_revision: None,
                },
            )
            .await
            .expect_err("test: an unbound confirmation is refused");
        assert!(
            matches!(
                &error,
                ConfigTransactionError::ConfirmationRequired { required: "if_match", revision: Some(reported), .. }
                    if *reported == revision
            ),
            "{error}"
        );
        assert!(!error.to_string().contains("sentinel-rotated-root"), "{error}");
        assert_eq!(fixture.tree(), on_disk, "nothing may be written");

        let stale = "0".repeat(64);
        let plan = fixture.service().plan(high_risk()).await.expect("test: plan");
        let error = fixture
            .service()
            .apply(
                plan,
                ConfigActor::GatewayConfigApi,
                ApplyOptions {
                    expected_revision: Some(&revision),
                    confirm_high_risk: true,
                    confirmed_revision: Some(&stale),
                },
            )
            .await
            .expect_err("test: a confirmation of another revision is refused");
        assert!(matches!(error, ConfigTransactionError::Conflict(_)), "{error}");
        assert_eq!(fixture.tree(), on_disk, "nothing may be written");

        let plan = fixture.service().plan(high_risk()).await.expect("test: plan");
        fixture
            .service()
            .apply(
                plan,
                ConfigActor::GatewayConfigApi,
                ApplyOptions {
                    expected_revision: Some(&revision),
                    confirm_high_risk: true,
                    confirmed_revision: Some(&revision),
                },
            )
            .await
            .expect("test: a bound confirmation applies");
    }

    /// D-050 item 3, RED before SECRETS-i: a stored configuration that does
    /// not load was no baseline, and the plan compared the change with
    /// nothing (every rule that looks at the prior value failed open). After:
    /// the plan is high risk with `baseline_unavailable`.
    #[tokio::test]
    async fn a_plan_without_a_loadable_baseline_is_high_risk() {
        let fixture = fixture().await;
        // The stored secrets no longer decrypt: the files are valid TOML, but
        // the configuration does not load.
        let key = fixture.config_path().with_file_name(".secret_key");
        std::fs::remove_file(&key).expect("test: remove the secret key");
        let mut config = fixture.desired();
        config.default_temperature = 0.25;
        let plan = fixture
            .service()
            .plan(ConfigChange::Structured(Box::new(config)))
            .await
            .expect("test: plan");
        assert_eq!(plan.summary().risk, ConfigChangeRisk::High, "{:?}", plan.summary());
        assert!(
            plan.summary().risk_reasons.contains(&RiskReason::BaselineUnavailable),
            "{:?}",
            plan.summary().risk_reasons
        );
    }

    // ── PMM-D-5: model validation for every affected selection ───────────

    /// A loopback-shaped endpoint no test contacts: selections are resolved,
    /// never sent.
    const D5_ENDPOINT: &str = "custom:http://127.0.0.1:9/v1";

    fn d5_declare(config: &mut Config, model: &str, operations: &str) {
        let parsed: crate::config::schema::ProviderModelConfig = toml::from_str(&format!(
            "provider = \"{D5_ENDPOINT}\"\nmodel = \"{model}\"\noperations = {operations}\n"
        ))
        .expect("test: a valid [[providers.models]] entry");
        config.providers.models.push(parsed);
    }

    fn d5_route(model: &str) -> crate::config::ModelRouteConfig {
        crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "d5".into(),
            provider: D5_ENDPOINT.into(),
            model: model.into(),
            api_key: None,
            credential_ref: None,
        }
    }

    /// RED before PMM-D-5: a new model route to a model the operator's own
    /// declaration says cannot chat was committed (only the default was ever
    /// checked, and only on POST). After: refused, nothing written.
    #[tokio::test]
    async fn a_route_to_a_model_that_cannot_chat_is_refused_before_anything_is_written() {
        let fixture = fixture().await;
        let before = fixture.tree();

        let result = apply_structured(
            &fixture,
            |config| {
                d5_declare(config, "embed-only", "[\"embedding\"]");
                config.model_routes.push(d5_route("embed-only"));
            },
            None,
        )
        .await;

        let Err(ConfigTransactionError::Invalid(error)) = &result else {
            panic!("a route to an embedding-only model must be refused: {result:?}");
        };
        assert!(format!("{error:#}").contains("hint:d5"), "{error:#}");
        assert_eq!(fixture.tree(), before, "nothing may be written");
    }

    /// RED before PMM-D-5: the same through the raw file API.
    #[tokio::test]
    async fn a_raw_write_of_a_route_that_cannot_chat_is_refused() {
        let fixture = fixture().await;
        let before = fixture.tree();
        let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
        let routing = std::fs::read_to_string(fixture.dir.path().join("config.d/routing.toml")).unwrap_or_default();
        let contents = format!(
            "{routing}\n[[model_routes]]\nhint = \"d5\"\nprovider = \"{D5_ENDPOINT}\"\nmodel = \"embed-only\"\n"
        );
        let declared = format!(
            "{main}\n[[providers.models]]\nprovider = \"{D5_ENDPOINT}\"\nmodel = \"embed-only\"\noperations = [\"embedding\"]\n"
        );
        apply_raw(&fixture, "config.toml", declared, true)
            .await
            .expect("test: the declaration alone is accepted");
        let declared_tree = fixture.tree();

        let result = apply_raw(&fixture, "routing.toml", contents, true).await;

        assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
        assert_eq!(fixture.tree(), declared_tree, "nothing may be written");
        assert_ne!(fixture.tree(), before);
    }

    /// RED before PMM-D-5: an agent binding is a selection too.
    #[tokio::test]
    async fn an_agent_bound_to_a_model_that_cannot_chat_is_refused() {
        let fixture = fixture().await;
        let result = apply_structured(
            &fixture,
            |config| {
                d5_declare(config, "embed-only", "[\"embedding\"]");
                let mut agent = config
                    .agents
                    .values()
                    .next()
                    .cloned()
                    .expect("test: the fixture has an agent");
                agent.provider = D5_ENDPOINT.into();
                agent.model = "embed-only".into();
                agent.api_key = None;
                config.agents.insert("d5".into(), agent);
            },
            None,
        )
        .await;
        let Err(ConfigTransactionError::Invalid(error)) = &result else {
            panic!("an agent bound to an embedding-only model must be refused: {result:?}");
        };
        assert!(format!("{error:#}").contains("agent `d5`"), "{error:#}");
    }

    /// RED before PMM-D-5: changing a declaration so that an UNCHANGED
    /// selection can no longer run is refused too (the selection is affected
    /// by the change even though its fields did not move).
    #[tokio::test]
    async fn a_declaration_that_rules_out_an_existing_route_is_refused() {
        let fixture = fixture().await;
        apply_structured(
            &fixture,
            |config| {
                d5_declare(config, "model-a", "[\"chat\"]");
                config.model_routes.push(d5_route("model-a"));
            },
            None,
        )
        .await
        .expect("test: a route to a declared chat model is accepted");
        let before = fixture.tree();

        let result = apply_structured(
            &fixture,
            |config| {
                config.providers.models.clear();
                d5_declare(config, "model-a", "[\"embedding\"]");
            },
            None,
        )
        .await;

        assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
        assert_eq!(fixture.tree(), before);
    }

    /// RED before PMM-D-5: a durable selection made by a model (the agent
    /// `config.patch` actor) is strict at the transaction layer itself: an
    /// unverified model is refused (D-034), whatever surface built the plan.
    #[tokio::test]
    async fn a_model_made_selection_of_an_unverified_model_is_refused() {
        let fixture = fixture().await;
        let before = fixture.tree();
        let service = fixture.service();
        let mut config = fixture.desired();
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "d5".into(),
            provider: "openrouter".into(),
            model: "vendor/never-seen-model".into(),
            api_key: None,
            credential_ref: None,
        });
        let plan = service
            .plan(ConfigChange::Structured(Box::new(config)))
            .await
            .expect("test: plan");
        let planned = plan.planned_revision().map(str::to_string);

        let result = service
            .apply(
                plan,
                ConfigActor::AgentConfigPatch,
                ApplyOptions {
                    expected_revision: None,
                    confirm_high_risk: true,
                    confirmed_revision: planned.as_deref(),
                },
            )
            .await;

        assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
        assert_eq!(fixture.tree(), before);
    }

    /// The operator's surfaces accept a selection the evidence neither
    /// proves nor refutes and report it (D-034).
    #[tokio::test]
    async fn an_operator_selection_of_an_unverified_model_is_accepted_with_a_warning() {
        let fixture = fixture().await;
        let outcome = apply_structured(
            &fixture,
            |config| {
                config.model_routes.push(crate::config::ModelRouteConfig {
                    settings: Default::default(),
                    hint: "d5".into(),
                    provider: "openrouter".into(),
                    model: "vendor/never-seen-model".into(),
                    api_key: None,
                    credential_ref: None,
                });
            },
            None,
        )
        .await
        .expect("test: accepted");
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert!(
            outcome
                .warnings
                .first()
                .is_some_and(|warning| warning.contains("hint:d5")),
            "{:?}",
            outcome.warnings
        );
    }

    // ── PMM-D-8b: URL userinfo re-introduced by a rollback ───────────────

    /// RED before PMM-D-8: a rollback that brings back URL userinfo
    /// (`user:password@` in a provider URL) the current configuration no
    /// longer holds needed no `restore_credentials`, unlike a rotated-out key.
    #[tokio::test]
    async fn a_rollback_that_restores_rotated_url_userinfo_needs_restore_credentials() {
        let fixture = fixture().await;
        let old = "custom:http://alice:pmmd8-old-password@127.0.0.1:9/v1";
        let new = "custom:http://alice:pmmd8-new-password@127.0.0.1:9/v1";
        apply_structured(&fixture, |config| config.default_provider = Some(old.into()), None)
            .await
            .expect("test: store the old userinfo");
        let rotated = apply_structured(&fixture, |config| config.default_provider = Some(new.into()), None)
            .await
            .expect("test: rotate it");
        let on_disk = fixture.tree();
        let revision = current_revision(&fixture);

        let result = fixture
            .service()
            .rollback(confirmed(&rotated.record_id.expect("test: recorded"), &revision))
            .await;

        assert!(
            matches!(
                &result,
                Err(ConfigTransactionError::ConfirmationRequired {
                    required: "restore_credentials",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(fixture.tree(), on_disk, "nothing may be written");
    }

    /// PMM-D-8: purging the history waits for a transaction in flight, so a
    /// record being written is never purged half-way (R-PMMDf §3).
    #[tokio::test]
    async fn purging_the_history_waits_for_a_transaction_in_flight() {
        let fixture = fixture().await;
        apply_structured(&fixture, |config| config.default_temperature = 0.5, None)
            .await
            .expect("test: a recorded change");
        let service = fixture.service();
        let held = transaction_lock(&fixture.config_path());
        let guard = held.lock().await;

        let purge = tokio::spawn(async move { service.purge_history().await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!purge.is_finished(), "the purge ran while a transaction held the lock");
        drop(guard);

        let removed = purge.await.expect("test: join").expect("test: purge");
        assert_eq!(removed, 1);
        assert!(
            fixture
                .service()
                .history()
                .expect("test: history")
                .list()
                .expect("test: list")
                .is_empty(),
            "expected an empty value"
        );
    }

    // ── PMMDtail-g (D-080) ───────────────────────────────────────────────

    /// RED before PMMDtail-g (D-080 N-3): a rollback asked for
    /// `restore_credentials` and `confirm` before it checked the model
    /// selections it would restore, so an operator could confirm a restore
    /// that was then refused. The selections are checked first.
    #[tokio::test]
    async fn a_rollback_checks_its_selections_before_asking_for_confirmation() {
        let fixture = fixture().await;
        apply_structured(
            &fixture,
            |config| {
                config.model_routes.push(crate::config::ModelRouteConfig {
                    settings: Default::default(),
                    hint: "gone".into(),
                    provider: "openrouter".into(),
                    model: "vendor/image-maker".into(),
                    api_key: None,
                    credential_ref: None,
                });
            },
            None,
        )
        .await
        .expect("test: an unverified route is accepted");
        let removed = apply_structured(
            &fixture,
            |config| {
                config.model_routes.retain(|route| route.hint != "gone");
                config.api_key = Some("sentinel-rotated-root".into());
            },
            None,
        )
        .await
        .expect("test: the route is removed and the key rotated");
        // The model cache now says the model only makes images.
        let payload = serde_json::json!({"data": [{
            "id": "vendor/image-maker",
            "architecture": {"input_modalities": ["text"], "output_modalities": ["image"]},
        }]});
        crate::providers::catalog::discovery::record_discovered(
            &fixture.desired().workspace_dir,
            "openrouter",
            &crate::providers::catalog::discovery::parse_openai_compatible_models(&payload),
            9,
        )
        .expect("test: record the catalog");
        let on_disk = fixture.tree();
        let revision = current_revision(&fixture);

        let error = fixture
            .service()
            .rollback(RollbackRequest {
                record_id: &removed.record_id.expect("test: recorded"),
                expected_revision: &revision,
                confirm: false,
                restore_credentials: false,
                disable_encryption: false,
            })
            .await
            .expect_err("test: refused");

        assert!(
            matches!(error, ConfigTransactionError::Invalid(_)),
            "the refusal must come before any confirmation is asked for: {error}"
        );
        assert!(error.to_string().contains("hint:gone"), "{error}");
        assert!(!error.to_string().contains(fixture::SENTINEL_PREFIX), "{error}");
        assert_eq!(fixture.tree(), on_disk);
    }
}
