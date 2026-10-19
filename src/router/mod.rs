#[cfg(feature = "llm-router")]
pub mod automix;
#[cfg(feature = "llm-router")]
pub mod capability;
pub mod context;
#[cfg(feature = "llm-router")]
pub mod elo;
#[cfg(feature = "llm-router")]
pub mod history;
#[cfg(feature = "llm-router")]
pub mod intent;
#[cfg(feature = "llm-router")]
pub mod knn;
#[cfg(feature = "llm-router")]
pub mod models;
#[cfg(feature = "llm-router")]
pub mod scorer;

#[cfg(feature = "llm-router")]
use anyhow::Result;
#[cfg(feature = "llm-router")]
use parking_lot::RwLock;
#[cfg(feature = "llm-router")]
use std::collections::VecDeque;
#[cfg(feature = "llm-router")]
use std::sync::Arc;

#[cfg(feature = "llm-router")]
use crate::agent::classifier::TaskIntent;
#[cfg(feature = "llm-router")]
use crate::config::{ModelRouteConfig, RouterConfig};
#[cfg(feature = "llm-router")]
use crate::memory::Memory;
#[cfg(feature = "llm-router")]
use crate::memory::embeddings::EmbeddingProvider;

#[cfg(feature = "llm-router")]
use self::capability::{
    ModelCapabilityEntry, append_success_event, filter_models_by_providers, is_model_reachable, load_recent_successes,
    reachable_provider_names,
};
#[cfg(feature = "llm-router")]
use self::elo::update_elo;
#[cfg(feature = "llm-router")]
use self::history::RouterHistory;
#[cfg(feature = "llm-router")]
use self::intent::infer_router_intent;
#[cfg(feature = "llm-router")]
use self::knn::KnnStore;
#[cfg(feature = "llm-router")]
pub use self::scorer::RouterResult;
#[cfg(feature = "llm-router")]
use self::scorer::rank_models;
pub use context::resolve_effective_compaction_config;
// RouterResult remains the scorer output; RouteDecision is the durable
// timeline projection used by ingress/provider execution paths.

#[cfg(feature = "llm-router")]
pub struct RouterEngine {
    /// Original caller policy, before reachability filtering/default expansion.
    configured_inputs: RouterConfig,
    config: RouterConfig,
    default_provider: String,
    model_routes: Vec<ModelRouteConfig>,
    models: RwLock<Vec<ModelCapabilityEntry>>,
    memory: Arc<dyn Memory>,
    history: Option<RouterHistory>,
    local_learning: tokio::sync::Mutex<()>,
    local_slot: RwLock<Option<LocalRouterState>>,
}

#[cfg(feature = "llm-router")]
struct LocalRouterState {
    scope: (String, String, String),
    models: Vec<ModelCapabilityEntry>,
    version: i64,
}

#[cfg(feature = "llm-router")]
#[derive(Clone)]
struct OutcomePersistenceSnapshot {
    model_id: String,
    metrics_entry: ModelCapabilityEntry,
}

#[cfg(feature = "llm-router")]
impl RouterEngine {
    pub async fn new(
        mut config: RouterConfig,
        default_provider: String,
        model_routes: Vec<ModelRouteConfig>,
        memory: Arc<dyn Memory>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
        cost: &crate::config::schema::CostConfig,
    ) -> Result<Self> {
        let configured_inputs = config.clone();
        let reachable_providers = reachable_provider_names(&default_provider, &model_routes);
        if config.models.is_empty() {
            config.models =
                filter_models_by_providers(self::models::builtin_model_capabilities(cost), &reachable_providers);
        } else {
            config.models = filter_models_by_providers(config.models.clone(), &reachable_providers);
        }
        if config.models.is_empty() {
            // Provider labels may be `custom:` URLs with `user:password@`.
            let shown_reachable: Vec<String> = reachable_providers
                .iter()
                .map(|provider| crate::providers::redact_endpoint_userinfo(provider).into_owned())
                .collect();
            tracing::warn!(
                default_provider = %crate::providers::redact_endpoint_userinfo(&default_provider),
                reachable_providers = ?shown_reachable,
                "Router has no reachable models after provider filtering"
            );
        }
        let mut models = ModelCapabilityEntry::load_all(&config.models, memory.as_ref()).await;
        models = filter_reachable_entries(models, &default_provider, &model_routes);
        if models.is_empty() {
            tracing::warn!(
                default_provider = %crate::providers::redact_endpoint_userinfo(&default_provider),
                "Router has no reachable models after defensive post-load filtering"
            );
        }
        let history = if let Some(embedder) = embedder.filter(|embedder| embedder.dimensions() > 0) {
            let store = KnnStore::new(Arc::clone(&memory))?;
            Some(RouterHistory::new(
                store,
                embedder,
                config.knn_k,
                config.knn_min_records,
            ))
        } else {
            None
        };
        Ok(Self {
            configured_inputs,
            config,
            default_provider,
            model_routes,
            models: RwLock::new(models),
            memory,
            history,
            local_learning: tokio::sync::Mutex::new(()),
            local_slot: RwLock::new(None),
        })
    }

    /// Attach a concrete uncharged embedding instance before the engine is
    /// injected into an Agent. Dynamic embedding providers retain the legacy
    /// API and are not silently reclassified as local.
    pub fn with_local_hash_history(
        mut self,
        embedder: Arc<crate::memory::embeddings::LocalHashEmbedding>,
    ) -> Result<Self> {
        anyhow::ensure!(
            self.history.is_none() && self.local_slot.read().is_none(),
            "Router history must be bound before source admission"
        );
        self.history = Some(RouterHistory::new_local_hash(
            KnnStore::new(Arc::clone(&self.memory))?,
            embedder,
            self.config.knn_k,
            self.config.knn_min_records,
        )?);
        self.ensure_local_path()?;
        Ok(self)
    }

    pub(crate) const fn has_history(&self) -> bool {
        self.history.is_some()
    }

