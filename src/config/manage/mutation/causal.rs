//! Typed decision-plane mutations (SP-CDP-A 5.5 under D-040; CDP-12).
//!
//! The one mutation service of `[causal_tree]`: the CLI (`prx causal
//! config`), init and onboarding (CDP-14) and the Gateway (CDP-15) all build
//! their change here, so the plan and the refusal text are the same on every
//! surface. [`CausalConfigService::mutate`] is pure; [`CausalConfigService::plan`]
//! turns its result into a [`ConfigPlan`] of the transaction layer, where the
//! one decision-backend check of `config::manage::validate` runs on every
//! plan, rollbacks included.
//!
//! - `SetBackend` also declares the model for `decision_endpoint` in
//!   `[[providers.models]]`, so no hand edit is needed (CDP 13.2). It only
//!   ever adds that operation (D-094 C-2): a model-scoped entry with its own
//!   list gains it; otherwise the model-scoped entry (existing, or new)
//!   lists the model's effective operations, those the selection check's
//!   catalog supports today (a provider-wide list, discovered metadata) and
//!   those a current selection of the model uses (D-102 H-1), plus
//!   `decision_endpoint`, because `operations` is a complete list and a
//!   shorter one would take the others away. Declarations are never
//!   removed: a declaration enables nothing by itself.
//! - `ApplyCalibration` copies the thresholds a stored evaluation report
//!   proposes, with its calibration profile; the report must have been
//!   measured on the configured, pinned backend.
//! - `store_credential` stores a decision credential write-only under a new
//!   reference owned by the backend's provider, plans the reference in the
//!   same call and removes the new credential when the plan is refused
//!   (D-094 C-5): a removal that fails is part of the error. After a
//!   successful apply the previous decision credential of the same provider,
//!   now unreferenced, is removed; a removal that fails is part of the
//!   result. It is bound to the revision its caller saw (C-9): a caller's
//!   stale revision is refused before anything is stored, and without one
//!   the revision the call started from guards the apply. A credential left
//!   by a process that stopped in between is reported by
//!   [`orphaned_decision_credentials`] (`prx doctor`, and at startup).

use std::path::Path;

use super::super::credentials::{CredentialReferenceError, CredentialReferences, CredentialStatus, SecretString};
use super::super::history::ConfigActor;
use super::super::transaction::{
    ApplyOptions, ConfigChange, ConfigPlan, ConfigTransactionError, ConfigTransactionOutcome, ConfigTransactions,
};
use crate::causal_tree::config::{BackendSelection, CalibrationProfile, CausalPolicy, CredentialRefSetting};
use crate::config::Config;
use crate::config::files::ConfigRevisionConflict;
use crate::config::schema::{ModelOperationSetting, ProviderModelConfig};
use crate::decision::BackendKind;
use crate::providers::catalog::{ModelOperation, Support};

/// One change to the decision plane's configuration.
#[derive(Debug, Clone, PartialEq)]
pub enum CausalMutation {
    /// Turn the decision plane on or off.
    SetEnabled(bool),
    /// Choose the backend: its kind and, when given, its endpoint, pinned
    /// model, pinned snapshot (hosted kinds; D-043 C-8) and timeout. The
    /// model is declared for `decision_endpoint`. A kind that is not hosted
    /// clears the snapshot.
    SetBackend {
        kind: BackendKind,
        endpoint: Option<String>,
        model: Option<String>,
        pinned_snapshot: Option<String>,
        timeout_ms: Option<u32>,
    },
    /// Allow or forbid decision state leaving this machine.
    SetRemote(bool),
    /// Set the credential reference (`""`, `"none"` or `cred-<name>`).
    SetCredentialRef(CredentialRefSetting),
    /// Change explicit policy fields.
    SetPolicy(PolicyPatch),
    /// Copy the thresholds and the calibration profile a stored evaluation
    /// report proposes.
    ApplyCalibration { report_id: String },
    /// Put every policy field back to its default.
    ResetPolicyToDefaults,
}

/// Explicit policy fields; `None` keeps the current value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PolicyPatch {
    pub max_candidates: Option<u8>,
    pub max_decision_hops: Option<u8>,
    pub extra_latency_budget_ms: Option<u64>,
    pub commit_threshold: Option<f32>,
    pub minimum_choice_confidence: Option<f64>,
    pub minimum_top_margin: Option<f64>,
    pub extra_token_ratio_limit: Option<f32>,
}

/// Why a decision-plane change was refused. No message carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum CausalMutationError {
    /// The resulting configuration is not valid.
    #[error("the decision plane change is refused: {0}")]
    Invalid(String),
    /// The evaluation report cannot be applied.
    #[error("the calibration cannot be applied: {0}")]
    Calibration(String),
    /// The credential could not be stored or referenced.
    #[error(transparent)]
    Credential(#[from] CredentialReferenceError),
    /// The transaction layer refused or failed.
    #[error(transparent)]
    Transaction(#[from] ConfigTransactionError),
    /// The change was refused, and the credential stored for it could not
    /// be removed: it stays in the store, unreferenced.
    #[error(
        "{cause}; the decision credential {id} stored for it could not be removed and stays unreferenced: {removal}"
    )]
    CredentialLeft {
        cause: Box<Self>,
        id: String,
        removal: CredentialReferenceError,
    },
}

/// What [`CausalConfigService::store_credential`] did.
#[derive(Debug)]
pub struct StoredCredential {
    /// The applied transaction.
    pub outcome: ConfigTransactionOutcome,
    /// The new credential reference.
    pub id: String,
    /// The previous decision credential, removed now that nothing
    /// references it.
    pub removed_previous: Option<String>,
    /// The previous decision credential that could not be removed, and why:
    /// it stays in the store, unreferenced.
    pub previous_left: Option<(String, CredentialReferenceError)>,
}

/// The name every decision credential starts with (`cred-decision-...`).
const DECISION_CREDENTIAL_PREFIX: &str = "decision-";

