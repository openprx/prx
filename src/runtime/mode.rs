//! CLI command dispatch.
//!
//! D3 (FIX-P1-21a): this module hosts the main dispatch `match` that maps a
//! fully-parsed [`Commands`] variant onto the corresponding subsystem entry
//! point. It is a **verbatim physical move** of the dispatch `match` that used
//! to live in `main.rs` immediately after the primary config load
//! (`Config::load_or_init_with_config_dir`); no behaviour, ordering, argument,
//! or control-flow change is introduced.
//!
//! Crate-layout note: although the source file lives under `src/runtime/`, this
//! module is declared from `main.rs` via `#[path = "runtime/mode.rs"] mod mode;`
//! so that it is a child of the **binary** crate root rather than the shared
//! `openprx` library crate. The dispatch references binary-only items
//! (`Commands`, the `handle_*_command` helpers, binary-only modules such as
//! `chat`/`doctor`/`integrations`/`evolution_cli`/`migration`/`service`); the
//! `runtime/` directory is compiled into *both* the lib and the bin, so a
//! `pub mod mode;` inside `runtime/mod.rs` would fail to compile in the lib.
//! Scoping the declaration to the binary keeps `crate::` resolving to these
//! items while preserving the requested file location.
//!
//! Scope notes (intentional, per the D-series plan §2.4):
//! - The five early-exit commands (`SessionWorker`, `Completions`, `Init`,
//!   `Onboard`, `Go`) are still handled in `main.rs` **before** this dispatch is
//!   reached — `SessionWorker`/`Completions`/`Init` ahead of logging init,
//!   `Onboard`/`Go` ahead of config load. They never enter [`dispatch`]. The
//!   five defensive `bail!("BUG: ... handled earlier")` arms are kept here as
//!   "should never reach" assertions, preserving the `match` exhaustiveness
//!   over [`Commands`] (a missing arm is a compile error).
//! - The `message.or(message_pos)` merge for `Commands::Agent` stays inside the
//!   `Agent` arm of this dispatch, exactly as before.
//! - The plan's standalone `enum RunMode` + `Commands -> RunMode` conversion and
//!   the `ModeRunner` trait are deliberately **not** introduced in D3. Adding a
//!   parallel enum + conversion layer would be a behaviour-risk with no payoff
//!   until the unified signature exists; that formalization is the natural D5
//!   (`ModeRunner` trait) landing point. D3 is purely the dispatch move plus
//!   retention of `Commands` exhaustiveness.

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::{self, Config};
use crate::{
    ChannelCommands, Commands, ConfigCommands, ConfigShowFormat, DoctorCommands, IntegrationCommands, ModelCommands,
};
use crate::{
    agent, channels, chat, cron, daemon, doctor, evolution_cli, gateway, integrations, memory, migration, onboard,
    providers, service, skills,
};
use crate::{
    handle_approval_command, handle_audit_command, handle_auth_command, handle_memory_command, redact_config_show_value,
};

// ══════════════════════════════════════════════════════════════════════════════
// D5/D9: ModeRunner trait + per-mode runners
// ══════════════════════════════════════════════════════════════════════════════

/// Unified entry point for the five long-running / ctrl_c-interruptible modes
/// (chat / agent / gateway / daemon / channel-start).
///
/// Each implementor owns its own constructor arguments (config / host / port /
/// …); [`run`](ModeRunner::run) only additionally receives the external **root**
/// shutdown token. Red line (D5 scope): this trait unifies *signal plumbing only*
/// — root token cancelled = "please begin graceful exit". Each mode keeps its own
/// teardown ordering/semantics; the trait carries **no** `Arc<AppContext>` (that
/// is bootstrapped inside each `run`) and introduces **no** `ModeOutcome` (all
/// `run`s return `Result<()>`).
#[async_trait::async_trait]
pub trait ModeRunner: Send {
    async fn run(self: Box<Self>, shutdown: CancellationToken) -> Result<()>;
}

/// Interactive or single-shot chat session (graceful, ctrl_c-sensitive).
pub struct ChatRunner {
    pub config: Config,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub temperature: f64,
    pub plain: bool,
    pub session: Option<String>,
    pub continue_last: bool,
    pub list_sessions: bool,
}

