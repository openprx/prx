//! Decision publication belongs to the same immutable gateway generation as
//! provider/model/tools. Initialization is shared by concurrent first turns.
//! The service is published again when the installed skills change, so a skill
//! created while the gateway runs is offered by the next turn.
use crate::runtime::turn_publication::TurnRuntimeService;
use std::sync::Arc;

pub(in crate::gateway) struct GatewayPublicationSlot {
    pub(in crate::gateway) controlled: Option<Arc<crate::runtime::h2_entry::ControlledH2Profile>>,
    pub(in crate::gateway) cleanup: Arc<super::CleanupTracker>,
    service: tokio::sync::Mutex<Option<Arc<TurnRuntimeService>>>,
    security: Arc<crate::security::SecurityPolicy>,
    plugin_generation: u64,
    /// Set only for a targeted agent turn.
    targeted: Option<crate::runtime::turn_publication::TargetedSurface>,
}

impl GatewayPublicationSlot {
    pub(in crate::gateway) fn new(security: Arc<crate::security::SecurityPolicy>, plugin_generation: u64) -> Self {
        Self {
            controlled: None,
            cleanup: Arc::new(super::CleanupTracker::default()),
            service: tokio::sync::Mutex::new(None),
            security,
            plugin_generation,
            targeted: None,
        }
    }

    /// A slot for one targeted agent turn: the same cleanup owner and
    /// generation as `self`, its own capped security policy and tool surface,
    /// and its own decision service (never the host's published one).
    pub(in crate::gateway) fn targeted(
        &self,
        security: Arc<crate::security::SecurityPolicy>,
        surface: crate::runtime::turn_publication::TargetedSurface,
    ) -> Self {
        Self {
            controlled: self.controlled.clone(),
            cleanup: Arc::clone(&self.cleanup),
            service: tokio::sync::Mutex::new(None),
            security,
            plugin_generation: self.plugin_generation,
            targeted: Some(surface),
        }
    }

    /// The same controlled profile, cleanup owner, security policy and
    /// targeting as `self`, with an empty decision service. A generation-only
    /// swap uses it so the next publication is built from the generation it
    /// is given (the child process binding is sealed against that generation).
    pub(in crate::gateway) fn republished(&self) -> Self {
        Self {
            controlled: self.controlled.clone(),
            cleanup: Arc::clone(&self.cleanup),
            service: tokio::sync::Mutex::new(None),
            security: Arc::clone(&self.security),
            plugin_generation: self.plugin_generation,
            targeted: self.targeted.clone(),
        }
    }

    pub(in crate::gateway) fn with_controlled(
        mut self,
        profile: Option<Arc<crate::runtime::h2_entry::ControlledH2Profile>>,
    ) -> Self {
        self.controlled = profile;
        self
    }

    pub(in crate::gateway) async fn publish(
        &self,
        runtime: &crate::gateway::TurnRuntimeGeneration,
        state: &crate::gateway::AppState,
    ) -> anyhow::Result<Arc<TurnRuntimeService>> {
        self.publish_parts(
            &runtime.config_generation,
            &runtime.tools_registry,
            runtime.mcp_tool.as_ref(),
            state,
        )
        .await
    }

    /// [`publish`](Self::publish) from the parts a generation without a chat
    /// connection also holds.
    pub(in crate::gateway) async fn publish_parts(
        &self,
        generation: &Arc<crate::config::ConfigGeneration>,
        registry: &Arc<Vec<Box<dyn crate::tools::Tool>>>,
        mcp_tool: Option<&Arc<crate::tools::McpTool>>,
        state: &crate::gateway::AppState,
    ) -> anyhow::Result<Arc<TurnRuntimeService>> {
        if let Some(profile) = &self.controlled {
            profile.verify(&generation.effective)?;
        }
        let mut published = self.service.lock().await;
        if let Some(service) = published.as_ref()
            && !service.needs_republish()
        {
            return Ok(Arc::clone(service));
        }
        let service = crate::runtime::turn_publication::TurnPublication {
            generation: Arc::clone(generation),
            registry: Arc::clone(registry),
            mcp_tool: mcp_tool.cloned(),
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
        if let Some(previous) = published.replace(Arc::clone(&service)) {
            self.retire(previous);
        }
        Ok(service)
    }

    /// Drain a service's decision writes in the background and register the
    /// retirement with the cleanup owner. Turns that opened on it keep it alive.
    fn retire(&self, service: Arc<TurnRuntimeService>) {
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

#[cfg(test)]
impl Default for GatewayPublicationSlot {
    fn default() -> Self {
        Self::new(Arc::new(crate::security::SecurityPolicy::default()), 0)
    }
}

impl Drop for GatewayPublicationSlot {
    fn drop(&mut self) {
        if let Some(service) = self.service.get_mut().take() {
            self.retire(service);
        }
    }
}

pub(in crate::gateway) use crate::runtime::turn_terminal::TurnTerminalGuard as GatewayTerminalGuard;

pub(in crate::gateway) use crate::runtime::turn_terminal::commit_terminal;