/// Whether `id` names a credential [`CausalConfigService::store_credential`]
/// created.
fn is_decision_credential(id: &str) -> bool {
    id.strip_prefix(super::super::credentials::REFERENCE_PREFIX)
        .is_some_and(|name| name.starts_with(DECISION_CREDENTIAL_PREFIX))
}

/// The decision credentials of `config`'s store that its decision backend
/// does not reference: left by a process that stopped between storing one
/// and applying its reference, or by a failed removal (D-094 C-5).
///
/// # Errors
///
/// When the credential store cannot be read.
pub fn orphaned_decision_credentials(config: &Config) -> Result<Vec<CredentialStatus>, CredentialReferenceError> {
    let referenced = match &config.causal_tree.backend.credential_ref {
        CredentialRefSetting::Profile(id) => Some(id.as_str()),
        CredentialRefSetting::Unset | CredentialRefSetting::None => None,
    };
    Ok(CredentialReferences::for_config(config)?
        .list()?
        .into_iter()
        .filter(|status| is_decision_credential(&status.id) && Some(status.id.as_str()) != referenced)
        .collect())
}

/// Remove the previous decision credential `previous` when nothing in the
/// stored files of `config` names it any more: every referrer is checked,
/// not only the decision backend (D-102 H-3). `Err` names it and says why it
/// was left.
fn release_previous(
    references: &CredentialReferences,
    config: &Config,
    previous: String,
) -> Result<String, (String, CredentialReferenceError)> {
    match super::super::service::credential_referrers(config, &previous) {
        Ok(referrers) if referrers.is_empty() => match references.remove(&previous) {
            Ok(_) => Ok(previous),
            Err(error) => Err((previous, error)),
        },
        Ok(referrers) => Err((
            previous,
            CredentialReferenceError::Store(format!("it is still referenced by {}", referrers.join(", "))),
        )),
        Err(error) => Err((previous, CredentialReferenceError::Store(error.to_string()))),
    }
}

/// `cause`, after removing the credential `id` stored for the refused
/// change; a removal that fails is part of the error.
fn compensate(references: &CredentialReferences, id: String, cause: CausalMutationError) -> CausalMutationError {
    match references.remove(&id) {
        Ok(_) => {
            tracing::warn!(id = %id, "the decision credential was removed: its change was refused");
            cause
        }
        Err(removal) => CausalMutationError::CredentialLeft {
            cause: Box::new(cause),
            id,
            removal,
        },
    }
}

/// The thresholds a report proposes (the `calibration` section of an
/// evaluation report).
#[derive(Debug, serde::Deserialize)]
struct Proposal {
    profile: String,
    /// The endpoint the report measured (D-102 H-2).
    endpoint: String,
    minimum_choice_confidence: f64,
    minimum_top_margin: f64,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ProposalSection {
    Available { value: Proposal },
    Unavailable { reason: String },
}

#[derive(Debug, serde::Deserialize)]
struct StoredReport {
    report_version: u32,
    calibration: ProposalSection,
}

/// The largest evaluation report read.
const MAX_REPORT_BYTES: u64 = 16 << 20;

/// The decision-plane mutation service over one configuration's
/// transaction layer.
#[derive(Debug, Clone)]
pub struct CausalConfigService {
    transactions: ConfigTransactions,
}

impl CausalConfigService {
    #[must_use]
    pub const fn new(transactions: ConfigTransactions) -> Self {
        Self { transactions }
    }

    /// `config` with `mutations` applied in order, normalized and
    /// validated. Nothing is read but a report an `ApplyCalibration` names
    /// and, for `SetBackend`, the model metadata cache.
    ///
    /// # Errors
    ///
    /// [`CausalMutationError`] when a mutation is refused or the result is
    /// not a valid decision-plane configuration.
    pub fn mutate(config: &Config, mutations: &[CausalMutation]) -> Result<Config, CausalMutationError> {
        let mut next = config.clone();
        for mutation in mutations {
            apply_one(&mut next, mutation)?;
        }

        next.causal_tree
            .validate()
            .map_err(|error| CausalMutationError::Invalid(error.to_string()))?;
        Ok(next)
    }

    /// The plan of `mutations` over the desired configuration; nothing is
    /// written. The decision backend check runs when the plan is applied.
    ///
    /// # Errors
    ///
    /// [`CausalMutationError`] when a mutation is refused or the plan is
    /// invalid.
    pub async fn plan(&self, mutations: &[CausalMutation]) -> Result<ConfigPlan, CausalMutationError> {
        let desired = self.transactions.desired_config();
        let candidate = Self::mutate(&desired, mutations)?;
        Ok(self
            .transactions
            .plan(ConfigChange::Structured(Box::new(candidate)))
            .await?)
    }

