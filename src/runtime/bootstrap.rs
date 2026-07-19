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

#[cfg(feature = "llm-router")]
use crate::causal_tree::CausalTreeEngine;

/// Read-only core: immutable after construction, no interior `Mutex`, shared by
/// `Arc` clone across modes and child tasks (iron rule 7: `Arc` over deep copy).
///
/// Hot-swappable resources (`provider` / `model`) and config hot-reload are
/// deliberately **not** here — they live in the slot layer (dev-plan §2.2) and
/// are built by each mode. Only the read-only core lives in `AppContext`.
pub struct AppContext {
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
    ///
    /// Also `None` for `Interactive` (chat): chat needs an *owned* `Vec` it can
    /// append the chat-side sessions tools to (built after provider + channel),
    /// so the `Interactive` profile delivers the base registry through
    /// `base_tools` instead. The "security & memory precede tools" ordering
    /// invariant is preserved either way (see `golden_trace_*` tests).
    pub tools: Option<Arc<Vec<Box<dyn Tool>>>>,
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
    /// Heuristic LLM router. Only built for profiles that need it and only when
    /// the `llm-router` feature is enabled; always constructed after memory
    /// (`agent/agent.rs:446` invariant).
    // Not yet consumed by a wired mode (chat does not use the router); removed
    // when a server/channel/worker mode adopts this field.
    #[cfg(feature = "llm-router")]
    #[allow(dead_code)]
    pub router: Option<Arc<RouterEngine>>,
    /// Speculative branch-prediction engine (CausalTree, CTE). `Some` only under
    /// the dedicated `AgentLoop` profile AND when `config.causal_tree.enabled`;
    /// `None` on every other path (including `prx chat` / `Interactive`), so no
    /// other run mode pays anything for it. Experimental, opt-in. Consumed by
    /// `loop_::run` (the live agent tool loop) — the sole CTE consumer.
    // Consumed by `loop_::run` from Commit 3 of this series onward; carries a
    // temporary dead_code allow until then.
    #[cfg(feature = "llm-router")]
    #[allow(dead_code)]
    pub cte: Option<Arc<CausalTreeEngine>>,
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
    /// chat (interactive): full memory + tools, no router, **no CTE**.
    Interactive,
    /// `loop_::run` (the live agent tool loop): the same core as `Interactive`
    /// (full memory + tools, no router) PLUS the opt-in CausalTreeEngine. This is
    /// a dedicated variant so that *only* the agent loop pays for CTE — `prx chat`
    /// (which uses `Interactive`) never builds it. Without this split, attaching
    /// CTE to the shared `Interactive` profile would assemble the engine on the
    /// chat path too, where nothing consumes it: dead wiring (iron rule 2) and a
    /// non-zero cost on a path that gains nothing.
    AgentLoop,
    /// gateway / daemon: full.
    #[allow(dead_code)]
    Server,
    /// channels: full.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    /// need the router — `loop_::run` resolves a single provider/model directly
    /// and never routes.
    #[cfg(feature = "llm-router")]
    const fn needs_router(self) -> bool {
        matches!(self, Self::Server | Self::Channel | Self::Worker)
    }

