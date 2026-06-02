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
//! two profiles are therefore live and carry no `#[allow(dead_code)]`. Items not
//! yet reached by any wired mode — the `Minimal`/`Server`/`Channel`/`Worker`
//! profiles, the `workspace_dir` and `router` fields — keep a targeted
//! `#[allow(dead_code)]` until their owning mode adopts `AppContext`.

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
    /// Read-only config snapshot. Hot-reload travels through the slot layer (D2),
    /// not through this field.
    pub config: Arc<Config>,
    /// Single observer for the whole context (replaces 7 `create_observer` sites).
    pub observer: Arc<dyn Observer>,
    /// Security policy with the configured `security.audit` block already
    /// attached — built once here so no path can forget `with_audit_config`
    /// (collapses the 17 hand-wired sites, survey §1 / dev-plan §2.1).
    pub security: Arc<SecurityPolicy>,
    /// Workspace root as `Arc<Path>` (iron rule 7: no `PathBuf` clone, no
    /// intermediate `String`).
    // Not yet consumed by a wired mode (chat reads `config.workspace_dir`);
    // removed when a mode adopts this field.
    #[allow(dead_code)]
    pub workspace_dir: Arc<Path>,
    /// Memory backend. `None` under `Minimal` (no memory built) — keeps the
    /// `status`/`doctor` etc. early-exit paths free of new failure surface (F4).
    pub memory: Option<Arc<dyn Memory>>,
    /// Tool registry, shared as `Arc<Vec<Box<dyn Tool>>>` (matches chat's
    /// `tools_registry`). `None` for `Minimal`/`MemoryOnly` — e.g. chat
    /// `--list-sessions` early-exits before tools are needed (F4).
    pub tools: Option<Arc<Vec<Box<dyn Tool>>>>,
    /// Heuristic LLM router. Only built for profiles that need it and only when
    /// the `llm-router` feature is enabled; always constructed after memory
    /// (`agent/agent.rs:446` invariant).
    // Not yet consumed by a wired mode (chat does not use the router); removed
    // when a server/channel/worker mode adopts this field.
    #[cfg(feature = "llm-router")]
    #[allow(dead_code)]
    pub router: Option<Arc<RouterEngine>>,
}

/// Selects which subsystems a given run mode needs, so lightweight commands do
/// not pay for `memory`/`tools`/`router` they never use (dev-plan §2.3).
///
/// `Minimal` and `MemoryOnly` exist specifically to preserve the two early-exit
/// paths (survey F4): `status`/`doctor` build only the core, and chat
/// `--list-sessions` builds memory but not tools/provider/router.
///
/// `MemoryOnly` and `Interactive` are live (consumed by `chat::run`). The
/// remaining variants are not yet reached by a wired mode and carry a targeted
/// `#[allow(dead_code)]` removed when their owning mode adopts `AppContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapProfile {
    /// `status` / `doctor` / `models` / `providers`: only config/observer/security.
    #[allow(dead_code)]
    Minimal,
    /// chat `--list-sessions` early-exit: + memory, no tools/router.
    MemoryOnly,
    /// chat (interactive): full memory + tools, no router.
    Interactive,
    /// gateway / daemon: full.
    #[allow(dead_code)]
    Server,
    /// channels: full.
    #[allow(dead_code)]
    Channel,
    /// session_worker: full. NOTE — the real worker uses
    /// `manifest.workspace_dir` + `NoopObserver` + a directly-wired
    /// `SqliteMemory` (survey F5). This profile builds the generic full set for
    /// now; that specialization lands in the wiring step.
    #[allow(dead_code)]
    Worker,
}

impl BootstrapProfile {
    /// Whether this profile requires a memory backend.
    const fn needs_memory(self) -> bool {
        !matches!(self, Self::Minimal)
    }

    /// Whether this profile requires the tool registry. Tools depend on
    /// security + runtime + memory, so this also implies memory.
    const fn needs_tools(self) -> bool {
        matches!(self, Self::Interactive | Self::Server | Self::Channel | Self::Worker)
    }

    /// Whether this profile may construct the LLM router (only relevant when the
    /// `llm-router` feature is enabled). Router needs memory; it is the
    /// agent-builder style full profiles that use it.
    #[cfg(feature = "llm-router")]
    const fn needs_router(self) -> bool {
        matches!(self, Self::Server | Self::Channel | Self::Worker)
    }
}

/// Single construction entry point: all modes obtain their `Arc<AppContext>`
/// from here.
pub struct RuntimeBootstrap;

