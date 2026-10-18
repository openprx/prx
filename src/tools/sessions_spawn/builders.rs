use super::{AbortOnDrop, ParentToolRegistry, SessionsSpawnTool, SubAgentRun, process_terminal};
#[cfg(test)]
use super::{TestWorkerExecutable, TestWorkerProtocolControl, TestWorkerRecoveryResult, TestWorkerRouteCapture};
use crate::agent::turn_request::SpawnEventSink;
use crate::channels::traits::Channel;
use crate::config::{DelegateAgentConfig, MultimodalConfig, SessionsSpawnConfig};
use crate::memory::{Memory, MemoryEventRecording, MemoryFabric};
use crate::providers;
#[cfg(test)]
use crate::providers::Provider;
use crate::security::SecurityPolicy;
use crate::session_worker::protocol::config_source_generation;
use anyhow::Context as _;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

impl SessionsSpawnTool {
    pub(super) fn resolved_process_config_source(
        options: &providers::ProviderRuntimeOptions,
        pinned_config: Option<&crate::config::Config>,
    ) -> anyhow::Result<(PathBuf, String)> {
        let configured_dir = options
            .openprx_dir
            .as_ref()
            .context("sessions_spawn process mode requires the resolved parent config directory")?;
        let config_dir = std::fs::canonicalize(configured_dir)
            .with_context(|| format!("failed to resolve parent config directory {}", configured_dir.display()))?;
        if let Some(config) = pinned_config {
            let pinned_path = std::fs::canonicalize(&config.config_path)
                .context("failed to resolve pinned worker configuration source")?;
            let worker_path = std::fs::canonicalize(config_dir.join("config.toml"))
                .context("failed to resolve worker configuration source")?;
            anyhow::ensure!(
                pinned_path == worker_path,
                "worker configuration differs from pinned parent source"
            );
        }
        let generation = config_source_generation(&config_dir)?;
        Ok((config_dir, generation))
    }