    /// Whether this profile attaches the CausalTreeEngine (CTE). It attaches
    /// **only** to the live agent tool loop (`AgentLoop`), which is the sole
    /// consumer (`loop_::run`). `Interactive` (chat) must not build it — chat has
    /// no per-turn CTE consumption point, so building it there would be dead
    /// wiring with non-zero cost (iron rule 2). `Server`/`Channel` likewise have
    /// no per-turn CTE consumer wired, and `Worker` is a reserved placeholder
    /// (session_worker takes a dedicated path, not `RuntimeBootstrap`).
    #[cfg(feature = "llm-router")]
    const fn needs_cte(self) -> bool {
        matches!(self, Self::AgentLoop)
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
        // everywhere a borrow is needed. The bootstrap also owns the one
        // process-level generation manager used by every config-aware tool.
        let config_manager = crate::config::new_shared(config);
        let config_generation = config_manager.pin();
        let config = Arc::clone(&config_generation.effective);

        // 1. observer — first, no dependencies beyond config.
        let observer: Arc<dyn Observer> = Arc::from(observability::create_observer(&config.observability));

        // 2. security (with audit) — single source of truth; always carries the
        //    configured `security.audit` block (dev-plan §2.1, collapses 17 sites).
        let security = build_security_policy(&config);

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
        let router: Option<Arc<RouterEngine>> = if profile.needs_router() {
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

        // 5b. causal_tree (CTE) — experimental, opt-in. Only the dedicated
        //     `AgentLoop` profile (consumed by `loop_::run`) attaches CTE; every
        //     other path leaves it `None` and pays nothing. CTE depends only on the
        //     observer (built in step 1) + config, so it is constructed here after
        //     the router and before tools. Short-circuit on `needs_cte() &&
        //     config.causal_tree.enabled`: when disabled, `CausalTreeEngine::new`
        //     and all five sub-engines are never constructed (zero allocation).
        #[cfg(feature = "llm-router")]
        let cte: Option<Arc<CausalTreeEngine>> = if profile.needs_cte() && config.causal_tree.enabled {
            Some(Arc::new(CausalTreeEngine::new(
                Arc::new(crate::causal_tree::expander::DefaultTreeExpander::new()),
                Arc::new(crate::causal_tree::rehearsal::DefaultRehearsalEngine::new()),
                Arc::new(crate::causal_tree::scorer::DefaultBranchScorer::new()),
                Arc::new(crate::causal_tree::selector::DefaultPathSelector::new()),
                Arc::new(crate::causal_tree::feedback::SelfSystemFeedbackWriter::new(
                    &config.workspace_dir,
                )),
                // Reuse the single context observer (matches the legacy
                // `cte_observer = observer.clone()` wiring) so CTE telemetry lands
                // on the same backend as the rest of the run.
                observer.clone(),
                config.causal_tree.clone(),
            )))
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
            let registry = tools::all_tools_with_runtime(
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
                &config.agents,
                config.api_key.as_deref(),
                &config,
            );
            if matches!(profile, BootstrapProfile::Interactive) {
                base_tools = Some(parking_lot::Mutex::new(Some(registry)));
            } else {
                tools = Some(Arc::new(registry));
            }
        }

        Ok(Arc::new(AppContext {
            config_manager,
            config_generation,
            config,
            observer,
            security,
            workspace_dir,
            memory,
            tools,
            base_tools,
            #[cfg(feature = "llm-router")]
            router,
            #[cfg(feature = "llm-router")]
            cte,
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
        // AgentLoop has the same router footprint as Interactive: never routes.
        #[cfg(feature = "llm-router")]
        assert!(ctx.router.is_none(), "AgentLoop must not build router");
    }

    // --- CTE attachment: only AgentLoop + enabled builds it (Codex condition #1) -

    /// Critical: the shared `Interactive` profile (used by `prx chat`) must NEVER
    /// build CTE, even when `config.causal_tree.enabled` is true — chat has no CTE
    /// consumer, so wiring it there would be dead code with non-zero cost.
    #[cfg(feature = "llm-router")]
    #[tokio::test]
    async fn interactive_profile_never_builds_cte_even_when_enabled() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let mut config = test_config(tmp.path());
        config.causal_tree.enabled = true;
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::Interactive)
            .await
            .expect("test: interactive build");
        assert!(
            ctx.cte.is_none(),
            "Interactive (chat) must not attach CTE even with causal_tree.enabled=true"
        );
    }

    /// `AgentLoop` + `enabled = false` (the default) → no CTE, zero cost.
    #[cfg(feature = "llm-router")]
    #[tokio::test]
    async fn agent_loop_profile_disabled_has_no_cte() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let config = test_config(tmp.path());
        // test_config leaves causal_tree.enabled at its default (false).
        assert!(!config.causal_tree.enabled, "test precondition: CTE default off");
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::AgentLoop)
            .await
            .expect("test: agent-loop build (CTE off)");
        assert!(ctx.cte.is_none(), "AgentLoop with enabled=false must not attach CTE");
    }

