//! D1 — `AppContext` + `RuntimeBootstrap`: single, ordered construction of the
//! read-only core shared across all run modes.
//!
//! Background: today every run mode (chat / agent / gateway / daemon / channels /
//! session_worker) re-derives `observer` / `security` / `audit_config` /
//! `memory` / `tools` on its own, duplicating 7 `create_observer(...)` and 17
//! `SecurityPolicy::from_config(...).with_audit_config(...)` call sites. This
//! module centralizes that wiring so each consumer shares one `Arc<AppContext>`.
//!
//! Authoritative design: `/opt/worker/report/prx-d1-init-order-survey-2026-06-01.md`
//! (7 entry-point timelines, 7 hard ordering constraints, 6 high-risk findings)
//! and `prx-d-series-dev-plan-2026-06-01.md` §1.3 / §2.1 / §2.3.
//!
//! Hard ordering constraints fixed here (survey §2):
//!   observer → security (with audit) → memory → runtime → tools, and (when the
//!   `llm-router` feature is on and the profile needs it) router strictly after
//!   memory (`agent/agent.rs:446` asserts memory must precede the router).
//!
//! NOTE (D1 step 2): the first mode wiring (chat::run) now consumes this module
//! via `RuntimeBootstrap::build` with `BootstrapProfile::{MemoryOnly,Interactive}`.
//! The `AppContext` core fields (config/observer/security/memory/tools) and those
//! two profiles are therefore live. Items not yet reached by any wired mode —
//! the `Minimal`/`Server`/`Channel`/`Worker` profiles, the `workspace_dir` and
//! `router` fields — are public API of this module, reserved for the mode that
//! adopts `AppContext`.
//!
//! The gateway does **not** route its core through `RuntimeBootstrap` (its
//! observer/memory/tools diverge from the generic core, and the OTel-backed
//! `create_observer` has a global init side effect whose timing must stay put,
//! survey F7/F8). It instead shares only the genuinely error-prone bit — the
//! audit-bearing `SecurityPolicy` — via the free `build_security_policy` helper
//! below, keeping its observer/memory/tools/config wiring byte-for-byte as before.
//!
//! NOTE (D1 step 2, session_worker divergence): `session_worker::run_validated_manifest`
//! is deliberately **NOT** wired through `RuntimeBootstrap`. Its core resources are
//! manifest-driven and behaviorally distinct from this generic core (survey §1.7 / F5):
//!
//! - `security` binds `manifest.workspace_dir` (which overrides `config.workspace_dir`),
//!   not `config.workspace_dir` as this module does.
//! - `observer` is a local `NoopObserver`, not `create_observer(&config.observability)`.
//! - `memory` is a `SqliteMemory` with `NoopEmbedding` wired directly to
//!   `manifest.memory_db_path` (`new_with_path_and_acl`), not the backend-selected,
//!   embedding-routed, identity/policy-bound `create_memory_with_storage_and_routes_with_acl`.
//! - `tools` are the manifest's `select_tools_for_worker(...)` subset over
//!   `manifest.workspace_dir`, not the full `all_tools_with_runtime` set over
//!   `config.workspace_dir`.
//!
//! Routing the worker through `AppContext` cannot reproduce any of these without
//! changing observable behavior (workspace root, observer type, memory backend +
//! embedder + ACL semantics, tool set). Under the D1 "behavior-unchanged > unification"
//! guardrail the worker therefore keeps its dedicated construction path. The
//! `BootstrapProfile::Worker` variant is retained as a *reserved* placeholder (its
//! generic full build is exercised only by the unit test below); it is intentionally
//! not constructed by `session_worker`, which is the authoritative source of truth
//! for a sub-process IPC manifest.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::memory::{self, Memory};
use crate::observability::{self, Observer};
use crate::security::SecurityPolicy;
use crate::tools::{self, Tool};

#[cfg(feature = "llm-router")]
use crate::router::RouterEngine;