    pub(crate) fn history_domain(
        &self,
        source: &dyn crate::runtime::turn_source::TurnSourceProof,
    ) -> Result<crate::memory::RouterHistoryDomain> {
        self.bind_local_source(source)?;
        let history = self
            .history
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Router history is absent"))?;
        let embedder = history.admitted_embedder()?;
        let mut policy: [u8; 32] = hex::decode(self.local_policy()?)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Router policy digest length differs"))?;
        if history.local_embedder_if_present().is_none() {
            use sha2::{Digest, Sha256};
            let mut digest = Sha256::new();
            digest.update(b"router-paid-embedding-domain-v1");
            digest.update(policy);
            digest.update(source.config_source_revision().as_bytes());
            policy = digest.finalize().into();
        }
        let domain = crate::memory::RouterHistoryDomain {
            workspace: source.workspace_id().to_owned(),
            owner: source.owner_id().to_owned(),
            session: source.session_key().to_owned(),
            policy_sha256: policy,
            embedding_model: embedder.model().to_owned(),
            dimensions: u16::try_from(embedder.dimensions())?,
            embedding_version: 1,
        };
        domain.validate()?;
        Ok(domain)
    }

    pub(crate) fn history(&self) -> Result<&RouterHistory> {
        self.history
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Router history is absent"))
    }

    pub(crate) fn rank_scoped_history(
        &self,
        source: &dyn crate::runtime::turn_source::TurnSourceProof,
        task: TaskIntent,
        scores: &std::collections::HashMap<(String, String), f32>,
    ) -> Result<RouterResult> {
        self.bind_local_source(source)?;
        let slot = self.local_slot.read();
        let state = slot
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Router source scope missing"))?;
        let models = self.reachable_models(&state.models);
        Ok(scorer::rank_models_scoped(
            &infer_router_intent(task, source.routing_input()),
            estimate_tokens(source.routing_input()),
            &models,
            &self.config,
            scores,
        ))
    }

    #[allow(clippy::indexing_slicing)]
    #[cfg(test)]
    fn new_with_history(
        config: RouterConfig,
        default_provider: String,
        model_routes: Vec<ModelRouteConfig>,
        memory: Arc<dyn Memory>,
        history: Option<RouterHistory>,
    ) -> Self {
        let configured_inputs = config.clone();
        let reachable_providers = reachable_provider_names(&default_provider, &model_routes);
        let filtered_config = RouterConfig {
            models: if config.models.is_empty() {
                filter_models_by_providers(
                    self::models::builtin_model_capabilities(&crate::config::schema::CostConfig::default()),
                    &reachable_providers,
                )
            } else {
                filter_models_by_providers(config.models.clone(), &reachable_providers)
            },
            ..config
        };
        let models =
            futures::executor::block_on(ModelCapabilityEntry::load_all(&filtered_config.models, memory.as_ref()));
        let models = filter_reachable_entries(models, &default_provider, &model_routes);
        Self {
            configured_inputs,
            config: filtered_config,
            default_provider,
            model_routes,
            models: RwLock::new(models),
            memory,
            history,
            local_learning: tokio::sync::Mutex::new(()),
            local_slot: RwLock::new(None),
        }
    }

    pub async fn select_model(&self, message: &str, task_intent: &TaskIntent) -> RouterResult {
        let similarity_scores = if let Some(history) = &self.history {
            history.similarity_scores(message).await
        } else {
            std::collections::HashMap::new()
        };
        self.rank_with_similarity(message, *task_intent, &similarity_scores)
    }

