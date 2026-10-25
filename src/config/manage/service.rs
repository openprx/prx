//! The provider/model management service (SP-PMM §2.5): typed operations
//! over the objects PRX manages, on top of the configuration transaction.
//!
//! A [`ManagedChange`] names one operation on one object:
//!
//! | Object | Configuration it edits |
//! | --- | --- |
//! | provider | membership in the configured set (the default provider, `reliability.fallback_providers`), the default provider's endpoint (`api_url`), and its declarations (`[[providers.models]]`, `[[providers.tool_dialects]]`) |
//! | curated model declaration | one `[[providers.models]]` entry, keyed by `(provider, model)` |
//! | default selection | `default_provider`/`default_model` (chat), `memory.embedding_provider`/`embedding_model` (embeddings) |
//! | route | one `[[model_routes]]` (chat) or `[[embedding_routes]]` (embeddings) entry, keyed by `hint` |
//! | agent binding | `agents.<name>.provider`/`model` |
//!
//! [`ManagementService::plan`] turns the change into a candidate
//! configuration and hands it to [`ConfigTransactions`]: one staging, one
//! write-boundary check, one selection validator (the same the write runs),
//! one redacted plan. [`ManagementService::apply`] commits it through the
//! same transaction (revision guard, history, activation, compensation).
//! Every surface calls this service in-process, so an equal change gives the
//! same plan text ([`super::render::render_plan`]) and the same refusal.
//!
//! Referential integrity lives here, so every surface inherits it (D-078
//! carried): removing a provider that a selection still uses, or a
//! credential reference the stored configuration still names, is refused
//! with [`ManageError::Referenced`] listing every referrer, and nothing is
//! written.

use super::credentials::{CredentialReferenceError, CredentialReferences, CredentialStatus};
use super::history::ConfigActor;
use super::plan::ConfigChangeSummary;
use super::render::render_plan;
use super::transaction::{
    ApplyOptions, ConfigChange, ConfigPlan, ConfigTransactionError, ConfigTransactionOutcome, ConfigTransactions,
};
use super::validate;
use crate::config::schema::ProviderModelConfig;
use crate::config::secret_paths::{PathElem, display_path};
use crate::config::{Config, EmbeddingRouteConfig, ModelRouteConfig, SharedConfig};
use crate::providers::ProviderId;
use serde::Serialize;
use std::sync::Arc;

/// Which default or which route table a selection belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOperation {
    /// Chat: `default_provider`/`default_model`, `[[model_routes]]`.
    Chat,
    /// Embeddings: `memory.embedding_*`, `[[embedding_routes]]`.
    Embedding,
}

/// What happens to the root connection (`api_key`, `api_url`,
/// `reliability.api_keys`) when the default chat provider changes. The root
/// connection is owned by the default provider, so switching the default
/// would hand it to the new provider; that is never implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootCredential {
    /// Refuse a switch while the root connection holds anything.
    #[default]
    Refuse,
    /// The operator states that the root connection belongs to the new
    /// default provider.
    Keep,
    /// Clear the root connection with the switch.
    Clear,
}

/// One management operation.
pub enum ManagedChange {
    /// Migrate published model replacements atomically without moving credentials.
    MigrateRetired,
    /// Persist a validated model level and explicit parameter overrides.
    SetModelSettings {
        model: String,
        settings: crate::config::model_settings::ModelSettings,
    },
    /// Add a provider to the configured set: appended to
    /// `reliability.fallback_providers` (the default is chosen with
    /// [`ManagedChange::SetDefault`]).
    AddProvider { provider: String },
    /// Add a fallback model for the default selection and ensure provider membership.
    AddProviderWithModel { provider: String, model: String },
    /// Set (`Some`) or clear (`None`) the endpoint of the default provider
    /// (`api_url`, which only the default provider uses).
    SetProviderEndpoint { provider: String, api_url: Option<String> },
    /// Remove a provider: its fallback membership and its declarations.
    /// Refused while any selection still uses it.
    RemoveProvider { provider: String },
    /// Add a curated `[[providers.models]]` declaration.
    AddModel(ProviderModelConfig),
    /// Change an existing declaration: every declared field of the argument
    /// replaces the stored one; the rest is kept.
    UpdateModel(ProviderModelConfig),
    /// Remove a declaration.
    RemoveModel { provider: String, model: Option<String> },
    /// Set the default model for an operation.
    SetDefault {
        settings: Option<crate::config::model_settings::ModelSettings>,
        operation: SelectionOperation,
        provider: String,
        model: String,
        root_credential: RootCredential,
    },
    /// Add a route.
    AddRoute {
        operation: SelectionOperation,
        hint: String,
        provider: String,
        model: String,
    },
    /// Change a route's provider or model.
    UpdateRoute {
        operation: SelectionOperation,
        hint: String,
        provider: Option<String>,
        model: Option<String>,
    },
    /// Remove a route (the validator refuses it while a selection names it).
    RemoveRoute {
        operation: SelectionOperation,
        hint: String,
    },
    /// Bind an existing agent to a provider and model.
    BindAgent {
        agent: String,
        provider: String,
        model: String,
    },
}

/// Why a management operation was refused. No variant carries a credential
/// value; provider text is shown with URL userinfo redacted.
#[derive(Debug, thiserror::Error)]
pub enum ManageError {
    /// The operation's input is not acceptable; nothing was written.
    #[error("{0}; nothing was written")]
    Invalid(String),
    /// The object does not exist; nothing was written.
    #[error("{0} does not exist; nothing was written")]
    NotFound(String),
    /// The object already exists; nothing was written.
    #[error("{0} already exists; nothing was written")]
    AlreadyExists(String),
    /// The object is still referenced; nothing was written or removed.
    #[error(
        "{object} is still referenced by: {}. Change or remove those first; nothing was written",
        referrers.join("; ")
    )]
    Referenced { object: String, referrers: Vec<String> },
    /// A credential reference operation was refused.
    #[error(transparent)]
    Credential(#[from] CredentialReferenceError),
    /// The transaction refused or failed.
    #[error(transparent)]
    Transaction(#[from] ConfigTransactionError),
}

/// A planned management change: validated, rendered, not written.
pub struct ManagedPlan {
    plan: ConfigPlan,
    /// The revision the change was derived from; a high-risk confirmation
    /// names it.
    revision: Option<String>,
    /// One sentence per selection the change makes that the evidence does
    /// not verify (accepted for the operator, D-034).
    warnings: Vec<String>,
}

impl std::fmt::Debug for ManagedPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedPlan")
            .field("plan", &self.plan)
            .field("revision", &self.revision)
            .field("warnings", &self.warnings)
            .finish()
    }
}

impl ManagedPlan {
    /// The redacted semantic plan.
    #[must_use]
    pub const fn summary(&self) -> &ConfigChangeSummary {
        self.plan.summary()
    }

    /// The revision the change is based on: what a high-risk confirmation
    /// must name.
    #[must_use]
    pub fn revision(&self) -> Option<&str> {
        self.revision.as_deref()
    }

    /// One sentence per selection the change makes that the evidence does
    /// not verify (accepted for an operator, D-034).
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The plan as text, through the one shared renderer.
    #[must_use]
    pub fn text(&self) -> String {
        render_plan(self.plan.summary(), &self.warnings)
    }
}

/// The management service of one process configuration.
#[derive(Clone)]
pub struct ManagementService {
    shared: SharedConfig,
    transactions: ConfigTransactions,
}

impl std::fmt::Debug for ManagementService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ManagementService").finish_non_exhaustive()
    }
}

impl ManagementService {
    #[must_use]
    pub fn new(shared: SharedConfig) -> Self {
        Self {
            transactions: ConfigTransactions::new(Arc::clone(&shared)),
            shared,
        }
    }

    /// The service of a process that does not run this configuration's
    /// runtime (the `prx` CLI): applied changes are recorded
    /// `activation_deferred` (see
    /// [`ConfigTransactions::deferring_activation`]).
    #[must_use]
    pub fn deferred(shared: SharedConfig) -> Self {
        Self {
            transactions: ConfigTransactions::new(Arc::clone(&shared)).deferring_activation(),
            shared,
        }
    }

