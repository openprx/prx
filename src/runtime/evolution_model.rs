//! Generation-pinned evolution model calls. The daemon owns configuration,
//! provider construction and cleanup; generated text never acquires authority.
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::decision_accounting::{AccountingScope, OwnedReceipt, PhysicalEstimate};
use crate::config::Config;
use crate::providers::attempt_accounting::{self, AttemptAccounting, AttemptReceipt, PhysicalRequest};
use crate::skills::lifecycle::{
    ManagedProviderModel, ModelBinding, ModelCallError, ModelReply, ProposerModel, TallyCounters,
};
use async_trait::async_trait;

#[derive(Clone, Copy)]
pub(crate) enum EvolutionPurpose {
    Proposer,
    Judge,
}
impl EvolutionPurpose {
    const fn label(self) -> &'static str {
        match self {
            Self::Proposer => "evolution_proposer",
            Self::Judge => "evolution_judge",
        }
    }
}

/// Constructed only by the runtime from the configured default selection.
/// Each complete call receives a fresh bounded scope; retries share that scope.
pub(crate) struct EvolutionModel {
    config: Arc<Config>,
    provider: Arc<dyn crate::providers::Provider>,
    binding: ModelBinding,
    purpose: EvolutionPurpose,
    key: Arc<crate::decision::DecisionKey>,
}
impl EvolutionModel {
    pub(crate) fn from_config(config: &Config, purpose: EvolutionPurpose) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            config.self_system.evolution_model_max_tokens > 0,
            "evolution model calls are disabled"
        );
        let resolved =
            crate::providers::selection::check_default(config, crate::providers::catalog::UnknownPolicy::Invocation)?;
        let catalog = crate::providers::selection::catalog_for(config);
        let provider = crate::providers::create_provider_for(
            &resolved,
            &catalog,
            &config.reliability,
            &config.cost,
            &crate::providers::provider_runtime_options_from_config(config),
        )?;
        anyhow::ensure!(
            provider.supports_attempt_accounting(),
            "evolution provider has no physical attempt accounting"
        );
        anyhow::ensure!(
            provider.accounting_targets(resolved.model()).is_some(),
            "evolution provider has no transport binding"
        );
        Ok(Arc::new(Self {
            key: crate::decision::DecisionKey::load_or_create(&config.workspace_dir)?,
            config: Arc::new(config.clone()),
            provider: Arc::from(provider),
            purpose,
            binding: ModelBinding {
                provider_label: resolved.provider_label().to_owned(),
                model: resolved.model().to_owned(),
                temperature_milli: 100,
            },
        }))
    }

    fn call_model(&self) -> anyhow::Result<(ManagedProviderModel, String)> {
        let cleanup = super::turn_cleanup::current_tracker()
            .ok_or_else(|| anyhow::anyhow!("evolution call has no runtime cleanup owner"))?;
        let key = Arc::clone(&self.key);
        let call_id = uuid::Uuid::new_v4().to_string();
        let scope_id = key.hmac(
            crate::decision::KeyDomain::TerminalClaim,
            &[
                b"evolution-model-call-v1",
                self.purpose.label().as_bytes(),
                call_id.as_bytes(),
                self.config.workspace_dir.to_string_lossy().as_bytes(),
                self.binding.digest().as_bytes(),
            ],
        );
        let duration = Duration::from_secs(self.config.self_system.evolution_judge_timeout_secs);
        let scope = Arc::new(AccountingScope::new(
            Arc::clone(&self.config),
            Arc::clone(&key),
            cleanup,
            scope_id,
            crate::causal_tree::state::TurnBudgets {
                tokens: self.config.self_system.evolution_model_max_tokens,
                micro_usd: self.config.self_system.evolution_model_max_micro_usd,
                latency_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            },
            Instant::now() + duration,
        ));
        let targets = self
            .provider
            .accounting_targets(&self.binding.model)
            .ok_or_else(|| anyhow::anyhow!("evolution transport binding is unavailable"))?;
        let accounting_scope_id = scope.scope_id();
        Ok((
            ManagedProviderModel {
                provider: Arc::clone(&self.provider),
                binding: self.binding.clone(),
                accounting: Arc::new(EvolutionAccounting {
                    targets,
                    key,
                    scope,
                    provider_label: self.binding.provider_label.clone(),
                    purpose: self.purpose,
                }),
            },
            accounting_scope_id,
        ))
    }
}
#[async_trait]
impl ProposerModel for EvolutionModel {
    fn binding(&self) -> &ModelBinding {
        &self.binding
    }
    async fn complete(
        &self,
        system: &str,
        user: &str,
        tally: Arc<TallyCounters>,
    ) -> Result<ModelReply, ModelCallError> {
        let (model, scope_id) = self.call_model().map_err(|_| ModelCallError::BudgetDenied)?;
        let mut reply = model.complete(system, user, tally).await?;
        reply.accounting_scope_id = Some(scope_id);
        Ok(reply)
    }
}