    /// The embedded source has already been durably recorded by the caller.
    /// This path runs the actual ranker but never invokes KNN/embedding IO.
    pub(crate) fn select_model_local(
        &self,
        message: &str,
        task_intent: TaskIntent,
        source: &dyn crate::runtime::turn_source::TurnSourceProof,
    ) -> Result<RouterResult> {
        anyhow::ensure!(
            self.history.is_none(),
            "Router history requires a purpose-bound selected read"
        );
        self.bind_local_source(source)?;
        let state = self.local_slot.read();
        let state = state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("local Router scope missing"))?;
        Ok(self.rank_models_with_similarity(message, task_intent, &std::collections::HashMap::new(), &state.models))
    }

    pub(crate) fn ensure_local_path(&self) -> Result<()> {
        if let Some(history) = &self.history {
            history.admitted_embedder()?;
        }
        anyhow::ensure!(
            self.config.models.len() <= 128
                && self.config.models.iter().all(|model| !model.provider.is_empty()
                    && model.provider.len() <= 512
                    && !model.model_id.is_empty()
                    && model.model_id.len() <= 512),
            "Router local policy exceeds model bounds"
        );
        let mut identities = std::collections::HashSet::new();
        anyhow::ensure!(
            self.config.models.iter().all(|model| identities.insert((
                crate::providers::redact_endpoint_userinfo(&model.provider).into_owned(),
                model.model_id.clone()
            ))),
            "Router local policy contains duplicate model identities"
        );
        self.local_policy()?;
        Ok(())
    }

    fn local_policy(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let encoded = serde_json::to_vec(&(
            "router-learning-v2:actual-serving-terminal-status:elo-skillratings-default:success-window-100:latency-0.7-0.3",
            &self.configured_inputs,
            &self.config,
            &self.default_provider,
            &self.model_routes,
        ))?;
        anyhow::ensure!(encoded.len() <= 65_536, "Router local policy exceeds byte bound");
        if let Some(history) = &self.history {
            let descriptor = serde_json::to_vec(&history.policy_descriptor()?)?;
            let mut digest = Sha256::new();
            digest.update(b"router-local-history-policy-v1");
            digest.update((encoded.len() as u64).to_be_bytes());
            digest.update(encoded);
            digest.update(descriptor);
            Ok(hex::encode(digest.finalize()))
        } else {
            Ok(hex::encode(Sha256::digest(encoded)))
        }
    }

    pub(crate) fn bind_local_source(&self, source: &dyn crate::runtime::turn_source::TurnSourceProof) -> Result<()> {
        self.ensure_local_path()?;
        anyhow::ensure!(
            source.source_event_id().is_some(),
            "local Router requires durable source"
        );
        anyhow::ensure!(
            source.routing_input().len() <= 65_536
                && source.config_source_revision().len() <= 512
                && i64::try_from(source.config_generation_id()).is_ok(),
            "local Router source exceeds learning bounds"
        );
        for value in [source.workspace_id(), source.owner_id(), source.session_key()] {
            anyhow::ensure!(
                !value.is_empty() && value.len() <= 512,
                "local Router source exceeds identity bound"
            );
        }
        let scope = (
            source.workspace_id().to_owned(),
            source.owner_id().to_owned(),
            source.session_key().to_owned(),
        );
        let mut slot = self.local_slot.write();
        if let Some(state) = slot.as_ref() {
            anyhow::ensure!(state.scope == scope, "local Router is bound to another source scope");
        } else {
            let models = self
                .config
                .models
                .iter()
                .map(|config| ModelCapabilityEntry {
                    config: config.clone(),
                    dynamic_elo: config.elo_rating,
                    success_rate: 1.0,
                    recent_latency_ms: config.latency_ms,
                })
                .collect();
            *slot = Some(LocalRouterState {
                scope,
                models,
                version: 0,
            });
        }
        Ok(())
    }

    /// Verify the actual injected instance against the pinned source inputs;
    /// never replace the caller's configured ranker with a static route.
    pub(crate) fn validate_embedded_binding(
        &self,
        config: &crate::config::Config,
        provider: &str,
        memory: &Arc<dyn Memory>,
    ) -> Result<()> {
        for policy in [&self.configured_inputs, &config.router] {
            policy.validate()?;
            anyhow::ensure!(
                [policy.alpha, policy.beta, policy.gamma, policy.delta, policy.epsilon]
                    .iter()
                    .all(|value| value.is_finite())
                    && policy
                        .models
                        .iter()
                        .all(|model| model.cost_per_million_tokens.is_finite()
                            && model.cost_per_million_tokens >= 0.0
                            && model.elo_rating.is_finite()),
                "embedded Router configuration contains an invalid numeric policy"
            );
        }
        anyhow::ensure!(
            Arc::ptr_eq(&self.memory, memory),
            "embedded Router memory differs from pinned source"
        );
        anyhow::ensure!(
            self.default_provider == provider,
            "embedded Router provider differs from pinned source"
        );
        anyhow::ensure!(
            serde_json::to_value(&self.model_routes)? == serde_json::to_value(&config.model_routes)?,
            "embedded Router model routes differ from pinned source"
        );
        anyhow::ensure!(
            serde_json::to_value(&self.configured_inputs)? == serde_json::to_value(&config.router)?,
            "embedded Router configuration differs from pinned source"
        );
        Ok(())
    }

    fn rank_with_similarity(
        &self,
        message: &str,
        task_intent: TaskIntent,
        similarity_scores: &std::collections::HashMap<String, f32>,
    ) -> RouterResult {
        let models = self.models.read();
        let reachable_models = self.reachable_models(&models);
        drop(models);
        self.rank_reachable_models(message, task_intent, similarity_scores, reachable_models)
    }

    fn rank_models_with_similarity(
        &self,
        message: &str,
        task_intent: TaskIntent,
        similarity_scores: &std::collections::HashMap<String, f32>,
        models: &[ModelCapabilityEntry],
    ) -> RouterResult {
        self.rank_reachable_models(message, task_intent, similarity_scores, self.reachable_models(models))
    }

    fn reachable_models(&self, models: &[ModelCapabilityEntry]) -> Vec<ModelCapabilityEntry> {
        models
            .iter()
            .filter(|model| {
                is_model_reachable(
                    &self.default_provider,
                    &self.model_routes,
                    &model.config.provider,
                    &model.config.model_id,
                )
            })
            .cloned()
            .collect()
    }

    fn rank_reachable_models(
        &self,
        message: &str,
        task_intent: TaskIntent,
        similarity_scores: &std::collections::HashMap<String, f32>,
        reachable_models: Vec<ModelCapabilityEntry>,
    ) -> RouterResult {
        let intent = infer_router_intent(task_intent, message);
        let estimated_tokens = estimate_tokens(message);
        if reachable_models.is_empty() {
            return RouterResult {
                chosen_model: None,
                chosen_provider: None,
                score: 0.0,
                candidates: Vec::new(),
                intent: intent.category_name().to_string(),
                estimated_tokens,
            };
        }
        let result = rank_models(
            &intent,
            estimated_tokens,
            &reachable_models,
            &self.config,
            Some(similarity_scores),
        );
        let mut result = result;
        if let (Some(provider), Some(model)) = (result.chosen_provider.as_deref(), result.chosen_model.as_deref()) {
            if !is_model_reachable(&self.default_provider, &self.model_routes, provider, model) {
                tracing::warn!(
                    provider = %crate::providers::redact_endpoint_userinfo(provider),
                    model,
                    "Router selected unreachable model; falling back to default model"
                );
                result.chosen_provider = None;
                result.chosen_model = None;
                result.score = 0.0;
            }
        }

        let (candidates, filtered) = decision_log_candidates(&result);
        tracing::info!(
            chosen = result.chosen_model.as_deref().unwrap_or(""),
            provider = %crate::providers::redact_endpoint_userinfo(result.chosen_provider.as_deref().unwrap_or("")),
            score = result.score,
            intent = result.intent.as_str(),
            estimated_tokens = result.estimated_tokens,
            candidates = ?candidates,
            filtered = ?filtered,
            "Router decision"
        );

        result
    }

    pub const fn automix_config(&self) -> Option<&crate::config::AutomixConfig> {
        Some(&self.config.automix)
    }

    pub fn model_cost_per_million_tokens(&self, model_id: &str) -> Option<f32> {
        self.models
            .read()
            .iter()
            .find(|entry| {
                entry.config.model_id == model_id
                    || format!("{}/{}", entry.config.provider, entry.config.model_id) == model_id
            })
            .map(|entry| entry.config.cost_per_million_tokens)
    }

    /// Record an outcome for the routed request.
    ///
    /// This is the backward-compatible entry point: it attributes the outcome
    /// to `model_id` with no fallback context (the planned model was the model
    /// that actually served the request). Call [`record_outcome_detailed`] when
    /// the execution path knows whether a fallback chain was exercised and which
    /// model ultimately served the request.
    ///
    /// [`record_outcome_detailed`]: Self::record_outcome_detailed
    pub async fn record_outcome(&self, message: &str, model_id: &str, success: bool, latency_ms: u64) -> Result<()> {
        self.record_outcome_detailed(message, model_id, model_id, false, success, latency_ms)
            .await
    }

    /// Record an outcome, distinguishing the planned model from the model that
    /// actually served the request and whether a fallback chain was exercised.
    ///
    /// # Arguments
    /// * `message` — the user message that produced this request.
    /// * `planned_model` — the model the router originally selected.
    /// * `actual_model` — the model that ultimately served the request. When the
    ///   reliability layer fell back, this differs from `planned_model`; the ELO
    ///   / success-rate learning signal MUST be attributed to the model that did
    ///   the work, not the one that failed first.
    /// * `fallback_chain_used` — `true` when a provider/model fallback (rather
    ///   than the first candidate) produced the outcome. This lets the learning
    ///   layer avoid over-penalizing a planned model whose fallback recovered.
    /// * `success` — whether the request succeeded.
    /// * `latency_ms` — observed end-to-end latency.
    pub async fn record_outcome_detailed(
        &self,
        message: &str,
        planned_model: &str,
        actual_model: &str,
        fallback_chain_used: bool,
        success: bool,
        latency_ms: u64,
    ) -> Result<()> {
        self.record_outcome_inner(
            message,
            planned_model,
            actual_model,
            fallback_chain_used,
            success,
            latency_ms,
            false,
        )
        .await
    }

    pub(crate) fn history_training_evidence(
        &self,
        receipt: &crate::agent::terminal::TurnTerminalReceipt,
        source: &dyn crate::runtime::turn_source::TurnSourceProof,
        metadata: &crate::agent::embedded_source::VerifiedEmbeddedInputMetadata,
        embedding: crate::runtime::turn_decision::RouterEmbedding,
    ) -> Result<crate::memory::RouterHistoryWriteEvidence> {
        self.bind_local_source(source)?;
        let policy = if self.history()?.local_embedder_if_present().is_some() {
            self.local_policy()?
        } else {
            hex::encode(self.history_domain(source)?.policy_sha256)
        };
        let terminal = capability::local_learning_evidence(receipt, source, metadata, policy)?;
        crate::memory::RouterHistoryWriteEvidence::from_terminal(terminal, embedding)
    }

    pub(crate) async fn record_local_outcome(
        &self,
        receipt: &crate::agent::terminal::TurnTerminalReceipt,
        source: &dyn crate::runtime::turn_source::TurnSourceProof,
        metadata: &crate::agent::embedded_source::VerifiedEmbeddedInputMetadata,
        deadline: std::time::Instant,
    ) -> Result<()> {
        use crate::memory::traits::{RouterLearningCommit, RouterLearningMetrics, RouterLearningSnapshot};
        self.bind_local_source(source)?;
        let _serial = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), self.local_learning.lock())
            .await
            .map_err(|_| anyhow::anyhow!("Router learning deadline expired"))?;
        let policy = self.local_policy()?;
        let evidence = capability::local_learning_evidence(receipt, source, metadata, policy)?;
        let (provider, model) = evidence.served();
        let (success, latency) = evidence.outcome();
        for _ in 0..3 {
            anyhow::ensure!(std::time::Instant::now() < deadline, "Router learning deadline expired");
            let read = self.memory.read_router_learning(&evidence, deadline).await?;
            if read.replayed {
                let stored = read
                    .snapshot
                    .ok_or_else(|| anyhow::anyhow!("Router replay snapshot missing"))?;
                self.publish_local_metrics(source, &stored)?;
                return Ok(());
            }
            // New scoped policy state starts from configured priors, never legacy global memory.
            let mut next = read.snapshot.unwrap_or_else(|| RouterLearningSnapshot {
                version: 0,
                models: self
                    .config
                    .models
                    .iter()
                    .map(|entry| RouterLearningMetrics {
                        provider: crate::providers::redact_endpoint_userinfo(&entry.provider).into_owned(),
                        model: entry.model_id.clone(),
                        elo: entry.elo_rating,
                        success_rate: 1.0,
                        latency_ms: entry.latency_ms,
                        successes: Default::default(),
                    })
                    .collect(),
            });
            next.validate()?;
            let mut models = self.local_models_from_snapshot(&next)?;
            let target = next
                .models
                .iter_mut()
                .find(|entry| entry.provider == provider && entry.model == model)
                .ok_or_else(|| anyhow::anyhow!("Router served model is outside learning policy"))?;
            // Preserve the actual tuple, including model IDs and provider URLs containing slashes.
            let chosen_index = models
                .iter()
                .position(|entry| {
                    entry.config.model_id == model
                        && crate::providers::redact_endpoint_userinfo(&entry.config.provider) == provider
                })
                .ok_or_else(|| anyhow::anyhow!("Router served model missing"))?;
            let updated = apply_outcome_at_index(&mut models, chosen_index, success, latency, &mut target.successes)
                .ok_or_else(|| anyhow::anyhow!("Router served model cannot be updated"))?;
            target.elo = updated.metrics_entry.dynamic_elo;
            target.success_rate = updated.metrics_entry.success_rate;
            target.latency_ms = updated.metrics_entry.recent_latency_ms;
            let expected = next.version;
            next.version = expected
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Router version overflow"))?;
            match self
                .memory
                .commit_router_learning(&evidence, expected, &next, deadline)
                .await?
            {
                RouterLearningCommit::Conflict => continue,
                RouterLearningCommit::Committed(stored) => {
                    // A replay returns the database's current committed snapshot, never our proposal.
                    self.publish_local_metrics(source, &stored)?;
                    return Ok(());
                }
            }
        }
        anyhow::bail!("Router learning concurrent updates exceeded retry bound")
    }

    fn local_models_from_snapshot(
        &self,
        snapshot: &crate::memory::traits::RouterLearningSnapshot,
    ) -> Result<Vec<ModelCapabilityEntry>> {
        snapshot.validate()?;
        anyhow::ensure!(
            snapshot.models.len() == self.config.models.len(),
            "Router snapshot policy model count differs"
        );
        self.config
            .models
            .iter()
            .map(|config| {
                let entry = snapshot
                    .models
                    .iter()
                    .find(|entry| {
                        entry.model == config.model_id
                            && entry.provider == crate::providers::redact_endpoint_userinfo(&config.provider)
                    })
                    .ok_or_else(|| anyhow::anyhow!("Router snapshot contains a different model policy"))?;
                Ok(ModelCapabilityEntry {
                    config: config.clone(),
                    dynamic_elo: entry.elo,
                    success_rate: entry.success_rate,
                    recent_latency_ms: entry.latency_ms,
                })
            })
            .collect()
    }

    fn publish_local_metrics(
        &self,
        source: &dyn crate::runtime::turn_source::TurnSourceProof,
        snapshot: &crate::memory::traits::RouterLearningSnapshot,
    ) -> Result<()> {
        let models = self.local_models_from_snapshot(snapshot)?;
        let mut slot = self.local_slot.write();
        let slot = slot
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("local Router scope missing"))?;
        anyhow::ensure!(
            slot.scope.0 == source.workspace_id()
                && slot.scope.1 == source.owner_id()
                && slot.scope.2 == source.session_key()
                && slot.version <= snapshot.version,
            "local Router publication scope or version drift"
        );
        slot.models = models;
        slot.version = snapshot.version;
        Ok(())
    }

    async fn record_outcome_inner(
        &self,
        message: &str,
        planned_model: &str,
        actual_model: &str,
        fallback_chain_used: bool,
        success: bool,
        latency_ms: u64,
        strict_persistence: bool,
    ) -> Result<()> {
        // Attribute the learning signal to the model that actually served the
        // request. When a fallback recovered, `actual_model` is the served model
        // and the planned model's failure is captured by the fallback flag.
        let attributed_model = actual_model;
        let mut recent_successes =
            load_recent_successes(self.memory.as_ref(), &normalize_model_id(attributed_model)).await;
        let snapshot = {
            let mut models = self.models.write();
            apply_outcome_update_locked(
                &mut models,
                attributed_model,
                success,
                latency_ms,
                &mut recent_successes,
            )
        };
        let Some(snapshot) = snapshot else {
            tracing::warn!(
                planned_model = %crate::providers::redact_endpoint_userinfo(planned_model),
                actual_model = %crate::providers::redact_endpoint_userinfo(actual_model),
                fallback_chain_used,
                "Router outcome ignored for unknown model"
            );
            return Ok(());
        };

        if let Err(error) = append_success_event(self.memory.as_ref(), &snapshot.model_id, success).await {
            if strict_persistence {
                return Err(error);
            }
            tracing::warn!("Router success event append failed");
        }
        if let Err(error) = snapshot.metrics_entry.save_metrics(self.memory.as_ref()).await {
            if strict_persistence {
                return Err(error);
            }
            tracing::warn!("Router metrics persistence failed");
        }
        if let Some(history) = &self.history {
            if history
                .record_query(message, &snapshot.model_id, success)
                .await
                .is_err()
            {
                // Embedding/storage errors can contain the original message.
                tracing::warn!("Router history record failed");
            }
        }
        // A model target may be `<provider label>/<model>`, and a `custom:`
        // label may carry `user:password@`.
        tracing::info!(
            planned_model = %crate::providers::redact_endpoint_userinfo(planned_model),
            actual_model = %crate::providers::redact_endpoint_userinfo(actual_model),
            fallback_chain_used,
            success,
            latency_ms,
            "Router outcome recorded"
        );
        Ok(())
    }
}