    /// The configuration changes are derived from (the desired one).
    #[must_use]
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.shared.desired().config)
    }

    /// The revision the desired configuration was loaded from (`None` for
    /// a configuration that did not come from disk): what a rollback or a
    /// change derived from [`config`](Self::config) is based on.
    #[must_use]
    pub fn revision(&self) -> Option<String> {
        ConfigTransactions::base_revision(&self.shared.desired())
    }

    /// The transaction layer (history, rollback, raw files).
    #[must_use]
    pub const fn transactions(&self) -> &ConfigTransactions {
        &self.transactions
    }

    /// Validate `change` and describe it; nothing is written. The plan has
    /// passed every check the write runs first (staging, write boundary,
    /// every affected selection under `actor`'s policy).
    ///
    /// # Errors
    ///
    /// An invalid, missing, duplicate or still-referenced object, or the
    /// transaction's refusal.
    pub async fn plan(&self, change: &ManagedChange, actor: ConfigActor) -> Result<ManagedPlan, ManageError> {
        let desired = self.shared.desired();
        let candidate = candidate(&desired.config, change)?;
        let revision = ConfigTransactions::base_revision(&desired);
        self.plan_candidate(candidate, revision, actor).await
    }

    /// Write-only Brave credential planning through the same slot validation,
    /// encryption and revision-checked transaction used by the gateway.
    pub async fn plan_brave_credential(
        &self,
        secret: &super::credentials::SecretString,
        actor: ConfigActor,
    ) -> Result<ManagedPlan, ManageError> {
        let desired = self.shared.desired();
        let mut candidate = desired.config.as_ref().clone();
        let value = crate::config::credential_slots::validate_credential(secret.expose())
            .map_err(|error| ManageError::Invalid(error.to_string()))?;
        crate::config::credential_slots::CredentialSlot::BraveSearch
            .apply(
                &mut candidate,
                crate::config::credential_slots::CredentialValue::One(value),
            )
            .map_err(|error| ManageError::Invalid(error.to_string()))?;
        self.plan_candidate(candidate, ConfigTransactions::base_revision(&desired), actor)
            .await
    }

    /// Plan a raw file write (`config.toml` or a `config.d` fragment), as the
    /// Gateway raw editor does; nothing is written.
    /// `allow_secret_clear` explicitly permits omission of stored secrets;
    /// mask, encryption, risk and revision checks still apply.
    ///
    /// # Errors
    ///
    /// The transaction's refusal; its message can quote the posted text, so
    /// the caller scrubs it.
    pub async fn plan_file(
        &self,
        filename: String,
        contents: String,
        allow_secret_clear: bool,
        actor: ConfigActor,
    ) -> Result<ManagedPlan, ManageError> {
        let plan = self
            .transactions
            .plan(ConfigChange::RawFile {
                filename,
                contents,
                allow_secret_clear,
            })
            .await?;
        let warnings = self.transactions.check_selections(&plan, actor).await?;
        let revision = plan.planned_revision().map(str::to_string);
        Ok(ManagedPlan {
            plan,
            revision,
            warnings,
        })
    }

    async fn plan_candidate(
        &self,
        candidate: Config,
        revision: Option<String>,
        actor: ConfigActor,
    ) -> Result<ManagedPlan, ManageError> {
        let plan = self
            .transactions
            .plan(ConfigChange::Structured(Box::new(candidate)))
            .await?;
        let warnings = self.transactions.check_selections(&plan, actor).await?;
        let revision = revision.or_else(|| plan.planned_revision().map(str::to_string));
        Ok(ManagedPlan {
            plan,
            revision,
            warnings,
        })
    }

    /// Commit a plan. A high-risk plan needs `confirmed_revision` equal to
    /// the plan's [`revision`](ManagedPlan::revision) (D-049 B).
    ///
    /// # Errors
    ///
    /// The transaction's refusal (confirmation, conflict, validation,
    /// activation).
    pub async fn apply(
        &self,
        plan: ManagedPlan,
        actor: ConfigActor,
        confirmed_revision: Option<&str>,
    ) -> Result<ConfigTransactionOutcome, ManageError> {
        let ManagedPlan { plan, revision, .. } = plan;
        Ok(self
            .transactions
            .apply(
                plan,
                actor,
                ApplyOptions {
                    expected_revision: revision.as_deref(),
                    confirm_high_risk: confirmed_revision.is_some(),
                    confirmed_revision,
                },
            )
            .await?)
    }

    /// Plan decision-plane `mutations` (CDP-14, `prx causal config`) over
    /// the desired configuration: the candidate comes from the one mutation
    /// service (`mutation::causal`), and the plan, its text, the decision
    /// backend check and the apply are this service's, the same as for every
    /// other management change. Nothing is written.
    ///
    /// # Errors
    ///
    /// A refused mutation or an invalid section ([`ManageError::Invalid`]),
    /// or the transaction's refusal.
    pub async fn plan_causal(
        &self,
        mutations: &[super::mutation::causal::CausalMutation],
        actor: ConfigActor,
    ) -> Result<ManagedPlan, ManageError> {
        let desired = self.shared.desired();
        let candidate =
            super::mutation::causal::CausalConfigService::mutate(&desired.config, mutations).map_err(causal_error)?;
        let revision = ConfigTransactions::base_revision(&desired);
        self.plan_candidate(candidate, revision, actor).await
    }

    /// Store `secret` as the decision backend's credential and apply the
    /// reference (`CausalConfigService::store_credential`: write-only,
    /// compensated on refusal, the previous decision credential removed after
    /// a rotation). A credential change is high risk: `confirmed_revision`
    /// must name the revision the caller was shown ([`Self::revision`]).
    ///
    /// # Errors
    ///
    /// No decision backend is configured, the revision is stale or
    /// unconfirmed, or the store or the transaction refused.
    pub async fn store_decision_credential(
        &self,
        secret: super::credentials::SecretString,
        actor: ConfigActor,
        confirmed_revision: Option<&str>,
    ) -> Result<super::mutation::causal::StoredCredential, ManageError> {
        super::mutation::causal::CausalConfigService::new(self.transactions.clone())
            .store_credential(
                secret,
                actor,
                ApplyOptions {
                    expected_revision: confirmed_revision,
                    confirm_high_risk: confirmed_revision.is_some(),
                    confirmed_revision,
                },
            )
            .await
            .map_err(causal_error)
    }

    /// The credential references of this configuration.
    ///
    /// # Errors
    ///
    /// When the configuration has no directory.
    pub fn credentials(&self) -> Result<CredentialReferences, ManageError> {
        Ok(CredentialReferences::for_config(&self.config())?)
    }

    /// What is legacy in the desired configuration, and what applying the
    /// migration would change (SP-PMM §2.7). Writes nothing.
    ///
    /// # Errors
    ///
    /// When the credential store cannot be read.
    pub fn migration_report(&self) -> Result<super::migrate::MigrationReport, ManageError> {
        super::migrate::report(&self.config())
    }

    /// Plan the migration: the report, and the plan of the migrated
    /// configuration (validated and rendered, not written). `None` when
    /// nothing would change. The plan's reference checks run at apply,
    /// after [`apply_migration`](Self::apply_migration) has stored the
    /// references.
    ///
    /// # Errors
    ///
    /// When the credential store cannot be read, or the transaction's
    /// refusal of the migrated configuration.
    pub async fn plan_migration(&self) -> Result<Option<(super::migrate::MigrationReport, ManagedPlan)>, ManageError> {
        let desired = self.shared.desired();
        let report = super::migrate::report(&desired.config)?;
        if report.nothing_to_apply() {
            return Ok(None);
        }
        let candidate = super::migrate::migrated(&desired.config, &report);
        let plan = self
            .transactions
            .plan(ConfigChange::Structured(Box::new(candidate)))
            .await?;
        let revision =
            ConfigTransactions::base_revision(&desired).or_else(|| plan.planned_revision().map(str::to_string));
        Ok(Some((
            report,
            ManagedPlan {
                plan,
                revision,
                warnings: Vec::new(),
            },
        )))
    }

    /// Apply a planned migration: store each inline key as its credential
    /// reference (under the transaction locks; replacing one an interrupted
    /// run left is recorded), then commit the migrated configuration through
    /// the transaction, which checks every reference it names. The plan
    /// removes inline keys, so it is high risk: nothing is stored before the
    /// confirmation names the plan's revision. When the commit is refused,
    /// the references this call created are removed again.
    ///
    /// # Errors
    ///
    /// The confirmation, a store failure, or the transaction's refusal.
    pub async fn apply_migration(
        &self,
        report: &super::migrate::MigrationReport,
        plan: ManagedPlan,
        actor: ConfigActor,
        confirmed_revision: Option<&str>,
    ) -> Result<ConfigTransactionOutcome, ManageError> {
        if plan.summary().risk == super::plan::ConfigChangeRisk::High {
            let revision = plan.revision().map(str::to_string);
            match confirmed_revision {
                None => {
                    return Err(ManageError::Transaction(ConfigTransactionError::ConfirmationRequired {
                        required: "confirm_high_risk",
                        reasons: plan.summary().risk_reasons.clone(),
                        revision,
                    }));
                }
                Some(confirmed) if Some(confirmed) != revision.as_deref() => {
                    return Err(ManageError::Transaction(ConfigTransactionError::Conflict(
                        crate::config::files::ConfigRevisionConflict {
                            expected: confirmed.to_string(),
                            actual: revision.unwrap_or_default(),
                        },
                    )));
                }
                Some(_) => {}
            }
        }
        let values = super::migrate::inline_values(&self.config(), report);
        let references = self.credentials()?;
        // A reference the migration would store must not exist yet (one left
        // by an interrupted run, or created by hand): replacing it could not
        // be undone if the commit is refused (D-101 F-8).
        for key in &report.inline_keys {
            let id = key.reference_id();
            if references.status(&id)?.is_some() {
                return Err(ManageError::Invalid(format!(
                    "credential reference '{id}' already exists (left by an interrupted migration?); remove it with \
                     `prx providers credential remove {id}` and run the migration again"
                )));
            }
        }
        let mut created = Vec::new();
        let mut stored = Ok(());
        for key in &report.inline_keys {
            let Some(value) = values.get(&key.path) else {
                continue;
            };
            match self
                .set_credential(key.provider.clone(), key.reference_name.clone(), value.clone(), actor)
                .await
            {
                Ok(_) => created.push(key.reference_id()),
                Err(error) => {
                    stored = Err(error);
                    break;
                }
            }
        }
        let applied = match stored {
            Ok(()) => self.apply(plan, actor, confirmed_revision).await,
            Err(error) => Err(error),
        };
        if applied.is_err() {
            for id in created {
                if let Err(error) = references.remove(&id) {
                    tracing::warn!(reference = %id, error = %error, "Failed to remove a credential reference a refused migration created");
                }
            }
        }
        applied
    }

    /// Store `secret` as the credential reference `name` of provider
    /// `owner` (created or replaced; never moved to another provider), under
    /// the transaction locks. Replacing an existing reference changes what
    /// every route, embedding route and agent naming it sends, so it is
    /// recorded in the configuration history like a credential write (D-089
    /// 11): a `pending` record before the store is written, `activated`
    /// after it (`commit_failed` when the store refused). The record names
    /// the reference id only; its before-image is the configuration as it
    /// was, which the write does not change.
    ///
    /// # Errors
    ///
    /// An invalid name, owner or value, a reference owned by another
    /// provider, a lock or store failure, or a history record that cannot be
    /// written (then nothing is stored). No error carries the value.
    pub async fn set_credential(
        &self,
        owner: String,
        name: String,
        secret: String,
        actor: ConfigActor,
    ) -> Result<CredentialStatus, ManageError> {
        let config = self.config();
        self.transactions
            .locked(move |config_path| -> Result<CredentialStatus, ManageError> {
                let references = CredentialReferences::for_config(&config)?;
                let id = CredentialReferences::id_for(&name)?;
                let record = if references.status(&id)?.is_some() {
                    let history = super::history::ConfigHistory::for_config(config_path)
                        .map_err(ConfigTransactionError::Failed)?;
                    let before = crate::config::files::current_before_image(config_path)
                        .map_err(ConfigTransactionError::Failed)?;
                    let record = history
                        .create_pending(
                            super::history::NewRecord {
                                actor,
                                base_revision: None,
                                restores: None,
                                summary: ConfigChangeSummary::credential_reference_replaced(&id),
                            },
                            &before,
                        )
                        .map_err(ConfigTransactionError::Failed)?;
                    Some((history, record))
                } else {
                    None
                };
                let stored = references.set(&owner, &name, &secret);
                if let Some((history, mut record)) = record {
                    // Truthful: the store holds the new value; a running
                    // process sends it from its next configuration reload or
                    // restart, and the previous value is not kept, so the
                    // record is not restorable (D-101 F-2).
                    record.status = if stored.is_ok() {
                        super::history::RecordStatus::CredentialReplaced
                    } else {
                        super::history::RecordStatus::CommitFailed
                    };
                    record.reason = stored.as_ref().err().map(ToString::to_string);
                    if let Err(error) = history.update(&mut record) {
                        tracing::warn!(record = %record.id, error = %error, "Failed to update a configuration history record");
                    }
                }
                Ok(stored?)
            })
            .await
    }

    /// Remove the credential reference `id`, unless the stored configuration
    /// still names it. The referrer check and the removal run under the
    /// transaction locks, so no configuration write that names the
    /// reference can land in between (D-089 11). `Ok(false)` when it did not
    /// exist.
    ///
    /// # Errors
    ///
    /// [`ManageError::Referenced`] listing every referrer, an invalid id, or
    /// a lock or store failure. Nothing is removed on an error.
    pub async fn remove_credential(&self, id: &str) -> Result<bool, ManageError> {
        self.remove_owned_credential(None, id).await
    }

    /// [`remove_credential`](Self::remove_credential) of a reference that must
    /// belong to `owner` (the provider a request names): the owner check runs
    /// under the same locks as the removal, so a reference removed and
    /// re-created for another provider in between is never removed through
    /// this provider (D-101 F-6).
    ///
    /// # Errors
    ///
    /// A missing reference or one owned by another provider, and every error
    /// of [`remove_credential`](Self::remove_credential).
    pub async fn remove_owned_credential(&self, owner: Option<&str>, id: &str) -> Result<bool, ManageError> {
        CredentialReferences::reference(id)?;
        let config = self.config();
        let id = id.to_string();
        let owner = owner.map(|owner| ProviderId::of(owner.trim()).to_string());
        self.transactions
            .locked(move |_| -> Result<bool, ManageError> {
                if let Some(owner) = owner {
                    match CredentialReferences::for_config(&config)?.status(&id)? {
                        None => return Err(ManageError::Credential(CredentialReferenceError::Missing(id))),
                        Some(status) if status.owner != owner => {
                            return Err(ManageError::Credential(CredentialReferenceError::OwnedByOther {
                                id,
                                owner: status.owner,
                            }));
                        }
                        Some(_) => {}
                    }
                }
                let referrers = credential_referrers(&config, &id)?;
                if !referrers.is_empty() {
                    return Err(ManageError::Referenced {
                        object: format!("credential reference '{id}'"),
                        referrers,
                    });
                }
                Ok(CredentialReferences::for_config(&config)?.remove(&id)?)
            })
            .await
    }
}