#[async_trait::async_trait]
impl ModeRunner for ChatRunner {
    async fn run(self: Box<Self>, shutdown: CancellationToken) -> Result<()> {
        let me = *self;
        chat::run(
            me.config,
            me.provider,
            me.model,
            me.temperature,
            me.plain,
            me.session.or_else(|| me.continue_last.then(|| "last".to_string())),
            me.list_sessions,
            shutdown,
        )
        .await
    }
}

/// Agent loop — single-shot when `message` is set, otherwise interactive.
pub struct AgentRunner {
    pub config: Config,
    pub message: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub temperature: f64,
}

#[async_trait::async_trait]
impl ModeRunner for AgentRunner {
    async fn run(self: Box<Self>, shutdown: CancellationToken) -> Result<()> {
        let me = *self;
        agent::run(me.config, me.message, me.provider, me.model, me.temperature, shutdown)
            .await
            .map(|_| ())
    }
}

/// HTTP gateway server (graceful drain on shutdown).
pub struct GatewayRunner {
    pub host: String,
    pub port: u16,
    pub config: Config,
}

#[async_trait::async_trait]
impl ModeRunner for GatewayRunner {
    async fn run(self: Box<Self>, shutdown: CancellationToken) -> Result<()> {
        let me = *self;
        // Direct `prx gateway` CLI path: no daemon-owned hot-reload watcher exists,
        // so pass `None` and let run_gateway build its own SharedConfig fallback.
        gateway::run_gateway(&me.host, me.port, me.config, None, shutdown).await
    }
}

/// Background daemon (gateway + supervised channels, abort-style teardown).
pub struct DaemonRunner {
    pub config: Config,
    pub host: String,
    pub port: u16,
}

#[async_trait::async_trait]
impl ModeRunner for DaemonRunner {
    async fn run(self: Box<Self>, shutdown: CancellationToken) -> Result<()> {
        let me = *self;
        daemon::run(me.config, me.host, me.port, shutdown).await
    }
}

/// `prx channel start` — supervise all configured inbound channels.
pub struct ChannelStartRunner {
    pub config: Config,
}

#[async_trait::async_trait]
impl ModeRunner for ChannelStartRunner {
    async fn run(self: Box<Self>, shutdown: CancellationToken) -> Result<()> {
        let me = *self;
        channels::start_channels(me.config, shutdown).await
    }
}

/// Whether `dispatch` should install a process-level signal task (SIGINT +
/// SIGTERM on unix, ctrl_c elsewhere) that cancels the root shutdown token for
/// this command (D5/D9 §3.3.3 explicit whitelist).
///
/// Only the four "long-running + gracefully signal-interruptible" modes opt in:
/// gateway / daemon / channel-start / **single-shot** agent. Everything else —
/// **chat**, **interactive** agent, and every query/management subcommand —
/// returns `false`:
///
/// - chat owns ctrl_c internally (single-press cancels generation, double-press
///   exits); a dispatch-level signal task would steal the first ctrl_c and break
///   that contract.
/// - interactive agent reads stdin synchronously and cannot `select!` a token;
///   registering `ctrl_c()` would also rob the process of its default
///   termination, deadlocking the session.
///
/// `command` is borrowed (`&Commands`) — `matches!` here must not move it, or the
/// subsequent owning `match` in [`dispatch`] would fail to compile.
pub const fn should_bind_signal(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Gateway { .. }
            | Commands::Daemon { .. }
            | Commands::Channel {
                channel_command: ChannelCommands::Start
            }
    ) || matches!(
        command,
        Commands::Agent { message_pos, message, .. } if message.is_some() || message_pos.is_some()
    )
}