/// Read-only core: immutable after construction, no interior `Mutex`, shared by
/// `Arc` clone across modes and child tasks (iron rule 7: `Arc` over deep copy).
///
/// Hot-swappable resources (`provider` / `model`) and config hot-reload are
/// deliberately **not** here — they live in the slot layer (dev-plan §2.2) and
/// are built by each mode. Only the read-only core lives in `AppContext`.
pub struct AppContext {
    pub(crate) controlled_entry: Option<Arc<super::h2_entry::ControlledH2Profile>>,
    pub(crate) terminal_producer: parking_lot::Mutex<Option<super::h2_entry::TerminalInputProducer>>,
    pub(crate) controlled_case: Option<Arc<super::h2_entry::ControlledCaseScope>>,
    /// Sole configuration generation owner for this process/runtime bootstrap.
    pub config_manager: crate::config::SharedConfig,
    /// Startup generation pinned by non-hot-reloading CLI modes.
    pub config_generation: Arc<crate::config::ConfigGeneration>,
    /// Read-only config snapshot. Hot-reload travels through the slot layer (D2),
    /// not through this field.
    pub config: Arc<Config>,
    /// Single observer for the whole context (replaces 7 `create_observer` sites).
    pub observer: Arc<dyn Observer>,
    /// Security policy with the configured `security.audit` block already
    /// attached — built once here so no path can forget `with_audit_config`
    /// (collapses the 17 hand-wired sites, survey §1 / dev-plan §2.1).
    pub security: Arc<SecurityPolicy>,
    /// Process-local lifecycle hook generation shared by the loop and its
    /// control tool, so diagnostics and mutations observe the executing state.
    pub hooks: Arc<crate::hooks::HookManager>,
    /// Workspace root as `Arc<Path>` (iron rule 7: no `PathBuf` clone, no
    /// intermediate `String`).
    // Not yet consumed by a wired mode (chat reads `config.workspace_dir`).
    pub workspace_dir: Arc<Path>,
    /// Memory backend. `None` under `Minimal` (no memory built) — keeps the
    /// `status`/`doctor` etc. early-exit paths free of new failure surface (F4).
    pub memory: Option<Arc<dyn Memory>>,
    /// Tool registry, shared as `Arc<Vec<Box<dyn Tool>>>` (matches chat's
    /// `tools_registry`). `None` for `Minimal`/`MemoryOnly` — e.g. chat
    /// `--list-sessions` early-exits before tools are needed (F4).
    ///
    /// Also `None` for `Interactive` (chat): chat needs an *owned* `Vec` it can
    /// append the chat-side sessions tools to (built after provider + channel),
    /// so the `Interactive` profile delivers the base registry through
    /// `base_tools` instead. The "security & memory precede tools" ordering
    /// invariant is preserved either way (see `golden_trace_*` tests).
    pub tools: Option<Arc<Vec<Box<dyn Tool>>>>,
    /// The MCP instance installed in the finalized registry. The decision
    /// publisher snapshots this same instance after discovery, so its
    /// capability generation cannot come from a second MCP construction.
    pub(crate) mcp_tool: Option<Arc<tools::McpTool>>,
    /// AgentLoop's decision service published from the same finalized tool
    /// registry. Interactive appends tools later and publishes at that point.
    pub(crate) turn_decisions: Option<Arc<super::turn_decision::TurnDecisionService>>,
    /// Owned base tool registry for the `Interactive` (chat) profile only.
    ///
    /// `Box<dyn Tool>` is not `Clone`, so a shared `Arc<Vec<…>>` cannot be
    /// unpacked back into an owned `Vec` without sole ownership. chat needs to
    /// append the four chat-side sessions tools (`sessions_spawn`/`sessions_list`
    /// /`session_status`/`sessions_send`) *after* the provider and
    /// `TerminalChannel` exist, then wrap the result in `Arc` itself. To avoid a
    /// from-`Arc` move, the `Interactive` profile hands chat an owned `Vec` here
    /// and leaves `tools` as `None`.
    ///
    /// `AppContext` is shared as `Arc<AppContext>`, so the owned `Vec` lives
    /// behind a `parking_lot::Mutex<Option<…>>` to allow a one-shot
    /// `lock().take()` through the shared reference. chat takes it exactly once
    /// before entering the main loop (a pure sync `take`, never held across an
    /// `.await`). `None` for every other profile (they use the shared `tools`
    /// `Arc`); after chat's `take`, the inner `Option` is `None` (no dead state).
    pub base_tools: Option<parking_lot::Mutex<Option<Vec<Box<dyn Tool>>>>>,
    /// Sole WASM plugin generation owner for this process/workspace.
    #[cfg(feature = "wasm-plugins")]
    pub plugin_runtime: Option<Arc<crate::plugins::PluginRuntime>>,
    /// Heuristic LLM router. Only built for profiles that need it and only when
    /// the `llm-router` feature is enabled; always constructed after memory
    /// (`agent/agent.rs:446` invariant).
    // Not yet consumed by a wired mode (chat does not use the router).
    #[cfg(feature = "llm-router")]
    pub router: Option<Arc<RouterEngine>>,
}

/// Selects which subsystems a given run mode needs, so lightweight commands do
/// not pay for `memory`/`tools`/`router` they never use (dev-plan §2.3).
///
/// `Minimal` and `MemoryOnly` exist specifically to preserve the two early-exit
/// paths (survey F4): `status`/`doctor` build only the core, and chat
/// `--list-sessions` builds memory but not tools/provider/router.
///
/// `MemoryOnly` and `Interactive` are live (consumed by `chat::run`), and
/// `AgentLoop` by `entry::run`. The remaining variants are not yet reached by a
/// wired mode; they are reserved for the mode that adopts `AppContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapProfile {
    /// `status` / `doctor` / `models` / `providers`: only config/observer/security.
    Minimal,
    /// chat `--list-sessions` early-exit: + memory, no tools/router.
    MemoryOnly,
    /// Chat: full memory + tools; publishes decisions after registry finalization.
    Interactive,
    /// CLI agent: full memory + tools, no router, with its completed registry
    /// published as the source-bound turn decision service during bootstrap.
    AgentLoop,
    /// gateway / daemon: full.
    Server,
    /// channels: full.
    Channel,
    /// Reserved placeholder — **not** used by `session_worker`. The real worker
    /// (`session_worker::run_validated_manifest`) keeps a dedicated, manifest-driven
    /// path: `manifest.workspace_dir`-bound security, a local `NoopObserver`, a
    /// directly-wired `SqliteMemory` over `manifest.memory_db_path`, and a
    /// `select_tools_for_worker` subset (survey §1.7 / F5). Those cannot be
    /// reproduced by this generic full build without changing observable behavior,
    /// so the worker is intentionally excluded from `RuntimeBootstrap` under the D1
    /// behavior-unchanged guardrail (see the module-level note). This variant
    /// remains only as a reserved profile, exercised by the unit test below.
    Worker,
}

impl BootstrapProfile {
    /// Whether this profile requires a memory backend.
    const fn needs_memory(self) -> bool {
        !matches!(self, Self::Minimal)
    }

    /// Whether this profile requires the tool registry. Tools depend on
    /// security + runtime + memory, so this also implies memory. `AgentLoop` has
    /// the same tool footprint as `Interactive`.
    const fn needs_tools(self) -> bool {
        matches!(
            self,
            Self::Interactive | Self::AgentLoop | Self::Server | Self::Channel | Self::Worker
        )
    }

    /// Whether this profile may construct the LLM router (only relevant when the
    /// `llm-router` feature is enabled). Router needs memory; it is the
    /// agent-builder style full profiles that use it. `AgentLoop` does **not**
    /// need the router — `entry::run` resolves a single provider/model directly
    /// and never routes.
    #[cfg(feature = "llm-router")]
    const fn needs_router(self) -> bool {
        matches!(self, Self::Server | Self::Channel | Self::Worker)
    }
}

/// Single-source construction of a `SecurityPolicy` that always carries the
/// configured `security.audit` block.
///
/// This is the one place any run mode should obtain its primary `SecurityPolicy`
/// from: it collapses the 17 hand-wired
/// `SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
///  .with_audit_config(config.security.audit.clone())` call sites into a single
/// helper, so no path can forget `with_audit_config` (the BUG-D1-01 class of
/// omission). `RuntimeBootstrap::build` and the gateway both route through it;
/// the wiring is identical to the former local construction at each site.
pub(crate) fn build_security_policy(config: &Config) -> Arc<SecurityPolicy> {
    Arc::new(
        SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
            .with_audit_config(config.security.audit.clone()),
    )
}