/// A decision-plane mutation's refusal as a management error: the
/// transaction's own refusal keeps its type (confirmation, conflict), the rest
/// is invalid input. No message carries a secret.
fn causal_error(error: super::mutation::causal::CausalMutationError) -> ManageError {
    match error {
        super::mutation::causal::CausalMutationError::Transaction(error) => ManageError::Transaction(error),
        super::mutation::causal::CausalMutationError::Credential(error) => ManageError::Credential(error),
        other => ManageError::Invalid(other.to_string()),
    }
}

/// Every place of the stored configuration (all files, merged) that names
/// the credential reference `id`: a `credential_ref` value, or an element of
/// a `credential_refs` list, anywhere. The stored tree is read rather than
/// the typed configuration, so a reference in a section this build does not
/// model yet is found too.
///
/// # Errors
///
/// When the stored configuration cannot be read.
pub fn credential_referrers(config: &Config, id: &str) -> Result<Vec<String>, ManageError> {
    if !config.config_path.exists() {
        return Ok(Vec::new());
    }
    let stored = crate::config::files::read_merged_toml_with_gate(&config.config_path).map_err(|error| {
        ManageError::Invalid(format!(
            "the stored configuration cannot be read to check who references the credential ({})",
            crate::providers::sanitize_api_error(&format!("{error:#}"))
        ))
    })?;
    let mut found = Vec::new();
    collect_credential_referrers(&stored, id, false, &mut Vec::new(), &mut found);
    Ok(found)
}

fn collect_credential_referrers(
    node: &toml::Value,
    id: &str,
    under_reference_key: bool,
    path: &mut Vec<PathElem>,
    found: &mut Vec<String>,
) {
    match node {
        toml::Value::String(value) if under_reference_key && value.trim() == id => {
            found.push(display_path(path));
        }
        toml::Value::Table(table) => {
            for (key, child) in table {
                let names_reference = key.ends_with("credential_ref") || key.ends_with("credential_refs");
                path.push(PathElem::Key(key.clone()));
                collect_credential_referrers(child, id, names_reference, path, found);
                path.pop();
            }
        }
        toml::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                path.push(PathElem::Index(index));
                collect_credential_referrers(child, id, under_reference_key, path, found);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Every selection of `config` that uses the provider `id` (every spelling
/// of one provider counts), each once, by the validator's name for it; the
/// decision backend too (D-094 C-3), so `show` names it and a removal is
/// refused as referenced by it. It is left out of `validate::provider_uses`,
/// which the model listings and the Gateway's chat connection test read: a
/// decision-only service is not a chat provider.
fn uses_of(config: &Config, id: &ProviderId) -> Vec<String> {
    let mut names = Vec::new();
    for (name, provider) in validate::provider_uses(config) {
        if ProviderId::of(&provider) == *id && !names.contains(&name) {
            names.push(name);
        }
    }
    if let Some(selection) = crate::decision::factory::configured_selection(&config.causal_tree)
        && ProviderId::of(&selection.provider) == *id
    {
        names.push("the decision backend (causal_tree.backend)".to_string());
    }
    names
}

/// A provider of the configuration as a surface lists it. No credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderEntry {
    /// The provider as configured, URL userinfo redacted.
    pub provider: String,
    /// Its canonical identity (`catalog:openai`, `custom:http://...`).
    pub identity: String,
    /// Whether it is the default provider.
    pub default: bool,
    /// Whether it is in the failover chain (`reliability.fallback_providers`).
    pub fallback: bool,
    /// The endpoint override (`api_url`) when it is the default, redacted.
    pub endpoint: Option<String>,
    /// How many `[[providers.models]]` declarations name it.
    pub declarations: usize,
    /// Every selection that uses it, by the validator's name for it.
    pub used_by: Vec<String>,
}

/// Every provider `config` names: the default, the failover chain, then
/// every other provider a selection or a declaration names, each once (by
/// identity).
#[must_use]
pub fn configured_providers(config: &Config) -> Vec<ProviderEntry> {
    let mut spellings = vec![crate::providers::selection::default_provider(config).to_string()];
    spellings.extend(config.reliability.fallback_providers.iter().cloned());
    spellings.extend(
        validate::provider_uses(config)
            .into_iter()
            .map(|(_, provider)| provider),
    );
    spellings.extend(config.providers.models.iter().map(|entry| entry.provider.clone()));
    let mut seen = Vec::new();
    let mut entries = Vec::new();
    for spelling in spellings {
        let id = ProviderId::of(&spelling);
        if spelling.trim().is_empty() || seen.contains(&id) || crate::providers::selection::is_prx_embedder(&spelling) {
            continue;
        }
        entries.push(provider_entry(config, &spelling));
        seen.push(id);
    }
    entries
}

/// Whether `provider` is configured: the default provider, a failover
/// member, or one a selection uses (compared by identity, never by display
/// text). What the Gateway's connection test is limited to.
#[must_use]
pub fn is_configured_provider(config: &Config, provider: &str) -> bool {
    let id = ProviderId::of(provider.trim());
    ProviderId::of(crate::providers::selection::default_provider(config)) == id
        || config
            .reliability
            .fallback_providers
            .iter()
            .any(|member| ProviderId::of(member) == id)
        || validate::provider_uses(config)
            .iter()
            .any(|(_, used)| ProviderId::of(used) == id)
}

/// How `config` uses `provider`, whether or not it is configured.
#[must_use]
pub fn provider_entry(config: &Config, provider: &str) -> ProviderEntry {
    let id = ProviderId::of(provider);
    let default = id == ProviderId::of(crate::providers::selection::default_provider(config));
    ProviderEntry {
        provider: shown(provider),
        identity: redact_provider_text(&id.to_string()),
        default,
        fallback: config
            .reliability
            .fallback_providers
            .iter()
            .any(|member| ProviderId::of(member) == id),
        endpoint: config
            .api_url
            .as_deref()
            .filter(|url| default && !url.trim().is_empty())
            .map(shown),
        declarations: config
            .providers
            .models
            .iter()
            .filter(|entry| ProviderId::of(&entry.provider) == id)
            .count(),
        used_by: uses_of(config, &id),
    }
}

/// Where a provider's credentials are. Never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCredentials {
    /// The root connection (`api_key`, `api_url`, `reliability.api_keys`)
    /// belongs to this provider (it is the default) and holds something.
    pub root_connection: bool,
    /// Inline keys of routes and agents that use this provider, by path.
    pub inline_slots: Vec<String>,
    /// Ids of the provider's own auth profiles (`prx auth`), which a
    /// selection without a more specific credential uses.
    pub store_profiles: Vec<String>,
    /// Credential references (`cred-<name>`) issued for this provider. A
    /// model route, embedding route, agent or decision backend uses one when
    /// its `credential_ref` names it.
    pub references: Vec<CredentialStatus>,
    /// For each reference in [`references`](Self::references), every place
    /// of the stored configuration that names it (empty: unused, see
    /// [`UNREFERENCED`]).
    pub reference_uses: std::collections::BTreeMap<String, Vec<String>>,
    /// The credential PRX sends to this provider today when a selection
    /// names nothing more specific.
    pub in_use: CredentialInUse,
    /// One sentence naming that credential and how to set it (D-085 F-5).
    pub guidance: String,
}

/// Which credential PRX sends to a provider today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialInUse {
    /// The root connection: the provider is the default and `api_key`,
    /// `api_url` or `reliability.api_keys` holds something.
    RootConnection,
    /// The provider's own auth profile (`prx auth paste-token`).
    ProviderStore,
}

/// What every surface says about a `cred-<name>` reference no configuration
/// field names yet.
pub const UNREFERENCED: &str = "not referenced: set `credential_ref` on a model route, embedding route, agent or \
     decision backend to name it (`prx providers migrate --apply` moves inline keys into references)";

/// Compatibility name for decision-plane status reporting.
pub const REFERENCE_NOT_USED: &str = UNREFERENCED;

/// Describe every known use of one credential reference without reading its value.
pub fn reference_use(config: &Config, id: &str) -> Result<String, ManageError> {
    let decision_refers = matches!(
        &config.causal_tree.backend.credential_ref,
        crate::causal_tree::config::CredentialRefSetting::Profile(used) if used == id
    );
    let mut referrers = credential_referrers(config, id)?;
    const DECISION_PATH: &str = "causal_tree.backend.credential_ref";
    if decision_refers {
        referrers.retain(|path| path != DECISION_PATH);
    }
    referrers.sort();
    referrers.dedup();
    let decision_can_run = config.causal_tree.enabled
        && crate::decision::factory::configured_selection(&config.causal_tree).is_some_and(|selection| {
            selection.locality != crate::decision::BackendLocality::Remote || config.causal_tree.backend.remote_enabled
        });
    if decision_refers && decision_can_run {
        let mut description = format!("configured for the enabled decision backend ({DECISION_PATH})");
        if !referrers.is_empty() {
            description.push_str(&format!("; also referenced by {}", referrers.join(", ")));
        }
        return Ok(description);
    }
    if decision_refers {
        referrers.push(DECISION_PATH.to_string());
        referrers.sort();
    }
    Ok(if referrers.is_empty() {
        REFERENCE_NOT_USED.to_string()
    } else {
        format!("referenced by {}", referrers.join(", "))
    })
}

/// The sentence naming the credential PRX uses for `provider` today and
/// how to set it. Never a value.
fn credential_guidance(provider: &str, in_use: CredentialInUse, encrypted_at_rest: bool) -> String {
    let provider = shown(provider);
    match in_use {
        // Never `prx onboard`: quick setup regenerates the whole
        // configuration outside the transaction (D-088 C-2).
        CredentialInUse::RootConnection if encrypted_at_rest => format!(
            "PRX sends '{provider}' the root connection today (config.toml `api_key`, `reliability.api_keys`); \
             set that key with `PUT /api/config/secrets` (slot `api_key`), which stores it encrypted"
        ),
        CredentialInUse::RootConnection => format!(
            "PRX sends '{provider}' the root connection today (config.toml `api_key`, `reliability.api_keys`); \
             set that key with `PUT /api/config/secrets` (slot `api_key`) or `prx config apply --file <path> --yes`"
        ),
        CredentialInUse::ProviderStore => format!(
            "PRX sends '{provider}' its own auth profile today; set that key with \
             `prx auth paste-token --provider {provider} --from-stdin`"
        ),
    }
}