/// The candidate lists the router's decision line shows: the kept candidates
/// (provider label redacted, INT-2B USERINFO-3) and the filtered ones.
#[cfg(feature = "llm-router")]
fn decision_log_candidates(result: &RouterResult) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let candidates = result
        .candidates
        .iter()
        .filter(|candidate| candidate.filtered_reason.is_none())
        .map(|candidate| {
            serde_json::json!({
                "model": candidate.model_id,
                "provider": crate::providers::redact_endpoint_userinfo(&candidate.provider),
                "similarity": candidate.similarity_score,
                "score": candidate.total_score,
            })
        })
        .collect();
    let filtered = result
        .candidates
        .iter()
        .filter_map(|candidate| {
            candidate.filtered_reason.as_ref().map(|reason| {
                serde_json::json!({
                    "model": candidate.model_id,
                    "reason": reason,
                    "detail": candidate.filtered_detail,
                })
            })
        })
        .collect();
    (candidates, filtered)
}

#[cfg(feature = "llm-router")]
fn normalize_model_id(model_id: &str) -> String {
    model_id
        .rsplit_once('/')
        .map_or_else(|| model_id.to_string(), |(_, model)| model.to_string())
}

#[cfg(feature = "llm-router")]
fn filter_reachable_entries(
    entries: Vec<ModelCapabilityEntry>,
    default_provider: &str,
    model_routes: &[ModelRouteConfig],
) -> Vec<ModelCapabilityEntry> {
    entries
        .into_iter()
        .filter(|entry| {
            is_model_reachable(
                default_provider,
                model_routes,
                &entry.config.provider,
                &entry.config.model_id,
            )
        })
        .collect()
}

