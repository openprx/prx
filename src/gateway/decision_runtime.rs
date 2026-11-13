//! Decision publication belongs to the same immutable gateway generation as
//! provider/model/tools. Initialization is shared by concurrent first turns.
use crate::runtime::turn_decision::TurnDecisionService;
use std::sync::Arc;

pub(super) struct GatewayDecisionSlot {
    pub(super) controlled: Option<Arc<crate::runtime::h2_entry::ControlledH2Profile>>,
    pub(super) cleanup: Arc<super::decision_cleanup::CleanupTracker>,
    service: tokio::sync::OnceCell<Arc<TurnDecisionService>>,
    security: Arc<crate::security::SecurityPolicy>,
    plugin_generation: u64,
    /// Set only for a targeted agent turn.
    targeted: Option<crate::runtime::turn_publication::TargetedSurface>,
}

impl GatewayDecisionSlot {
    pub(super) fn new(security: Arc<crate::security::SecurityPolicy>, plugin_generation: u64) -> Self {
        Self {
            controlled: None,
            cleanup: Arc::new(super::decision_cleanup::CleanupTracker::default()),
            service: tokio::sync::OnceCell::new(),
            security,
            plugin_generation,
            targeted: None,
        }
    }

    /// A slot for one targeted agent turn: the same cleanup owner and
    /// generation as `self`, its own capped security policy and tool surface,
    /// and its own decision service (never the host's published one).
    pub(super) fn targeted(
        &self,
        security: Arc<crate::security::SecurityPolicy>,
        surface: crate::runtime::turn_publication::TargetedSurface,
    ) -> Self {
        Self {
            controlled: self.controlled.clone(),
            cleanup: Arc::clone(&self.cleanup),
            service: tokio::sync::OnceCell::new(),
            security,
            plugin_generation: self.plugin_generation,
            targeted: Some(surface),
        }
    }

    pub(super) fn with_controlled(
        mut self,
        profile: Option<Arc<crate::runtime::h2_entry::ControlledH2Profile>>,
    ) -> Self {
        self.controlled = profile;
        self
    }

    pub(super) async fn publish(
        &self,
        runtime: &super::TurnRuntimeGeneration,
        state: &super::AppState,
    ) -> anyhow::Result<&Arc<TurnDecisionService>> {
        if let Some(profile) = &self.controlled {
            profile.verify(&runtime.config_generation.effective)?;
        }
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
                .publish_scoped(
                    crate::tools::capability::turn_state::TurnSurfaceKind::Server,
                    None,
                    self.targeted.clone(),
                )
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

pub(super) use crate::runtime::turn_terminal::TurnTerminalGuard as GatewayTerminalGuard;

pub(super) use crate::runtime::turn_terminal::commit_terminal;