/// Configuration owned by the invoking runtime. Scheduled model turns keep
/// the scheduler's exact generation and manager instead of reconstructing a
/// generation label from the current configuration file.
pub(crate) enum AgentRuntimeConfig {
    Standalone(Box<Config>),
    ControlledCase {
        config: Box<Config>,
        scope: Arc<super::h2_entry::ControlledCaseScope>,
    },
    ControlledEntry {
        config: Box<Config>,
        producer: Box<super::h2_entry::TerminalInputProducer>,
    },
    Pinned {
        generation: Arc<crate::config::ConfigGeneration>,
        manager: crate::config::SharedConfig,
    },
}

impl From<Config> for AgentRuntimeConfig {
    fn from(config: Config) -> Self {
        Self::Standalone(Box::new(config))
    }
}

impl AgentRuntimeConfig {
    pub(crate) fn effective(&self) -> &Config {
        match self {
            Self::Standalone(config) | Self::ControlledCase { config, .. } | Self::ControlledEntry { config, .. } => {
                config
            }
            Self::Pinned { generation, .. } => &generation.effective,
        }
    }
}

/// Single construction entry point: all modes obtain their `Arc<AppContext>`
/// from here.
pub struct RuntimeBootstrap;

impl AppContext {
    /// Publish from the finalized registry owned by the ingress. Interactive
    /// chat calls this only after appending its session tools; the base registry
    /// is not a capability authority for a running chat session.
    pub(crate) async fn publish_turn_decisions(
        &self,
        registry: Arc<Vec<Box<dyn Tool>>>,
        surface: crate::causal_tree::state::TurnSurfaceKind,
    ) -> Result<Arc<super::turn_decision::TurnDecisionService>> {
        super::turn_publication::TurnDecisionPublication {
            generation: Arc::clone(&self.config_generation),
            registry,
            mcp_tool: self.mcp_tool.clone(),
            security: Arc::clone(&self.security),
            observer: Arc::clone(&self.observer),
            hooks: Arc::clone(&self.hooks),
            memory: self
                .memory
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("decision publication requires memory"))?
                .clone(),
            plugin_generation: {
                #[cfg(feature = "wasm-plugins")]
                {
                    self.plugin_runtime
                        .as_ref()
                        .map_or(0, |runtime| runtime.generation_id())
                }
                #[cfg(not(feature = "wasm-plugins"))]
                {
                    0
                }
            },
        }
        .publish(surface)
        .await
    }
}

impl RuntimeBootstrap {
    /// Build the read-only core for `profile`, constructing subsystems in the
    /// hard-ordered sequence (survey §2). Every fallible step propagates via `?`
    /// (iron rules 1/6: no `unwrap`/`expect`/`panic`; fail-fast on subsystem
    /// construction errors).
    pub async fn build(config: Config, profile: BootstrapProfile) -> Result<Arc<AppContext>> {
        Self::build_inner(
            config.into(),
            profile,
            crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
            None,
        )
        .await
    }

    pub(crate) async fn build_controlled(
        config: Config,
        profile: BootstrapProfile,
        producer: super::h2_entry::TerminalInputProducer,
    ) -> Result<Arc<AppContext>> {
        Self::build_inner(
            AgentRuntimeConfig::ControlledEntry {
                config: Box::new(config),
                producer: Box::new(producer),
            },
            profile,
            crate::causal_tree::state::TurnSurfaceKind::Interactive,
            None,
        )
        .await
    }

    /// Publish once for the actual agent ingress, using the same effective
    /// policy as the scheduled execution adapter. This does not issue a turn
    /// source proof or turn a task into a local human approval surface.
    pub(crate) async fn build_agent(
        config_source: AgentRuntimeConfig,
        envelope: Option<&crate::runtime::envelope::RuntimeEnvelope>,
        scheduled: Option<&crate::tools::execution::ScheduledToolScope>,
    ) -> Result<Arc<AppContext>> {
        use crate::causal_tree::state::TurnSurfaceKind;
        use crate::runtime::envelope::RuntimeSource;
        let config = config_source.effective();
        if let AgentRuntimeConfig::Pinned { generation, .. } = &config_source
            && let Some(envelope) = envelope
        {
            anyhow::ensure!(
                envelope.config_generation_id == Some(generation.id.0)
                    && envelope.config_source_revision.as_deref()
                        == Some(generation.source_revision.fingerprint_sha256.as_str()),
                "scheduled source does not match its pinned configuration"
            );
        }
        let surface = match envelope.map(|envelope| envelope.source) {
            None | Some(RuntimeSource::Agent) => TurnSurfaceKind::AgentLoop,
            Some(RuntimeSource::Cron) => TurnSurfaceKind::Cron,
            Some(RuntimeSource::Xin) => TurnSurfaceKind::Xin,
            Some(_) => anyhow::bail!("unsupported scoped agent ingress"),
        };
        let policy = scheduled
            .map(|scheduled| {
                let source =
                    envelope.ok_or_else(|| anyhow::anyhow!("scheduled execution requires its runtime source"))?;
                let execution = &scheduled.context.envelope;
                anyhow::ensure!(
                    matches!(source.source, RuntimeSource::Cron | RuntimeSource::Xin)
                        && execution.source == source.source
                        && execution.workspace_id == source.workspace_id
                        && execution.session_key == source.session_key
                        && execution.run_id == source.run_id
                        && execution.task_id == source.task_id
                        && execution.attempt_id == source.attempt_id
                        && execution.lease_epoch == source.lease_epoch
                        && execution.authority_guard == source.authority_guard
                        && execution.parent_run_id == source.parent_run_id
                        && execution.agent_id == source.agent_id
                        && execution.persona_id == source.persona_id
                        && execution.topic_id == source.topic_id
                        && execution.source_message_event_id == source.source_message_event_id
                        && execution.config_generation_id == source.config_generation_id
                        && execution.config_source_revision == source.config_source_revision
                        && source
                            .owner_id
                            .as_ref()
                            .is_none_or(|owner| execution.owner_id.as_ref() == Some(owner))
                        && (!source.owner_isolated || execution.owner_isolated)
                        && scheduled.policy.workspace_dir == config.workspace_dir,
                    "scheduled policy and execution scope do not belong to this run"
                );
                Ok::<_, anyhow::Error>(Arc::clone(&scheduled.policy))
            })
            .transpose()?;
        Self::build_inner(config_source, BootstrapProfile::AgentLoop, surface, policy).await
    }

