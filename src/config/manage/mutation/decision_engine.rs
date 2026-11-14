//! Typed decision-plane mutations (SP-CDP-A 5.5 under D-040; CDP-12).
//!
//! The one mutation service of `[decision_engine]`: the CLI (`prx decision-engine
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
//! - `SetEngine` sets one key of the `[decision_engine]` allowlist, the
//!   turn limits `latency_budget_ms`, `token_ratio_limit` and
//!   `max_candidates` included
//!   ([`engine_setting`]); any other key, a secret-bearing key and the
//!   retired `apply-calibration` are refused with [`EngineKeyError`].
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

use super::super::credentials::{CredentialReferenceError, CredentialReferences, CredentialStatus, SecretString};
use super::super::history::ConfigActor;
use super::super::transaction::{
    ApplyOptions, ConfigChange, ConfigPlan, ConfigTransactionError, ConfigTransactionOutcome, ConfigTransactions,
};
use crate::config::Config;
use crate::config::files::ConfigRevisionConflict;
use crate::config::schema::decision_engine::BackendKind;
use crate::config::schema::decision_engine::{BackendSelection, CredentialRefSetting, DecisionBackendConfig};
use crate::config::schema::{ModelOperationSetting, ProviderModelConfig};
use crate::providers::catalog::{ModelOperation, Support};

/// One change to the decision plane's configuration.
#[derive(Debug, Clone, PartialEq)]
pub enum CausalMutation {
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
    /// Go back to `kind = "auto"` (the factory's order: a configured
    /// endpoint, else the builtin): clears the endpoint, model, pinned
    /// snapshot and credential reference, and turns remote processing off in
    /// the same plan. The stored credential is not deleted; it stays
    /// unreferenced and `prx doctor` lists it as an orphaned decision credential.
    ResetBackendToAuto,
    /// Allow or forbid decision state leaving this machine.
    SetRemote(bool),
    /// Set the credential reference (`""`, `"none"` or `cred-<name>`).
    SetCredentialRef(CredentialRefSetting),
    /// Set one allowlisted `[decision_engine]` key (`prx decision-engine
    /// config set`, the Gateway's `/api/decision-engine/config/*`).
    SetEngine(EngineSetting),
}

/// One allowlisted `[decision_engine]` setting with its parsed value
/// (P2-R8). Backend settings change the effective engine backend.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineSetting {
    Enabled(bool),
    Mode(crate::config::schema::decision_engine::DecisionMode),
    PromptBudgetTokens(u32),
    Candidates(u32),
    TimeoutMs(u32),
    TelemetryCap(u32),
    ObserveQueue(u32),
    LatencyBudgetMs(u64),
    TokenRatioLimit(f32),
    MaxCandidates(u8),
    BackendKind(BackendSelection),
    BackendEndpoint(String),
    BackendModel(String),
    BackendPinnedSnapshot(String),
    BackendRemoteEnabled(bool),
    BackendTimeoutMs(u32),
    BackendMaxRequestBytes(u32),
    BackendMaxResponseBytes(u32),
    BackendFailureThreshold(u32),
    BackendCooldownSecs(u32),
}

impl EngineSetting {
    /// Whether the setting changes the backend.
    #[must_use]
    pub const fn is_backend(&self) -> bool {
        !matches!(
            self,
            Self::Enabled(_)
                | Self::Mode(_)
                | Self::PromptBudgetTokens(_)
                | Self::Candidates(_)
                | Self::TimeoutMs(_)
                | Self::TelemetryCap(_)
                | Self::ObserveQueue(_)
                | Self::LatencyBudgetMs(_)
                | Self::TokenRatioLimit(_)
                | Self::MaxCandidates(_)
        )
    }