/// Spawn the process-level shutdown signal task for whitelisted modes. On unix
/// this listens for both SIGINT (ctrl_c) and SIGTERM; elsewhere ctrl_c only.
/// First signal cancels the root token, asking the active mode to drain.
fn spawn_signal_task(root: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
            match sigterm {
                Ok(mut sigterm) => {
                    tokio::select! {
                        res = tokio::signal::ctrl_c() => {
                            if let Err(e) = res {
                                tracing::warn!("failed to listen for ctrl_c: {e}");
                                return;
                            }
                        }
                        _ = sigterm.recv() => {}
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to register SIGTERM handler, ctrl_c only: {e}");
                    if let Err(e) = tokio::signal::ctrl_c().await {
                        tracing::warn!("failed to listen for ctrl_c: {e}");
                        return;
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            if let Err(e) = tokio::signal::ctrl_c().await {
                tracing::warn!("failed to listen for ctrl_c: {e}");
                return;
            }
        }
        tracing::info!("shutdown signal received; requesting graceful shutdown");
        root.cancel();
    });
}

/// Dispatch a fully-parsed CLI command after the primary config load.
///
/// This is the verbatim move of the former `main.rs` `match cli.command { ... }`
/// block; `command` is the value previously matched as `cli.command` and
/// `config` is the value previously bound from `Config::load_or_init_with_config_dir`.
#[allow(clippy::too_many_lines)]
pub async fn dispatch(command: Commands, config: Config) -> Result<()> {
    // D5/D9 §3.3.3: single root shutdown token for this dispatch. The signal
    // source is installed via an *explicit whitelist* (see `should_bind_signal`)
    // — only gateway / daemon / channel-start / single-shot agent get a
    // dispatch-owned SIGINT+SIGTERM task. chat / interactive agent / query
    // commands never do (chat owns ctrl_c internally; interactive agent must keep
    // the process default termination). Borrow `&command` so the owning `match`
    // below still consumes it.
    let root_shutdown = CancellationToken::new();
    if should_bind_signal(&command) {
        spawn_signal_task(root_shutdown.clone());
    }
    match command {
        Commands::Init { .. } => anyhow::bail!("BUG: Init command should have been handled earlier"),
        Commands::Onboard { .. } => anyhow::bail!("BUG: Onboard command should have been handled earlier"),
        Commands::Completions { .. } => anyhow::bail!("BUG: Completions command should have been handled earlier"),
        Commands::SessionWorker { .. } => anyhow::bail!("BUG: SessionWorker command should have been handled earlier"),
        Commands::Go { .. } => anyhow::bail!("BUG: Go command should have been handled earlier"),

        Commands::Agent {
            message_pos,
            message,
            provider,
            model,
            temperature,
        } => {
            // Accept the message either positionally (`prx agent 'msg'`, UNIX-style)
            // or via -m/--message. clap's `conflicts_with` guarantees at most one is
            // set, so `.or()` simply picks whichever was provided.
            let message = message.or(message_pos);
            // D5/D9 (A6): drive via ModeRunner with the dispatch root token. A
            // signal task is bound only for the single-shot case (see
            // `should_bind_signal`); interactive agent receives the same token but
            // no signal source ever cancels it (synchronous stdin path).
            Box::new(AgentRunner {
                config,
                message,
                provider,
                model,
                temperature,
            })
            .run(root_shutdown.clone())
            .await
        }

        Commands::Chat {
            provider,
            model,
            temperature,
            plain,
            session,
            continue_last,
            list_sessions,
        } => {
            // D5/D9 (A6): drive via ModeRunner with the dispatch root token. chat
            // is *not* whitelisted for a dispatch signal task — its internal
            // ctrl_c single/double-press handler is the sole owner of ctrl_c and
            // cancels this same root token. Passing it here keeps the plumbing
            // uniform without introducing a competing external signal source.
            Box::new(ChatRunner {
                config,
                provider,
                model,
                temperature,
                plain,
                session,
                continue_last,
                list_sessions,
            })
            .run(root_shutdown.clone())
            .await
        }

        Commands::Gateway { port, host } => {
            let port = port.unwrap_or(config.gateway.port);
            let host = host.unwrap_or_else(|| config.gateway.host.clone());
            if port == 0 {
                info!("🚀 Starting OpenPRX Gateway on {host} (random port)");
            } else {
                info!("🚀 Starting OpenPRX Gateway on {host}:{port}");
            }
            // D5/D9 (A6): whitelisted — dispatch installs a SIGINT+SIGTERM signal
            // task that cancels the root token; run_gateway drains gracefully.
            Box::new(GatewayRunner { host, port, config })
                .run(root_shutdown.clone())
                .await
        }

        Commands::Daemon { port, host } => {
            let port = port.unwrap_or(config.gateway.port);
            let host = host.unwrap_or_else(|| config.gateway.host.clone());
            if port == 0 {
                info!("🧠 Starting OpenPRX Daemon on {host} (random port)");
            } else {
                info!("🧠 Starting OpenPRX Daemon on {host}:{port}");
            }
            // D5/D9 (A6): whitelisted — dispatch installs a SIGINT+SIGTERM signal
            // task that cancels the root token; daemon aborts its supervised tasks.
            Box::new(DaemonRunner { config, host, port })
                .run(root_shutdown.clone())
                .await
        }

        Commands::Status => {
            println!("🦀 OpenPRX Status");
            println!();
            println!("Version:     {}", env!("CARGO_PKG_VERSION"));
            println!("Workspace:   {}", config.workspace_dir.display());
            println!("Config:      {}", config.config_path.display());
            println!();
            println!(
                "🤖 Provider:      {}",
                config.default_provider.as_deref().unwrap_or("openrouter")
            );
            println!(
                "   Model:         {}",
                config.default_model.as_deref().unwrap_or("(default)")
            );
            println!("📊 Observability:  {}", config.observability.backend);
            println!("🛡️  Autonomy:      {:?}", config.autonomy.level);
            println!("⚙️  Runtime:       {}", config.runtime.kind);
            let effective_memory_backend =
                memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
            println!(
                "💓 Heartbeat:      {}",
                if config.heartbeat.enabled {
                    format!("every {}min", config.heartbeat.interval_minutes)
                } else {
                    "disabled".into()
                }
            );
            println!(
                "🧠 Memory:         {} (semantic auto-promote: {})",
                effective_memory_backend,
                if config.memory.auto_save && config.memory.semantic.auto_promote_user_messages {
                    "on"
                } else {
                    "off"
                }
            );

            println!();
            println!("Security:");
            println!("  Autonomy level:    {:?}", config.autonomy.level);
            println!("  Workspace only:    {}", config.autonomy.workspace_only);
            println!("  Max actions/hour:  {}", config.autonomy.max_actions_per_hour);
            println!(
                "  Max cost/day:      ${:.2}",
                f64::from(config.autonomy.max_cost_per_day_cents) / 100.0
            );
            println!(
                "  Audit logging:     {}",
                if config.security.audit.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            println!("  Audit log path:    {}", config.security.audit.log_path);
            println!();
            println!("Channels:");
            println!("  CLI:      ✅ always");
            for (name, configured) in [
                ("Telegram", config.channels_config.telegram.is_some()),
                ("Discord", config.channels_config.discord.is_some()),
                ("Slack", config.channels_config.slack.is_some()),
                ("Webhook", config.channels_config.webhook.is_some()),
                ("Nextcloud", config.channels_config.nextcloud_talk.is_some()),
            ] {
                println!(
                    "  {name:9} {}",
                    if configured {
                        "✅ configured"
                    } else {
                        "❌ not configured"
                    }
                );
            }
            println!();
            Ok(())
        }

        Commands::Memory { memory_command } => handle_memory_command(memory_command, &config).await,

        Commands::Evolution {
            json,
            evolution_command,
        } => evolution_cli::handle_command(evolution_command, json, &config).await,

        Commands::Cron { cron_command } => cron::handle_command(cron_command, &config),

        Commands::Models { model_command } => match model_command.unwrap_or(ModelCommands::List { provider: None }) {
            ModelCommands::List { provider } => onboard::run_models_list(&config, provider.as_deref()),
            ModelCommands::Refresh { provider, force } => {
                let config_for_refresh = config.clone();
                tokio::task::spawn_blocking(move || {
                    onboard::run_models_refresh(&config_for_refresh, provider.as_deref(), force)
                })
                .await
                .map_err(|e| anyhow::anyhow!("models refresh task failed: {e}"))?
            }
        },

        Commands::Providers => {
            let providers = providers::list_providers();
            let current = config
                .default_provider
                .as_deref()
                .unwrap_or("openrouter")
                .trim()
                .to_ascii_lowercase();
            println!("Supported providers ({} total):\n", providers.len());
            println!("  ID (use in config)  DESCRIPTION");
            println!("  ─────────────────── ───────────");
            for p in &providers {
                let is_active = p.name.eq_ignore_ascii_case(&current)
                    || p.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(&current));
                let marker = if is_active { " (active)" } else { "" };
                let local_tag = if p.local { " [local]" } else { "" };
                let aliases = if p.aliases.is_empty() {
                    String::new()
                } else {
                    format!("  (aliases: {})", p.aliases.join(", "))
                };
                println!("  {:<19} {}{}{}{}", p.name, p.display_name, local_tag, marker, aliases);
            }
            println!("\n  custom:<URL>   Any OpenAI-compatible endpoint");
            println!("  anthropic-custom:<URL>  Any Anthropic-compatible endpoint");
            Ok(())
        }

        Commands::Service {
            service_command,
            service_init,
        } => {
            let init_system = service_init.parse()?;
            service::handle_command(&service_command, &config, init_system)
        }

        Commands::Doctor { doctor_command } => match doctor_command {
            Some(DoctorCommands::Models { provider, use_cache }) => {
                let config_for_models = config.clone();
                tokio::task::spawn_blocking(move || {
                    doctor::run_models(&config_for_models, provider.as_deref(), use_cache)
                })
                .await
                .map_err(|e| anyhow::anyhow!("doctor models task failed: {e}"))?
            }
            Some(DoctorCommands::Memory) => doctor::run_memory(&config),
            Some(DoctorCommands::Runtime) => doctor::run_runtime(&config).await,
            None => doctor::run(&config),
        },

        Commands::Channel { channel_command } => match channel_command {
            ChannelCommands::Start => {
                // D5/D9 (A6): whitelisted — dispatch installs a SIGINT+SIGTERM
                // signal task that cancels the root token; the channel supervisor
                // breaks out of its listener loop on cancellation.
                Box::new(ChannelStartRunner { config }).run(root_shutdown.clone()).await
            }
            ChannelCommands::Doctor => channels::doctor_channels(config).await,
            other => channels::handle_command(other, &config).await,
        },

        Commands::Integrations { integration_command } => {
            integrations::handle_command(integration_command.unwrap_or(IntegrationCommands::List), &config)
        }

        Commands::Skills { skill_command } => skills::handle_command(skill_command, &config),

        Commands::Migrate { migrate_command } => migration::handle_command(migrate_command, &config).await,

        Commands::Auth { auth_command } => handle_auth_command(auth_command, &config).await,

        Commands::Approval { approval_command } => handle_approval_command(approval_command, &config),

        Commands::Audit { audit_command } => handle_audit_command(audit_command, &config),

        Commands::Config { config_command } => match config_command {
            ConfigCommands::Show { format } => {
                let mut value = config.to_stored_toml_value()?;
                redact_config_show_value(&mut value);
                match format {
                    ConfigShowFormat::Toml => {
                        println!(
                            "{}",
                            toml::to_string_pretty(&value).context("Failed to serialize config as TOML")?
                        );
                    }
                    ConfigShowFormat::Json => {
                        let json = serde_json::to_value(&value).context("Failed to convert config to JSON")?;
                        println!("{}", serde_json::to_string_pretty(&json)?);
                    }
                }
                Ok(())
            }
            ConfigCommands::Schema => {
                let schema = schemars::schema_for!(config::Config);
                println!("{}", serde_json::to_string_pretty(&schema)?);
                Ok(())
            }
            ConfigCommands::Split { dry_run } => {
                let preview = config::files::write_split_config(&config, dry_run).await?;
                if dry_run {
                    println!("{preview}");
                } else {
                    println!(
                        "Split configuration written to {} and {}",
                        config.config_path.display(),
                        config::files::config_dir_path(&config.config_path).display()
                    );
                }
                Ok(())
            }
            ConfigCommands::Merge => {
                config::files::merge_split_config(&config).await?;
                println!("Merged configuration into {}", config.config_path.display());
                Ok(())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelCommands, Commands};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_util::sync::CancellationToken;

    // ── Mock ModeRunner: verifies the trait is usable, that `run` is invoked
    //    exactly once, and that the external shutdown token propagates through. ──

    struct MockRunner {
        ran: Arc<AtomicBool>,
        observed_cancel: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl ModeRunner for MockRunner {
        async fn run(self: Box<Self>, shutdown: CancellationToken) -> Result<()> {
            self.ran.store(true, Ordering::SeqCst);
            // The runner observes the external root token: when the caller cancels
            // it, the runner sees the request and returns gracefully.
            shutdown.cancelled().await;
            self.observed_cancel.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn mock_runner_run_is_invoked_and_shutdown_propagates() {
        let ran = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        let runner = Box::new(MockRunner {
            ran: ran.clone(),
            observed_cancel: observed.clone(),
        });
        let token = CancellationToken::new();
        let token_for_run = token.clone();
        let handle = tokio::spawn(async move { runner.run(token_for_run).await });

        // Give the runner a chance to start and park on `cancelled()`.
        tokio::task::yield_now().await;
        assert!(ran.load(Ordering::SeqCst), "run() must have been invoked");
        assert!(!observed.load(Ordering::SeqCst), "must still be awaiting cancellation");

        // Cancelling the *external* root token must unblock the runner.
        token.cancel();
        let result = handle.await.expect("test: runner task must join");
        assert!(result.is_ok(), "graceful run returns Ok");
        assert!(
            observed.load(Ordering::SeqCst),
            "runner must observe external shutdown cancellation"
        );
    }

    #[tokio::test]
    async fn mock_runner_returns_without_shutdown() {
        // A runner that finishes on its own (never observing cancellation) still
        // returns Ok — the token is a request, not a mandatory await point.
        struct ImmediateRunner;
        #[async_trait::async_trait]
        impl ModeRunner for ImmediateRunner {
            async fn run(self: Box<Self>, _shutdown: CancellationToken) -> Result<()> {
                Ok(())
            }
        }
        let token = CancellationToken::new();
        assert!(Box::new(ImmediateRunner).run(token).await.is_ok());
    }

    // ── Signal whitelist regression (§3.3.4 速查表). The four long-running,
    //    gracefully-interruptible modes opt in; chat / interactive agent / all
    //    query+management commands must NOT bind a dispatch signal task. ──

    fn agent_single(message: bool, positional: bool) -> Commands {
        Commands::Agent {
            message_pos: if positional { Some("hi".into()) } else { None },
            message: if message { Some("hi".into()) } else { None },
            provider: None,
            model: None,
            temperature: 0.7,
        }
    }

    #[test]
    fn whitelist_binds_gateway_daemon_channel_start() {
        assert!(should_bind_signal(&Commands::Gateway { port: None, host: None }));
        assert!(should_bind_signal(&Commands::Daemon { port: None, host: None }));
        assert!(should_bind_signal(&Commands::Channel {
            channel_command: ChannelCommands::Start
        }));
    }

    #[test]
    fn whitelist_binds_single_shot_agent_only() {
        // Single-shot agent (message via -m, or positional) → bind.
        assert!(should_bind_signal(&agent_single(true, false)));
        assert!(should_bind_signal(&agent_single(false, true)));
        // Interactive agent (no message at all) → must NOT bind (synchronous
        // stdin cannot select a token; binding would steal default termination).
        assert!(!should_bind_signal(&agent_single(false, false)));
    }

    #[test]
    fn whitelist_excludes_chat() {
        // chat owns ctrl_c internally (single-press cancel / double-press exit);
        // a dispatch signal task would steal the first ctrl_c and break that.
        assert!(!should_bind_signal(&Commands::Chat {
            provider: None,
            model: None,
            temperature: 0.7,
            plain: false,
            session: None,
            continue_last: false,
            list_sessions: false,
        }));
    }

    #[test]
    fn whitelist_excludes_non_start_channel_subcommands() {
        // Only `channel start` is long-running; doctor/list/etc. are query-style.
        assert!(!should_bind_signal(&Commands::Channel {
            channel_command: ChannelCommands::Doctor
        }));
        assert!(!should_bind_signal(&Commands::Channel {
            channel_command: ChannelCommands::List
        }));
    }

    #[test]
    fn whitelist_excludes_query_and_management_commands() {
        // A representative sample of query / management commands — none long-running.
        assert!(!should_bind_signal(&Commands::Status));
        assert!(!should_bind_signal(&Commands::Providers));
        assert!(!should_bind_signal(&Commands::Doctor { doctor_command: None }));
    }
}