    async fn build_inner(
        config_source: AgentRuntimeConfig,
        profile: BootstrapProfile,
        agent_surface: crate::causal_tree::state::TurnSurfaceKind,
        security_override: Option<Arc<SecurityPolicy>>,
    ) -> Result<Arc<AppContext>> {
        // Wrap config once so all downstream users share the same allocation
        // (iron rule 7: Arc over deep copy). Arc<Config> deref-coerces to &Config
        // everywhere a borrow is needed. The bootstrap also owns the one
        // process-level generation manager used by every config-aware tool.
        let controlled_entry = match &config_source {
            AgentRuntimeConfig::ControlledEntry { config, producer } => {
                let profile = producer.profile();
                profile.verify(config)?;
                Some(profile)
            }
            _ => None,
        };
        let mut terminal_producer = None;
        let controlled_case = match &config_source {
            AgentRuntimeConfig::ControlledCase { config, scope } => {
                scope.verify_config(config)?;
                Some(Arc::clone(scope))
            }
            _ => None,
        };
        let (config_manager, config_generation) = match config_source {
            AgentRuntimeConfig::Standalone(config) | AgentRuntimeConfig::ControlledCase { config, .. } => {
                let manager = crate::config::new_shared(*config);
                let generation = manager.pin();
                (manager, generation)
            }
            AgentRuntimeConfig::ControlledEntry { config, producer } => {
                terminal_producer = Some(*producer);
                let manager = crate::config::new_shared(*config);
                let generation = manager.pin();
                (manager, generation)
            }
            AgentRuntimeConfig::Pinned { generation, manager } => (manager, generation),
        };
        if controlled_entry.is_some() || controlled_case.is_some() {
            config_manager.freeze_runtime_writes();
        }
        let config = Arc::clone(&config_generation.effective);

        // 1. observer — first, no dependencies beyond config.
        let observer: Arc<dyn Observer> = Arc::from(observability::create_observer(&config.observability));

        // 2. security (with audit) — single source of truth; always carries the
        //    configured `security.audit` block (dev-plan §2.1, collapses 17 sites).
        let security = security_override.unwrap_or_else(|| build_security_policy(&config));
        let hooks = Arc::new(
            crate::hooks::HookManager::new(config.workspace_dir.clone()).with_config(Arc::clone(&config_manager)),
        );

        // workspace_dir as Arc<Path>: borrow the config path, no String detour.
        let workspace_dir: Arc<Path> = Arc::from(config.workspace_dir.as_path());

        // A WASM storage backend must be loaded before the synchronous memory
        // factory can resolve it. The runtime is therefore created in a
        // memory-less first phase only for `memory.backend = "wasm:<name>"`;
        // after resolution we atomically rebuild its adapters with that live
        // backend attached. Native backends keep the original construction path.
        #[cfg(feature = "wasm-plugins")]
        let wasm_storage_bootstrap = if profile.needs_memory()
            && crate::memory::effective_memory_backend_name(
                &config.memory.backend,
                Some(&config.storage.provider.config),
            )
            .starts_with("wasm:")
        {
            crate::plugins::init_plugin_runtime(&config, None).await
        } else {
            None
        };

        // 3. memory — only when the profile needs it (Minimal stays None; keeps
        //    early-exit/Minimal paths free of new failure surface, F4).
        let memory: Option<Arc<dyn Memory>> = if profile.needs_memory() {
            let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage_and_routes_with_acl(
                &config.memory,
                &config.embedding_routes_with_references(),
                Some(&config.storage.provider.config),
                &config.workspace_dir,
                memory::EmbeddingRootCredential::from_config(&config),
                &config.identity_bindings,
                &config.user_policies,
            )?);
            tracing::info!(backend = mem.name(), "Memory initialized");
            Some(mem)
        } else {
            None
        };

        // 4. runtime — constructed after memory and before router/tools, enforcing
        //    the strict observer→security→memory→runtime→router→tools ordering
        //    (survey §2 constraint 1). Only built for profiles that need tools;
        //    Minimal/MemoryOnly never reach this block.
        let runtime: Option<Arc<dyn crate::runtime::RuntimeAdapter>> = if profile.needs_tools() {
            Some(Arc::from(crate::runtime::create_runtime(&config.runtime)?))
        } else {
            None
        };

        #[cfg(feature = "wasm-plugins")]
        let plugin_runtime = if profile.needs_tools() || wasm_storage_bootstrap.is_some() {
            if let Some(runtime) = wasm_storage_bootstrap {
                if let Some(memory) = memory.as_ref()
                    && let Err(error) = runtime.attach_memory(Arc::clone(memory)).await
                {
                    return Err(anyhow::anyhow!("failed to attach WASM memory backend: {error}"));
                }
                Some(runtime)
            } else {
                crate::plugins::init_plugin_runtime(&config, memory.clone()).await
            }
        } else {
            None
        };
        #[cfg(feature = "wasm-plugins")]
        if let Some(plugin_runtime) = &plugin_runtime {
            hooks.set_plugin_runtime(Arc::clone(plugin_runtime)).await;
        }