/// The credential status of `provider` in `config`'s directory. Reads the
/// auth-profile store without writing it.
///
/// # Errors
///
/// When the store cannot be read.
pub fn provider_credentials(config: &Config, provider: &str) -> Result<ProviderCredentials, ManageError> {
    let id = ProviderId::of(provider);
    let is_default = id == ProviderId::of(crate::providers::selection::default_provider(config));
    let held = |key: Option<&String>| key.is_some_and(|key| !key.trim().is_empty());
    let mut inline_slots = Vec::new();
    for (index, route) in config.model_routes.iter().enumerate() {
        if ProviderId::of(&route.provider) == id && held(route.api_key.as_ref()) {
            inline_slots.push(format!("model_routes[{index}].api_key"));
        }
    }
    for (index, route) in config.embedding_routes.iter().enumerate() {
        if ProviderId::of(&route.provider) == id && held(route.api_key.as_ref()) {
            inline_slots.push(format!("embedding_routes[{index}].api_key"));
        }
    }
    let mut agents = config.agents.iter().collect::<Vec<_>>();
    agents.sort_by(|left, right| left.0.cmp(right.0));
    for (name, agent) in agents {
        if ProviderId::of(&agent.provider) == id && held(agent.api_key.as_ref()) {
            inline_slots.push(format!("agents.{name}.api_key"));
        }
    }
    let references = CredentialReferences::for_config(config)?;
    let owner = id.to_string();
    // A missing store is read as empty without creating it (or its lock).
    let store = config
        .config_path
        .parent()
        .map(|dir| crate::auth::profiles::AuthProfilesStore::new(dir, config.secrets.encrypt))
        .filter(|store| store.path().exists());
    let store_exists = store.is_some();
    let store_profiles = match store {
        Some(store) => store
            .load_read_only()
            .map_err(|error| {
                ManageError::Invalid(format!(
                    "the credential store could not be read ({})",
                    crate::providers::sanitize_api_error(&format!("{error:#}"))
                ))
            })?
            .profiles
            .values()
            .filter(|profile| {
                !profile.id.starts_with(super::credentials::REFERENCE_PREFIX) && ProviderId::of(&profile.provider) == id
            })
            .map(|profile| profile.id.clone())
            .collect(),
        None => Vec::new(),
    };
    let in_use = match crate::providers::selection::credential_for(config, provider) {
        crate::providers::credentials::CredentialRef::LegacyInline(_) => CredentialInUse::RootConnection,
        _ => CredentialInUse::ProviderStore,
    };
    let references = if store_exists {
        references
            .list()?
            .into_iter()
            .filter(|reference| reference.owner == owner)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut reference_uses = std::collections::BTreeMap::new();
    for reference in &references {
        reference_uses.insert(reference.id.clone(), credential_referrers(config, &reference.id)?);
    }
    Ok(ProviderCredentials {
        in_use,
        guidance: credential_guidance(provider, in_use, config.secrets.encrypt),
        root_connection: is_default && root_holds_material(config),
        inline_slots,
        store_profiles,
        references,
        reference_uses,
    })
}

/// Which models a listing shows.
#[derive(Debug, Clone, Default)]
pub struct ModelFilter {
    /// Only this provider (compared by identity).
    pub provider: Option<String>,
    /// Only models the evidence does not rule out for this operation.
    pub operation: Option<crate::providers::catalog::ModelOperation>,
    /// Only models the evidence proves can execute `operation` (chat when
    /// none is named): unverified models are left out.
    pub executable: bool,
}

/// One model as every surface lists it: what the evidence says it can do,
/// and where that evidence comes from. No credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    /// The provider as configured or cached, redacted.
    pub provider: String,
    /// Its canonical identity, redacted.
    pub identity: String,
    pub model: String,
    /// Every operation: `supported`, `unknown` or `unsupported: <reason>`.
    pub operations: std::collections::BTreeMap<String, String>,
    /// Every chat feature, the same way.
    pub features: std::collections::BTreeMap<String, String>,
    pub context_window: Option<u32>,
    /// Where the evidence comes from (`operator declaration`, ...).
    pub source: String,
    /// When the catalog the evidence came from was fetched.
    pub fetched_at_unix: Option<u64>,
    /// Present when the embedded release catalog still permits this model but
    /// recommends migration to an active row.
    pub lifecycle_warning: Option<String>,
}

/// The operation named `text` (`chat`, `embedding`, `decision_endpoint`, ...).
///
/// # Errors
///
/// [`ManageError::Invalid`] naming the accepted spellings.
pub fn parse_operation(text: &str) -> Result<crate::providers::catalog::ModelOperation, ManageError> {
    use crate::providers::catalog::ModelOperation;
    let text = text.trim();
    ModelOperation::ALL
        .into_iter()
        .find(|operation| operation.as_str().eq_ignore_ascii_case(text))
        .ok_or_else(|| {
            ManageError::Invalid(format!(
                "the operation is one of {}",
                ModelOperation::ALL
                    .iter()
                    .map(|operation| operation.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

fn support_text(support: &crate::providers::catalog::Support) -> String {
    use crate::providers::catalog::Support;
    match support {
        Support::Supported => "supported".to_string(),
        Support::Unknown => "unknown".to_string(),
        Support::Unsupported(reason) => format!("unsupported: {reason}"),
    }
}

fn model_entry_of(catalog: &crate::providers::catalog::CatalogView, provider: &str, model: &str) -> ModelEntry {
    use crate::providers::catalog::{DescriptorSource, ModelOperation};
    let id = ProviderId::of(provider);
    let descriptor = catalog.describe(&id, model);
    let features = &descriptor.features;
    let lifecycle_warning = catalog
        .release_catalog()
        .ok()
        .and_then(|release| release.model(&id, model))
        .and_then(crate::providers::catalog::ReleaseModel::selection_warning);
    ModelEntry {
        provider: shown(provider),
        identity: redact_provider_text(&id.to_string()),
        model: model.trim().to_string(),
        operations: ModelOperation::ALL
            .into_iter()
            .map(|operation| {
                (
                    operation.as_str().to_string(),
                    support_text(&descriptor.operation(operation)),
                )
            })
            .collect(),
        features: [
            ("native_tools", &features.native_tools),
            ("vision_input", &features.vision_input),
            ("streaming", &features.streaming),
            ("structured_output", &features.structured_output),
        ]
        .into_iter()
        .map(|(name, support)| (name.to_string(), support_text(support)))
        .collect(),
        context_window: descriptor.context_window,
        source: descriptor.source.label().to_string(),
        fetched_at_unix: match &descriptor.source {
            DescriptorSource::Discovery { fetched_at_unix } => Some(*fetched_at_unix),
            _ => None,
        },
        lifecycle_warning,
    }
}

/// Every model `config` knows of — each configured selection, each
/// `[[providers.models]]` declaration that names a model, and each model
/// the catalog cache (`prx models refresh`) holds — described by the same
/// evidence the resolver uses, then filtered. Local: reads the cache, sends
/// nothing. The listing `prx models list --operation/--executable` and the
/// Gateway's model endpoint share (D-085 F-6).
#[must_use]
pub fn model_catalog(config: &Config, filter: &ModelFilter) -> Vec<ModelEntry> {
    use crate::providers::catalog::ModelOperation;
    let catalog = crate::providers::selection::catalog_for(config);
    let mut candidates = validate::selected_models(config);
    candidates.extend(config.providers.models.iter().filter_map(|entry| {
        entry
            .model
            .as_ref()
            .map(|model| (entry.provider.clone(), model.clone()))
    }));
    match catalog.release_catalog() {
        Ok(release) => candidates.extend(
            release
                .models
                .iter()
                .filter(|model| model.is_listed())
                .map(|model| (model.provider.clone(), model.api_model.clone())),
        ),
        Err(error) => tracing::error!(error = %error, "embedded model catalog is invalid"),
    }
    match crate::providers::catalog::discovery::load_model_cache_state(&config.workspace_dir) {
        Ok(state) => {
            for entry in state.entries {
                let provider = entry.provider.clone();
                candidates.extend(
                    entry
                        .models
                        .into_iter()
                        .chain(entry.descriptors.into_iter().map(|model| model.id))
                        .map(|model| (provider.clone(), model)),
                );
            }
        }
        Err(error) => tracing::warn!(
            error = %crate::providers::sanitize_api_error(&format!("{error:#}")),
            "the model cache could not be read; listing the configured models only"
        ),
    }
    let wanted = filter.provider.as_deref().map(ProviderId::of);
    let mut seen = Vec::new();
    let mut entries = Vec::new();
    for (provider, model) in candidates {
        let id = ProviderId::of(&provider);
        let key = (id.clone(), model.trim().to_ascii_lowercase());
        if model.trim().is_empty()
            || !id.is_resolvable()
            || wanted.as_ref().is_some_and(|wanted| *wanted != id)
            || seen.contains(&key)
        {
            continue;
        }
        seen.push(key);
        let descriptor = catalog.describe(&id, &model);
        let operation = filter.operation.unwrap_or(ModelOperation::Chat);
        let support = descriptor.operation(operation);
        if (filter.operation.is_some() && support.is_unsupported()) || (filter.executable && !support.is_supported()) {
            continue;
        }
        entries.push(model_entry_of(&catalog, &provider, &model));
    }
    entries.sort_by(|left, right| (&left.identity, &left.model).cmp(&(&right.identity, &right.model)));
    entries
}

/// One model of one provider, described by the resolver's evidence
/// (`prx models show`). Local.
///
/// # Errors
///
/// A provider PRX cannot construct, or an empty model.
pub fn model_entry(config: &Config, provider: &str, model: &str) -> Result<ModelEntry, ManageError> {
    let (provider, _) = provider_arg(provider)?;
    let model = non_empty(model, "the model")?;
    Ok(model_entry_of(
        &crate::providers::selection::catalog_for(config),
        &provider,
        &model,
    ))
}

/// A provider as operators and messages see it: trimmed, with URL userinfo
/// and credential-looking query parameters (`?api_key=...`) redacted
/// (D-085 F-10).
fn shown(provider: &str) -> String {
    redact_provider_text(provider.trim())
}

/// Redact what a provider spelling or a message about one may carry: the
/// secrets `crate::security::redaction` recognises, then every URL's
/// userinfo, query values (any parameter name), valueless parameters and
/// fragment through the one URL redactor
/// (`security::redaction::redact_url_components`, D-085 F-10, D-088 C-6).
/// Every surface that lists providers uses it.
#[must_use]
pub fn redact_provider_text(text: &str) -> String {
    crate::security::redaction::redact_provider_text(text)
}

/// A provider argument PRX can construct. Unresolvable text is never echoed:
/// a secret pasted into the provider position must not come back.
fn provider_arg(provider: &str) -> Result<(String, ProviderId), ManageError> {
    let provider = provider.trim();
    let id = ProviderId::of(provider);
    if provider.is_empty() || !id.is_resolvable() {
        return Err(ManageError::Invalid(
            "the provider is not one PRX can construct (a name from `prx providers list`, `custom:<url>` or \
             `anthropic-custom:<url>`)"
                .to_string(),
        ));
    }
    Ok((provider.to_string(), id))
}

fn non_empty(value: &str, what: &str) -> Result<String, ManageError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ManageError::Invalid(format!("{what} must not be empty")));
    }
    Ok(value.to_string())
}

/// A route hint: a name, as `hint:<name>` spells it.
fn hint_arg(hint: &str) -> Result<String, ManageError> {
    let hint = hint.trim();
    let hint = hint.strip_prefix("hint:").unwrap_or(hint);
    if hint.is_empty()
        || hint
            .chars()
            .any(|character| character.is_whitespace() || character == ':')
    {
        return Err(ManageError::Invalid(
            "a route hint is a non-empty name without spaces or `:`".to_string(),
        ));
    }
    Ok(hint.to_string())
}

fn same_model(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.trim().eq_ignore_ascii_case(right.trim()),
        _ => false,
    }
}

fn declaration_name(provider: &str, model: Option<&str>) -> String {
    model.map_or_else(
        || format!("the declaration for every model of provider '{}'", shown(provider)),
        |model| {
            format!(
                "the declaration of model '{}' on provider '{}'",
                model.trim(),
                shown(provider)
            )
        },
    )
}

const fn route_table(operation: SelectionOperation) -> &'static str {
    match operation {
        SelectionOperation::Chat => "model route",
        SelectionOperation::Embedding => "embedding route",
    }
}

/// Whether the root connection holds anything the default provider owns.
fn root_holds_material(config: &Config) -> bool {
    [config.api_key.as_deref(), config.api_url.as_deref()]
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty())
        || config.reliability.api_keys.iter().any(|key| !key.trim().is_empty())
}