// SAFETY: chosen_index and other_index are produced by .position()/.enumerate()
// on the same `models` slice, so they are always valid indices.
#[allow(clippy::indexing_slicing)]
#[cfg(feature = "llm-router")]
fn apply_outcome_update_locked(
    models: &mut [ModelCapabilityEntry],
    model_id: &str,
    success: bool,
    latency_ms: u64,
    recent_successes: &mut VecDeque<bool>,
) -> Option<OutcomePersistenceSnapshot> {
    let chosen_index = models.iter().position(|model| {
        model.config.model_id == model_id || format!("{}/{}", model.config.provider, model.config.model_id) == model_id
    })?;
    apply_outcome_at_index(models, chosen_index, success, latency_ms, recent_successes)
}

#[cfg(feature = "llm-router")]
#[allow(clippy::indexing_slicing)]
fn apply_outcome_at_index(
    models: &mut [ModelCapabilityEntry],
    chosen_index: usize,
    success: bool,
    latency_ms: u64,
    recent_successes: &mut VecDeque<bool>,
) -> Option<OutcomePersistenceSnapshot> {
    if chosen_index >= models.len() {
        return None;
    }
    let baseline = models
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != chosen_index)
        .max_by(|(_, left), (_, right)| {
            left.dynamic_elo
                .partial_cmp(&right.dynamic_elo)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index);

    recent_successes.push_back(success);
    while recent_successes.len() > 100 {
        recent_successes.pop_front();
    }

    let success_count = recent_successes.iter().filter(|value| **value).count() as f32;
    models[chosen_index].success_rate = if recent_successes.is_empty() {
        1.0
    } else {
        success_count / recent_successes.len() as f32
    };

    let old_latency = models[chosen_index].recent_latency_ms as f32;
    let new_latency = latency_ms.min(u64::from(u32::MAX)) as f32;
    models[chosen_index].recent_latency_ms = (old_latency.mul_add(0.7, new_latency * 0.3)).round() as u32;

    if let Some(other_index) = baseline {
        let baseline_elo = models[other_index].dynamic_elo;
        let updated_chosen_elo = if success {
            update_elo(models[chosen_index].dynamic_elo, baseline_elo).0
        } else {
            update_elo(baseline_elo, models[chosen_index].dynamic_elo).1
        };
        models[chosen_index].dynamic_elo = updated_chosen_elo;
    }

    let metrics_entry = models[chosen_index].clone();
    Some(OutcomePersistenceSnapshot {
        model_id: metrics_entry.config.model_id.clone(),
        metrics_entry,
    })
}

