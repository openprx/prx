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

/// Dispatch a fully-parsed CLI command after the primary config load.
///
/// This is the verbatim move of the former `main.rs` `match cli.command { ... }`
/// block; `command` is the value previously matched as `cli.command` and
/// `config` is the value previously bound from `Config::load_or_init_with_config_dir`.
#[allow(clippy::too_many_lines)]
pub async fn dispatch(command: Commands, config: Config) -> Result<()> {
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
            // D5/D9 step 2: placeholder shutdown token (never cancelled at this
            // stage). The real root token + signal wiring lands in A6.
            let shutdown = tokio_util::sync::CancellationToken::new();
            agent::run(config, message, provider, model, temperature, shutdown)
                .await
                .map(|_| ())
        }

        Commands::Chat {
            provider,
            model,
            temperature,
            plain,
            session,
            list_sessions,
        } => {
            // D5/D9 step 1: placeholder shutdown token (never cancelled at this
            // stage). The real root token + signal wiring lands in A6.
            let shutdown = tokio_util::sync::CancellationToken::new();
            chat::run(
                config,
                provider,
                model,
                temperature,
                plain,
                session,
                list_sessions,
                shutdown,
            )
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
            // Direct `prx gateway` CLI path: no daemon-owned hot-reload watcher
            // exists, so pass `None` and let run_gateway build its own SharedConfig
            // fallback (only the in-gateway ConfigReloadTool / API reload can update
            // it; file-watch hot-reload is daemon-only).
            // D5/D9 step 3: placeholder shutdown token (never cancelled at this
            // stage). The real root token + signal wiring lands in A6.
            let shutdown = tokio_util::sync::CancellationToken::new();
            gateway::run_gateway(&host, port, config, None, shutdown).await
        }

        Commands::Daemon { port, host } => {
            let port = port.unwrap_or(config.gateway.port);
            let host = host.unwrap_or_else(|| config.gateway.host.clone());
            if port == 0 {
                info!("🧠 Starting OpenPRX Daemon on {host} (random port)");
            } else {
                info!("🧠 Starting OpenPRX Daemon on {host}:{port}");
            }
            daemon::run(config, host, port).await
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
            println!("  Workspace only:    {}", config.autonomy.workspace_only);
            println!("  Allowed commands:  {}", config.autonomy.allowed_commands.join(", "));
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
            ChannelCommands::Start => channels::start_channels(config).await,
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