        // 5. router — strictly after memory (agent/agent.rs:446 invariant), only
        //    when the feature is on and the profile needs it.
        #[cfg(feature = "llm-router")]
        let router: Option<Arc<RouterEngine>> = if profile.needs_router() {
            // memory must already be set; needs_router() implies needs_memory().
            let mem = memory
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("memory backend must be set before enabling router"))?;
            let provider_name = crate::providers::selection::default_provider(&config);
            let (router_embedder, local_history) = match memory::create_configured_embedder(&config) {
                memory::ConfiguredEmbedding::LocalHash(embedder) => (None, Some(embedder)),
                memory::ConfiguredEmbedding::Other(embedder) => (Some(embedder), None),
            };
            let engine = RouterEngine::new(
                config.router.clone(),
                provider_name.to_string(),
                config.model_routes.clone(),
                mem.clone(),
                router_embedder,
                &config.cost,
            )
            .await?;
            let engine = match local_history {
                Some(embedder) => engine.with_local_hash_history(embedder)?,
                None => engine,
            };
            Some(Arc::new(engine))
        } else {
            None
        };

        // 6. tools — last; depends on security + runtime + memory all being ready
        //    (all_tools_with_runtime inputs, survey §2 constraint 1).
        //
        // The `Interactive` (chat) profile receives the registry as an *owned*
        // `Vec` in `base_tools` (so chat can append its sessions tools after the
        // provider/channel exist) and leaves `tools` as `None`; every other
        // tools-bearing profile keeps the shared `Arc` in `tools`.
        let mut tools: Option<Arc<Vec<Box<dyn Tool>>>> = None;
        let mut mcp_tool: Option<Arc<tools::McpTool>> = None;
        let mut base_tools: Option<parking_lot::Mutex<Option<Vec<Box<dyn Tool>>>>> = None;
        if profile.needs_tools() {
            // runtime is always Some when needs_tools() is true (built in step 4).
            let rt = runtime.ok_or_else(|| anyhow::anyhow!("runtime must be set before building tools"))?;
            let mem = memory
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("memory backend must be set before building tools"))?
                .clone();
            let (composio_key, composio_entity_id) = if config.composio.configured() {
                (
                    config.composio.api_key.as_deref(),
                    Some(config.composio.entity_id.as_str()),
                )
            } else {
                (None, None)
            };
            #[allow(unused_mut)]
            let mut extensions = hooks.control_tool_arcs(Arc::clone(&security));
            #[cfg(feature = "wasm-plugins")]
            if let Some(plugin_runtime) = &plugin_runtime {
                extensions.extend(plugin_runtime.control_tool_arcs(Arc::clone(&security)));
            }
            if let Some(case) = &controlled_case {
                tools = Some(Arc::new(case.registry(&config, &security)?));
            } else if let Some(entry) = &controlled_entry {
                let registry = entry.registry(&config, &security)?;
                if matches!(profile, BootstrapProfile::Interactive) {
                    base_tools = Some(parking_lot::Mutex::new(Some(registry)));
                } else {
                    tools = Some(Arc::new(registry));
                }
            } else {
                let registry_result = tools::all_tools_with_runtime_ext_and_extensions(
                    Arc::clone(&config),
                    Arc::clone(&config_manager),
                    &security,
                    rt,
                    mem,
                    composio_key,
                    composio_entity_id,
                    &config.browser,
                    &config.http_request,
                    &config.workspace_dir,
                    &config.agents_with_references(),
                    config.api_key.as_deref(),
                    &config,
                    extensions,
                    Some(Arc::clone(&hooks)),
                );
                mcp_tool = registry_result.mcp_tool;
                let registry = registry_result.tools;
                if matches!(profile, BootstrapProfile::Interactive) {
                    base_tools = Some(parking_lot::Mutex::new(Some(registry)));
                } else {
                    tools = Some(Arc::new(registry));
                }
            }
        }

        let mut context = AppContext {
            controlled_entry,
            terminal_producer: parking_lot::Mutex::new(terminal_producer),
            controlled_case,
            config_manager,
            config_generation,
            config,
            observer,
            security,
            hooks,
            workspace_dir,
            memory,
            tools,
            mcp_tool,
            turn_decisions: None,
            base_tools,
            #[cfg(feature = "wasm-plugins")]
            plugin_runtime,
            #[cfg(feature = "llm-router")]
            router,
        };
        if matches!(profile, BootstrapProfile::AgentLoop) {
            let registry = context
                .tools
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("AgentLoop decision publication requires its tool registry"))?;
            context.turn_decisions = Some(
                context
                    .publish_turn_decisions(Arc::clone(registry), agent_surface)
                    .await?,
            );
        }
        Ok(Arc::new(context))
    }
}

#[cfg(test)]
mod tests {
    mod decision_feature_contract;

    use super::*;

    fn assert_model_managed_capability_surface(tools: &[Box<dyn Tool>]) {
        let names = tools.iter().map(|tool| tool.name()).collect::<Vec<_>>();
        for expected in [
            "skills_list",
            "skill_read",
            "skills_manage",
            "skill_execute",
            "mcp_call",
            "mcp_status",
            "hooks_status",
            "hooks_manage",
        ] {
            assert!(
                names.contains(&expected),
                "missing model-managed capability tool {expected}"
            );
        }
        // `[plugins] enabled` defaults to false (finding 95), and these fixtures
        // keep the default: the WASM control tools exist only with a plugin
        // runtime, so none of them may be offered.
        #[cfg(feature = "wasm-plugins")]
        for absent in [
            "wasm_plugin_call",
            "wasm_plugins_status",
            "wasm_plugin_reload",
            "wasm_plugins_manage",
        ] {
            assert!(
                !names.contains(&absent),
                "WASM control tool {absent} offered while plugins are disabled"
            );
        }
    }

    /// Minimal usable test config: an in-memory/markdown-free SQLite memory in a
    /// temp workspace, observer disabled, router disabled. Mirrors how other
    /// modules construct `Config` for tests (`Config::default` + field tweaks).
    fn test_config(workspace: &Path) -> Config {
        let mut config = Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.config_path = workspace.join("config.toml");
        // These profile fixtures assert the explicitly disabled/off contract.
        config.causal_tree.enabled = false;
        // Keep the observer cheap and deterministic in tests.
        config.observability.backend = "noop".into();
        config
    }