impl RuntimeBootstrap {
    /// Build the read-only core for `profile`, constructing subsystems in the
    /// hard-ordered sequence (survey §2). Every fallible step propagates via `?`
    /// (iron rules 1/6: no `unwrap`/`expect`/`panic`; fail-fast on subsystem
    /// construction errors).
    pub async fn build(config: Config, profile: BootstrapProfile) -> Result<Arc<AppContext>> {
        // Wrap config once so all downstream users share the same allocation
        // (iron rule 7: Arc over deep copy). Arc<Config> deref-coerces to &Config
        // everywhere a borrow is needed.
        let config = Arc::new(config);

        // 1. observer — first, no dependencies beyond config.
        let observer: Arc<dyn Observer> = Arc::from(observability::create_observer(&config.observability));

        // 2. security (with audit) — single source of truth; always carries the
        //    configured `security.audit` block (dev-plan §2.1, collapses 17 sites).
        let security = Arc::new(
            SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
                .with_audit_config(config.security.audit.clone()),
        );

        // workspace_dir as Arc<Path>: borrow the config path, no String detour.
        let workspace_dir: Arc<Path> = Arc::from(config.workspace_dir.as_path());

        // 3. memory — only when the profile needs it (Minimal stays None; keeps
        //    early-exit/Minimal paths free of new failure surface, F4).
        let memory: Option<Arc<dyn Memory>> = if profile.needs_memory() {
            let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage_and_routes_with_acl(
                &config.memory,
                &config.embedding_routes,
                Some(&config.storage.provider.config),
                &config.workspace_dir,
                config.api_key.as_deref(),
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

        // 5. router — strictly after memory (agent/agent.rs:446 invariant), only
        //    when the feature is on and the profile needs it.
        #[cfg(feature = "llm-router")]
        let router: Option<Arc<RouterEngine>> = if profile.needs_router() && config.router.enabled {
            // memory must already be set; needs_router() implies needs_memory().
            let mem = memory
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("memory backend must be set before enabling router"))?;
            let provider_name = config.default_provider.as_deref().unwrap_or("openrouter");
            let router_embedder = memory::create_embedder_from_config(&config, config.api_key.as_deref());
            let engine = RouterEngine::new(
                config.router.clone(),
                provider_name.to_string(),
                config.model_routes.clone(),
                mem.clone(),
                Some(router_embedder),
            )
            .await?;
            Some(Arc::new(engine))
        } else {
            None
        };

        // 6. tools — last; depends on security + runtime + memory all being ready
        //    (all_tools_with_runtime inputs, survey §2 constraint 1).
        let tools: Option<Arc<Vec<Box<dyn Tool>>>> = if profile.needs_tools() {
            // runtime is always Some when needs_tools() is true (built in step 4).
            let rt = runtime.ok_or_else(|| anyhow::anyhow!("runtime must be set before building tools"))?;
            let mem = memory
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("memory backend must be set before building tools"))?
                .clone();
            let (composio_key, composio_entity_id) = if config.composio.enabled {
                (
                    config.composio.api_key.as_deref(),
                    Some(config.composio.entity_id.as_str()),
                )
            } else {
                (None, None)
            };
            let registry = tools::all_tools_with_runtime(
                Arc::clone(&config),
                &security,
                rt,
                mem,
                composio_key,
                composio_entity_id,
                &config.browser,
                &config.http_request,
                &config.workspace_dir,
                &config.agents,
                config.api_key.as_deref(),
                &config,
            );
            Some(Arc::new(registry))
        } else {
            None
        };

        Ok(Arc::new(AppContext {
            config,
            observer,
            security,
            workspace_dir,
            memory,
            tools,
            #[cfg(feature = "llm-router")]
            router,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal usable test config: an in-memory/markdown-free SQLite memory in a
    /// temp workspace, observer disabled, router disabled. Mirrors how other
    /// modules construct `Config` for tests (`Config::default` + field tweaks).
    fn test_config(workspace: &Path) -> Config {
        let mut config = Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.config_path = workspace.join("config.toml");
        // Keep the observer cheap and deterministic in tests.
        config.observability.backend = "noop".into();
        // Disable router so the llm-router path stays off regardless of feature.
        config.router.enabled = false;
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
        assert!(ctx.tools.is_some(), "Interactive must build tools");
        assert!(
            !ctx.tools.as_ref().expect("test: tools present").is_empty(),
            "tool registry should be non-empty"
        );
        // Interactive does not enable the router.
        #[cfg(feature = "llm-router")]
        assert!(ctx.router.is_none(), "Interactive must not build router");
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
        // Router disabled in config => None even though the profile allows it.
        #[cfg(feature = "llm-router")]
        assert!(
            ctx.router.is_none(),
            "router stays None when config.router.enabled is false"
        );
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

    /// security is always constructed by `build` and carries the audit config
    /// from the source `Config` (the central wiring D1 collapses 17 sites into).
    #[tokio::test]
    async fn security_carries_audit_config_from_build() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let mut config = test_config(tmp.path());
        // Flip a distinctive audit setting and confirm it survives into ctx.security.
        config.security.audit.enabled = !crate::config::AuditConfig::default().enabled;
        let expected_enabled = config.security.audit.enabled;

        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Minimal)
            .await
            .expect("test: build for audit assertion");

        assert_eq!(
            ctx.security.audit_config.enabled, expected_enabled,
            "security must carry the audit_config supplied at build time"
        );
        // security workspace_dir is derived from the same config path.
        assert_eq!(ctx.security.workspace_dir, tmp.path());
    }
}