/// The candidate configuration `change` produces from `current`. Pure: no
/// file is read or written.
///
/// # Errors
///
/// An invalid, missing, duplicate or still-referenced object.
#[allow(clippy::too_many_lines)]
fn candidate(current: &Config, change: &ManagedChange) -> Result<Config, ManageError> {
    let mut next = current.clone();
    let default_id = ProviderId::of(crate::providers::selection::default_provider(current));
    match change {
        ManagedChange::MigrateRetired => {
            next = super::lifecycle::candidate(current).map_err(|error| ManageError::Invalid(error.to_string()))?;
        }
        ManagedChange::SetModelSettings { model, settings } => {
            settings
                .validate(model)
                .map_err(|error| ManageError::Invalid(error.to_string()))?;
            next.model_settings.insert(model.clone(), settings.clone());
        }
        ManagedChange::AddProvider { provider } => {
            let (provider, id) = provider_arg(provider)?;
            let configured = id == default_id
                || next
                    .reliability
                    .fallback_providers
                    .iter()
                    .any(|member| ProviderId::of(member) == id);
            if configured {
                return Err(ManageError::AlreadyExists(format!("provider '{}'", shown(&provider))));
            }
            next.reliability.fallback_providers.push(provider);
        }
        ManagedChange::AddProviderWithModel { provider, model } => {
            let (provider, _) = provider_arg(provider)?;
            if !is_configured_provider(current, &provider) {
                next = candidate(current, &ManagedChange::AddProvider { provider })?;
            }
            let source = current
                .default_model
                .as_deref()
                .filter(|model| !model.trim().is_empty())
                .ok_or_else(|| {
                    ManageError::Invalid("select an explicit default model before adding model fallbacks".into())
                })?;
            let model = model.trim();
            if model.is_empty() || model == source || model.chars().any(char::is_control) {
                return Err(ManageError::Invalid(
                    "a fallback model must be nonempty, distinct from the default and contain no control characters"
                        .into(),
                ));
            }
            let models = next.reliability.model_fallbacks.entry(source.to_string()).or_default();
            if !models.iter().any(|existing| existing == model) {
                models.push(model.to_string());
            }
        }
        ManagedChange::SetProviderEndpoint { provider, api_url } => {
            let (provider, id) = provider_arg(provider)?;
            if id != default_id {
                return Err(ManageError::Invalid(format!(
                    "`api_url` is the endpoint of the default provider only, and '{}' is not the default; a \
                     provider with its own endpoint is named `custom:<url>`",
                    shown(&provider)
                )));
            }
            next.api_url = match api_url.as_deref().map(str::trim) {
                Some("") => return Err(ManageError::Invalid("the endpoint URL must not be empty".to_string())),
                Some(url) => Some(url.to_string()),
                None => None,
            };
        }
        ManagedChange::RemoveProvider { provider } => {
            let (provider, id) = provider_arg(provider)?;
            let before = (
                next.reliability.fallback_providers.len(),
                next.providers.models.len(),
                next.providers.tool_dialects.len(),
            );
            next.reliability
                .fallback_providers
                .retain(|member| ProviderId::of(member) != id);
            next.providers
                .models
                .retain(|entry| ProviderId::of(&entry.provider) != id);
            next.providers
                .tool_dialects
                .retain(|entry| ProviderId::of(&entry.provider) != id);
            let removed_anything = before
                != (
                    next.reliability.fallback_providers.len(),
                    next.providers.models.len(),
                    next.providers.tool_dialects.len(),
                );
            let referrers = uses_of(&next, &id);
            if !referrers.is_empty() {
                return Err(ManageError::Referenced {
                    object: format!("provider '{}'", shown(&provider)),
                    referrers,
                });
            }
            if !removed_anything {
                return Err(ManageError::NotFound(format!(
                    "configured provider '{}'",
                    shown(&provider)
                )));
            }
        }
        ManagedChange::AddModel(declaration) => {
            let declaration = normalized_declaration(declaration)?;
            let id = ProviderId::of(&declaration.provider);
            if next.providers.models.iter().any(|entry| {
                ProviderId::of(&entry.provider) == id
                    && same_model(entry.model.as_deref(), declaration.model.as_deref())
            }) {
                return Err(ManageError::AlreadyExists(declaration_name(
                    &declaration.provider,
                    declaration.model.as_deref(),
                )));
            }
            if !declaration.declares_anything() {
                return Err(ManageError::Invalid(
                    "a declaration must declare at least one fact (operations, a feature, a dialect or the \
                     context window)"
                        .to_string(),
                ));
            }
            next.providers.models.push(declaration);
        }
        ManagedChange::UpdateModel(update) => {
            let update = normalized_declaration(update)?;
            let id = ProviderId::of(&update.provider);
            let entry = next
                .providers
                .models
                .iter_mut()
                .find(|entry| {
                    ProviderId::of(&entry.provider) == id && same_model(entry.model.as_deref(), update.model.as_deref())
                })
                .ok_or_else(|| ManageError::NotFound(declaration_name(&update.provider, update.model.as_deref())))?;
            if !update.declares_anything() {
                return Err(ManageError::Invalid(
                    "the update declares nothing to change".to_string(),
                ));
            }
            merge_declaration(entry, update);
        }
        ManagedChange::RemoveModel { provider, model } => {
            let (provider, id) = provider_arg(provider)?;
            let model = model.as_deref().map(str::trim).filter(|model| !model.is_empty());
            let before = next.providers.models.len();
            next.providers
                .models
                .retain(|entry| !(ProviderId::of(&entry.provider) == id && same_model(entry.model.as_deref(), model)));
            if next.providers.models.len() == before {
                return Err(ManageError::NotFound(declaration_name(&provider, model)));
            }
        }
        ManagedChange::SetDefault {
            settings,
            operation: SelectionOperation::Chat,
            provider,
            model,
            root_credential,
        } => {
            let (provider, id) = provider_arg(provider)?;
            let model = non_empty(model, "the model")?;
            if id != default_id && root_holds_material(&next) {
                match root_credential {
                    RootCredential::Refuse => {
                        return Err(ManageError::Invalid(format!(
                            "the root connection (`api_key`, `api_url`, `reliability.api_keys`) belongs to the \
                             current default provider '{}'; switching the default to '{}' would hand it over. \
                             Say which: keep it for the new provider, or clear it",
                            shown(crate::providers::selection::default_provider(current)),
                            shown(&provider)
                        )));
                    }
                    RootCredential::Keep => {}
                    RootCredential::Clear => {
                        next.api_key = None;
                        next.api_url = None;
                        next.reliability.api_keys.clear();
                    }
                }
            }
            if let Some(settings) = settings {
                let release = crate::providers::catalog::ReleaseCatalog::embedded_ref()
                    .map_err(|error| ManageError::Invalid(error.to_string()))?;
                let row = release
                    .model(&id, &model)
                    .ok_or_else(|| ManageError::Invalid("model settings require a release catalog model".into()))?;
                settings
                    .validate(&row.id)
                    .map_err(|error| ManageError::Invalid(error.to_string()))?;
                next.model_settings.insert(row.id.clone(), settings.clone());
            }
            next.default_provider = Some(provider);
            next.default_model = Some(model);
        }
        ManagedChange::SetDefault {
            settings,
            operation: SelectionOperation::Embedding,
            provider,
            model,
            root_credential,
        } => {
            if settings.is_some() {
                return Err(ManageError::Invalid("model levels apply to chat defaults only".into()));
            }
            // The embedder's root credential is owner-bound on its own; the
            // chat root-connection choice means nothing here (D-085 F-9).
            if *root_credential != RootCredential::Refuse {
                return Err(ManageError::Invalid(
                    "keeping or clearing the root connection applies to the chat default only".to_string(),
                ));
            }
            let provider = provider.trim();
            let provider = if crate::providers::selection::is_prx_embedder(provider) {
                provider.to_ascii_lowercase()
            } else {
                provider_arg(provider)?.0
            };
            next.memory.embedding_provider = provider;
            next.memory.embedding_provider_explicit = true;
            next.memory.embedding_model = non_empty(model, "the model")?;
        }
        ManagedChange::AddRoute {
            operation,
            hint,
            provider,
            model,
        } => {
            let hint = hint_arg(hint)?;
            let (provider, _) = provider_arg(provider)?;
            let model = non_empty(model, "the model")?;
            let exists = match operation {
                SelectionOperation::Chat => next.model_routes.iter().any(|route| route.hint == hint),
                SelectionOperation::Embedding => next.embedding_routes.iter().any(|route| route.hint == hint),
            };
            if exists {
                return Err(ManageError::AlreadyExists(format!(
                    "{} `hint:{hint}`",
                    route_table(*operation)
                )));
            }
            match operation {
                SelectionOperation::Chat => next.model_routes.push(ModelRouteConfig {
                    settings: Default::default(),
                    hint,
                    provider,
                    model,
                    api_key: None,
                    credential_ref: None,
                }),
                SelectionOperation::Embedding => next.embedding_routes.push(EmbeddingRouteConfig {
                    hint,
                    provider,
                    model,
                    dimensions: None,
                    api_key: None,
                    credential_ref: None,
                }),
            }
        }
        ManagedChange::UpdateRoute {
            operation,
            hint,
            provider,
            model,
        } => {
            let hint = hint_arg(hint)?;
            let provider = provider
                .as_deref()
                .map(provider_arg)
                .transpose()?
                .map(|(provider, _)| provider);
            let model = model
                .as_deref()
                .map(|model| non_empty(model, "the model"))
                .transpose()?;
            if provider.is_none() && model.is_none() {
                return Err(ManageError::Invalid("name a new provider or a new model".to_string()));
            }
            let missing = || ManageError::NotFound(format!("{} `hint:{hint}`", route_table(*operation)));
            let (route_provider, route_model) = match operation {
                SelectionOperation::Chat => next
                    .model_routes
                    .iter_mut()
                    .find(|route| route.hint == hint)
                    .map(|route| (&mut route.provider, &mut route.model))
                    .ok_or_else(missing)?,
                SelectionOperation::Embedding => next
                    .embedding_routes
                    .iter_mut()
                    .find(|route| route.hint == hint)
                    .map(|route| (&mut route.provider, &mut route.model))
                    .ok_or_else(missing)?,
            };
            if let Some(provider) = provider {
                *route_provider = provider;
            }
            if let Some(model) = model {
                *route_model = model;
            }
        }
        ManagedChange::RemoveRoute { operation, hint } => {
            let hint = hint_arg(hint)?;
            let removed = match operation {
                SelectionOperation::Chat => {
                    let before = next.model_routes.len();
                    next.model_routes.retain(|route| route.hint != hint);
                    before != next.model_routes.len()
                }
                SelectionOperation::Embedding => {
                    let before = next.embedding_routes.len();
                    next.embedding_routes.retain(|route| route.hint != hint);
                    before != next.embedding_routes.len()
                }
            };
            if !removed {
                return Err(ManageError::NotFound(format!(
                    "{} `hint:{hint}`",
                    route_table(*operation)
                )));
            }
        }
        ManagedChange::BindAgent { agent, provider, model } => {
            let name = non_empty(agent, "the agent name")?;
            let (provider, _) = provider_arg(provider)?;
            let model = non_empty(model, "the model")?;
            let entry = next
                .agents
                .get_mut(&name)
                .ok_or_else(|| ManageError::NotFound(format!("agent `{name}`")))?;
            entry.provider = provider;
            entry.model = model;
        }
    }
    Ok(next)
}