    #[tokio::test]
    async fn minimal_profile_has_no_memory_or_tools() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let config = test_config(tmp.path());
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Minimal)
            .await
            .expect("test: minimal build");

        assert!(ctx.memory.is_none(), "Minimal must not build memory");
        assert!(ctx.tools.is_none(), "Minimal must not build tools");
        // Core resources are always present.
        assert_eq!(ctx.observer.name(), "noop");
        assert_eq!(ctx.workspace_dir.as_ref(), tmp.path());
        assert_eq!(ctx.config.workspace_dir, tmp.path());
        #[cfg(feature = "llm-router")]
        assert!(ctx.router.is_none(), "Minimal must not build router");
    }

    #[tokio::test]
    async fn memory_only_profile_builds_memory_but_not_tools() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let config = test_config(tmp.path());
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::MemoryOnly)
            .await
            .expect("test: memory-only build");

        assert!(ctx.memory.is_some(), "MemoryOnly must build memory");
        assert!(ctx.tools.is_none(), "MemoryOnly must not build tools");
        #[cfg(feature = "llm-router")]
        assert!(ctx.router.is_none(), "MemoryOnly must not build router");
    }

    #[tokio::test]
    async fn interactive_profile_builds_memory_and_tools() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let config = test_config(tmp.path());
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Interactive)
            .await
            .expect("test: interactive build");

        assert!(ctx.memory.is_some(), "Interactive must build memory");
        // Interactive delivers the base registry as an owned `Vec` in
        // `base_tools` (so chat can append its sessions tools), leaving `tools`
        // as `None`.
        assert!(
            ctx.tools.is_none(),
            "Interactive delivers tools via base_tools, not tools"
        );
        let base = ctx
            .base_tools
            .as_ref()
            .expect("test: Interactive must build base_tools");
        let taken = base.lock().take().expect("test: base_tools present once");
        assert!(!taken.is_empty(), "tool registry should be non-empty");
        assert_model_managed_capability_surface(&taken);
        // The owned Vec is a one-shot take; the inner Option is now None.
        assert!(base.lock().is_none(), "base_tools take is one-shot");
        // Interactive does not enable the router.
        #[cfg(feature = "llm-router")]
        assert!(ctx.router.is_none(), "Interactive must not build router");
    }

    #[tokio::test]
    async fn agent_loop_profile_builds_memory_and_tools() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let config = test_config(tmp.path());
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::AgentLoop)
            .await
            .expect("test: agent-loop build");

        assert!(ctx.memory.is_some(), "AgentLoop must build memory");
        assert!(ctx.tools.is_some(), "AgentLoop must build tools");
        assert!(
            ctx.mcp_tool.is_some(),
            "AgentLoop must retain its registry MCP instance"
        );
        assert!(
            ctx.turn_decisions.is_some(),
            "AgentLoop must publish its decision service"
        );
        assert!(
            !tmp.path().join("self/causal_tree/.decision_key").exists(),
            "disabled AgentLoop publication must not create a decision key"
        );
        assert!(
            !tmp.path().join("self/causal_tree/decisions.db").exists(),
            "disabled AgentLoop publication must not create a decision database"
        );
        assert_model_managed_capability_surface(ctx.tools.as_deref().expect("AgentLoop tools"));
        // AgentLoop has the same router footprint as Interactive: never routes.
        #[cfg(feature = "llm-router")]
        assert!(ctx.router.is_none(), "AgentLoop must not build router");
    }

    #[tokio::test]
    async fn server_profile_builds_memory_and_tools() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let config = test_config(tmp.path());
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Server)
            .await
            .expect("test: server build");

        assert!(ctx.memory.is_some(), "Server must build memory");
        assert!(ctx.tools.is_some(), "Server must build tools");
        #[cfg(feature = "llm-router")]
        assert!(ctx.router.is_some(), "router is always constructed for server profiles");
    }

    #[tokio::test]
    async fn channel_profile_builds_memory_and_tools() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let config = test_config(tmp.path());
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Channel)
            .await
            .expect("test: channel build");

        assert!(ctx.memory.is_some(), "Channel must build memory");
        assert!(ctx.tools.is_some(), "Channel must build tools");
    }

    /// Copy the committed WASM examples into `<workspace>/plugins`.
    #[cfg(feature = "wasm-plugins")]
    fn plant_wasm_examples(workspace: &Path, examples: &[&str]) {
        for example in examples {
            let destination = workspace.join("plugins").join(example);
            std::fs::create_dir_all(&destination).unwrap();
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("pdk/rust/examples")
                .join(example);
            std::fs::copy(fixture.join("plugin.toml"), destination.join("plugin.toml")).unwrap();
            std::fs::copy(fixture.join("plugin.wasm"), destination.join("plugin.wasm")).unwrap();
        }
    }

    #[cfg(feature = "wasm-plugins")]
    #[tokio::test]
    async fn wasm_storage_bootstraps_before_memory_and_provider_factory_resolution() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        // The operator's state directory sits outside the workspace.
        let state = tempfile::tempdir().expect("test: create state dir");
        let examples = ["map-storage", "echo-provider"];
        plant_wasm_examples(tmp.path(), &examples);
        let mut config = test_config(tmp.path());
        config.config_path = state.path().join("config.toml");
        config.plugins.enabled = true;
        config.memory.backend = "wasm:wasm-map".to_string();
        config.default_provider = Some("wasm:wasm-echo".to_string());
        // Trust is the operator's: record it in the trust file the runtime reads.
        let policy = crate::plugins::PluginPolicy::from_config(&config).expect("test: plugin policy");
        for example in examples {
            let material = crate::plugins::trust::PluginMaterial::read(&tmp.path().join("plugins").join(example))
                .expect("test: read example plugin");
            let granted = material.manifest.permissions.required.clone();
            policy
                .record(material.record_for(&granted).expect("test: trust record"))
                .expect("test: write trust file");
        }

        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Interactive)
            .await
            .expect("WASM storage bootstrap should succeed");
        assert_eq!(ctx.memory.as_ref().map(|memory| memory.name()), Some("wasm-map"));
        let runtime = ctx.plugin_runtime.as_ref().expect("plugin runtime");
        assert_eq!(runtime.storage_names(), vec!["wasm-map"]);
        assert_eq!(runtime.provider_names(), vec!["wasm-echo"]);
        assert!(crate::providers::create_provider("wasm:wasm-echo", None).is_ok());
    }

    /// Finding 95: the switch defaults to off, and with it off the workspace
    /// plugins are neither scanned nor loaded, even ones the operator trusted.
    #[cfg(feature = "wasm-plugins")]
    #[tokio::test]
    async fn plugins_stay_absent_while_the_switch_is_off() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        plant_wasm_examples(tmp.path(), &["pipeline-middleware", "echo-provider"]);
        crate::plugins::trust_workspace_plugins(tmp.path());
        let config = test_config(tmp.path());
        assert!(!config.plugins.enabled);

        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Worker)
            .await
            .expect("test: worker build");
        assert!(ctx.plugin_runtime.is_none(), "no plugin runtime while disabled");
        assert!(ctx.tools.is_some());
    }

    #[tokio::test]
    async fn worker_profile_builds_memory_and_tools() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let config = test_config(tmp.path());
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Worker)
            .await
            .expect("test: worker build");

        assert!(ctx.memory.is_some(), "Worker must build memory");
        assert!(ctx.tools.is_some(), "Worker must build tools");
    }

    // --- golden-trace construction-order invariants (Codex condition #2) ---------
    //
    // `RuntimeBootstrap::build` is a single linear function: the resources are
    // constructed in source order observer → security → memory → runtime → router →
    // tools (see steps 1..6 above). That ordering is *enforced by the Rust type
    // system*, not just by convention: `tools` (step 6) takes `&security` and the
    // already-built `memory`/`runtime` as inputs to `all_tools_with_runtime`, and
    // `router` (step 5) borrows `memory`. A future edit that moved `tools`/`router`
    // construction before `security`/`memory` simply would not compile, because the
    // bindings it consumes would not yet be in scope. We cannot inject a runtime
    // "trace" of construction order without restructuring `build` (and adding cost
    // to the hot path), so the golden-trace guarantee is split between:
    //   (a) the type-level dependency above (compile-time, free), and
    //   (b) the behavioural invariants asserted below — the observable shape of the
    //       core for each profile, which is what a re-ordering / regression would
    //       perturb. Together with the source-scan gate (`gate_*` tests) this is the
    //       "main-mode construction path changed → a test fires" tripwire.

    /// Golden-trace: for every tools-bearing profile, a successful `build` *implies*
    /// security and memory were ready before tools. `all_tools_with_runtime` consumes
    /// `&security` and a cloned `memory` `Arc`, so `tools.is_some()` cannot hold
    /// unless both predecessors existed at the tool-construction point. Asserting the
    /// joint post-condition locks the "security & memory precede tools" ordering as an
    /// observable invariant across all four full profiles.
    #[tokio::test]
    async fn golden_trace_security_and_memory_precede_tools() {
        for profile in [
            BootstrapProfile::Interactive,
            BootstrapProfile::AgentLoop,
            BootstrapProfile::Server,
            BootstrapProfile::Channel,
            BootstrapProfile::Worker,
        ] {
            let tmp = tempfile::tempdir().expect("test: create temp dir");
            let config = test_config(tmp.path());
            let ctx = RuntimeBootstrap::build(config, profile)
                .await
                .unwrap_or_else(|e| panic!("test: build {profile:?} must succeed: {e}"));

            // tools present => its hard predecessors (security, memory) were ready
            // first; this is the ordering constraint, observed at the output.
            // Interactive delivers the registry via `base_tools` (owned Vec for
            // chat to extend); every other tools-bearing profile via `tools`.
            let tools_built = if matches!(profile, BootstrapProfile::Interactive) {
                ctx.base_tools.is_some()
            } else {
                ctx.tools.is_some()
            };
            assert!(tools_built, "{profile:?}: tools-bearing profile must build tools");
            assert!(
                ctx.memory.is_some(),
                "{profile:?}: memory must be ready before tools (ordering invariant)"
            );
            // security is unconditional and always carries the audit block.
            assert_eq!(
                ctx.security.workspace_dir,
                tmp.path(),
                "{profile:?}: security must be bound to the config workspace"
            );
        }
    }

    /// Golden-trace: `build` never panics, even when an optional subsystem is absent
    /// — the lightweight profiles (no memory / no tools / no router) must still
    /// return `Ok` rather than tripping an internal `unwrap`/`expect`. This guards
    /// the early-exit ordering (Minimal/MemoryOnly stop before runtime/router/tools)
    /// against a future change that assumes a dependency is always present.
    #[tokio::test]
    async fn golden_trace_build_never_panics_on_missing_optional_deps() {
        for profile in [BootstrapProfile::Minimal, BootstrapProfile::MemoryOnly] {
            let tmp = tempfile::tempdir().expect("test: create temp dir");
            let config = test_config(tmp.path());
            // Must be Ok (not a panic) despite memory/tools/router being skipped.
            let ctx = RuntimeBootstrap::build(config, profile)
                .await
                .unwrap_or_else(|e| panic!("test: lightweight build {profile:?} must succeed: {e}"));
            assert!(
                ctx.tools.is_none(),
                "{profile:?}: lightweight profile must not build tools"
            );
        }
    }

    // --- static source gate: lock the audit-wiring convergence (Codex condition #2) -
    //
    // BUG-D1-01 class: someone re-introduces a hand-written
    // `SecurityPolicy::from_config(...).with_audit_config(...)` at a main-mode entry
    // and forgets the audit block. D1 collapsed those into `build_security_policy`.
    // This gate scans `src/` and asserts that the *production* (non-`#[cfg(test)]`)
    // hand-written `.with_audit_config(` call sites are exactly the known baseline —
    // any new one fails the test until it is either routed through
    // `build_security_policy` or explicitly added to the whitelist below with a
    // reason.

    /// Files (relative to the crate `src/`) that are *allowed* to hand-write
    /// `.with_audit_config(` in production code, each with the reason it is not (yet)
    /// routed through `build_security_policy`. `bootstrap.rs` itself is excluded from
    /// the scan entirely (it is the canonical convergence point / helper).
    ///
    /// Baseline established 2026-06-01 after D1 collapsed chat/daemon/gateway-core
    /// onto `build_security_policy`. Reasons:
    ///   - `session_worker/runner/run.rs`: manifest-driven divergent core
    ///     (`manifest.workspace_dir`-bound security); intentionally NOT routed through
    ///     `RuntimeBootstrap` under the D1 behavior-unchanged guardrail (see module note).
    ///   - `cron/scheduler.rs`: cron job path, not yet converged.
    ///
    /// A8 (2026-06-03): the legacy `Agent::from_config` in `agent/agent.rs` — the
    /// only production `.with_audit_config(` site in that file — was removed when the
    /// dead legacy `Agent::run` / `from_config` / `run_interactive` shell was deleted
    /// (the live path is `entry::run`). `agent/agent.rs` was therefore REMOVED from
    /// this whitelist to keep the gate tight (`gate_whitelist_entries_are_all_live`).
    ///
    /// D2 (2026-06-02): the four gateway authorization sites — `gateway/mod.rs`
    /// (`authorize_gateway_resource_mutation`), `gateway/api/mod.rs`
    /// (`authorize_resource_mutation_for_config`), `gateway/api/config.rs`
    /// (`post_config_reload`) and `gateway/api/sessions.rs` (console runtime turn) —
    /// were all converged onto `build_security_policy` and so were REMOVED from this
    /// whitelist. They no longer hand-write the audit-config wiring, which keeps the
    /// gate tight (`gate_whitelist_entries_are_all_live`).
    ///
    /// E2 (2026-09-23): `agent/loop_.rs` was REMOVED from this whitelist with the
    /// library-only `process_message`, its only hand-wired site.
    const HANDWIRED_AUDIT_WHITELIST: &[&str] = &["session_worker/runner/run.rs", "cron/scheduler.rs"];

    /// Recursively collect every `.rs` file under `dir`.
    fn collect_rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("test: read_dir {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("test: dir entry");
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// The production/test rule the architecture scanners use, from the one
    /// shared copy: `is_test_only_file` (the file-level `#![cfg(test)]` marker,
    /// nested block comments honoured) and `strip_cfg_test_items` (every
    /// `#[cfg(test)]` construct blanked, extents computed on comment- and
    /// literal-blanked text).
    // Scanner code shared with `tests/architecture_boundaries/`: every index
    // is bounded by its loop condition.
    #[allow(clippy::indexing_slicing)]
    mod cfg_test_scan {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/cfg_test_scan.rs"));
    }
    use cfg_test_scan::{is_test_only_file, strip_cfg_test_items};

    /// Count the lines of `src` that hand-write `.with_audit_config(` outside
    /// test code (see `cfg_test_scan`). A test-only file has none.
    fn count_production_handwired_audit(src: &str) -> usize {
        strip_cfg_test_items(src)
            .lines()
            .filter(|line| line.contains(".with_audit_config("))
            .count()
    }

    /// The file-level marker, and only the marker, makes a file test code for
    /// this gate; the same production text without it is still counted.
    #[test]
    fn a_file_level_cfg_test_marker_makes_the_whole_file_test_code() {
        let site = "fn build() {\n    let _ = SecurityPolicy::from_config(&c).with_audit_config(a);\n}\n";
        for marked in [
            format!("//! Tests of a production module.\n\n#![cfg(test)]\n#![allow(clippy::unwrap_used)]\n\n{site}"),
            format!("/* leading block comment */\n#![cfg(test)] // trailing comment\n{site}"),
        ] {
            assert!(is_test_only_file(&marked), "{marked}");
            assert_eq!(count_production_handwired_audit(&marked), 0, "{marked}");
        }
        for unmarked in [
            site.to_string(),
            format!("use super::*;\n#![cfg(test)]\n{site}"),
            format!("#![allow(clippy::unwrap_used)]\n#![cfg(test)]\n{site}"),
            format!("#![cfg(any(test, feature = \"probe\"))]\n{site}"),
            format!("#![cfg(not(test))]\n{site}"),
            format!("/* a comment that mentions\n#![cfg(test)]\n*/\n{site}"),
            format!("{site}#[cfg(test)]\nmod tests {{}}\n"),
            format!("#[cfg(test)]\nconst LIMIT: usize = 3;\n#[cfg(test)]\nmod probe_tests;\n{site}"),
            // A nested block comment around the marker is still a comment.
            format!("/* a /* b */\n#![cfg(test)]\n*/\n{site}"),
            // Brackets inside a test item's literals and comments move nothing.
            format!("#[cfg(test)]\nfn helper() {{\n    let _ = \"(\";\n    let _ = '[';\n    // (\n}}\n{site}"),
            // A comma-terminated test construct ends at its comma.
            "fn wire(\n    #[cfg(test)] probe: u8,\n    real: u8,\n) {\n    let _ = \
             SecurityPolicy::from_config(&c).with_audit_config(a);\n}\n"
                .to_string(),
        ] {
            assert!(!is_test_only_file(&unmarked), "{unmarked}");
            assert_eq!(count_production_handwired_audit(&unmarked), 1, "{unmarked}");
        }
    }

    #[test]
    fn gate_no_unwhitelisted_handwired_audit_wiring() {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs_files(&src_dir, &mut files);
        assert!(
            !files.is_empty(),
            "test: src scan found no .rs files under {}",
            src_dir.display()
        );

        // The canonical convergence point — excluded from the scan entirely.
        let bootstrap_rel = Path::new("runtime").join("bootstrap.rs");

        let mut offenders: Vec<String> = Vec::new();
        for file in &files {
            let rel = file.strip_prefix(&src_dir).expect("test: file under src_dir");
            if rel == bootstrap_rel {
                continue; // helper / convergence point owns the only sanctioned site
            }
            let contents = std::fs::read_to_string(file).expect("test: read source file for scan");
            let n = count_production_handwired_audit(&contents);
            if n == 0 {
                continue;
            }
            // Normalize to forward slashes so the whitelist is platform-stable.
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if !HANDWIRED_AUDIT_WHITELIST.contains(&rel_str.as_str()) {
                offenders.push(format!("{rel_str} ({n} site(s))"));
            }
        }

        assert!(
            offenders.is_empty(),
            "Found {} new hand-written `.with_audit_config(` site(s) outside the \
             whitelist: [{}]. These bypass `build_security_policy` and risk dropping \
             the audit block (cf. BUG-D1-01). Route the construction through \
             `crate::runtime::bootstrap::build_security_policy(config)`, or — if this \
             is a deliberate divergent path — add the file to \
             HANDWIRED_AUDIT_WHITELIST with a documented reason.",
            offenders.len(),
            offenders.join(", "),
        );
    }

    /// Pins the *exact* whitelisted set to the verified baseline so the whitelist
    /// cannot silently grow stale: every whitelisted file must still exist and still
    /// contain at least one production `.with_audit_config(`. If a path was converged
    /// (site removed) this fails, prompting the whitelist to shrink — keeping the
    /// gate tight rather than letting stale exemptions accumulate.
    #[test]
    fn gate_whitelist_entries_are_all_live() {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stale: Vec<&str> = Vec::new();
        for &rel in HANDWIRED_AUDIT_WHITELIST {
            let path = src_dir.join(rel);
            let still_handwired = std::fs::read_to_string(&path)
                .map(|c| count_production_handwired_audit(&c) > 0)
                .unwrap_or(false);
            if !still_handwired {
                stale.push(rel);
            }
        }
        assert!(
            stale.is_empty(),
            "Whitelisted file(s) no longer contain a production `.with_audit_config(` \
             site: [{}]. They appear to have been converged onto \
             `build_security_policy` — remove them from HANDWIRED_AUDIT_WHITELIST to \
             keep the audit-wiring gate tight.",
            stale.join(", "),
        );
    }

    /// security is always constructed by `build` and carries the audit config
    /// from the source `Config` (the central wiring D1 collapses 17 sites into).
    #[tokio::test]
    async fn security_carries_audit_config_from_build() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let mut config = test_config(tmp.path());
        config.security.audit.max_size_mb = 321;

        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Minimal)
            .await
            .expect("test: build for audit assertion");

        assert_eq!(
            ctx.security.audit_config.max_size_mb, 321,
            "security must carry the audit_config supplied at build time"
        );
        // security workspace_dir is derived from the same config path.
        assert_eq!(ctx.security.workspace_dir, tmp.path());
    }
}