    /// Store `secret` as a new credential reference of the configured
    /// backend's provider, and apply the configuration that references it
    /// (see the module documentation for what happens to the new and the
    /// previous credential, and the revision binding).
    ///
    /// # Errors
    ///
    /// [`CausalMutationError`] when no backend is configured, the caller's
    /// revision is stale, the credential cannot be stored, or the plan is
    /// refused or not applied ([`CausalMutationError::CredentialLeft`] when
    /// the credential stored for it could not be removed).
    pub async fn store_credential(
        &self,
        secret: SecretString,
        actor: ConfigActor,
        options: ApplyOptions<'_>,
    ) -> Result<StoredCredential, CausalMutationError> {
        let (desired, revision) = self.transactions.desired_with_revision();
        if let Some(expected) = options.expected_revision
            && revision.as_deref() != Some(expected)
        {
            return Err(ConfigTransactionError::Conflict(ConfigRevisionConflict {
                expected: expected.to_string(),
                actual: revision.unwrap_or_default(),
            })
            .into());
        }
        let Some(selection) = crate::decision::factory::configured_selection(&desired.causal_tree) else {
            return Err(CausalMutationError::Invalid(
                "no decision backend that takes a credential is configured".to_string(),
            ));
        };
        let previous = match &desired.causal_tree.backend.credential_ref {
            CredentialRefSetting::Profile(id) if is_decision_credential(id) => Some(id.clone()),
            _ => None,
        };
        let references = CredentialReferences::for_config(&desired)?;
        let name = format!(
            "{DECISION_CREDENTIAL_PREFIX}{}-{}",
            selection.kind.as_str().replace('_', "-"),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let id = references.set(&selection.provider, &name, secret.expose())?.id;
        drop(secret);
        let bound = ApplyOptions {
            expected_revision: options.expected_revision.or(revision.as_deref()),
            ..options
        };
        let applied = async {
            let plan = self
                .plan(&[CausalMutation::SetCredentialRef(CredentialRefSetting::Profile(
                    id.clone(),
                ))])
                .await?;
            Ok::<_, CausalMutationError>(self.transactions.apply(plan, actor, bound).await?)
        }
        .await;
        let outcome = match applied {
            Ok(outcome) => outcome,
            Err(cause) => return Err(compensate(&references, id, cause)),
        };
        let mut stored = StoredCredential {
            outcome,
            id,
            removed_previous: None,
            previous_left: None,
        };
        // The previous decision credential of the same provider is
        // unreferenced now: remove it, so rotations do not accumulate
        // secrets (D-094 C-5).
        if let Some(previous) = previous.filter(|previous| *previous != stored.id)
            && references.check_reference(&previous, &selection.provider).is_ok()
        {
            match release_previous(&references, &self.transactions.desired_config(), previous) {
                Ok(removed) => stored.removed_previous = Some(removed),
                Err(left) => stored.previous_left = Some(left),
            }
        }
        Ok(stored)
    }
}

/// `mutations` with the D-094 C-6 pairing: a switch to the builtin while
/// remote processing is on (in `config`, and not turned off by the
/// mutations themselves) also turns it off, first, in the same plan: the
/// decision check refuses the pair otherwise. Every surface plans through
/// it (`prx causal config`, the Gateway), so an equal change gives an equal
/// plan (V8.08).
#[must_use]
pub fn paired(config: &Config, mutations: Vec<CausalMutation>) -> Vec<CausalMutation> {
    let to_builtin = mutations.iter().any(|mutation| {
        matches!(
            mutation,
            CausalMutation::SetBackend {
                kind: BackendKind::Builtin,
                ..
            }
        )
    });
    let turns_remote = mutations
        .iter()
        .any(|mutation| matches!(mutation, CausalMutation::SetRemote(_)));
    if to_builtin && !turns_remote && config.causal_tree.backend.remote_enabled {
        std::iter::once(CausalMutation::SetRemote(false))
            .chain(mutations)
            .collect()
    } else {
        mutations
    }
}

/// Apply one mutation to `config`.
fn apply_one(config: &mut Config, mutation: &CausalMutation) -> Result<(), CausalMutationError> {
    let section = &mut config.causal_tree;
    match mutation {
        CausalMutation::SetEnabled(enabled) => section.enabled = *enabled,
        CausalMutation::SetRemote(remote) => section.backend.remote_enabled = *remote,
        CausalMutation::SetCredentialRef(reference) => section.backend.credential_ref = reference.clone(),
        CausalMutation::SetBackend {
            kind,
            endpoint,
            model,
            pinned_snapshot,
            timeout_ms,
        } => {
            section.backend.kind = BackendSelection::Fixed(*kind);
            if let Some(snapshot) = pinned_snapshot {
                section.backend.pinned_snapshot = snapshot.trim().to_string();
            } else if !matches!(kind, BackendKind::TypeSafe | BackendKind::OpenRouter) {
                section.backend.pinned_snapshot.clear();
            }
            if let Some(endpoint) = endpoint {
                section.backend.endpoint = endpoint.trim().to_string();
            }
            if let Some(model) = model {
                section.backend.model = model.trim().to_string();
            }
            if let Some(timeout_ms) = timeout_ms {
                section.backend.timeout_ms = *timeout_ms;
            }
            if let Some(selection) = crate::decision::factory::configured_selection(section) {
                let model = section.backend.model.trim().to_string();
                if let Ok(catalog) = crate::providers::catalog::ReleaseCatalog::embedded_ref()
                    && let Some(entry) = catalog.model(&crate::providers::ProviderId::of(&selection.provider), &model)
                {
                    if pinned_snapshot.is_none()
                        && matches!(kind, BackendKind::TypeSafe | BackendKind::OpenRouter)
                        && let Some(snapshot) = &entry.pinned_snapshot
                    {
                        section.backend.pinned_snapshot.clone_from(snapshot);
                    }
                    if timeout_ms.is_none()
                        && let Some(recommended) = entry.transport.recommended_timeout_ms
                        && let Ok(recommended) = u32::try_from(recommended)
                    {
                        section.backend.timeout_ms = recommended;
                    }
                    if section.policy.extra_latency_budget_ms
                        == crate::causal_tree::config::CausalPolicy::default().extra_latency_budget_ms
                        && let Some(recommended) = entry.transport.extra_latency_budget_ms
                    {
                        section.policy.extra_latency_budget_ms = recommended;
                    }
                }
                declare_decision_model(config, &selection.provider, &model);
            }
        }
        CausalMutation::SetPolicy(patch) => {
            let policy = &mut section.policy;
            if let Some(value) = patch.max_candidates {
                policy.max_candidates = value;
            }
            if let Some(value) = patch.max_decision_hops {
                policy.max_decision_hops = value;
            }
            if let Some(value) = patch.extra_latency_budget_ms {
                policy.extra_latency_budget_ms = value;
            }
            if let Some(value) = patch.commit_threshold {
                policy.commit_threshold = value;
            }
            if let Some(value) = patch.minimum_choice_confidence {
                policy.minimum_choice_confidence = value;
            }
            if let Some(value) = patch.minimum_top_margin {
                policy.minimum_top_margin = value;
            }
            if let Some(value) = patch.extra_token_ratio_limit {
                policy.extra_token_ratio_limit = value;
            }
        }
        CausalMutation::ResetPolicyToDefaults => section.policy = CausalPolicy::default(),
        CausalMutation::ApplyCalibration { report_id } => {
            let proposal = read_proposal(&config.workspace_dir, report_id)?;
            let section = &mut config.causal_tree;
            let (kind, model) =
                CalibrationProfile::measured(&proposal.profile).map_err(CausalMutationError::Calibration)?;
            let pinned = section.backend.kind == BackendSelection::Fixed(kind)
                && section.backend.model.trim() == model
                && section.backend.endpoint.trim() == proposal.endpoint.trim()
                && !crate::decision::model_pin::is_unpinned_model(&model);
            if !pinned {
                return Err(CausalMutationError::Calibration(
                    "the report was measured on another backend, endpoint or model than the configured, pinned one"
                        .to_string(),
                ));
            }
            section.policy.minimum_choice_confidence = proposal.minimum_choice_confidence;
            section.policy.minimum_top_margin = proposal.minimum_top_margin;
            // The profile names the endpoint it is applied to: another server
            // serving the same model does not inherit it (D-094 C-7).
            section.policy.calibration_profile = Some(CalibrationProfile {
                kind,
                endpoint: section.backend.endpoint.trim().to_string(),
                model,
            });
        }
    }
    Ok(())
}

/// The setting that names `operation`.
const fn operation_setting(operation: ModelOperation) -> ModelOperationSetting {
    match operation {
        ModelOperation::Chat => ModelOperationSetting::Chat,
        ModelOperation::Embedding => ModelOperationSetting::Embedding,
        ModelOperation::DecisionEndpoint => ModelOperationSetting::DecisionEndpoint,
        ModelOperation::Rerank => ModelOperationSetting::Rerank,
        ModelOperation::ImageGeneration => ModelOperationSetting::ImageGeneration,
        ModelOperation::AudioGeneration => ModelOperationSetting::AudioGeneration,
        ModelOperation::VideoGeneration => ModelOperationSetting::VideoGeneration,
    }
}

/// Declare `model` of `provider` for `decision_endpoint` (see the module
/// documentation).
fn declare_decision_model(config: &mut Config, provider: &str, model: &str) {
    let identity = crate::providers::ProviderId::of(provider);
    let descriptor = crate::providers::selection::catalog_for(config).describe(&identity, model);
    // What the catalog supports, and what a current selection of this model
    // uses even without evidence (an operator's unverified chat selection of
    // a custom model, D-102 H-1): a shorter complete list would refuse it.
    let selected = super::super::validate::operations_selected_for(config, provider, model);
    let mut effective: Vec<ModelOperationSetting> = ModelOperation::ALL
        .into_iter()
        .filter(|operation| {
            matches!(descriptor.operation(*operation), Support::Supported) || selected.contains(operation)
        })
        .map(operation_setting)
        .collect();
    if !effective.contains(&ModelOperationSetting::DecisionEndpoint) {
        effective.push(ModelOperationSetting::DecisionEndpoint);
    }
    let same_provider = |entry: &ProviderModelConfig| crate::providers::ProviderId::of(&entry.provider) == identity;
    let models = &mut config.providers.models;
    if let Some(entry) = models.iter_mut().find(|entry| {
        same_provider(entry)
            && entry
                .model
                .as_deref()
                .is_some_and(|declared| declared.trim().eq_ignore_ascii_case(model))
    }) {
        match &mut entry.operations {
            Some(operations) if operations.contains(&ModelOperationSetting::DecisionEndpoint) => {}
            Some(operations) => operations.push(ModelOperationSetting::DecisionEndpoint),
            // No list of its own: the model's operations come from elsewhere,
            // and they stay (D-094 C-2).
            None => entry.operations = Some(effective),
        }
        return;
    }
    if let Ok(entry) = serde_json::from_value::<ProviderModelConfig>(serde_json::json!({
        "provider": provider,
        "model": model,
        "operations": effective,
    })) {
        models.push(entry);
    }
}

/// The calibration proposal of the stored report `report_id`.
fn read_proposal(workspace_dir: &Path, report_id: &str) -> Result<Proposal, CausalMutationError> {
    use std::io::Read as _;
    let refused = |reason: &str| CausalMutationError::Calibration(reason.to_string());
    let id = uuid::Uuid::parse_str(report_id.trim()).map_err(|_| refused("the report id is not an id"))?;
    let path = workspace_dir
        .join("self")
        .join("causal_tree")
        .join("reports")
        .join(format!("{}.json", id.as_hyphenated()));
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(&path).map_err(|_| refused("no such report"))?;
    let mut bytes = Vec::new();
    file.take(MAX_REPORT_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|_| refused("the report cannot be read"))?;
    let report: StoredReport = serde_json::from_slice(&bytes).map_err(|_| refused("the report does not parse"))?;
    if report.report_version != crate::causal_tree::evaluate::REPORT_VERSION {
        return Err(refused("the report was written by another version"));
    }
    match report.calibration {
        ProposalSection::Available { value } => Ok(value),
        ProposalSection::Unavailable { reason } => Err(CausalMutationError::Calibration(format!(
            "the report proposes no thresholds: {reason}"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::manage::history::RecordStatus;
    use crate::config::manage::transaction::RollbackRequest;

    const LOOPBACK: &str = "http://127.0.0.1:8900/v1";

    fn loopback_backend(model: &str) -> CausalMutation {
        CausalMutation::SetBackend {
            kind: BackendKind::CompatibleHttp,
            endpoint: Some(LOOPBACK.to_string()),
            model: Some(model.to_string()),
            pinned_snapshot: None,
            timeout_ms: None,
        }
    }

    fn declarations(config: &Config) -> Vec<(String, Option<String>, Vec<ModelOperationSetting>)> {
        config
            .providers
            .models
            .iter()
            .map(|entry| {
                (
                    entry.provider.clone(),
                    entry.model.clone(),
                    entry.operations.clone().unwrap_or_default(),
                )
            })
            .collect()
    }

    /// CDP 13.2: choosing a backend declares its model for
    /// `decision_endpoint` in the same change; a provider-wide list is copied
    /// into a new model-scoped entry, and a model-scoped entry gains the
    /// operation.
    #[test]
    fn set_backend_writes_the_decision_declaration_in_the_same_plan() {
        let next = CausalConfigService::mutate(&Config::default(), &[loopback_backend("decider-1")]).unwrap();
        assert_eq!(
            declarations(&next),
            [(
                format!("custom:{LOOPBACK}"),
                Some("decider-1".to_string()),
                vec![ModelOperationSetting::DecisionEndpoint]
            )]
        );
        assert_eq!(
            next.causal_tree.backend.kind,
            BackendSelection::Fixed(BackendKind::CompatibleHttp)
        );

        let mut wide = Config::default();
        wide.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": format!("custom:{LOOPBACK}"),
                "operations": ["chat"],
            }))
            .unwrap(),
        );
        let next = CausalConfigService::mutate(&wide, &[loopback_backend("decider-1")]).unwrap();
        assert_eq!(next.providers.models.len(), 2, "the provider-wide entry stays");
        assert_eq!(
            declarations(&next)[1].2,
            [ModelOperationSetting::Chat, ModelOperationSetting::DecisionEndpoint]
        );

        let again = CausalConfigService::mutate(&next, &[loopback_backend("decider-1")]).unwrap();
        assert_eq!(
            declarations(&again),
            declarations(&next),
            "declaring twice changes nothing"
        );
        let switched = CausalConfigService::mutate(
            &again,
            &[CausalMutation::SetBackend {
                kind: BackendKind::Builtin,
                endpoint: None,
                model: None,
                pinned_snapshot: None,
                timeout_ms: None,
            }],
        )
        .unwrap();
        assert_eq!(
            declarations(&switched),
            declarations(&again),
            "a switch never removes a declaration"
        );
    }

    #[test]
    fn set_backend_applies_hosted_transport_defaults_from_release_catalog() {
        let catalog = crate::providers::catalog::ReleaseCatalog::embedded_ref().expect("release catalog");
        let entry = catalog
            .model(&crate::providers::ProviderId::of("openrouter"), "typesafe/jev-1.13")
            .expect("hosted decision entry");
        let next = CausalConfigService::mutate(
            &Config::default(),
            &[CausalMutation::SetBackend {
                kind: BackendKind::OpenRouter,
                endpoint: None,
                model: Some(entry.api_model.clone()),
                pinned_snapshot: None,
                timeout_ms: None,
            }],
        )
        .expect("catalog defaults produce a valid hosted backend");

        assert_eq!(
            next.causal_tree.backend.pinned_snapshot,
            entry.pinned_snapshot.as_deref().expect("catalog snapshot")
        );
        assert_eq!(
            u64::from(next.causal_tree.backend.timeout_ms),
            entry.transport.recommended_timeout_ms.expect("catalog timeout")
        );
        assert_eq!(
            next.causal_tree.policy.extra_latency_budget_ms,
            entry.transport.extra_latency_budget_ms.expect("catalog latency budget")
        );
        assert!(next.cost.prices.is_empty(), "built-in prices stay in the catalog");
        assert!(matches!(
            crate::llm::route_decision::usage_cost_resolution(
                "openrouter",
                &entry.api_model,
                1,
                1,
                0,
                0,
                &next.cost,
            ),
            crate::llm::route_decision::UsagePrice::Priced(cost) if cost > 0.0
        ));
    }

    /// D-094 C-2: a model-scoped entry without its own list (here one
    /// that only sets a tool dialect) keeps the operations it inherits from
    /// the provider-wide entry, and only gains `decision_endpoint`.
    #[test]
    fn set_backend_keeps_the_effective_operations() {
        let mut config = Config::default();
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": format!("custom:{LOOPBACK}"),
                "operations": ["chat"],
            }))
            .unwrap(),
        );
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": format!("custom:{LOOPBACK}"),
                "model": "decider-1",
                "native_tools": true,
            }))
            .unwrap(),
        );
        let next = CausalConfigService::mutate(&config, &[loopback_backend("decider-1")]).unwrap();
        assert_eq!(next.providers.models.len(), 2, "no entry is added");
        assert_eq!(
            declarations(&next)[1].2,
            [ModelOperationSetting::Chat, ModelOperationSetting::DecisionEndpoint]
        );
        assert_eq!(next.providers.models[1].native_tools, Some(true));
        let catalog = crate::providers::catalog::CatalogView::from_config(&next);
        let descriptor = catalog.describe(
            &crate::providers::ProviderId::of(&format!("custom:{LOOPBACK}")),
            "decider-1",
        );
        assert!(matches!(descriptor.operation(ModelOperation::Chat), Support::Supported));
    }

    /// D-102 H-1: a custom model the default chat selection uses without
    /// any evidence keeps chat when it becomes the decision model too.
    #[test]
    fn set_backend_keeps_the_operations_selections_use() {
        let mut config = Config::default();
        config.default_provider = Some(format!("custom:{LOOPBACK}"));
        config.default_model = Some("decider-1".to_string());
        let next = CausalConfigService::mutate(&config, &[loopback_backend("decider-1")]).unwrap();
        assert_eq!(
            declarations(&next)[0].2,
            [ModelOperationSetting::Chat, ModelOperationSetting::DecisionEndpoint]
        );
    }

    /// Policy patches set only the named fields; reset restores every
    /// default; an invalid value is refused by the section's validation.
    #[test]
    fn policy_patches_are_explicit_and_validated() {
        let next = CausalConfigService::mutate(
            &Config::default(),
            &[CausalMutation::SetPolicy(PolicyPatch {
                minimum_top_margin: Some(0.2),
                ..PolicyPatch::default()
            })],
        )
        .unwrap();
        assert!((next.causal_tree.policy.minimum_top_margin - 0.2).abs() < f64::EPSILON);
        assert_eq!(
            next.causal_tree.policy.max_candidates,
            CausalPolicy::default().max_candidates
        );
        let reset = CausalConfigService::mutate(&next, &[CausalMutation::ResetPolicyToDefaults]).unwrap();
        assert_eq!(reset.causal_tree.policy, CausalPolicy::default());
        assert!(matches!(
            CausalConfigService::mutate(
                &Config::default(),
                &[CausalMutation::SetPolicy(PolicyPatch {
                    minimum_top_margin: Some(0.9),
                    ..PolicyPatch::default()
                })]
            ),
            Err(CausalMutationError::Invalid(_))
        ));
    }

    fn write_report(workspace: &Path, id: uuid::Uuid, calibration: &serde_json::Value) {
        let dir = workspace.join("self/causal_tree/reports");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_vec(&serde_json::json!({
                "report_version": crate::causal_tree::evaluate::REPORT_VERSION,
                "calibration": calibration,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// D-092: a calibration applies only from a stored report that proposes
    /// thresholds for the configured, pinned backend.
    #[test]
    fn apply_calibration_needs_a_report_and_a_pin() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: temp.path().to_path_buf(),
            ..Config::default()
        };
        config = CausalConfigService::mutate(&config, &[loopback_backend("decider-2026-09")]).unwrap();
        let id = uuid::Uuid::now_v7();
        let apply = |config: &Config, id: &str| {
            CausalConfigService::mutate(
                config,
                &[CausalMutation::ApplyCalibration {
                    report_id: id.to_string(),
                }],
            )
        };
        assert!(
            matches!(
                apply(&config, &id.to_string()),
                Err(CausalMutationError::Calibration(_))
            ),
            "no report"
        );
        write_report(
            temp.path(),
            id,
            &serde_json::json!({
                "status": "available",
                "value": {
                    "profile": "compatible_http:decider-2026-09",
                    "endpoint": LOOPBACK,
                    "decisions": 120,
                    "minimum_choice_confidence": 0.7,
                    "minimum_top_margin": 0.15,
                }
            }),
        );
        let applied = apply(&config, &id.to_string()).unwrap();
        let policy = &applied.causal_tree.policy;
        assert!((policy.minimum_choice_confidence - 0.7).abs() < f64::EPSILON);
        assert!((policy.minimum_top_margin - 0.15).abs() < f64::EPSILON);
        assert_eq!(
            policy
                .calibration_profile
                .as_ref()
                .map(|profile| (profile.model.as_str(), profile.endpoint.as_str())),
            Some(("decider-2026-09", LOOPBACK))
        );
        // D-094 C-7: another server serving the same model is not
        // calibrated by it.
        let mut moved = applied.clone();
        moved.causal_tree.backend.endpoint = "http://127.0.0.2:8900/v1".to_string();
        assert!(moved.causal_tree.validate().is_err());

        // D-102 H-2: a report measured on another endpoint serving the same
        // model is not applied after a re-point.
        let repointed = CausalConfigService::mutate(
            &config,
            &[CausalMutation::SetBackend {
                kind: BackendKind::CompatibleHttp,
                endpoint: Some("http://127.0.0.2:8900/v1".to_string()),
                model: Some("decider-2026-09".to_string()),
                pinned_snapshot: None,
                timeout_ms: None,
            }],
        )
        .unwrap();
        assert!(
            matches!(
                apply(&repointed, &id.to_string()),
                Err(CausalMutationError::Calibration(_))
            ),
            "measured on another endpoint"
        );

        let other = CausalConfigService::mutate(&config, &[loopback_backend("decider-2027-01")]).unwrap();
        assert!(
            matches!(apply(&other, &id.to_string()), Err(CausalMutationError::Calibration(_))),
            "measured on another model"
        );
        let unavailable = uuid::Uuid::now_v7();
        write_report(
            temp.path(),
            unavailable,
            &serde_json::json!({"status": "unavailable", "reason": "the window ran on more than one backend or model"}),
        );
        assert!(matches!(
            apply(&config, &unavailable.to_string()),
            Err(CausalMutationError::Calibration(_))
        ));
        assert!(matches!(
            apply(&config, "../../etc/passwd"),
            Err(CausalMutationError::Calibration(_))
        ));
    }

    /// A service over a configuration written to `dir`.
    async fn service(dir: &Path) -> (CausalConfigService, crate::config::SharedConfig) {
        let mut config = Config {
            config_path: dir.join("config.toml"),
            workspace_dir: dir.join("workspace"),
            ..Config::default()
        };
        config.secrets.encrypt = false;
        crate::config::files::write_split_config(&config, false)
            .await
            .expect("test: write");
        let loaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load");
        let shared = crate::config::new_shared(loaded);
        (
            CausalConfigService::new(ConfigTransactions::new(Arc::clone(&shared))),
            shared,
        )
    }

    fn confirmed(revision: Option<&str>) -> ApplyOptions<'_> {
        ApplyOptions {
            expected_revision: revision,
            confirm_high_risk: true,
            confirmed_revision: revision,
        }
    }

    async fn apply(
        service: &CausalConfigService,
        mutations: &[CausalMutation],
    ) -> Result<ConfigTransactionOutcome, CausalMutationError> {
        let plan = service.plan(mutations).await?;
        let revision = plan.planned_revision().map(str::to_string);
        Ok(service
            .transactions
            .apply(plan, ConfigActor::GatewayConfigApi, confirmed(revision.as_deref()))
            .await?)
    }

    /// V2.02 (R-CXR P0-B t1): a credential reference owned by another
    /// provider is refused before anything is written.
    #[tokio::test]
    async fn a_cross_owner_credential_ref_is_rejected_before_the_plan() {
        let temp = tempfile::TempDir::new().unwrap();
        let (service, shared) = service(temp.path()).await;
        let config = shared.desired().config.as_ref().clone();
        let references = CredentialReferences::for_config(&config).unwrap();
        references
            .set("openai", "chat-key", "sk-CROSS-OWNER-SENTINEL-000")
            .unwrap();
        let before = std::fs::read(temp.path().join("config.toml")).unwrap();
        let refused = apply(
            &service,
            &[
                // An enabled plane: a disabled one never opens the credential
                // store (D-082 N-9).
                CausalMutation::SetEnabled(true),
                loopback_backend("decider-1"),
                CausalMutation::SetCredentialRef(CredentialRefSetting::Profile("cred-chat-key".to_string())),
            ],
        )
        .await;
        let message = format!("{:#}", refused.expect_err("test: refused"));
        assert!(
            message.contains("belongs to") || message.contains("owner") || message.contains("credential"),
            "{message}"
        );
        assert!(!message.contains("CROSS-OWNER-SENTINEL"), "{message}");
        assert_eq!(
            std::fs::read(temp.path().join("config.toml")).unwrap(),
            before,
            "nothing written"
        );
    }

    /// R-CXR P0-B t3: a rollback restores the enabled switch and the backend
    /// together, in one transaction, and the restored configuration passes
    /// the same decision check.
    #[tokio::test]
    async fn rollback_restores_mode_and_backend_in_one_transaction() {
        let temp = tempfile::TempDir::new().unwrap();
        let (service, shared) = {
            let mut config = Config {
                config_path: temp.path().join("config.toml"),
                workspace_dir: temp.path().join("workspace"),
                ..Config::default()
            };
            config.causal_tree.enabled = false;
            config.secrets.encrypt = false;
            crate::config::files::write_split_config(&config, false)
                .await
                .expect("test: write");
            let loaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load");
            let shared = crate::config::new_shared(loaded);
            (
                CausalConfigService::new(ConfigTransactions::new(Arc::clone(&shared))),
                shared,
            )
        };
        let first = apply(
            &service,
            &[CausalMutation::SetEnabled(true), loopback_backend("decider-1")],
        )
        .await
        .expect("test: applied");
        // `[causal_tree]` activates on the next start without a rebuilding
        // participant; the desired configuration holds the change.
        assert!(shared.desired().config.causal_tree.enabled);
        let record = first.record_id.expect("test: recorded");
        let revision = ConfigTransactions::base_revision(&shared.desired()).expect("test: revision");
        service
            .transactions
            .rollback(RollbackRequest {
                record_id: &record,
                expected_revision: &revision,
                confirm: true,
                restore_credentials: false,
                disable_encryption: false,
            })
            .await
            .expect("test: rolled back");
        let desired = shared.desired().config.clone();
        assert!(!desired.causal_tree.enabled);
        assert_eq!(desired.causal_tree.backend.kind, BackendSelection::Auto);
        let history = service.transactions.history().unwrap();
        assert_eq!(history.get(&record).unwrap().status, RecordStatus::Activated);
    }

    /// A refused plan removes the credential it stored; an applied one keeps
    /// it, owned by the backend's provider and referenced by the
    /// configuration.
    #[tokio::test]
    async fn a_refused_plan_removes_the_credential_it_stored() {
        let temp = tempfile::TempDir::new().unwrap();
        let (service, shared) = service_with_backend(temp.path()).await;
        let references = CredentialReferences::for_config(&shared.load_full()).unwrap();
        let revision = ConfigTransactions::base_revision(&shared.desired());
        // Refused: a credential change is high risk and this one is not
        // confirmed.
        let refused = service
            .store_credential(
                SecretString::from("sk-REFUSED-PLAN-SENTINEL-0001".to_string()),
                ConfigActor::GatewayCredential,
                ApplyOptions {
                    expected_revision: revision.as_deref(),
                    ..ApplyOptions::default()
                },
            )
            .await;
        assert!(refused.is_err());
        assert!(
            references.list().unwrap().is_empty(),
            "the stored credential was removed"
        );
        assert_eq!(
            shared.desired().config.causal_tree.backend.credential_ref,
            CredentialRefSetting::Unset
        );

        let (service, shared) = service_with_backend(temp.path()).await;
        // A credential change is high risk: it is confirmed against the
        // revision the caller saw.
        let revision = ConfigTransactions::base_revision(&shared.desired());
        let stored = service
            .store_credential(
                SecretString::from("sk-KEPT-SENTINEL-0002".to_string()),
                ConfigActor::GatewayCredential,
                confirmed(revision.as_deref()),
            )
            .await
            .expect("test: applied");
        assert!(stored.outcome.record_id.is_some());
        let desired = shared.desired().config.clone();
        assert_eq!(
            desired.causal_tree.backend.credential_ref,
            CredentialRefSetting::Profile(stored.id.clone())
        );
        let listed = CredentialReferences::for_config(&desired).unwrap().list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, stored.id);
        assert_eq!(listed[0].owner, format!("custom:{LOOPBACK}"));
    }

    /// D-094 C-9: a caller's stale revision is refused before anything is
    /// stored: the credential store is not even created.
    #[tokio::test]
    async fn a_stale_revision_stores_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let (service, shared) = service_with_backend(temp.path()).await;
        let store = shared.load_full().config_path.with_file_name("auth-profiles.json");
        let refused = service
            .store_credential(
                SecretString::from("sk-STALE-REVISION-SENTINEL-0003".to_string()),
                ConfigActor::GatewayCredential,
                confirmed(Some("not-the-current-revision")),
            )
            .await;
        assert!(
            matches!(
                refused,
                Err(CausalMutationError::Transaction(ConfigTransactionError::Conflict(_)))
            ),
            "{refused:?}"
        );
        assert!(!store.exists(), "nothing was stored");
    }

    /// D-094 C-5: a rotation removes the previous decision credential, so
    /// exactly one remains, the referenced one; none is an orphan.
    #[tokio::test]
    async fn a_rotation_leaves_exactly_one_decision_credential() {
        let temp = tempfile::TempDir::new().unwrap();
        let (service, shared) = service_with_backend(temp.path()).await;
        let mut ids = Vec::new();
        for secret in ["sk-ROTATION-SENTINEL-0004", "sk-ROTATION-SENTINEL-0005"] {
            let revision = ConfigTransactions::base_revision(&shared.desired());
            let stored = service
                .store_credential(
                    SecretString::from(secret.to_string()),
                    ConfigActor::GatewayCredential,
                    confirmed(revision.as_deref()),
                )
                .await
                .expect("test: applied");
            assert!(stored.previous_left.is_none());
            assert_eq!(stored.removed_previous, ids.last().cloned());
            ids.push(stored.id);
        }
        let desired = shared.desired().config.clone();
        let listed = CredentialReferences::for_config(&desired).unwrap().list().unwrap();
        assert_eq!(
            listed.iter().map(|status| status.id.as_str()).collect::<Vec<_>>(),
            [ids[1].as_str()]
        );
        assert!(
            orphaned_decision_credentials(&desired).unwrap().is_empty(),
            "expected an empty value"
        );
    }

    /// D-102 H-3: the previous decision credential is removed only when
    /// nothing in the stored files names it; a reference outside the
    /// decision section (here a section this build does not load) keeps it,
    /// and the refusal names the path, never the value.
    #[tokio::test]
    async fn a_credential_referenced_elsewhere_is_never_released() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_service, shared) = service_with_backend(temp.path()).await;
        let config = shared.load_full();
        let references = CredentialReferences::for_config(&config).unwrap();
        let provider = format!("custom:{LOOPBACK}");
        let kept = references
            .set(
                &provider,
                "decision-compatible-http-0000a001",
                "sk-H3-KEPT-SENTINEL-0008",
            )
            .unwrap();
        let free = references
            .set(
                &provider,
                "decision-compatible-http-0000a002",
                "sk-H3-FREE-SENTINEL-0009",
            )
            .unwrap();
        let mut stored = std::fs::read_to_string(&config.config_path).unwrap();
        stored.push_str(&format!(
            "\n[decision.backends.hosted]\ncredential_ref = \"{}\"\n",
            kept.id
        ));
        std::fs::write(&config.config_path, stored).unwrap();

        let (left, why) = release_previous(&references, &config, kept.id.clone()).expect_err("test: kept");
        assert_eq!(left, kept.id);
        let why = why.to_string();
        assert!(why.contains("decision.backends.hosted.credential_ref"), "{why}");
        assert!(!why.contains("SENTINEL"), "{why}");
        assert_eq!(
            release_previous(&references, &config, free.id.clone()),
            Ok(free.id.clone())
        );
        let listed: Vec<String> = references.list().unwrap().into_iter().map(|status| status.id).collect();
        assert!(listed.contains(&kept.id) && !listed.contains(&free.id), "{listed:?}");
    }

    /// D-094 C-5: a decision credential nothing references (left by a
    /// process that stopped between store and apply) is an orphan; a
    /// removal that fails is part of the error, never only a log line.
    #[tokio::test]
    async fn orphans_are_reported_and_a_failed_removal_is_surfaced() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_service, shared) = service_with_backend(temp.path()).await;
        let config = shared.load_full();
        let references = CredentialReferences::for_config(&config).unwrap();
        let left = references
            .set(
                &format!("custom:{LOOPBACK}"),
                "decision-compatible-http-0badc0de",
                "sk-ORPHAN-SENTINEL-0006",
            )
            .unwrap();
        references
            .set("openai", "chat-key", "sk-NOT-A-DECISION-CREDENTIAL-0007")
            .unwrap();
        let orphans = orphaned_decision_credentials(&config).unwrap();
        assert_eq!(
            orphans.iter().map(|status| status.id.as_str()).collect::<Vec<_>>(),
            [left.id.as_str()]
        );

        std::fs::write(config.config_path.with_file_name("auth-profiles.json"), "not json").unwrap();
        let error = compensate(
            &references,
            left.id.clone(),
            CausalMutationError::Invalid("refused".to_string()),
        );
        let message = error.to_string();
        assert!(
            matches!(&error, CausalMutationError::CredentialLeft { id, .. } if *id == left.id),
            "{message}"
        );
        assert!(message.contains(&left.id) && !message.contains("SENTINEL"), "{message}");
    }

    /// A service whose stored configuration already selects a declared
    /// loopback backend.
    async fn service_with_backend(dir: &Path) -> (CausalConfigService, crate::config::SharedConfig) {
        let fresh = dir.join("with-backend");
        std::fs::create_dir_all(&fresh).unwrap();
        let (service, shared) = service(&fresh).await;
        apply(&service, &[loopback_backend("decider-1")])
            .await
            .expect("test: backend");
        (service, shared)
    }

    /// Every plan runs the decision backend check, whatever built it: a
    /// structured change to an undeclared decision model is refused before
    /// anything is written.
    #[tokio::test]
    async fn an_undeclared_decision_model_is_refused_by_every_plan() {
        let temp = tempfile::TempDir::new().unwrap();
        let (service, shared) = service(temp.path()).await;
        let mut candidate = shared.desired().config.as_ref().clone();
        candidate.causal_tree.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        candidate.causal_tree.backend.endpoint = LOOPBACK.to_string();
        candidate.causal_tree.backend.model = "undeclared".to_string();
        let before = std::fs::read(temp.path().join("config.toml")).unwrap();
        let plan = service
            .transactions
            .plan(ConfigChange::Structured(Box::new(candidate)))
            .await
            .unwrap();
        let revision = plan.planned_revision().map(str::to_string);
        let refused = service
            .transactions
            .apply(plan, ConfigActor::GatewayConfigApi, confirmed(revision.as_deref()))
            .await;
        assert!(
            matches!(refused, Err(ConfigTransactionError::Invalid(ref error)) if format!("{error:#}").contains("causal_tree.backend")),
            "{refused:?}"
        );
        assert_eq!(std::fs::read(temp.path().join("config.toml")).unwrap(), before);
    }
}