/// A declaration with a constructible provider and a trimmed model.
fn normalized_declaration(declaration: &ProviderModelConfig) -> Result<ProviderModelConfig, ManageError> {
    let (provider, _) = provider_arg(&declaration.provider)?;
    let mut declaration = declaration.clone();
    declaration.provider = provider;
    declaration.model = declaration
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);
    Ok(declaration)
}

/// Every field `update` declares replaces the stored one.
fn merge_declaration(entry: &mut ProviderModelConfig, update: ProviderModelConfig) {
    let ProviderModelConfig {
        provider: _,
        model: _,
        operations,
        native_tools,
        vision_input,
        streaming,
        structured_output,
        tool_schema_dialect,
        tool_call_dialect,
        context_window,
    } = update;
    if operations.is_some() {
        entry.operations = operations;
    }
    if native_tools.is_some() {
        entry.native_tools = native_tools;
    }
    if vision_input.is_some() {
        entry.vision_input = vision_input;
    }
    if streaming.is_some() {
        entry.streaming = streaming;
    }
    if structured_output.is_some() {
        entry.structured_output = structured_output;
    }
    if tool_schema_dialect.is_some() {
        entry.tool_schema_dialect = tool_schema_dialect;
    }
    if tool_call_dialect.is_some() {
        entry.tool_call_dialect = tool_call_dialect;
    }
    if context_window.is_some() {
        entry.context_window = context_window;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DelegateAgentConfig;

    /// A credential-shaped sentinel. Not a real secret.
    const SECRET: &str = "pmme-service-sentinel";

    async fn loaded(dir: &std::path::Path, edit: impl FnOnce(&mut Config)) -> Config {
        let mut config = Config {
            config_path: dir.join("config.toml"),
            workspace_dir: dir.join("workspace"),
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o".into()),
            ..Config::default()
        };
        edit(&mut config);
        config.save().await.expect("test: save");
        Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load")
    }

    fn service_for(config: Config) -> ManagementService {
        ManagementService::new(crate::config::new_shared(config))
    }

    fn route(hint: &str, provider: &str, model: &str) -> ModelRouteConfig {
        ModelRouteConfig {
            settings: Default::default(),
            hint: hint.into(),
            provider: provider.into(),
            model: model.into(),
            api_key: None,
            credential_ref: None,
        }
    }

    /// D-094 C-3: the decision backend is a use of its provider (a removal
    /// is refused naming it), and a reference it names is reported as used
    /// by it; any other reference is not used.
    #[tokio::test]
    async fn the_decision_backend_uses_its_provider_and_its_reference() {
        const ENDPOINT: &str = "http://127.0.0.1:8900/v1";
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded(dir.path(), |config| {
            config.causal_tree.enabled = true;
            config.causal_tree.backend.kind =
                crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
            config.causal_tree.backend.endpoint = ENDPOINT.into();
            config.causal_tree.backend.model = "decider-1".into();
            config.causal_tree.backend.credential_ref =
                crate::causal_tree::config::CredentialRefSetting::Profile("cred-decision-used".into());
            config.providers.models.push(
                serde_json::from_value(serde_json::json!({
                    "provider": format!("custom:{ENDPOINT}"),
                    "model": "decider-1",
                    "operations": ["decision_endpoint"],
                }))
                .expect("test: declaration"),
            );
        })
        .await;
        assert_eq!(
            reference_use(&config, "cred-decision-used").expect("test: use"),
            "configured for the enabled decision backend (causal_tree.backend.credential_ref)"
        );
        assert_eq!(
            reference_use(&config, "cred-spare").expect("test: use"),
            REFERENCE_NOT_USED
        );
        let error = service_for(config)
            .plan(
                &ManagedChange::RemoveProvider {
                    provider: format!("custom:{ENDPOINT}"),
                },
                ConfigActor::Cli,
            )
            .await
            .expect_err("test: the decision backend's provider is not removed");
        let ManageError::Referenced { referrers, .. } = &error else {
            panic!("expected Referenced, got {error}");
        };
        assert!(
            referrers.contains(&"the decision backend (causal_tree.backend)".to_string()),
            "{referrers:?}"
        );
    }

    /// D-078 carried: a provider a selection still uses is not removed; the
    /// refusal names every referrer and nothing is written.
    #[tokio::test]
    async fn removing_a_provider_a_selection_uses_is_refused_naming_the_referrers() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded(dir.path(), |config| {
            config.reliability.fallback_providers = vec!["openrouter".into()];
            config.model_routes = vec![route("fast", "openrouter", "openai/gpt-4o-mini")];
            config.agents.insert(
                "coder".into(),
                serde_json::from_value::<DelegateAgentConfig>(serde_json::json!({
                    "provider": "OpenRouter",
                    "model": "openai/gpt-4o",
                }))
                .expect("test: agent"),
            );
        })
        .await;
        let before = std::fs::read(&config.config_path).expect("test: read");
        let service = service_for(config);

        let error = service
            .plan(
                &ManagedChange::RemoveProvider {
                    provider: "openrouter".into(),
                },
                ConfigActor::Cli,
            )
            .await
            .expect_err("test: a used provider is not removed");
        let ManageError::Referenced { object, referrers } = &error else {
            panic!("expected Referenced, got {error}");
        };
        assert_eq!(object, "provider 'openrouter'");
        assert!(
            referrers.contains(&"model route `hint:fast`".to_string()),
            "{referrers:?}"
        );
        assert!(referrers.contains(&"agent `coder`".to_string()), "{referrers:?}");
        assert!(error.to_string().contains("nothing was written"), "{error}");
        assert_eq!(
            std::fs::read(&service.config().config_path).expect("test: read"),
            before
        );

        // The default provider is referenced by the default model.
        let error = service
            .plan(
                &ManagedChange::RemoveProvider {
                    provider: "openai".into(),
                },
                ConfigActor::Cli,
            )
            .await
            .expect_err("test: the default provider is not removed");
        assert!(
            matches!(&error, ManageError::Referenced { referrers, .. } if referrers.contains(&"default model".to_string())),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_unused_fallback_provider_is_removed_with_its_declarations() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded(dir.path(), |config| {
            config.reliability.fallback_providers = vec!["openrouter".into()];
        })
        .await;
        let service = service_for(config);
        let plan = service
            .plan(
                &ManagedChange::RemoveProvider {
                    provider: "openrouter".into(),
                },
                ConfigActor::Cli,
            )
            .await
            .expect("test: plan");
        assert!(
            plan.text()
                .contains("  - reliability.fallback_providers[0] (was \"openrouter\")\n"),
            "{}",
            plan.text()
        );
        let outcome = service.apply(plan, ConfigActor::Cli, None).await.expect("test: apply");
        assert!(outcome.record_id.is_some());
        let stored = std::fs::read_to_string(&service.config().config_path).expect("test: read");
        assert!(!stored.contains("openrouter"), "{stored}");
    }

    /// D-078 carried: a credential reference the stored configuration still
    /// names (in any section, modelled or not) is not removed.
    #[tokio::test]
    async fn removing_a_credential_the_configuration_names_is_refused() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded(dir.path(), |_| {}).await;
        let references = CredentialReferences::for_config(&config).expect("test: references");
        references.set("openai", "route-key", SECRET).expect("test: set");
        references.set("openai", "spare", SECRET).expect("test: set");
        // A section that names the reference (a hosted decision backend, or a
        // route once routes take references).
        let mut stored = std::fs::read_to_string(&config.config_path).expect("test: read");
        stored.push_str("\n[decision.backends.hosted]\ncredential_ref = \"cred-route-key\"\n");
        std::fs::write(&config.config_path, stored).expect("test: write");
        let service = service_for(config);

        let error = service
            .remove_credential("cred-route-key")
            .await
            .expect_err("test: a named reference is not removed");
        let ManageError::Referenced { object, referrers } = &error else {
            panic!("expected Referenced, got {error}");
        };
        assert_eq!(object, "credential reference 'cred-route-key'");
        assert_eq!(referrers, &["decision.backends.hosted.credential_ref"]);
        assert!(!error.to_string().contains(SECRET));
        let references = service.credentials().expect("test: references");
        assert!(references.status("cred-route-key").expect("test: status").is_some());

        assert!(service.remove_credential("cred-spare").await.expect("test: remove"));
        assert!(references.status("cred-spare").expect("test: status").is_none());
    }

    /// PMM-H: a model route, an embedding route and an agent that name a
    /// reference in `credential_ref` are its referrers, so the reference is
    /// not removed under them, and each is listed.
    #[tokio::test]
    async fn every_credential_ref_holder_is_a_referrer() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let seed = loaded(dir.path(), |_| {}).await;
        CredentialReferences::for_config(&seed)
            .expect("test: references")
            .set("openai", "shared", SECRET)
            .expect("test: set");
        let config = loaded(dir.path(), |config| {
            config.causal_tree.enabled = false;
            config.model_routes.push(ModelRouteConfig {
                credential_ref: Some("cred-shared".into()),
                ..route("fast", "openai", "gpt-4o-mini")
            });
            config.embedding_routes.push(EmbeddingRouteConfig {
                hint: "semantic".into(),
                provider: "openai".into(),
                model: "text-embedding-3-small".into(),
                dimensions: None,
                api_key: None,
                credential_ref: Some("cred-shared".into()),
            });
            config.agents.insert(
                "coder".into(),
                serde_json::from_value(serde_json::json!({
                    "provider": "openai",
                    "model": "gpt-4o",
                    "credential_ref": "cred-shared",
                }))
                .expect("test: agent"),
            );
        })
        .await;
        let service = service_for(config);

        let error = service
            .remove_credential("cred-shared")
            .await
            .expect_err("test: a reference in use is not removed");
        let ManageError::Referenced { referrers, .. } = &error else {
            panic!("expected Referenced, got {error}");
        };
        let mut referrers = referrers.clone();
        referrers.sort();
        assert_eq!(
            referrers,
            [
                "agents.coder.credential_ref",
                "embedding_routes[0].credential_ref",
                "model_routes[0].credential_ref"
            ]
        );
        let credentials = provider_credentials(&service.config(), "openai").expect("test: status");
        assert_eq!(
            credentials.reference_uses.get("cred-shared").map(Vec::len),
            Some(3),
            "{credentials:?}"
        );
        let mut combined = (*service.config()).clone();
        combined.causal_tree.backend.kind =
            crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
        combined.causal_tree.backend.endpoint = "http://127.0.0.1:8900/v1".into();
        combined.causal_tree.backend.model = "decider-1".into();
        combined.causal_tree.backend.credential_ref =
            crate::causal_tree::config::CredentialRefSetting::Profile("cred-shared".into());
        let disabled = reference_use(&combined, "cred-shared").expect("test: disabled status");
        assert!(disabled.contains("causal_tree.backend.credential_ref"), "{disabled}");
        assert!(
            !disabled.contains("configured for the enabled decision backend"),
            "{disabled}"
        );
        combined.causal_tree.enabled = true;
        let active = reference_use(&combined, "cred-shared").expect("test: active status");
        assert!(
            active.contains("configured for the enabled decision backend"),
            "{active}"
        );
        assert!(active.contains("model_routes[0].credential_ref"), "{active}");
        assert!(active.contains("embedding_routes[0].credential_ref"), "{active}");
        assert!(active.contains("agents.coder.credential_ref"), "{active}");
        combined.causal_tree.backend.endpoint = "https://example.com/v1".into();
        let remote_disabled = reference_use(&combined, "cred-shared").expect("test: remote disabled status");
        assert!(
            !remote_disabled.contains("configured for the enabled decision backend"),
            "{remote_disabled}"
        );
    }

    /// D-078 carried, PMM-H: a rollback that would restore a route naming a
    /// credential reference removed since is refused, naming the route and
    /// the reference, with nothing written; a change naming a reference of
    /// another provider is refused the same way.
    #[tokio::test]
    async fn a_rollback_to_a_missing_credential_reference_is_refused() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let seed = loaded(dir.path(), |_| {}).await;
        let references = CredentialReferences::for_config(&seed).expect("test: references");
        references.set("openai", "route-key", SECRET).expect("test: set");
        references.set("anthropic", "other", SECRET).expect("test: set");
        let config = loaded(dir.path(), |config| {
            config.model_routes.push(ModelRouteConfig {
                credential_ref: Some("cred-route-key".into()),
                ..route("fast", "openai", "gpt-4o-mini")
            });
        })
        .await;
        let service = service_for(config);

        let plan = service
            .plan(
                &ManagedChange::RemoveRoute {
                    operation: SelectionOperation::Chat,
                    hint: "fast".into(),
                },
                ConfigActor::Cli,
            )
            .await
            .expect("test: plan the removal");
        let record = service
            .apply(plan, ConfigActor::Cli, None)
            .await
            .expect("test: remove the route")
            .record_id
            .expect("test: recorded");
        assert!(service.remove_credential("cred-route-key").await.expect("test: remove"));
        let before = std::fs::read(&service.config().config_path).expect("test: read");

        let current =
            crate::config::files::current_config_revision(&service.config().config_path).expect("test: revision");
        let error = service
            .transactions()
            .rollback(super::super::RollbackRequest {
                record_id: &record,
                expected_revision: &current,
                confirm: true,
                restore_credentials: true,
                disable_encryption: false,
            })
            .await
            .expect_err("test: the reference is gone");
        let text = error.to_string();
        assert!(
            text.contains("model route `hint:fast`")
                && text.contains("cred-route-key")
                && text.contains("does not exist"),
            "{text}"
        );
        assert_eq!(
            std::fs::read(&service.config().config_path).expect("test: read"),
            before
        );

        // A route naming another provider's reference is refused by the
        // same check, before anything is written.
        let mut candidate = service.config().as_ref().clone();
        candidate.model_routes.push(ModelRouteConfig {
            credential_ref: Some("cred-other".into()),
            ..route("stolen", "openai", "gpt-4o")
        });
        let plan = service
            .transactions()
            .plan(ConfigChange::Structured(Box::new(candidate)))
            .await
            .expect("test: the candidate stages");
        let error = service
            .transactions()
            .check_selections(&plan, ConfigActor::Cli)
            .await
            .expect_err("test: another provider's reference is refused");
        assert!(error.to_string().contains("belongs to provider"), "{error}");
        assert_eq!(
            std::fs::read(&service.config().config_path).expect("test: read"),
            before
        );
    }

    /// RED before PMM-H (D-089 11): replacing a credential reference's value
    /// left no trace in the configuration history. It is now recorded, by
    /// reference id only; creating a new reference is not.
    #[tokio::test]
    async fn replacing_a_credential_reference_is_recorded_in_history() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let service = service_for(loaded(dir.path(), |_| {}).await);
        let history = service.transactions().history().expect("test: history");

        service
            .set_credential("openai".into(), "route-key".into(), SECRET.into(), ConfigActor::Cli)
            .await
            .expect("test: create");
        assert!(
            history.list().expect("test: list").is_empty(),
            "creating is not recorded"
        );

        service
            .set_credential(
                "openai".into(),
                "route-key".into(),
                "pmmh-rotated-sentinel".into(),
                ConfigActor::GatewayManagement,
            )
            .await
            .expect("test: replace");
        let records = history.list().expect("test: list");
        assert_eq!(records.len(), 1, "{records:?}");
        let record = records.first().expect("test: the record");
        assert_eq!(record.status, super::super::history::RecordStatus::CredentialReplaced);
        assert_eq!(record.actor, ConfigActor::GatewayManagement);
        assert_eq!(
            record.summary.changes.first().map(|change| change.path.as_str()),
            Some("credential_references.cred-route-key")
        );
        assert_eq!(record.summary.risk, super::super::plan::ConfigChangeRisk::High);
        let shown = serde_json::to_string(record).expect("test: json");
        assert!(
            !shown.contains(SECRET) && !shown.contains("pmmh-rotated-sentinel"),
            "{shown}"
        );
    }

    /// The root connection belongs to the default provider: switching the
    /// default never hands it over implicitly.
    #[tokio::test]
    async fn switching_the_default_provider_never_hands_the_root_key_over_implicitly() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded(dir.path(), |config| config.api_key = Some(SECRET.into())).await;
        let service = service_for(config);
        let switch = |root_credential| ManagedChange::SetDefault {
            settings: None,
            operation: SelectionOperation::Chat,
            provider: "anthropic".into(),
            model: "claude-sonnet-4-5".into(),
            root_credential,
        };
        let error = service
            .plan(&switch(RootCredential::Refuse), ConfigActor::Cli)
            .await
            .expect_err("test: refused");
        assert!(
            error
                .to_string()
                .contains("belongs to the current default provider 'openai'"),
            "{error}"
        );
        assert!(!error.to_string().contains(SECRET));

        // Keeping the root connection for the new provider is the operator's
        // explicit choice, and still a high-risk change (D-085 F-3).
        let kept = service
            .plan(&switch(RootCredential::Keep), ConfigActor::Cli)
            .await
            .expect("test: plan");
        assert!(
            kept.summary()
                .risk_reasons
                .contains(&super::super::plan::RiskReason::CredentialOwnerChanged {
                    path: "default_provider".into()
                }),
            "{}",
            kept.text()
        );
        let plan = service
            .plan(&switch(RootCredential::Clear), ConfigActor::Cli)
            .await
            .expect("test: plan");
        let text = plan.text();
        assert!(text.contains("  - api_key (was \"***\")\n"), "{text}");
        assert!(
            text.contains("  ~ default_provider: \"openai\" -> \"anthropic\"\n"),
            "{text}"
        );
        assert!(!text.contains(SECRET), "{text}");
        assert_eq!(plan.text(), render_plan(plan.summary(), &plan.warnings));
    }

    #[tokio::test]
    async fn routes_and_declarations_are_added_updated_and_removed_by_key() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded(dir.path(), |config| {
            config.model_routes = vec![route("fast", "openai", "gpt-4o-mini")];
        })
        .await;
        let service = service_for(config.clone());
        let duplicate = service
            .plan(
                &ManagedChange::AddRoute {
                    operation: SelectionOperation::Chat,
                    hint: "hint:fast".into(),
                    provider: "openai".into(),
                    model: "gpt-4o".into(),
                },
                ConfigActor::Cli,
            )
            .await
            .expect_err("test: duplicate");
        assert!(
            duplicate
                .to_string()
                .starts_with("model route `hint:fast` already exists"),
            "{duplicate}"
        );
        let Err(missing) = candidate(
            &config,
            &ManagedChange::UpdateRoute {
                operation: SelectionOperation::Embedding,
                hint: "fast".into(),
                provider: None,
                model: Some("text-embedding-3-small".into()),
            },
        ) else {
            panic!("test: an embedding route that does not exist is not updated");
        };
        assert!(matches!(missing, ManageError::NotFound(_)), "{missing}");

        let declared = candidate(
            &config,
            &ManagedChange::AddModel(ProviderModelConfig {
                provider: "custom:http://127.0.0.1:18086/v1".into(),
                model: Some("local-model".into()),
                operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
                native_tools: Some(true),
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: None,
                tool_call_dialect: None,
                context_window: None,
            }),
        )
        .unwrap_or_else(|error| panic!("test: add model: {error}"));
        let updated = candidate(
            &declared,
            &ManagedChange::UpdateModel(ProviderModelConfig {
                provider: "CUSTOM:http://127.0.0.1:18086/v1/".into(),
                model: Some("LOCAL-MODEL".into()),
                operations: None,
                native_tools: Some(false),
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: None,
                tool_call_dialect: None,
                context_window: Some(32_768),
            }),
        )
        .unwrap_or_else(|error| panic!("test: update model: {error}"));
        let entry = updated.providers.models.first().expect("test: the declaration");
        assert_eq!(entry.native_tools, Some(false));
        assert_eq!(entry.context_window, Some(32_768));
        assert!(entry.operations.is_some(), "an undeclared field is kept");
        let removed = candidate(
            &updated,
            &ManagedChange::RemoveModel {
                provider: "custom:http://127.0.0.1:18086/v1".into(),
                model: Some("local-model".into()),
            },
        )
        .unwrap_or_else(|error| panic!("test: remove model: {error}"));
        assert!(removed.providers.models.is_empty(), "expected an empty value");
    }

    /// D-085 F-6 (new read model): every known model — configured
    /// selections, declarations and the catalog cache — with the resolver's
    /// evidence, filtered by operation and by proven executability.
    #[tokio::test]
    async fn the_model_catalog_lists_known_models_by_operation_and_evidence() {
        use crate::providers::catalog::ModelOperation;
        use crate::providers::catalog::discovery::{DiscoveredModel, record_discovered};
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded(dir.path(), |config| {
            config.providers.models.push(
                toml::from_str(
                    "provider = \"openrouter\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
                )
                .expect("test: declaration"),
            );
            config.providers.models.push(
                toml::from_str("provider = \"openai\"\nmodel = \"gpt-4o\"\noperations = [\"chat\"]\n")
                    .expect("test: declaration"),
            );
        })
        .await;
        let mut embedder = DiscoveredModel::id_only("text-embedding-3-small");
        embedder.output_modalities = Some(vec!["embeddings".into()]);
        record_discovered(
            &config.workspace_dir,
            "openai",
            &[embedder, DiscoveredModel::id_only("gpt-4o-mini")],
            1_700_000_000,
        )
        .expect("test: cache");

        let all = model_catalog(&config, &ModelFilter::default());
        let names = |entries: &[ModelEntry]| {
            entries
                .iter()
                .map(|entry| format!("{}/{}", entry.provider, entry.model))
                .collect::<Vec<_>>()
        };
        let listed = names(&all);
        for expected in ["openai/gpt-4o", "openai/gpt-4o-mini", "openrouter/jev-decision-test"] {
            assert!(listed.contains(&expected.to_string()), "{expected}: {listed:?}");
        }

        let chat = model_catalog(
            &config,
            &ModelFilter {
                operation: Some(ModelOperation::Chat),
                ..ModelFilter::default()
            },
        );
        assert!(
            !names(&chat).contains(&"openrouter/jev-decision-test".to_string()),
            "a decision-only model is ruled out for chat: {:?}",
            names(&chat)
        );
        let executable = model_catalog(
            &config,
            &ModelFilter {
                operation: Some(ModelOperation::Chat),
                executable: true,
                ..ModelFilter::default()
            },
        );
        let executable_names = names(&executable);
        assert!(
            executable_names.contains(&"openai/gpt-4o".to_string()),
            "the declared chat model remains executable: {executable_names:?}"
        );
        assert!(
            executable_names.contains(&"openai/gpt-5.6".to_string()),
            "the embedded release catalog supplies executable chat models: {executable_names:?}"
        );
        assert!(
            !executable_names.contains(&"openrouter/jev-decision-test".to_string()),
            "a declared decision-only model remains excluded: {executable_names:?}"
        );
        assert!(
            executable.iter().all(|entry| {
                entry
                    .operations
                    .get("chat")
                    .is_some_and(|support| support == "supported")
            }),
            "every executable row must carry positive chat evidence: {executable:?}"
        );
        let release_entry = executable
            .iter()
            .find(|entry| entry.provider == "openai" && entry.model == "gpt-5.6")
            .expect("test: embedded catalog model is listed");
        assert_eq!(release_entry.source, "embedded release catalog");

        let shown = model_entry(&config, "openrouter", "jev-decision-test").expect("test: show");
        // The declaration lists the decision endpoint only, and OpenRouter's
        // decision adapter executes it (CDP-02): chat is negative evidence,
        // the decision endpoint positive.
        assert!(
            shown
                .operations
                .get("decision_endpoint")
                .is_some_and(|support| support.starts_with("supported")),
            "{:?}",
            shown.operations
        );
        assert!(
            shown
                .operations
                .get("chat")
                .is_some_and(|support| support.starts_with("unsupported"))
        );
        assert_eq!(shown.source, "operator declaration");
        assert!(parse_operation("teleport").is_err());
        assert_eq!(
            parse_operation("EMBEDDING").expect("test: parse"),
            ModelOperation::Embedding
        );
    }

    #[test]
    fn provider_text_redaction_masks_query_values_and_userinfo() {
        assert_eq!(
            redact_provider_text("custom:https://h.example/v1?region=eu&api_key=abc-1"),
            "custom:https://h.example/v1?region=[redacted]&api_key=[redacted]"
        );
        assert_eq!(
            redact_provider_text("fallback provider 'custom:http://h/v1?t=xyz' refused: what?"),
            "fallback provider 'custom:http://h/v1?t=[redacted]' refused: what?"
        );
        assert!(!redact_provider_text("custom:https://u:pw-sentinel@h/v1").contains("pw-sentinel"));
        assert_eq!(redact_provider_text("openai"), "openai");
    }

    /// RED before PMME-g (D-088 C-2): the guidance for the root connection
    /// named `prx onboard --from-stdin`, which regenerates the whole
    /// configuration outside the transaction.
    #[tokio::test]
    async fn the_root_connection_guidance_names_a_non_destructive_path() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded(dir.path(), |config| config.api_key = Some(SECRET.into())).await;
        let credentials = provider_credentials(&config, "openai").expect("test: status");
        assert_eq!(credentials.in_use, CredentialInUse::RootConnection);
        assert!(
            !credentials.guidance.contains("prx onboard"),
            "{}",
            credentials.guidance
        );
        assert!(
            credentials.guidance.contains("PUT /api/config/secrets"),
            "{}",
            credentials.guidance
        );
        assert!(!credentials.guidance.contains(SECRET));
    }

    /// D-085 F-9: keeping or clearing the root connection means nothing for
    /// the embedding default and is refused rather than ignored.
    #[test]
    fn root_connection_flags_are_refused_for_the_embedding_default() {
        let config = Config::default();
        for root_credential in [RootCredential::Keep, RootCredential::Clear] {
            let Err(error) = candidate(
                &config,
                &ManagedChange::SetDefault {
                    settings: None,
                    operation: SelectionOperation::Embedding,
                    provider: "openai".into(),
                    model: "text-embedding-3-small".into(),
                    root_credential,
                },
            ) else {
                panic!("test: refused");
            };
            assert!(error.to_string().contains("chat default only"), "{error}");
        }
    }

    /// RED before PMME-f (D-085 F-10): a credential in a provider URL's
    /// query string came back in referrer names and messages.
    #[tokio::test]
    async fn a_query_string_credential_in_a_provider_is_never_echoed() {
        const QUERY: &str = "custom:https://llm.example/v1?api_key=sentinel-query-token-123456";
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded(dir.path(), |config| {
            config.reliability.fallback_providers = vec![QUERY.into()];
            config.model_routes = vec![route("fast", QUERY, "local-model")];
        })
        .await;
        let service = service_for(config.clone());
        let error = service
            .plan(
                &ManagedChange::RemoveProvider { provider: QUERY.into() },
                ConfigActor::Cli,
            )
            .await
            .expect_err("test: still used");
        let text = error.to_string();
        assert!(text.contains("model route `hint:fast`"), "{text}");
        assert!(!text.contains("sentinel-query-token"), "{text}");
        let listed = serde_json::to_string(&configured_providers(&config)).expect("test: json");
        assert!(!listed.contains("sentinel-query-token"), "{listed}");
    }

    #[tokio::test]
    async fn incompatible_fallback_error_names_model_fallback_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let config = loaded(dir.path(), |config| {
            config.default_provider = Some("openrouter".into());
            config.default_model = Some("anthropic/claude-sonnet-5".into());
        })
        .await;
        let service = service_for(config);
        let error = service
            .plan(
                &ManagedChange::AddProvider {
                    provider: "kimi-code".into(),
                },
                ConfigActor::Cli,
            )
            .await
            .expect_err("incompatible primary is refused");
        let shown = error.to_string();
        assert!(shown.contains("not compatible"), "{shown}");
        assert!(shown.contains("[reliability.model_fallbacks]"), "{shown}");
    }

    #[tokio::test]
    async fn selecting_unpriced_default_warns_without_changing_admission() {
        let dir = tempfile::tempdir().unwrap();
        let config = loaded(dir.path(), |_| {}).await;
        let service = service_for(config);
        let plan = service
            .plan(
                &ManagedChange::SetDefault {
                    operation: SelectionOperation::Chat,
                    provider: "kimi-code".into(),
                    model: "kimi-for-coding".into(),
                    settings: None,
                    root_credential: RootCredential::Refuse,
                },
                ConfigActor::Cli,
            )
            .await
            .unwrap();
        assert!(
            plan.warnings()
                .iter()
                .any(|warning| warning.contains(r#"[cost.prices."kimi-code/kimi-for-coding"]"#))
        );
    }

    #[test]
    fn an_unresolvable_provider_is_refused_without_echoing_it() {
        let config = Config::default();
        let Err(error) = candidate(
            &config,
            &ManagedChange::AddProvider {
                provider: SECRET.into(),
            },
        ) else {
            panic!("test: an unresolvable provider is refused");
        };
        assert!(!error.to_string().contains(SECRET), "{error}");
    }
    #[tokio::test]
    async fn model_settings_are_planned_atomically_persisted_and_revision_guarded() {
        use crate::config::model_settings::ModelSettings;
        let dir = tempfile::TempDir::new().unwrap();
        let config = loaded(dir.path(), |_| {}).await;
        let service = ManagementService::deferred(crate::config::new_shared(config.clone()));
        let settings: ModelSettings =
            serde_json::from_value(serde_json::json!({"level":"high","max_output_tokens":64000})).unwrap();
        let change = ManagedChange::SetDefault {
            operation: SelectionOperation::Chat,
            provider: "openai".into(),
            model: "gpt-5.6".into(),
            root_credential: RootCredential::Refuse,
            settings: Some(settings.clone()),
        };
        let before = std::fs::read(&config.config_path).unwrap();
        let first = service.plan(&change, ConfigActor::Cli).await.unwrap();
        let stale = service.plan(&change, ConfigActor::Cli).await.unwrap();
        assert!(first.text().contains("model_settings"));
        assert_eq!(
            std::fs::read(&config.config_path).unwrap(),
            before,
            "planning must not write"
        );
        let outcome = service.apply(first, ConfigActor::Cli, None).await.unwrap();
        assert!(outcome.record_id.is_some());
        let written = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).unwrap();
        assert_eq!(written.default_model.as_deref(), Some("gpt-5.6"));
        assert_eq!(written.model_settings.get("openai/gpt-5.6"), Some(&settings));
        let committed = std::fs::read(&config.config_path).unwrap();
        assert!(service.apply(stale, ConfigActor::Cli, None).await.is_err());
        assert_eq!(std::fs::read(&config.config_path).unwrap(), committed);
        let invalid = ManagedChange::SetModelSettings {
            model: "openai/gpt-5.6".into(),
            settings: serde_json::from_value(serde_json::json!({"max_output_tokens":0})).unwrap(),
        };
        assert!(service.plan(&invalid, ConfigActor::Cli).await.is_err());
        assert_eq!(std::fs::read(&config.config_path).unwrap(), committed);
    }
}