    fn apply(&self, config: &mut Config) {
        let engine = &mut config.decision_engine;
        let backend = engine.backend.get_or_insert_with(DecisionBackendConfig::default);
        match self {
            Self::Enabled(value) => engine.enabled = *value,
            Self::Mode(value) => engine.mode = *value,
            Self::PromptBudgetTokens(value) => engine.prompt_budget_tokens = *value,
            Self::Candidates(value) => engine.candidates = *value,
            Self::TimeoutMs(value) => engine.timeout_ms = *value,
            Self::TelemetryCap(value) => engine.telemetry_cap = *value,
            Self::ObserveQueue(value) => engine.observe_queue = *value,
            Self::LatencyBudgetMs(value) => engine.latency_budget_ms = *value,
            Self::TokenRatioLimit(value) => engine.token_ratio_limit = *value,
            Self::MaxCandidates(value) => engine.max_candidates = *value,
            Self::BackendKind(value) => backend.kind = *value,
            Self::BackendEndpoint(value) => backend.endpoint.clone_from(value),
            Self::BackendModel(value) => backend.model.clone_from(value),
            Self::BackendPinnedSnapshot(value) => backend.pinned_snapshot.clone_from(value),
            Self::BackendRemoteEnabled(value) => backend.remote_enabled = *value,
            Self::BackendTimeoutMs(value) => backend.timeout_ms = *value,
            Self::BackendMaxRequestBytes(value) => backend.max_request_bytes = *value,
            Self::BackendMaxResponseBytes(value) => backend.max_response_bytes = *value,
            Self::BackendFailureThreshold(value) => backend.circuit_breaker.failure_threshold = *value,
            Self::BackendCooldownSecs(value) => backend.circuit_breaker.cooldown_secs = *value,
        }
    }
}

/// Why a `config set` key was refused. Credentials are set only through
/// `PUT /api/decision-engine/credential`; the message never echoes a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineKeyError {
    /// The key is not in the allowlist, names a credential or secret, or is
    /// the retired `apply-calibration`.
    #[error("decision_engine key `{key}` cannot be set here{hint}")]
    Refused { key: String, hint: &'static str },
    /// The value does not parse for the key.
    #[error("decision_engine key `{key}` takes {expected}")]
    InvalidValue { key: String, expected: &'static str },
}

/// Words that mark a credential or secret-bearing key.
const SECRET_WORDS: [&str; 7] = ["credential", "api_key", "apikey", "token", "secret", "password", "auth"];

/// The allowlisted setting `key` = `value` (P2-R8). Any other key, every
/// credential or secret-bearing key and `apply-calibration` are refused.
///
/// # Errors
///
/// [`EngineKeyError`] for a refused key or a value that does not parse.
pub fn engine_setting(key: &str, value: &str) -> Result<EngineSetting, EngineKeyError> {
    let normalized = key.trim().to_ascii_lowercase();
    let refused = |hint: &'static str| EngineKeyError::Refused {
        key: crate::providers::redact_endpoint_userinfo(key.trim())
            .chars()
            .take(64)
            .collect(),
        hint,
    };
    let value = value.trim();
    let invalid = |expected: &'static str| EngineKeyError::InvalidValue {
        key: normalized.clone(),
        expected,
    };
    let boolean = || value.parse::<bool>().map_err(|_| invalid("`true` or `false`"));
    let number = || value.parse::<u32>().map_err(|_| invalid("an unsigned integer"));
    Ok(match normalized.as_str() {
        "enabled" => EngineSetting::Enabled(boolean()?),
        "mode" => EngineSetting::Mode(match value {
            "pre" => crate::config::schema::decision_engine::DecisionMode::Pre,
            "post" => crate::config::schema::decision_engine::DecisionMode::Post,
            "both" => crate::config::schema::decision_engine::DecisionMode::Both,
            _ => return Err(invalid("`pre`, `post` or `both`")),
        }),
        "prompt_budget_tokens" => EngineSetting::PromptBudgetTokens(number()?),
        "candidates" => EngineSetting::Candidates(number()?),
        "timeout_ms" => EngineSetting::TimeoutMs(number()?),
        "telemetry_cap" => EngineSetting::TelemetryCap(number()?),
        "observe_queue" => EngineSetting::ObserveQueue(number()?),
        "latency_budget_ms" => {
            EngineSetting::LatencyBudgetMs(value.parse::<u64>().map_err(|_| invalid("an unsigned integer"))?)
        }
        "token_ratio_limit" => EngineSetting::TokenRatioLimit(
            value
                .parse::<f32>()
                .ok()
                .filter(|ratio| ratio.is_finite())
                .ok_or_else(|| invalid("a finite number"))?,
        ),
        "max_candidates" => {
            EngineSetting::MaxCandidates(value.parse::<u8>().map_err(|_| invalid("an integer from 0 to 255"))?)
        }
        "backend.kind" => EngineSetting::BackendKind(
            BackendSelection::try_from(value.to_string()).map_err(|_| invalid("a backend kind"))?,
        ),
        "backend.endpoint" => EngineSetting::BackendEndpoint(value.to_string()),
        "backend.model" => EngineSetting::BackendModel(value.to_string()),
        "backend.pinned_snapshot" => EngineSetting::BackendPinnedSnapshot(value.to_string()),
        "backend.remote_enabled" => EngineSetting::BackendRemoteEnabled(boolean()?),
        "backend.timeout_ms" => EngineSetting::BackendTimeoutMs(number()?),
        "backend.max_request_bytes" => EngineSetting::BackendMaxRequestBytes(number()?),
        "backend.max_response_bytes" => EngineSetting::BackendMaxResponseBytes(number()?),
        "backend.circuit_breaker.failure_threshold" => EngineSetting::BackendFailureThreshold(number()?),
        "backend.circuit_breaker.cooldown_secs" => EngineSetting::BackendCooldownSecs(number()?),
        "apply-calibration" | "apply_calibration" => {
            return Err(refused("; calibration from evaluation reports was removed"));
        }
        // The exact allowlist is matched first, so an allowlisted key whose
        // name contains a credential word (`prompt_budget_tokens`,
        // `token_ratio_limit`) is accepted; every other key is refused, a
        // credential or secret-bearing one with the credential route hint.
        _ if SECRET_WORDS.iter().any(|word| normalized.contains(word)) => {
            return Err(refused("; set credentials with PUT /api/decision-engine/credential"));
        }
        _ => return Err(refused("; it is not in the decision_engine allowlist")),
    })
}