    /// Create a new `SessionsSpawnTool` with the given channel and provider
    /// selection metadata. The admitted binding owns actual child providers.
    ///
    /// Thin wrapper over [`Self::new_with_registry`] that mints a fresh, empty
    /// `active_runs` registry owned solely by this tool. Behaviour is identical to
    /// the previous inline construction (channels/gateway call sites are
    /// unaffected).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel: Arc<dyn Channel>,
        #[cfg(test)] provider: Arc<dyn Provider>,
        provider_name: impl Into<String>,
        model: impl Into<String>,
        temperature: f64,
        security: Arc<SecurityPolicy>,
        workspace_dir: PathBuf,
        multimodal_config: MultimodalConfig,
        agents: HashMap<String, DelegateAgentConfig>,
        fallback_api_key: Option<String>,
        provider_runtime_options: providers::ProviderRuntimeOptions,
        spawn_config: SessionsSpawnConfig,
    ) -> Self {
        Self::new_with_registry(
            channel,
            #[cfg(test)]
            provider,
            provider_name,
            model,
            temperature,
            security,
            workspace_dir,
            multimodal_config,
            agents,
            fallback_api_key,
            provider_runtime_options,
            spawn_config,
            Arc::new(RwLock::new(Vec::new())),
        )
    }

    /// Create a new `SessionsSpawnTool` backed by a caller-provided `active_runs`
    /// registry.
    ///
    /// Identical to [`Self::new`] except the `active_runs` registry is injected
    /// rather than freshly minted, letting a single owner (e.g. chat) build one
    /// `Arc<RwLock<Vec<SubAgentRun>>>` and share it across `sessions_spawn`,
    /// `sessions_list`, `sessions_send`, `session_status`, and a side-channel
    /// handle — the single-source-of-truth registry for the chat session runtime.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_registry(
        channel: Arc<dyn Channel>,
        #[cfg(test)] provider: Arc<dyn Provider>,
        provider_name: impl Into<String>,
        model: impl Into<String>,
        _temperature: f64,
        security: Arc<SecurityPolicy>,
        workspace_dir: PathBuf,
        _multimodal_config: MultimodalConfig,
        agents: HashMap<String, DelegateAgentConfig>,
        fallback_api_key: Option<String>,
        _provider_runtime_options: providers::ProviderRuntimeOptions,
        _spawn_config: SessionsSpawnConfig,
        active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    ) -> Self {
        let provider_name: String = provider_name.into();
        let configured_providers = providers::ConfiguredProviders::default()
            .with_providers(std::iter::once(provider_name.as_str()))
            .with_providers(agents.values().map(|agent| agent.provider.as_str()));
        Self {
            channel: Arc::new(RwLock::new(channel)),
            #[cfg(test)]
            test_worker_executable: None,
            channels: Arc::new(HashMap::new()),
            #[cfg(test)]
            test_parent_provider: provider,
            fallback_api_key_owner: provider_name.clone(),
            fallback_uses_root_connection: true,
            provider_name,
            model: model.into(),
            security,
            default_recipient: Arc::new(RwLock::new(None)),
            active_runs,
            tools: Arc::new(ParentToolRegistry::default()),
            workspace_dir,
            configured_providers,
            model_catalog: Arc::default(),
            hint_config: None,
            default_route_source: None,
            agents: Arc::new(agents),
            fallback_api_key,
            cost_config: crate::config::schema::CostConfig::default(),
            memory: None,
            source_fabric: None,
            event_recording: MemoryEventRecording::default(),
            event_sink: None,
            approval_resolver_factory: None,
            batch_rosters: Arc::new(RwLock::new(Vec::new())),
            restart_recovery: None,
        }
    }

    /// One actual bounded recovery page, retaining every per-launch error.
    /// This exposes no grant, signing key, or alternate verification path.
    #[cfg(test)]
    pub(crate) async fn recover_process_page_for_test(
        &self,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<TestWorkerRecoveryResult>> {
        let fabric = self
            .memory_fabric()
            .context("recovery fixture requires source fabric")?;
        let workspace = PathBuf::from(fabric.workspace_id());
        let page = process_terminal::recover_restart_page(&fabric, &workspace, after_id, limit).await?;
        let mut outcomes = Vec::with_capacity(page.outcomes.len());
        for item in page.outcomes {
            let result = match item.result {
                Ok(Some(recovered)) => recovered.restore(&self.active_runs).await.map(Some),
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            };
            outcomes.push(TestWorkerRecoveryResult {
                launch_event_id: item.launch_event_id,
                result,
            });
        }
        Ok(outcomes)
    }

    /// Isolated AUT only: the caller validates a root-frozen build/profile manifest.
    /// Production launch always resolves current_exe and has no override.
    #[cfg(test)]
    pub(crate) fn with_test_worker_executable(
        mut self,
        path: PathBuf,
        sha256: String,
        private_home: PathBuf,
    ) -> anyhow::Result<Self> {
        let executable = TestWorkerExecutable {
            path,
            sha256,
            private_home,
            route_observer: None,
            protocol: None,
        };
        executable.verify()?;
        self.test_worker_executable = Some(executable);
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) fn with_test_worker_protocol(mut self, control: Arc<TestWorkerProtocolControl>) -> anyhow::Result<Self> {
        self.test_worker_executable
            .as_mut()
            .context("protocol control requires an attested worker artifact")?
            .protocol = Some(control);
        Ok(self)
    }

    /// Optional per-instance readback only; does not construct or mutate authority.
    #[cfg(test)]
    pub(crate) fn with_test_route_observer(
        mut self,
        observer: Arc<parking_lot::Mutex<TestWorkerRouteCapture>>,
    ) -> anyhow::Result<Self> {
        self.test_worker_executable
            .as_mut()
            .context("route observation requires an attested worker artifact")?
            .route_observer = Some(observer);
        Ok(self)
    }

    #[must_use]
    pub fn with_cost_config(mut self, cost_config: crate::config::schema::CostConfig) -> Self {
        self.cost_config = cost_config;
        self
    }

    /// Widen the providers a per-call `provider` argument may name to every
    /// provider the operator configured (see
    /// [`providers::ConfiguredProviders::from_config`]). Without it only the
    /// gateway provider and the named agents' providers are selectable.
    #[must_use]
    pub fn with_configured_providers(mut self, configured: &providers::ConfiguredProviders) -> Self {
        self.configured_providers = self.configured_providers.union(configured);
        self
    }

    /// Supply the model evidence a spawn is resolved against
    /// (`providers::selection::catalog_for`).
    #[must_use]
    pub fn with_model_catalog(mut self, catalog: providers::catalog::CatalogView) -> Self {
        self.model_catalog = Arc::new(catalog);
        self
    }

    #[must_use]
    pub fn with_hint_config(mut self, config: &crate::config::Config) -> Self {
        self.hint_config = Some(Arc::new(config.clone()));
        self
    }

    /// Preserve a sealed route selection independently of agent execution
    /// identity when a worker spawns another child.
    pub fn with_default_route_source(
        mut self,
        provider: &str,
        credential: providers::credentials::CredentialRef,
    ) -> Self {
        self.default_route_source = Some((provider.to_string(), credential));
        self
    }

    pub fn with_fallback_api_key_owner(mut self, provider: &str) -> Self {
        self.fallback_api_key_owner = provider.to_string();
        self
    }

    pub const fn with_fallback_root_connection(mut self, uses_root: bool) -> Self {
        self.fallback_uses_root_connection = uses_root;
        self
    }

    /// Attach a `SpawnEventSink` so task-mode sub-agents spawned by this tool
    /// stream their incremental output and tool-call notifications to the chat UI
    /// (live read-only attach, v1.1a).
    ///
    /// Only the chat `/bg` path sets this; channels/gateway leave it `None` and
    /// keep spawning silently (zero behaviour change for those callers).
    #[must_use]
    pub fn with_event_sink(mut self, sink: SpawnEventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Attach a [`SpawnApprovalResolverFactory`](crate::agent::turn_request::SpawnApprovalResolverFactory)
    /// so a task-mode sub-agent that hits the supervised approval gate suspends
    /// (NeedsInput) awaiting an operator decision instead of auto-failing.
    ///
    /// Only the chat `/bg` path sets this; channels/gateway leave it `None` and
    /// keep the historical auto-fail-on-gate semantics (no human is present to
    /// approve, so suspending would only create a zombie).
    // Called only by `chat::run`, the sole NeedsInput opt-in.
    #[must_use]
    pub(crate) fn with_approval_resolver_factory(
        mut self,
        factory: crate::agent::turn_request::SpawnApprovalResolverFactory,
    ) -> Self {
        self.approval_resolver_factory = Some(factory);
        self
    }

    /// Return a shareable handle to the default-recipient slot so callers can
    /// update it before each agent turn without replacing the tool registration.
    pub fn default_recipient_handle(&self) -> Arc<RwLock<Option<String>>> {
        self.default_recipient.clone()
    }

    /// Return a handle to the tools OnceLock so callers can set the registry
    /// post-construction (resolves the chicken-and-egg registration problem).
    pub fn tools_handle(&self) -> Arc<ParentToolRegistry> {
        self.tools.clone()
    }

    /// Convenience: update the default recipient from the current message's reply_target.
    pub async fn set_default_recipient(&self, recipient: Option<String>) {
        *self.default_recipient.write().await = recipient;
    }

    /// Return a snapshot of active sub-agent runs (for status queries).
    pub async fn active_runs_snapshot(&self) -> Vec<SubAgentRun> {
        self.active_runs.read().await.clone()
    }

    /// Return a shared Arc to the active runs registry.
    /// Used by sessions_list, sessions_send, and session_status to share state.
    pub fn active_runs_arc(&self) -> Arc<RwLock<Vec<SubAgentRun>>> {
        self.active_runs.clone()
    }

    /// Attach the full set of configured channels, keyed by [`Channel::name`].
    ///
    /// This is the per-turn routing registry: announce/kill resolve the launching
    /// message's channel object from here via the `channel_name` recorded on each
    /// [`SubAgentRun`], so routing is bound atomically to the originating message
    /// rather than to the shared "active channel" (which a concurrently-processed
    /// message can overwrite). Channels not found here fall back to the shared
    /// active channel.
    #[must_use]
    pub fn with_channels(mut self, channels: Arc<HashMap<String, Arc<dyn Channel>>>) -> Self {
        self.channels = channels;
        self
    }

    /// Attach shared memory so spawned runs are visible in the live fabric.
    /// Logical lifecycle storage is independent of execution-memory isolation.
    pub(crate) fn with_source_fabric(mut self, fabric: MemoryFabric) -> Self {
        self.source_fabric = Some(fabric);
        self
    }

    pub fn with_shared_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// The owning tool generation bounds recovery lifetime. Disabled decision
    /// mode starts no task and performs no storage or process probes.
    pub(crate) fn with_process_recovery(mut self, enabled: bool) -> anyhow::Result<Self> {
        if !enabled {
            return Ok(self);
        }
        anyhow::ensure!(self.restart_recovery.is_none(), "worker recovery is already attached");
        let fabric = self.memory_fabric().context("worker recovery requires shared memory")?;
        let workspace = std::path::PathBuf::from(fabric.workspace_id());
        let runs = Arc::clone(&self.active_runs);
        let handle = tokio::runtime::Handle::try_current()?;
        self.restart_recovery = Some(AbortOnDrop(handle.spawn(async move {
            process_terminal::run_restart_recovery(fabric, workspace, runs).await;
        })));
        Ok(self)
    }

    pub const fn with_event_recording(mut self, event_recording: MemoryEventRecording) -> Self {
        self.event_recording = event_recording;
        self
    }

    /// Resolve the channel a run must announce/kill-notify on.
    ///
    /// Prefers the run's per-turn `channel_name` (captured atomically from the
    /// launching message's scope) looked up in the channel registry. Falls back
    /// to the shared active channel when the name is absent (single-channel /
    /// unit-test paths) or not found in the registry (warns), so routing never
    /// panics — at worst it degrades to the previous shared-channel behaviour.
    pub(super) async fn resolve_announce_channel(&self, channel_name: Option<&str>) -> Arc<dyn Channel> {
        if let Some(name) = channel_name {
            if let Some(channel) = self.channels.get(name) {
                return Arc::clone(channel);
            }
            if !self.channels.is_empty() {
                tracing::warn!(
                    channel = %name,
                    "sessions_spawn: originating channel not found in registry; \
                     falling back to active channel for announcement"
                );
            }
        }
        self.channel.read().await.clone()
    }
}
