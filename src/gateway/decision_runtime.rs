//! Decision publication belongs to the same immutable gateway generation as
//! provider/model/tools. Initialization is shared by concurrent first turns.
use crate::runtime::turn_decision::TurnDecisionService;
use std::sync::Arc;

pub(super) struct GatewayDecisionSlot {
    pub(super) cleanup: Arc<super::decision_cleanup::CleanupTracker>,
    service: tokio::sync::OnceCell<Arc<TurnDecisionService>>,
    security: Arc<crate::security::SecurityPolicy>,
    plugin_generation: u64,
}

impl GatewayDecisionSlot {
    pub(super) fn new(security: Arc<crate::security::SecurityPolicy>, plugin_generation: u64) -> Self {
        Self {
            cleanup: Arc::new(super::decision_cleanup::CleanupTracker::default()),
            service: tokio::sync::OnceCell::new(),
            security,
            plugin_generation,
        }
    }

    pub(super) async fn publish(
        &self,
        runtime: &super::TurnRuntimeGeneration,
        state: &super::AppState,
    ) -> anyhow::Result<&Arc<TurnDecisionService>> {
        self.service
            .get_or_try_init(|| async {
                let service = crate::runtime::turn_publication::TurnDecisionPublication {
                    generation: Arc::clone(&runtime.config_generation),
                    registry: Arc::clone(&runtime.tools_registry),
                    mcp_tool: runtime.mcp_tool.clone(),
                    security: Arc::clone(&self.security),
                    observer: Arc::clone(&state.observer),
                    hooks: Arc::clone(&state.hooks),
                    memory: Arc::clone(&state.mem),
                    plugin_generation: self.plugin_generation,
                }
                .publish(crate::causal_tree::state::TurnSurfaceKind::Server)
                .await?;
                self.cleanup.retain_service(Arc::clone(&service));
                Ok(service)
            })
            .await
    }
}

#[cfg(test)]
impl Default for GatewayDecisionSlot {
    fn default() -> Self {
        Self::new(Arc::new(crate::security::SecurityPolicy::default()), 0)
    }
}

impl Drop for GatewayDecisionSlot {
    fn drop(&mut self) {
        let Some(service) = self.service.get().cloned() else {
            return;
        };
        let ticket = match self.cleanup.retire(&service) {
            Ok(ticket) => ticket,
            Err(error) => {
                tracing::error!(%error, "Gateway decision queue retirement was not registered");
                return;
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let drained = service
                        .drain_writes(std::time::Instant::now() + std::time::Duration::from_secs(2))
                        .await;
                    ticket.finish(drained);
                });
            }
            Err(_) => tracing::warn!("Gateway decision generation dropped without a runtime to drain writes"),
        }
    }
}

pub(super) struct GatewayDecisionClose<'a> {
    pub(super) resource: &'static str,
    pub(super) history_commit_len: usize,
    pub(super) terminal_guard: &'a GatewayTerminalGuard,
    pub(super) service: &'a TurnDecisionService,
    pub(super) config: &'a crate::config::Config,
    pub(super) envelope: &'a crate::runtime::envelope::RuntimeEnvelope,
    pub(super) started_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait::async_trait]
impl crate::runtime::turn_decision::CloseTurnPorts for GatewayDecisionClose<'_> {
    type Receipt = crate::agent::terminal::TurnTerminalReceipt;
    async fn finalize_closed_turn(
        &self,
        context: Arc<crate::runtime::turn_decision::TurnDecisionContext>,
        reply: &str,
    ) -> anyhow::Result<Self::Receipt> {
        super::authorize_gateway_config_resource_mutation(self.config, self.resource, super::ResourceRiskLevel::Low)
            .map_err(|(_, body)| {
                anyhow::anyhow!(
                    "{}",
                    body.0
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("gateway resource mutation denied")
                )
            })?;
        let terminal_id = self
            .envelope
            .run_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("gateway close has no run"))?;
        let scope = self.envelope.message_scope();
        self.terminal_guard
            .commit(crate::agent::terminal::TurnTerminalDraft {
                decision: Some(self.service.terminal_settlement_ref(context)?),
                terminal_id,
                scope: scope.clone(),
                status: crate::agent::terminal::TurnTerminalStatus::Completed,
                history: Some(crate::agent::terminal::TurnHistoryProjection {
                    assistant_content: reply.to_owned(),
                    history_commit_len: self.history_commit_len,
                }),
                history_scope: (self.envelope.source != crate::runtime::envelope::RuntimeSource::Console).then(|| {
                    scope
                        .with_sender("prx_runtime")
                        .with_recipient(self.envelope.sender.clone().unwrap_or_default())
                }),
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: "gateway decision closed".to_owned(),
                    started_at: self.started_at,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
            })
            .await
    }
}

pub(super) use crate::runtime::turn_terminal::TurnTerminalGuard as GatewayTerminalGuard;

pub(super) use crate::runtime::turn_terminal::commit_terminal;