    /// `AgentLoop` + `enabled = true` → CTE attached (`Some`).
    #[cfg(feature = "llm-router")]
    #[tokio::test]
    async fn agent_loop_profile_enabled_attaches_cte() {
        let tmp = tempfile::tempdir().expect("test: create temp dir");
        let mut config = test_config(tmp.path());
        config.causal_tree.enabled = true;
        let ctx = RuntimeBootstrap::build(config, BootstrapProfile::AgentLoop)
            .await
            .expect("test: agent-loop build (CTE on)");
        assert!(ctx.cte.is_some(), "AgentLoop with enabled=true must attach CTE");
        let cte = ctx.cte.as_ref().expect("test: cte present");
        assert!(cte.is_enabled(), "attached CTE engine must report enabled");
    }

    /// Lightweight profiles never attach CTE regardless of config.
    #[cfg(feature = "llm-router")]
    #[tokio::test]
    async fn lightweight_profiles_never_attach_cte() {
        for profile in [BootstrapProfile::Minimal, BootstrapProfile::MemoryOnly] {
            let tmp = tempfile::tempdir().expect("test: create temp dir");
            let mut config = test_config(tmp.path());
            config.causal_tree.enabled = true;
            let ctx = RuntimeBootstrap::build(config, profile)
                .await
                .unwrap_or_else(|e| panic!("test: build {profile:?}: {e}"));
            assert!(ctx.cte.is_none(), "{profile:?}: must not attach CTE");
        }
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
    ///   - `session_worker/runner.rs`: manifest-driven divergent core
    ///     (`manifest.workspace_dir`-bound security); intentionally NOT routed through
    ///     `RuntimeBootstrap` under the D1 behavior-unchanged guardrail (see module note).
    ///   - `agent/loop_.rs`: `process_message` path, not yet converged (later D-series).
    ///   - `cron/scheduler.rs`: cron job path, not yet converged.
    ///
    /// A8 (2026-06-03): the legacy `Agent::from_config` in `agent/agent.rs` — the
    /// only production `.with_audit_config(` site in that file — was removed when the
    /// dead legacy `Agent::run` / `from_config` / `run_interactive` shell was deleted
    /// (the live path is `loop_::run`). `agent/agent.rs` was therefore REMOVED from
    /// this whitelist to keep the gate tight (`gate_whitelist_entries_are_all_live`).
    ///
    /// D2 (2026-06-02): the four gateway authorization sites — `gateway/mod.rs`
    /// (`authorize_gateway_resource_mutation`), `gateway/api/mod.rs`
    /// (`authorize_resource_mutation_for_config`), `gateway/api/config.rs`
    /// (`post_config_reload`) and `gateway/api/sessions.rs` (console runtime turn) —
    /// were all converged onto `build_security_policy` and so were REMOVED from this
    /// whitelist. They no longer hand-write the audit-config wiring, which keeps the
    /// gate tight (`gate_whitelist_entries_are_all_live`).
    const HANDWIRED_AUDIT_WHITELIST: &[&str] = &["session_worker/runner.rs", "agent/loop_.rs", "cron/scheduler.rs"];

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

    /// Count `.with_audit_config(` occurrences in `src`, ignoring lines inside any
    /// `#[cfg(test)]` module. The stripper is line-based: when it sees a
    /// `#[cfg(test)]` attribute it skips to the matching closing brace of the block
    /// that follows (a `mod tests { ... }` / `fn ... { ... }`), tracking brace depth.
    /// This is intentionally conservative — it is the same shape used by the test
    /// modules in this codebase (`#[cfg(test)]\nmod tests {`).
    fn count_production_handwired_audit(src: &str) -> usize {
        let mut count = 0usize;
        let mut lines = src.lines();
        while let Some(line) = lines.next() {
            if line.trim_start().starts_with("#[cfg(test)]") {
                // Skip until the brace opened by the following item is closed.
                // Advance to the first line containing an opening brace, then balance.
                let mut depth: i32 = 0;
                let mut started = false;
                // Consume the attribute's target item line-by-line.
                for next in lines.by_ref() {
                    for ch in next.chars() {
                        if ch == '{' {
                            depth += 1;
                            started = true;
                        } else if ch == '}' {
                            depth -= 1;
                        }
                    }
                    if started && depth <= 0 {
                        break;
                    }
                }
                continue;
            }
            if line.contains(".with_audit_config(") {
                count += 1;
            }
        }
        count
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