#[cfg(feature = "llm-router")]
fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 3 + 100
}

#[cfg(all(test, feature = "llm-router"))]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;
    use crate::config::{RouterConfig, RouterModelConfig};
    use crate::memory::embeddings::EmbeddingProvider;
    use crate::memory::{Memory, MemoryCategory, MemoryEntry};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct TestMemory {
        entries: Mutex<HashMap<String, MemoryEntry>>,
    }

    #[async_trait]
    impl Memory for TestMemory {
        fn name(&self) -> &str {
            "test"
        }

        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> Result<()> {
            self.entries.lock().unwrap().insert(
                key.to_string(),
                MemoryEntry {
                    id: key.to_string(),
                    key: key.to_string(),
                    content: content.to_string(),
                    category,
                    timestamp: "2026-03-10T00:00:00Z".to_string(),
                    session_id: session_id.map(str::to_string),
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: Some(0),
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                },
            );
            Ok(())
        }

        async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
            Ok(self.entries.lock().unwrap().get(key).cloned())
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(self.entries.lock().unwrap().values().cloned().collect())
        }

        async fn forget(&self, key: &str) -> Result<bool> {
            Ok(self.entries.lock().unwrap().remove(key).is_some())
        }

        async fn count(&self) -> Result<usize> {
            Ok(self.entries.lock().unwrap().len())
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    fn router_config() -> RouterConfig {
        RouterConfig {
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 0.1,
            epsilon: 0.1,
            knn_min_records: 10,
            knn_k: 7,
            automix: crate::config::AutomixConfig::default(),
            models: vec![
                RouterModelConfig {
                    model_id: "model-a".to_string(),
                    provider: "openai".to_string(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 1_000,
                    categories: vec!["analysis".to_string()],
                    elo_rating: 1_000.0,
                },
                RouterModelConfig {
                    model_id: "model-b".to_string(),
                    provider: "openai".to_string(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 1_000,
                    categories: vec!["analysis".to_string()],
                    elo_rating: 1_000.0,
                },
            ],
        }
    }

    #[derive(Clone)]
    struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_record_outcome_updates_elo() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let router = RouterEngine::new(
            router_config(),
            "openai".into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap();

        router
            .record_outcome("hello analysis", "model-a", true, 1_600)
            .await
            .unwrap();

        let elo = memory.get("router/elo/model-a").await.unwrap().expect("elo persisted");
        assert_eq!(
            elo.session_id.as_deref(),
            Some(crate::self_system::SELF_SYSTEM_SESSION_ID)
        );
        let elo_snapshot: serde_json::Value = serde_json::from_str(&elo.content).unwrap();
        let updated_elo = elo_snapshot["dynamic_elo"].as_f64().unwrap() as f32;
        assert!(updated_elo > 1_000.0);

        let success_events: Vec<_> = memory
            .list(
                Some(&MemoryCategory::Custom("router".into())),
                Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
            )
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.key.starts_with("router/success/model-a/"))
            .collect();
        assert_eq!(success_events.len(), 1);

        let latency = memory
            .get("router/latency/model-a")
            .await
            .unwrap()
            .expect("latency persisted");
        assert_eq!(
            latency.session_id.as_deref(),
            Some(crate::self_system::SELF_SYSTEM_SESSION_ID)
        );
        let latency_snapshot: serde_json::Value = serde_json::from_str(&latency.content).unwrap();
        assert_eq!(latency_snapshot["recent_latency_ms"].as_u64().unwrap(), 1_180);
    }

    #[test]
    fn test_record_outcome_no_await_under_write_lock() {
        let mut models = vec![
            ModelCapabilityEntry {
                config: RouterModelConfig {
                    model_id: "model-a".to_string(),
                    provider: "openai".to_string(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 1_000,
                    categories: vec!["analysis".to_string()],
                    elo_rating: 1_000.0,
                },
                dynamic_elo: 1_000.0,
                success_rate: 0.5,
                recent_latency_ms: 1_000,
            },
            ModelCapabilityEntry {
                config: RouterModelConfig {
                    model_id: "model-b".to_string(),
                    provider: "openai".to_string(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 1_000,
                    categories: vec!["analysis".to_string()],
                    elo_rating: 1_050.0,
                },
                dynamic_elo: 1_050.0,
                success_rate: 0.8,
                recent_latency_ms: 900,
            },
        ];
        let mut recent_successes = VecDeque::from([true, false, true]);

        let snapshot = apply_outcome_update_locked(&mut models, "model-a", true, 1_600, &mut recent_successes);

        assert!(snapshot.is_some());
        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.model_id, "model-a");
        assert_eq!(recent_successes.len(), 4);
        assert!(models[0].dynamic_elo > 1_000.0);
        assert_eq!(models[0].recent_latency_ms, 1_180);
    }

    #[tokio::test]
    async fn test_success_rate_window() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let router = RouterEngine::new(
            router_config(),
            "openai".into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap();

        for _ in 0..100 {
            router
                .record_outcome("hello analysis", "model-a", true, 1_000)
                .await
                .unwrap();
        }
        router
            .record_outcome("hello analysis", "model-a", false, 1_000)
            .await
            .unwrap();

        let recent_successes = load_recent_successes(memory.as_ref(), "model-a").await;
        assert_eq!(recent_successes.len(), 100);
        assert_eq!(recent_successes.front().copied(), Some(true));
        assert_eq!(recent_successes.back().copied(), Some(false));
    }

    #[tokio::test]
    async fn test_latency_ema() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let router = RouterEngine::new(
            router_config(),
            "openai".into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap();

        router
            .record_outcome("hello analysis", "model-a", true, 1_600)
            .await
            .unwrap();
        router
            .record_outcome("hello analysis", "model-a", true, 2_000)
            .await
            .unwrap();

        let latency = memory
            .get("router/latency/model-a")
            .await
            .unwrap()
            .expect("latency persisted");
        let latency_snapshot: serde_json::Value = serde_json::from_str(&latency.content).unwrap();
        assert_eq!(latency_snapshot["recent_latency_ms"].as_u64().unwrap(), 1_426);
    }

    struct FixedEmbeddingProvider;

    #[async_trait]
    impl EmbeddingProvider for FixedEmbeddingProvider {
        fn name(&self) -> &str {
            "test-fixed"
        }

        fn dimensions(&self) -> usize {
            3
        }

        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0_f32, 0.0_f32, 0.0_f32]).collect())
        }
    }

    struct SlowEmbeddingProvider;

    #[async_trait]
    impl EmbeddingProvider for SlowEmbeddingProvider {
        fn name(&self) -> &str {
            "test-slow"
        }

        fn dimensions(&self) -> usize {
            3
        }

        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(texts.iter().map(|_| vec![1.0_f32, 0.0_f32, 0.0_f32]).collect())
        }
    }

    #[tokio::test]
    async fn test_knn_cold_start() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let store = KnnStore::new(Arc::clone(&memory)).unwrap();
        let history = RouterHistory::new(store, Arc::new(FixedEmbeddingProvider), 7, 10);

        for index in 0..9 {
            history
                .record_query("same query", &format!("model-{index}"), true)
                .await
                .unwrap();
        }

        assert_eq!(history.similarity_score("same query", "model-1").await, 0.0);
    }

    #[test]
    fn test_majority_vote() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let store = KnnStore::new(memory).unwrap();
        let voted = store.majority_vote(&[
            ("model-a".to_string(), 0.1),
            ("model-a".to_string(), 0.2),
            ("model-b".to_string(), 0.3),
        ]);

        assert_eq!(voted.map(|value| value.0), Some("model-a".to_string()));
    }

    #[test]
    fn test_distance_weighted_vote() {
        let neighbors = vec![
            ("model-a".to_string(), 0.01),
            ("model-b".to_string(), 0.20),
            ("model-b".to_string(), 0.30),
        ];

        let model_a_score = super::knn::weighted_model_score(&neighbors, "model-a");
        let model_b_score = super::knn::weighted_model_score(&neighbors, "model-b");

        assert!(model_a_score > model_b_score);
    }

    #[tokio::test]
    async fn test_knn_timeout_fallback() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let store = KnnStore::new(Arc::clone(&memory)).unwrap();
        let history =
            RouterHistory::new(store, Arc::new(SlowEmbeddingProvider), 7, 10).with_timeout(Duration::from_millis(10));
        let mut config = router_config();
        config.alpha = 1.0;
        let router =
            RouterEngine::new_with_history(config, "openai".into(), Vec::new(), Arc::clone(&memory), Some(history));

        let result = router.select_model("same query", &TaskIntent::Simple).await;

        assert!(result.chosen_model.is_some());
        assert!(
            result
                .candidates
                .iter()
                .all(|candidate| candidate.similarity_score == 0.0)
        );
    }

    #[tokio::test]
    async fn test_load_all_keeps_only_reachable_models() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let config = RouterConfig {
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 0.1,
            epsilon: 0.1,
            knn_min_records: 10,
            knn_k: 7,
            automix: crate::config::AutomixConfig::default(),
            models: vec![
                RouterModelConfig {
                    model_id: "claude-sonnet".into(),
                    provider: "anthropic".into(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 1_000,
                    categories: vec!["analysis".into()],
                    elo_rating: 1_100.0,
                },
                RouterModelConfig {
                    model_id: "gpt-4.1".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 1_000,
                    categories: vec!["analysis".into()],
                    elo_rating: 1_500.0,
                },
            ],
        };
        let router = RouterEngine::new(
            config,
            "anthropic".into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap();

        let models = router.models.read();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].config.provider, "anthropic");
    }

    /// INT-2B USERINFO-3, RED before: the router logged provider labels raw,
    /// so a `custom:` URL with `user:password@` reached the log in the
    /// no-reachable-models warning, the decision line (including its
    /// candidate list) and the outcome line (a `<label>/<model>` target).
    /// After: every one is redacted, and the lines still name the endpoint.
    /// The unreachable-selection warning is redacted the same way; ranking
    /// only sees reachable models, so no test reaches it.
    #[tokio::test]
    async fn router_logs_never_carry_provider_url_userinfo() {
        // Log-capturing tests share per-callsite interest process-wide.
        let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
        const LABEL: &str = "custom:http://u:ROUTER-SECRET-pw@127.0.0.1:1/v1";
        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer({
                let logs = Arc::clone(&logs);
                move || SharedLogWriter(Arc::clone(&logs))
            })
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        let model = |provider: &str| RouterModelConfig {
            model_id: "model-a".into(),
            provider: provider.into(),
            cost_per_million_tokens: 1.0,
            max_context: 128_000,
            reserved_output_tokens: None,
            latency_ms: 1_000,
            categories: vec!["analysis".into()],
            elo_rating: 1_000.0,
        };

        // No reachable model: the default provider and the reachable list.
        let mut unreachable = router_config();
        unreachable.models = vec![model("openai")];
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let _ = RouterEngine::new(
            unreachable,
            LABEL.into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap();

        // A decision among candidates on the credentialed endpoint.
        let mut reachable = router_config();
        reachable.models = vec![model(LABEL)];
        let router = RouterEngine::new(
            reachable,
            LABEL.into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap();
        let decision = router.select_model("hello analysis", &TaskIntent::Simple).await;

        // An outcome for the credentialed model target.
        router
            .record_outcome_detailed(
                "hello analysis",
                &format!("{LABEL}/model-a"),
                &format!("{LABEL}/model-a"),
                false,
                true,
                10,
            )
            .await
            .unwrap();

        let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        // The decision's candidate list is checked on the structured value
        // the log line prints (deterministic); the log capture can miss
        // events another test's subscriber shaped, so it is checked for
        // absence only. Each log site is pinned by the architecture guard
        // `provider_labels_never_reach_a_log_field_raw`.
        let (candidates, _) = decision_log_candidates(&decision);
        let shown = serde_json::to_string(&candidates).unwrap();
        assert!(shown.contains("[redacted]@127.0.0.1:1"), "{shown}");
        assert!(!shown.contains("ROUTER-SECRET-pw"), "{shown}");
        assert!(!output.contains("ROUTER-SECRET-pw"), "{output}");
    }

    #[tokio::test]
    async fn test_router_stays_available_when_no_reachable_models() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let mut config = router_config();
        config.models = vec![RouterModelConfig {
            model_id: "gpt-4.1".into(),
            provider: "openai".into(),
            cost_per_million_tokens: 1.0,
            max_context: 128_000,
            reserved_output_tokens: None,
            latency_ms: 1_000,
            categories: vec!["analysis".into()],
            elo_rating: 1_000.0,
        }];

        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer({
                let logs = Arc::clone(&logs);
                move || SharedLogWriter(Arc::clone(&logs))
            })
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let router = RouterEngine::new(
            config,
            "anthropic".into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap();

        let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(output.contains("no reachable models"));
        assert!(router.models.read().is_empty());
    }

    #[tokio::test]
    async fn local_history_stays_available_when_no_reachable_models() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let mut config = router_config();
        config.models = vec![RouterModelConfig {
            model_id: "gpt-4.1".into(),
            provider: "openai".into(),
            cost_per_million_tokens: 1.0,
            max_context: 128_000,
            reserved_output_tokens: None,
            latency_ms: 1_000,
            categories: vec!["analysis".into()],
            elo_rating: 1_000.0,
        }];

        let router = RouterEngine::new(
            config,
            "anthropic".into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap()
        .with_local_hash_history(Arc::new(crate::memory::embeddings::LocalHashEmbedding::new(
            "prx-local-hash-v1",
            384,
        )))
        .expect("an empty reachable candidate set falls back to the primary model");

        assert!(router.has_history());
        assert!(router.models.read().is_empty());
    }

    #[tokio::test]
    async fn test_select_model_returns_none_for_unreachable() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let config = RouterConfig {
            models: vec![RouterModelConfig {
                model_id: "gpt-4.1".into(),
                provider: "openai".into(),
                cost_per_million_tokens: 0.1,
                max_context: 128_000,
                reserved_output_tokens: None,
                latency_ms: 500,
                categories: vec!["conversation".into()],
                elo_rating: 1_500.0,
            }],
            ..RouterConfig::default()
        };
        let router = RouterEngine {
            configured_inputs: config.clone(),
            config,
            default_provider: "anthropic".into(),
            model_routes: Vec::new(),
            models: RwLock::new(vec![ModelCapabilityEntry {
                config: RouterModelConfig {
                    model_id: "gpt-4.1".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 0.1,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 500,
                    categories: vec!["conversation".into()],
                    elo_rating: 1_500.0,
                },
                dynamic_elo: 1_500.0,
                success_rate: 1.0,
                recent_latency_ms: 500,
            }]),
            memory,
            history: None,
            local_learning: tokio::sync::Mutex::new(()),
            local_slot: RwLock::new(None),
        };

        let result = router.select_model("hello", &TaskIntent::Simple).await;

        assert!(result.chosen_model.is_none());
        assert!(result.chosen_provider.is_none());
        assert_eq!(result.score, 0.0);
    }

    #[tokio::test]
    async fn test_select_model_prefers_reachable_candidates() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let config = RouterConfig {
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 0.1,
            epsilon: 0.1,
            knn_min_records: 10,
            knn_k: 7,
            automix: crate::config::AutomixConfig::default(),
            models: vec![
                RouterModelConfig {
                    model_id: "claude-sonnet".into(),
                    provider: "anthropic".into(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 1_000,
                    categories: vec!["analysis".into()],
                    elo_rating: 1_000.0,
                },
                RouterModelConfig {
                    model_id: "gpt-4.1".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 1_000,
                    categories: vec!["analysis".into()],
                    elo_rating: 2_000.0,
                },
            ],
        };
        let router = RouterEngine::new(
            config,
            "anthropic".into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap();

        let result = router.select_model("hello analysis", &TaskIntent::Simple).await;
        assert_eq!(result.chosen_provider.as_deref(), Some("anthropic"));
        assert_eq!(result.chosen_model.as_deref(), Some("claude-sonnet"));
        assert!(
            result
                .candidates
                .iter()
                .all(|candidate| candidate.provider == "anthropic")
        );
    }

    #[tokio::test]
    async fn test_record_outcome_updates_only_chosen_model_elo() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let config = RouterConfig {
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 1.0,
            epsilon: 0.3,
            knn_min_records: 10,
            knn_k: 7,
            automix: crate::config::AutomixConfig {
                confidence_threshold: 0.7,
                cheap_model_tiers: vec!["cheap".into()],
                premium_model_id: "openai/model-premium".into(),
            },
            models: vec![
                RouterModelConfig {
                    model_id: "model-cheap".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 0.1,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 500,
                    categories: vec!["conversation".into()],
                    elo_rating: 1_000.0,
                },
                RouterModelConfig {
                    model_id: "model-premium".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 10.0,
                    max_context: 128_000,
                    reserved_output_tokens: None,
                    latency_ms: 2_000,
                    categories: vec!["conversation".into()],
                    elo_rating: 1_000.0,
                },
            ],
        };
        let router = RouterEngine::new(
            config,
            "openai".into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap();

        router.record_outcome("hello", "model-cheap", true, 500).await.unwrap();

        let cheap_elo = memory
            .get("router/elo/model-cheap")
            .await
            .unwrap()
            .expect("cheap elo persisted");
        let cheap_snapshot: serde_json::Value = serde_json::from_str(&cheap_elo.content).unwrap();
        assert!(cheap_snapshot["dynamic_elo"].as_f64().unwrap() > 1_000.0);

        assert!(memory.get("router/elo/model-premium").await.unwrap().is_none());

        let router_entries = memory
            .list(
                Some(&MemoryCategory::Custom("router".into())),
                Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
            )
            .await
            .unwrap();
        assert!(
            router_entries
                .iter()
                .any(|entry| entry.key.starts_with("router/success/model-cheap/"))
        );
        assert!(
            router_entries
                .iter()
                .all(|entry| !entry.key.starts_with("router/success/model-premium/"))
        );
    }
}