/// Why a decision-plane change was refused. No message carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum CausalMutationError {
    /// The resulting configuration is not valid.
    #[error("the decision plane change is refused: {0}")]
    Invalid(String),
    /// The selected backend's release catalog recommends a larger
    /// `latency_budget_ms` than the configured one. Backend selection never
    /// changes the budget: the operator raises it explicitly first.
    #[error(
        "the decision backend model `{model}` needs latency_budget_ms of at least {recommended} ms, but \
         {configured} ms is configured: raise latency_budget_ms first"
    )]
    LatencyBudgetBelowRecommendation {
        model: String,
        configured: u64,
        recommended: u64,
    },
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
    let referenced = match &crate::config::schema::decision_engine::effective_engine_backend(config).credential_ref {
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
    /// validated. Nothing is read but, for `SetBackend`, the model metadata
    /// cache.
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

        next.decision_engine
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
        let backend = crate::config::schema::decision_engine::effective_engine_backend(&desired);
        let Some(selection) = crate::config::schema::decision_engine::configured_selection(backend) else {
            return Err(CausalMutationError::Invalid(
                "no decision backend that takes a credential is configured".to_string(),
            ));
        };
        let previous = match &backend.credential_ref {
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
/// it (`prx decision-engine config set`, the Gateway), so an equal change gives an equal
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
    if to_builtin
        && !turns_remote
        && crate::config::schema::decision_engine::effective_engine_backend(config).remote_enabled
    {
        std::iter::once(CausalMutation::SetRemote(false))
            .chain(mutations)
            .collect()
    } else {
        mutations
    }
}

/// The release catalog's `latency_budget_ms` recommendation for the backend a
/// `SetBackend` with `kind`, `endpoint` and `model` selects over `config`, if
/// the catalog has one.
#[must_use]
pub fn recommended_latency_budget(
    config: &Config,
    kind: BackendKind,
    endpoint: Option<&str>,
    model: Option<&str>,
) -> Option<u64> {
    let mut backend = crate::config::schema::decision_engine::effective_engine_backend(config).clone();
    backend.kind = BackendSelection::Fixed(kind);
    if let Some(endpoint) = endpoint {
        backend.endpoint = endpoint.trim().to_string();
    }
    if let Some(model) = model {
        backend.model = model.trim().to_string();
    }
    let selection = crate::config::schema::decision_engine::configured_selection(&backend)?;
    let catalog = crate::providers::catalog::ReleaseCatalog::embedded_ref().ok()?;
    catalog
        .model(
            &crate::providers::ProviderId::of(&selection.provider),
            backend.model.trim(),
        )?
        .transport
        .extra_latency_budget_ms
}

/// Setup and onboarding only: `mutations` led by an explicit
/// `latency_budget_ms` = the catalog recommendation when a backend selection
/// in them recommends more than `config` has, so the operator's setup choice
/// writes the budget in the same transaction. The plain mutation API never
/// calls this and keeps refusing such a selection.
#[must_use]
pub fn with_recommended_latency_budget(config: &Config, mutations: Vec<CausalMutation>) -> Vec<CausalMutation> {
    let recommended = mutations
        .iter()
        .filter_map(|mutation| match mutation {
            CausalMutation::SetBackend {
                kind, endpoint, model, ..
            } => recommended_latency_budget(config, *kind, endpoint.as_deref(), model.as_deref()),
            _ => None,
        })
        .max();
    match recommended {
        Some(recommended) if recommended > config.decision_engine.latency_budget_ms => {
            std::iter::once(CausalMutation::SetEngine(EngineSetting::LatencyBudgetMs(recommended)))
                .chain(mutations)
                .collect()
        }
        _ => mutations,
    }
}

/// Apply one mutation to `config`.
///
/// A backend change starts from the effective engine backend (R55) and
/// writes `[decision_engine.backend]`, so the change is the one the engine
/// builds from.
///
/// # Errors
///
/// [`CausalMutationError::LatencyBudgetBelowRecommendation`] when a backend
/// selection's catalog recommendation exceeds the configured
/// `latency_budget_ms`, whether the engine is on or off.
fn apply_one(config: &mut Config, mutation: &CausalMutation) -> Result<(), CausalMutationError> {
    let backend_change = matches!(
        mutation,
        CausalMutation::ResetBackendToAuto
            | CausalMutation::SetRemote(_)
            | CausalMutation::SetCredentialRef(_)
            | CausalMutation::SetBackend { .. }
    ) || matches!(mutation, CausalMutation::SetEngine(setting) if setting.is_backend());
    if backend_change {
        config.decision_engine.backend =
            Some(crate::config::schema::decision_engine::effective_engine_backend(config).clone());
    }
    match mutation {
        CausalMutation::SetEngine(setting) => setting.apply(config),
        CausalMutation::SetBackend {
            kind,
            endpoint,
            model,
            pinned_snapshot,
            timeout_ms,
        } => {
            let backend = config
                .decision_engine
                .backend
                .get_or_insert_with(DecisionBackendConfig::default);
            backend.kind = BackendSelection::Fixed(*kind);
            if let Some(snapshot) = pinned_snapshot {
                backend.pinned_snapshot = snapshot.trim().to_string();
            } else if !matches!(kind, BackendKind::TypeSafe | BackendKind::OpenRouter) {
                backend.pinned_snapshot.clear();
            }
            if let Some(endpoint) = endpoint {
                backend.endpoint = endpoint.trim().to_string();
            }
            if let Some(model) = model {
                backend.model = model.trim().to_string();
            }
            if let Some(timeout_ms) = timeout_ms {
                backend.timeout_ms = *timeout_ms;
            }
            if let Some(selection) = crate::config::schema::decision_engine::configured_selection(backend) {
                let model = backend.model.trim().to_string();
                if let Ok(catalog) = crate::providers::catalog::ReleaseCatalog::embedded_ref()
                    && let Some(entry) = catalog.model(&crate::providers::ProviderId::of(&selection.provider), &model)
                {
                    if pinned_snapshot.is_none()
                        && matches!(kind, BackendKind::TypeSafe | BackendKind::OpenRouter)
                        && let Some(snapshot) = &entry.pinned_snapshot
                    {
                        backend.pinned_snapshot.clone_from(snapshot);
                    }
                    if timeout_ms.is_none()
                        && let Some(recommended) = entry.transport.recommended_timeout_ms
                        && let Ok(recommended) = u32::try_from(recommended)
                    {
                        backend.timeout_ms = recommended;
                    }
                    // Backend selection never changes `latency_budget_ms`: a
                    // budget below the catalog recommendation refuses the
                    // selection, naming both values.
                    if let Some(recommended) = entry.transport.extra_latency_budget_ms
                        && config.decision_engine.latency_budget_ms < recommended
                    {
                        return Err(CausalMutationError::LatencyBudgetBelowRecommendation {
                            model,
                            configured: config.decision_engine.latency_budget_ms,
                            recommended,
                        });
                    }
                }
                declare_decision_model(config, &selection.provider, &model);
            }
        }
        CausalMutation::ResetBackendToAuto => {
            let backend = config
                .decision_engine
                .backend
                .get_or_insert_with(DecisionBackendConfig::default);
            backend.kind = BackendSelection::Auto;
            backend.endpoint.clear();
            backend.model.clear();
            backend.pinned_snapshot.clear();
            backend.credential_ref = CredentialRefSetting::Unset;
            backend.remote_enabled = false;
        }
        CausalMutation::SetRemote(remote) => {
            config
                .decision_engine
                .backend
                .get_or_insert_with(DecisionBackendConfig::default)
                .remote_enabled = *remote;
        }
        CausalMutation::SetCredentialRef(reference) => {
            config
                .decision_engine
                .backend
                .get_or_insert_with(DecisionBackendConfig::default)
                .credential_ref = reference.clone();
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::path::Path;
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
            crate::config::schema::decision_engine::effective_engine_backend(&next).kind,
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
    fn reset_backend_to_auto_clears_the_hosted_choice_and_remote() {
        let catalog = crate::providers::catalog::ReleaseCatalog::embedded_ref().expect("release catalog");
        let entry = catalog
            .model(&crate::providers::ProviderId::of("openrouter"), "typesafe/jev-1.13")
            .expect("hosted decision entry");
        let hosted = CausalConfigService::mutate(
            &Config::default(),
            &[
                CausalMutation::SetBackend {
                    kind: BackendKind::OpenRouter,
                    endpoint: None,
                    model: Some(entry.api_model.clone()),
                    pinned_snapshot: None,
                    timeout_ms: None,
                },
                CausalMutation::SetCredentialRef(CredentialRefSetting::Profile("cred-decision-openrouter".to_string())),
                CausalMutation::SetRemote(true),
            ],
        )
        .expect("hosted backend");
        assert!(matches!(
            crate::config::schema::decision_engine::effective_engine_backend(&hosted).kind,
            BackendSelection::Fixed(_)
        ));
        let auto = CausalConfigService::mutate(&hosted, &[CausalMutation::ResetBackendToAuto]).expect("back to auto");
        let backend = crate::config::schema::decision_engine::effective_engine_backend(&auto);
        assert_eq!(backend.kind, BackendSelection::Auto);
        assert!(backend.endpoint.is_empty() && backend.model.is_empty() && backend.pinned_snapshot.is_empty());
        assert_eq!(backend.credential_ref, CredentialRefSetting::Unset);
        assert!(!backend.remote_enabled);
    }

    #[test]
    fn set_backend_applies_hosted_transport_defaults_from_release_catalog() {
        let catalog = crate::providers::catalog::ReleaseCatalog::embedded_ref().expect("release catalog");
        let entry = catalog
            .model(&crate::providers::ProviderId::of("openrouter"), "typesafe/jev-1.13")
            .expect("hosted decision entry");
        let recommended = entry.transport.extra_latency_budget_ms.expect("catalog latency budget");
        let mut budgeted = Config::default();
        budgeted.decision_engine.latency_budget_ms = recommended;
        let next = CausalConfigService::mutate(
            &budgeted,
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
            crate::config::schema::decision_engine::effective_engine_backend(&next).pinned_snapshot,
            entry.pinned_snapshot.as_deref().expect("catalog snapshot")
        );
        assert_eq!(
            u64::from(crate::config::schema::decision_engine::effective_engine_backend(&next).timeout_ms),
            entry.transport.recommended_timeout_ms.expect("catalog timeout")
        );
        assert_eq!(
            next.decision_engine.latency_budget_ms, recommended,
            "the budget is unchanged"
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

    /// Backend selection never changes `latency_budget_ms`: a budget below
    /// the catalog recommendation refuses the selection with both values,
    /// with the engine on or off; an equal or larger budget is kept as is,
    /// and raising the budget first in the same plan admits the selection.
    #[test]
    fn set_backend_never_changes_the_latency_budget() {
        let hosted = CausalMutation::SetBackend {
            kind: BackendKind::OpenRouter,
            endpoint: None,
            model: Some("typesafe/jev-1.13".to_string()),
            pinned_snapshot: None,
            timeout_ms: None,
        };
        for enabled in [false, true] {
            let mut config = Config::default();
            config.decision_engine.enabled = enabled;
            let configured = config.decision_engine.latency_budget_ms;
            let refused = CausalConfigService::mutate(&config, std::slice::from_ref(&hosted));
            match refused {
                Err(CausalMutationError::LatencyBudgetBelowRecommendation {
                    model,
                    configured: named,
                    recommended,
                }) => {
                    assert_eq!(model, "typesafe/jev-1.13");
                    assert_eq!(named, configured);
                    assert_eq!(recommended, 3_000, "the catalog recommendation of typesafe/jev-1.13");
                    let message = CausalMutationError::LatencyBudgetBelowRecommendation {
                        model,
                        configured: named,
                        recommended,
                    }
                    .to_string();
                    assert!(
                        message.contains("3000") && message.contains(&configured.to_string()),
                        "{message}"
                    );
                }
                other => panic!("enabled = {enabled}: expected the typed refusal, got {other:?}"),
            }
        }
        let mut larger = Config::default();
        larger.decision_engine.latency_budget_ms = 5_000;
        let kept = CausalConfigService::mutate(&larger, std::slice::from_ref(&hosted)).unwrap();
        assert_eq!(kept.decision_engine.latency_budget_ms, 5_000);
        let raised_first = CausalConfigService::mutate(
            &Config::default(),
            &[
                CausalMutation::SetEngine(engine_setting("latency_budget_ms", "3000").unwrap()),
                hosted,
            ],
        )
        .unwrap();
        assert_eq!(raised_first.decision_engine.latency_budget_ms, 3_000);
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

    /// The three turn limits are written under `[decision_engine]` through
    /// the allowlist and validated with today's bounds.
    #[test]
    fn turn_limits_are_engine_settings_and_validated() {
        let next = CausalConfigService::mutate(
            &Config::default(),
            &[
                CausalMutation::SetEngine(engine_setting("latency_budget_ms", "3000").unwrap()),
                CausalMutation::SetEngine(engine_setting("token_ratio_limit", "0.5").unwrap()),
                CausalMutation::SetEngine(engine_setting("max_candidates", "4").unwrap()),
            ],
        )
        .unwrap();
        assert_eq!(next.decision_engine.latency_budget_ms, 3_000);
        assert!((next.decision_engine.token_ratio_limit - 0.5).abs() < f32::EPSILON);
        assert_eq!(next.decision_engine.max_candidates, 4);
        for (key, value) in [
            ("latency_budget_ms", "49"),
            ("latency_budget_ms", "5001"),
            ("token_ratio_limit", "0"),
            ("max_candidates", "0"),
            ("max_candidates", "33"),
        ] {
            assert!(
                matches!(
                    CausalConfigService::mutate(
                        &Config::default(),
                        &[CausalMutation::SetEngine(engine_setting(key, value).unwrap())],
                    ),
                    Err(CausalMutationError::Invalid(_))
                ),
                "{key} = {value}"
            );
        }
        assert!(matches!(
            engine_setting("token_ratio_limit", "NaN"),
            Err(EngineKeyError::InvalidValue { .. })
        ));
    }

    /// P2-R8: the allowlist parses every listed key; any other key, every
    /// secret-bearing key and `apply-calibration` are refused.
    #[test]
    fn engine_setting_accepts_only_the_allowlist() {
        for (key, value) in [
            ("enabled", "true"),
            ("mode", "both"),
            ("prompt_budget_tokens", "300"),
            ("candidates", "5"),
            ("timeout_ms", "1500"),
            ("telemetry_cap", "0"),
            ("observe_queue", "64"),
            ("latency_budget_ms", "500"),
            ("token_ratio_limit", "0.35"),
            ("max_candidates", "12"),
            ("backend.kind", "builtin"),
            ("backend.endpoint", LOOPBACK),
            ("backend.model", "decider-1"),
            ("backend.pinned_snapshot", "snap"),
            ("backend.remote_enabled", "false"),
            ("backend.timeout_ms", "500"),
            ("backend.max_request_bytes", "65536"),
            ("backend.max_response_bytes", "65536"),
            ("backend.circuit_breaker.failure_threshold", "3"),
            ("backend.circuit_breaker.cooldown_secs", "30"),
        ] {
            assert!(engine_setting(key, value).is_ok(), "{key}");
        }
        for key in [
            "backend.credential_ref",
            "backend.api_key",
            "token",
            "apply-calibration",
            "policy.max_candidates",
        ] {
            assert!(
                matches!(engine_setting(key, "x"), Err(EngineKeyError::Refused { .. })),
                "{key}"
            );
        }
        assert!(matches!(
            engine_setting("enabled", "maybe"),
            Err(EngineKeyError::InvalidValue { .. })
        ));
        let next = CausalConfigService::mutate(
            &Config::default(),
            &[CausalMutation::SetEngine(
                engine_setting("telemetry_cap", "10").unwrap(),
            )],
        )
        .unwrap();
        assert_eq!(next.decision_engine.telemetry_cap, 10);
        assert!(
            CausalConfigService::mutate(
                &Config::default(),
                &[CausalMutation::SetEngine(engine_setting("candidates", "0").unwrap())],
            )
            .is_err(),
            "the engine section is validated"
        );
    }

    /// A service over a configuration written to `dir`.
    async fn service(dir: &Path) -> (CausalConfigService, crate::config::SharedConfig) {
        service_with_engine(dir, false).await
    }

    async fn service_with_engine(
        dir: &Path,
        engine_enabled: bool,
    ) -> (CausalConfigService, crate::config::SharedConfig) {
        let mut config = Config {
            config_path: dir.join("config.toml"),
            workspace_dir: dir.join("workspace"),
            ..Config::default()
        };
        config.secrets.encrypt = false;
        config.decision_engine.enabled = engine_enabled;
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
        let (service, shared) = service_with_engine(temp.path(), true).await;
        let config = shared.desired().config.as_ref().clone();
        let references = CredentialReferences::for_config(&config).unwrap();
        references
            .set("openai", "chat-key", "sk-CROSS-OWNER-SENTINEL-000")
            .unwrap();
        let before = std::fs::read(temp.path().join("config.toml")).unwrap();
        let refused = apply(
            &service,
            &[
                // An enabled engine: a disabled one never opens the credential
                // store (D-082 N-9).
                CausalMutation::SetEngine(EngineSetting::Enabled(true)),
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
            &[
                CausalMutation::SetEngine(EngineSetting::Enabled(true)),
                loopback_backend("decider-1"),
            ],
        )
        .await
        .expect("test: applied");
        // The engine activates on the next start without a rebuilding
        // participant; the desired configuration holds the change.
        assert!(shared.desired().config.decision_engine.enabled);
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
        assert!(!desired.decision_engine.enabled);
        assert_eq!(
            crate::config::schema::decision_engine::effective_engine_backend(&desired).kind,
            BackendSelection::Auto
        );
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
            crate::config::schema::decision_engine::effective_engine_backend(&shared.desired().config).credential_ref,
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
            crate::config::schema::decision_engine::effective_engine_backend(&desired).credential_ref,
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
        candidate
            .decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        candidate
            .decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .endpoint = LOOPBACK.to_string();
        candidate
            .decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .model = "undeclared".to_string();
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
            matches!(refused, Err(ConfigTransactionError::Invalid(ref error)) if format!("{error:#}").contains("decision_engine.backend")),
            "{refused:?}"
        );
        assert_eq!(std::fs::read(temp.path().join("config.toml")).unwrap(), before);
    }
}