struct EvolutionAccounting {
    targets: crate::providers::AccountingTargets,
    key: Arc<crate::decision::DecisionKey>,
    scope: Arc<AccountingScope>,
    provider_label: String,
    purpose: EvolutionPurpose,
}
impl attempt_accounting::sealed::AccountingOwner for EvolutionAccounting {}
#[async_trait]
impl AttemptAccounting for EvolutionAccounting {
    fn deadline(&self) -> Instant {
        self.scope.deadline()
    }
    async fn begin(&self, request: PhysicalRequest<'_>) -> anyhow::Result<Box<dyn AttemptReceipt>> {
        use attempt_accounting::{AccountingRefusal, PhysicalKind};
        anyhow::ensure!(request.kind() == PhysicalKind::Chat, AccountingRefusal);
        let (provider, model) = self
            .targets
            .match_request(&request, &self.provider_label)
            .ok_or(AccountingRefusal)?;
        let binding = self.key.hmac(
            crate::decision::KeyDomain::AttemptAccounting,
            &[
                b"evolution-physical-chat-v1",
                self.purpose.label().as_bytes(),
                provider.as_bytes(),
                model.as_bytes(),
                request.endpoint().as_bytes(),
                &[u8::from(request.credential().is_some())],
                request.credential().unwrap_or("").as_bytes(),
            ],
        );
        let receipt = self
            .scope
            .begin_physical(PhysicalEstimate {
                purpose: self.purpose.label(),
                provider,
                model,
                body: request.body(),
                transport_binding: &binding,
                request_id: self.purpose.label(),
                deadline: self.scope.deadline(),
                remaining_attempts: 1,
            })
            .await?;
        Ok(Box::new(EvolutionReceipt(receipt)))
    }
}
struct EvolutionReceipt(OwnedReceipt);
impl attempt_accounting::sealed::Receipt for EvolutionReceipt {}
#[async_trait]
impl AttemptReceipt for EvolutionReceipt {
    fn deadline(&self) -> Instant {
        crate::decision::budget::DecisionAttemptReceipt::deadline(&self.0)
    }
    async fn settle(self: Box<Self>, usage: crate::llm::route_decision::TokenUsage) -> anyhow::Result<()> {
        Box::new(self.0).settle_provider_usage(usage).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
    use crate::runtime::turn_cleanup::CleanupScope;
    use tokio_util::sync::CancellationToken;

    fn config(workspace: &std::path::Path, provider: &str) -> Config {
        let mut config = Config {
            workspace_dir: workspace.to_path_buf(),
            config_path: workspace.join("config.toml"),
            default_provider: Some(provider.into()),
            default_model: Some(PROBE_MODEL.into()),
            ..Config::default()
        };
        config.reliability.provider_retries = 0;
        config.reliability.provider_backoff_ms = 0;
        config.auth.codex_auth_json_auto_import = false;
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": provider, "model": PROBE_MODEL, "operations": ["chat"], "native_tools": false
            }))
            .unwrap(),
        );
        config.cost.prices.insert(
            format!("{provider}/{PROBE_MODEL}"),
            crate::config::schema::ModelPricing {
                input: 0.0,
                output: 0.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        config
    }

    #[tokio::test]
    async fn evolution_model_reserves_and_settles_each_call_with_distinct_identity() -> anyhow::Result<()> {
        let workspace = tempfile::tempdir()?;
        let probe = CredentialProbe::start().await;
        let model = EvolutionModel::from_config(
            &config(workspace.path(), &probe.custom_provider()),
            EvolutionPurpose::Proposer,
        )?;
        CleanupScope::new(None, CancellationToken::new())?
            .run(async {
                for _ in 0..2 {
                    let reply = model
                        .complete(
                            "Test data only.",
                            "Return a fixture.",
                            Arc::new(TallyCounters::default()),
                        )
                        .await?;
                    assert_eq!(reply.usage.physical_requests, 1);
                    assert_eq!(reply.usage.settled_requests, 1);
                    assert!(reply.usage.reported);
                }
                Ok(())
            })
            .await?;
        assert_eq!(probe.requests().len(), 2);
        let rows = std::fs::read_to_string(workspace.path().join("state/costs.reservations.jsonl"))?;
        assert_eq!(rows.matches("estimated_settled_v1").count(), 2);
        assert!(rows.contains("evolution_proposer"));
        let values: Vec<serde_json::Value> = rows.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
        let ids: std::collections::BTreeSet<String> = values
            .iter()
            .filter_map(|row| {
                row.pointer("/request/reservation/turn_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        // The journal's attempt IDs must distinguish separate completed calls.
        assert!(ids.len() >= 2, "separate calls must not reuse a reservation: {rows}");
        Ok(())
    }

    #[tokio::test]
    async fn evolution_model_zero_money_rejects_priced_calls_before_transport() -> anyhow::Result<()> {
        let workspace = tempfile::tempdir()?;
        let probe = CredentialProbe::start().await;
        let mut cfg = config(workspace.path(), &probe.custom_provider());
        cfg.cost.prices.values_mut().for_each(|price| {
            price.input = 1.0;
            price.output = 1.0;
        });
        let model = EvolutionModel::from_config(&cfg, EvolutionPurpose::Proposer)?;
        CleanupScope::new(None, CancellationToken::new())?
            .run(async {
                let error = model
                    .complete(
                        "Test data only.",
                        "Return a fixture.",
                        Arc::new(TallyCounters::default()),
                    )
                    .await
                    .unwrap_err();
                assert!(matches!(error, ModelCallError::BudgetDenied));
                Ok(())
            })
            .await?;
        assert!(probe.requests().is_empty());
        assert!(!workspace.path().join("state/costs.reservations.jsonl").exists());
        Ok(())
    }

    #[tokio::test]
    async fn disabled_evolution_model_has_no_provider_or_key_side_effect() {
        let workspace = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            workspace_dir: workspace.path().to_path_buf(),
            ..Config::default()
        };
        cfg.self_system.evolution_model_max_tokens = 0;
        assert!(EvolutionModel::from_config(&cfg, EvolutionPurpose::Proposer).is_err());
        assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
    }
}
