#![warn(clippy::all, clippy::pedantic)]
#![allow(
    // CLI binary: println!/eprintln! are intentional user-facing output in this binary's main()
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::assigning_clones,
    clippy::bool_to_int_with_if,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::collapsible_if,
    clippy::default_trait_access,
    clippy::derivable_impls,
    clippy::doc_markdown,
    clippy::doc_link_with_quotes,
    clippy::enum_variant_names,
    clippy::field_reassign_with_default,
    clippy::float_cmp,
    clippy::format_collect,
    clippy::format_push_string,
    clippy::get_first,
    clippy::ignored_unit_patterns,
    clippy::implicit_clone,
    clippy::if_not_else,
    clippy::items_after_test_module,
    clippy::items_after_statements,
    clippy::large_futures,
    clippy::map_unwrap_or,
    clippy::manual_contains,
    clippy::manual_let_else,
    clippy::manual_is_multiple_of,
    clippy::manual_pattern_char_comparison,
    clippy::manual_string_new,
    clippy::match_same_arms,
    clippy::match_wildcard_for_single_variants,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_continue,
    clippy::needless_lifetimes,
    clippy::needless_return,
    clippy::needless_pass_by_value,
    clippy::needless_raw_string_hashes,
    clippy::needless_update,
    clippy::overly_complex_bool_expr,
    clippy::question_mark,
    clippy::ref_option,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::return_self_not_must_use,
    clippy::semicolon_if_nothing_returned,
    clippy::similar_names,
    clippy::single_match_else,
    clippy::should_implement_trait,
    clippy::struct_excessive_bools,
    clippy::struct_field_names,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::uninlined_format_args,
    clippy::unwrap_or_default,
    clippy::unused_self,
    clippy::bind_instead_of_map,
    clippy::cast_lossless,
    clippy::clone_on_copy,
    clippy::comparison_chain,
    clippy::elidable_lifetime_names,
    clippy::cast_precision_loss,
    clippy::assertions_on_constants,
    clippy::unnecessary_semicolon,
    clippy::unnecessary_cast,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_literal_bound,
    clippy::unnecessary_map_or,
    clippy::unnecessary_wraps,
    dead_code,
    clippy::excessive_nesting,
    clippy::single_option_map,
    clippy::trait_duplication_in_bounds,
    clippy::large_stack_frames,
    clippy::too_long_first_doc_paragraph
)]

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use dialoguer::{Input, Password};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf};
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

// P3-1: Reload handle exposed to the `chat` subcommand so it can redirect
// tracing output to ~/.openprx/chat.log while the TUI owns stderr.
//
// `BoxMakeWriter` lets us swap the writer at runtime without rebuilding the
// entire subscriber stack. The boxed writer is `Send + Sync + 'static`,
// which the reload layer requires.
//
// The fmt::Layer's `S` type parameter is the *inner* subscriber that the
// layer wraps. Since the global subscriber stack is
//   `Registry → EnvFilter → reload::Layer<fmt::Layer<...>>`,
// the fmt::Layer is layered onto `Layered<EnvFilter, Registry>`, so that's
// what `S` must be in both the layer's type and the reload handle's type.
pub(crate) type ChatSubscriber =
    tracing_subscriber::layer::Layered<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;
pub(crate) type ChatWriter = tracing_subscriber::fmt::writer::BoxMakeWriter;
pub(crate) type ChatFmtLayer = tracing_subscriber::fmt::Layer<
    ChatSubscriber,
    tracing_subscriber::fmt::format::DefaultFields,
    tracing_subscriber::fmt::format::Format<tracing_subscriber::fmt::format::Full>,
    ChatWriter,
>;
pub(crate) static CHAT_TRACING_RELOAD: std::sync::OnceLock<
    tracing_subscriber::reload::Handle<ChatFmtLayer, ChatSubscriber>,
> = std::sync::OnceLock::new();

const CONFIG_REDACTION_MASK: &str = "***";

fn parse_temperature(s: &str) -> std::result::Result<f64, String> {
    let t: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if !(0.0..=2.0).contains(&t) {
        return Err("temperature must be between 0.0 and 2.0".to_string());
    }
    Ok(t)
}

fn redact_config_show_value(value: &mut toml::Value) {
    redact_config_show_value_with_key(None, value);
}

fn redact_config_show_value_with_key(key: Option<&str>, value: &mut toml::Value) {
    if key.is_some_and(is_config_show_sensitive_key) {
        redact_config_show_sensitive_value(value);
        return;
    }

    match value {
        toml::Value::Array(items) => {
            for item in items {
                redact_config_show_value_with_key(key, item);
            }
        }
        toml::Value::Table(table) => {
            for (child_key, child_value) in table {
                redact_config_show_value_with_key(Some(child_key.as_str()), child_value);
            }
        }
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => {}
    }
}

fn redact_config_show_sensitive_value(value: &mut toml::Value) {
    match value {
        toml::Value::Array(items) => {
            for item in items {
                redact_config_show_sensitive_value(item);
            }
        }
        toml::Value::Table(table) => {
            for (_, item) in table {
                redact_config_show_sensitive_value(item);
            }
        }
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => *value = toml::Value::String(CONFIG_REDACTION_MASK.to_string()),
    }
}

fn is_config_show_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "api_key"
        || key == "api_keys"
        || key == "auth_token"
        || key == "token"
        || key == "secret"
        || key == "password"
        || key == "paired_tokens"
        || key == "db_url"
        || key == "private_key"
        || key == "access_key"
        || key == "credential"
        || key == "credentials"
        || key == "connection_string"
        || key == "signing_secret"
        || key == "webhook_secret"
        || key == "app_secret"
        || key.ends_with("_api_key")
        || key.ends_with("_api_keys")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_password")
        || key.ends_with("_key")
        || key.ends_with("_credential")
        || key.ends_with("_credentials")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("private_key")
}

mod acl;
mod agent;
mod approval;
mod auth;
mod causal_tree;
mod channels;
mod chat;
mod config;
mod cost;
mod cron;
mod daemon;
mod doctor;
mod evolution_cli;
mod gateway;
mod health;
mod heartbeat;
mod hooks;
mod identity;
mod integrations;
mod llm;
mod media;
mod memory;
mod migration;
mod multimodal;
mod nodes;
mod observability;
mod onboard;
#[cfg(feature = "wasm-plugins")]
mod plugins;
mod providers;
#[cfg(feature = "llm-router")]
mod router;
mod runtime;
mod schema_migration;
mod security;
mod self_system;
mod service;
mod session_worker;
mod skillforge;
mod skills;
mod tools;
mod tunnel;
mod util;
mod webhook;
mod xin;

use config::Config;

/// `OpenPRX` - 100% Rust. 100% Agnostic. Your AI, your rules.
#[derive(Parser, Debug)]
#[command(name = "prx")]
#[command(author = "theonlyhennygod")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "The fastest, smallest AI assistant.", long_about = None)]
struct Cli {
    #[arg(long, global = true)]
    config_dir: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum ServiceCommands {
    /// Install daemon service unit for auto-start and restart
    Install,
    /// Start daemon service
    Start,
    /// Stop daemon service
    Stop,
    /// Restart daemon service to apply latest config
    Restart,
    /// Check daemon service status
    Status,
    /// Uninstall daemon service unit
    Uninstall,
}

#[derive(Subcommand, Debug)]
enum MemoryCommands {
    /// Rebuild memory/document indexes and backfill stale embeddings where supported
    Reindex {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ApprovalCommands {
    /// List runtime approval grants
    List {
        /// Filter by approval grant schema version
        #[arg(long)]
        version: Option<u8>,
    },
    /// Verify runtime approval grants
    Verify {
        /// Verify all known grants
        #[arg(long)]
        all: bool,
        /// Grant id to verify
        grant_id: Option<String>,
    },
    /// Revoke a runtime approval grant
    Revoke {
        /// Grant id to revoke
        grant_id: String,
        /// Human-readable revocation reason
        #[arg(long, default_value = "operator revoked")]
        reason: String,
    },
}

#[derive(Subcommand, Debug)]
enum AuditCommands {
    /// Generate a local EU AI Act implementation attestation
    #[command(name = "attest-eu-ai-act")]
    AttestEuAiAct {
        /// Emit JSON to stdout
        #[arg(long)]
        json: bool,
        /// Output format
        #[arg(long, value_enum, default_value_t = AuditOutputFormat::Markdown)]
        format: AuditOutputFormat,
        /// Write the attestation to a file
        #[arg(long)]
        output: Option<PathBuf>,
        /// Include implementation notes in markdown output
        #[arg(long)]
        verbose: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum AuditOutputFormat {
    Markdown,
    Json,
}

#[derive(Debug, Serialize)]
struct EuAiActAttestation {
    generated_at: String,
    classification: String,
    default_provider: Option<String>,
    total_checks: usize,
    passed_count: usize,
    warning_count: usize,
    failed_count: usize,
    checks: Vec<EuAiActCheck>,
}

#[derive(Debug, Serialize)]
struct EuAiActCheck {
    id: &'static str,
    article: &'static str,
    control: &'static str,
    status: &'static str,
    evidence: &'static str,
    gap: &'static str,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum CompletionShell {
    #[value(name = "bash")]
    Bash,
    #[value(name = "fish")]
    Fish,
    #[value(name = "zsh")]
    Zsh,
    #[value(name = "powershell")]
    PowerShell,
    #[value(name = "elvish")]
    Elvish,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize PRX workspace with preset configuration
    Init {
        /// Configuration preset
        #[arg(long, value_enum, default_value = "minimal")]
        spec: crate::config::init::Spec,
        /// Target directory (default: ~/.openprx)
        #[arg(long)]
        dir: Option<String>,
        /// Overwrite existing configuration
        #[arg(long)]
        force: bool,
    },

    /// Initialize your workspace and configuration
    Onboard {
        /// Run the full interactive wizard (default is quick setup)
        #[arg(long)]
        interactive: bool,

        /// Reconfigure channels only (fast repair flow)
        #[arg(long)]
        channels_only: bool,

        /// API key (used in quick mode, ignored with --interactive)
        #[arg(long)]
        api_key: Option<String>,

        /// Provider name (used in quick mode, default: openrouter)
        #[arg(long)]
        provider: Option<String>,
        /// Model ID override (used in quick mode)
        #[arg(short = 'm', long)]
        model: Option<String>,
        /// Memory backend (sqlite, lucid, markdown, none) - used in quick mode, default: sqlite
        #[arg(long)]
        memory: Option<String>,
    },

    /// Instant start: provide an API key and chat immediately. No config files written.
    #[command(long_about = "\
Zero-configuration instant start.

Detects credentials from multiple file-based sources without reading \
environment variables. Provide an API key directly, or let PRX find \
one from auth-profiles, config.toml, or Claude Code OAuth.

No permanent files are written. A temporary workspace is used for \
the session.

Examples:
  prx go -k sk-ant-api03-xxx           # Anthropic (auto-detected)
  prx go -k sk-proj-xxx                # OpenAI (auto-detected)
  prx go -k sk-ant-xxx -m claude-sonnet-4-20250514
  prx go                               # use existing auth-profiles
  prx go --message 'Summarize this'    # single-shot mode")]
    Go {
        /// API key (required on first use, or use existing auth-profiles)
        #[arg(short = 'k', long = "key")]
        api_key: Option<String>,

        /// Provider (auto-detected from key prefix if omitted)
        #[arg(short = 'p', long)]
        provider: Option<String>,

        /// Model (uses provider default if omitted)
        #[arg(short = 'm', long)]
        model: Option<String>,

        /// Single message mode (non-interactive)
        #[arg(long)]
        message: Option<String>,
    },

    /// Start the AI agent loop
    #[command(long_about = "\
Start the AI agent loop.

Launches an interactive chat session with the configured AI provider. \
Use --message for single-shot queries without entering interactive mode.

Examples:
  prx agent                              # interactive session
  prx agent -m \"Summarize today's logs\"  # single message
  prx agent -p anthropic --model claude-sonnet-4-20250514")]
    Agent {
        /// Single message mode (don't enter interactive mode)
        #[arg(short, long)]
        message: Option<String>,

        /// Provider to use (openrouter, anthropic, openai, openai-codex)
        #[arg(short, long)]
        provider: Option<String>,

        /// Model to use
        #[arg(long)]
        model: Option<String>,

        /// Temperature (0.0 - 2.0)
        #[arg(short, long, default_value = "0.7", value_parser = parse_temperature)]
        temperature: f64,
    },

    /// Start an interactive chat session with streaming output
    #[command(long_about = "\
Start an interactive chat session with rich terminal experience.

Features streaming responses, tool execution display, and session history.
Uses the full Agent pipeline: memory recall, LLM routing, built-in tools,
and all configured providers.

Examples:
  prx chat                              # interactive session
  prx chat -p ollama -m llama3.3        # use local model
  prx chat -p anthropic                 # use Anthropic
  prx chat --plain                      # no ANSI colors
  prx chat --session last               # resume last session
  prx chat --session abc123             # resume specific session
  prx chat --list-sessions              # list saved sessions")]
    Chat {
        /// Provider to use (openrouter, anthropic, openai, ollama, etc.)
        #[arg(short, long)]
        provider: Option<String>,

        /// Model to use
        #[arg(short = 'm', long)]
        model: Option<String>,

        /// Temperature (0.0 - 2.0)
        #[arg(short, long, default_value = "0.7", value_parser = parse_temperature)]
        temperature: f64,

        /// Plain text output (no ANSI escapes, for piping)
        #[arg(long)]
        plain: bool,

        /// Resume a session by ID, or "last" for the most recent
        #[arg(short, long)]
        session: Option<String>,

        /// List all saved sessions and exit
        #[arg(long)]
        list_sessions: bool,
    },

    /// Start the gateway server (webhooks, websockets)
    #[command(long_about = "\
Start the gateway server (webhooks, websockets).

Runs the HTTP/WebSocket gateway that accepts incoming webhook events \
and WebSocket connections. Bind address defaults to the values in \
your config file (gateway.host / gateway.port).

Examples:
  prx gateway                  # use config defaults
  prx gateway -p 8080          # listen on port 8080
  prx gateway --host 0.0.0.0   # bind to all interfaces
  prx gateway -p 0             # random available port")]
    Gateway {
        /// Port to listen on (use 0 for random available port); defaults to config gateway.port
        #[arg(short, long)]
        port: Option<u16>,

        /// Host to bind to; defaults to config gateway.host
        #[arg(long)]
        host: Option<String>,
    },

    /// Start long-running autonomous runtime (gateway + channels + heartbeat + scheduler)
    #[command(long_about = "\
Start the long-running autonomous daemon.

Launches the full OpenPRX runtime: gateway server, all configured \
channels (Telegram, Discord, Slack, etc.), heartbeat monitor, and \
the cron scheduler. This is the recommended way to run OpenPRX in \
production or as an always-on assistant.

Use 'prx service install' to register the daemon as an OS \
service (systemd/launchd) for auto-start on boot.

Examples:
  prx daemon                   # use config defaults
  prx daemon -p 9090           # gateway on port 9090
  prx daemon --host 127.0.0.1  # localhost only")]
    Daemon {
        /// Port to listen on (use 0 for random available port); defaults to config gateway.port
        #[arg(short, long)]
        port: Option<u16>,

        /// Host to bind to; defaults to config gateway.host
        #[arg(long)]
        host: Option<String>,
    },

    /// Manage OS service lifecycle (launchd/systemd user service)
    Service {
        /// Init system to use: auto (detect), systemd, or openrc
        #[arg(long, default_value = "auto", value_parser = ["auto", "systemd", "openrc"])]
        service_init: String,

        #[command(subcommand)]
        service_command: ServiceCommands,
    },

    /// Run diagnostics for daemon/scheduler/channel freshness
    Doctor {
        #[command(subcommand)]
        doctor_command: Option<DoctorCommands>,
    },

    /// Show system status (full details)
    Status,

    /// Manage memory indexes and maintenance operations
    Memory {
        #[command(subcommand)]
        memory_command: MemoryCommands,
    },

    /// Evolution dashboard and operations
    Evolution {
        /// Output machine-readable JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        evolution_command: EvolutionCommands,
    },

    /// Configure and manage scheduled tasks
    #[command(long_about = "\
Configure and manage scheduled tasks.

Schedule recurring, one-shot, or interval-based tasks using cron \
expressions, RFC 3339 timestamps, durations, or fixed intervals.

Cron expressions use the standard 5-field format: \
'min hour day month weekday'. Timezones default to UTC; \
override with --tz and an IANA timezone name.

Examples:
  prx cron list
  prx cron add '0 9 * * 1-5' 'Good morning' --tz America/New_York
  prx cron add '*/30 * * * *' 'Check system health'
  prx cron add-at 2025-01-15T14:00:00Z 'Send reminder'
  prx cron add-every 60000 'Ping heartbeat'
  prx cron once 30m 'Run backup in 30 minutes'
  prx cron pause <task-id>
  prx cron update <task-id> --expression '0 8 * * *' --tz Europe/London")]
    Cron {
        #[command(subcommand)]
        cron_command: CronCommands,
    },

    /// Manage provider model catalogs
    Models {
        #[command(subcommand)]
        model_command: Option<ModelCommands>,
    },

    /// List supported AI providers
    Providers,

    /// Manage channels (telegram, discord, slack)
    #[command(long_about = "\
Manage communication channels.

Add, remove, list, and health-check channels that connect OpenPRX \
to messaging platforms. Supported channel types: telegram, discord, \
slack, whatsapp, matrix, imessage, email.

Examples:
  prx channel list
  prx channel doctor
  prx channel add telegram '{\"bot_token\":\"...\",\"name\":\"my-bot\"}'
  prx channel remove my-bot
  prx channel bind-telegram prx_user")]
    Channel {
        #[command(subcommand)]
        channel_command: ChannelCommands,
    },

    /// Browse 50+ integrations
    Integrations {
        #[command(subcommand)]
        integration_command: Option<IntegrationCommands>,
    },

    /// Manage skills (user-defined capabilities)
    Skills {
        #[command(subcommand)]
        skill_command: SkillCommands,
    },

    /// Migrate data from other agent runtimes
    Migrate {
        #[command(subcommand)]
        migrate_command: MigrateCommands,
    },

    /// Manage provider subscription authentication profiles
    Auth {
        #[command(subcommand)]
        auth_command: AuthCommands,
    },

    /// Manage runtime approval grants
    Approval {
        #[command(subcommand)]
        approval_command: ApprovalCommands,
    },

    /// Generate runtime audit attestations
    Audit {
        #[command(subcommand)]
        audit_command: AuditCommands,
    },

    /// Manage configuration
    #[command(long_about = "\
Manage OpenPRX configuration.

Inspect and export configuration settings. Use 'schema' to dump \
the full JSON Schema for the config file, which documents every \
available key, type, and default value.

Examples:
  prx config show                # print effective config as TOML
  prx config show --format json  # print effective config as JSON
  prx config schema              # print JSON Schema to stdout
  prx config schema > schema.json")]
    Config {
        #[command(subcommand)]
        config_command: ConfigCommands,
    },

    /// Internal worker entrypoint for process-isolated sessions
    SessionWorker {
        /// Optional task override (normally provided in stdin manifest)
        #[arg(long)]
        task: Option<String>,
        /// Optional workspace override (normally provided in stdin manifest)
        #[arg(long)]
        workspace: Option<String>,
        /// Optional memory DB override (normally provided in stdin manifest)
        #[arg(long)]
        memory_db: Option<String>,
        /// Optional allowed tools JSON array override (normally provided in stdin manifest)
        #[arg(long)]
        tools: Option<String>,
        /// Optional timeout override in seconds (normally provided in stdin manifest)
        #[arg(long)]
        timeout: Option<u64>,
    },

    /// Generate shell completion script to stdout
    #[command(long_about = "\
Generate shell completion scripts for `prx`.

The script is printed to stdout so it can be sourced directly:

Examples:
  source <(prx completions bash)
  prx completions zsh > ~/.zfunc/_prx
  prx completions fish > ~/.config/fish/completions/prx.fish")]
    Completions {
        /// Target shell
        #[arg(value_enum)]
        shell: CompletionShell,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommands {
    /// Print the effective merged configuration
    Show {
        /// Output format
        #[arg(long, value_enum, default_value_t = ConfigShowFormat::Toml)]
        format: ConfigShowFormat,
    },
    /// Dump the full configuration JSON Schema to stdout
    Schema,
    /// Split config.toml into config.d/*.toml fragments
    Split {
        /// Preview the generated files without writing them
        #[arg(long)]
        dry_run: bool,
    },
    /// Merge config.d/*.toml back into a single config.toml
    Merge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ConfigShowFormat {
    Toml,
    Json,
}

#[derive(Subcommand, Debug)]
enum EvolutionCommands {
    /// Show evolution runtime status dashboard
    Status,
    /// Show evolution history from JSONL logs
    History {
        /// Maximum rows to display
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show daily digest by date (YYYY-MM-DD)
    Digest {
        /// Date in YYYY-MM-DD format (default: today UTC)
        #[arg(long)]
        date: Option<String>,
    },
    /// Show parsed evolution_config.toml
    Config,
    /// Manually trigger one evolution cycle
    Trigger {
        /// Layer choice: L1 (memory), L2 (prompt), L3 (strategy/policy)
        #[arg(long, value_enum)]
        layer: Option<EvolutionLayerArg>,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum EvolutionLayerArg {
    #[value(name = "L1")]
    L1,
    #[value(name = "L2")]
    L2,
    #[value(name = "L3")]
    L3,
}

#[derive(Subcommand, Debug)]
enum AuthCommands {
    /// Login with OpenAI Codex OAuth
    Login {
        /// Provider (`openai-codex`)
        #[arg(long)]
        provider: String,
        /// Profile name (default: default)
        #[arg(long, default_value = "default")]
        profile: String,
        /// Use OAuth device-code flow
        #[arg(long)]
        device_code: bool,
    },
    /// Complete OAuth by pasting redirect URL or auth code
    PasteRedirect {
        /// Provider (`openai-codex`)
        #[arg(long)]
        provider: String,
        /// Profile name (default: default)
        #[arg(long, default_value = "default")]
        profile: String,
        /// Full redirect URL or raw OAuth code
        #[arg(long)]
        input: Option<String>,
    },
    /// Paste setup token / auth token (for Anthropic subscription auth)
    PasteToken {
        /// Provider (`anthropic`)
        #[arg(long)]
        provider: String,
        /// Profile name (default: default)
        #[arg(long, default_value = "default")]
        profile: String,
        /// Token value (if omitted, read interactively)
        #[arg(long)]
        token: Option<String>,
        /// Auth kind override (`authorization` or `api-key`)
        #[arg(long)]
        auth_kind: Option<String>,
    },
    /// Alias for `paste-token` (interactive by default)
    SetupToken {
        /// Provider (`anthropic`)
        #[arg(long)]
        provider: String,
        /// Profile name (default: default)
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Refresh OpenAI Codex access token using refresh token
    Refresh {
        /// Provider (`openai-codex`)
        #[arg(long)]
        provider: String,
        /// Profile name or profile id
        #[arg(long)]
        profile: Option<String>,
    },
    /// Remove auth profile
    Logout {
        /// Provider
        #[arg(long)]
        provider: String,
        /// Profile name (default: default)
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Set active profile for a provider
    Use {
        /// Provider
        #[arg(long)]
        provider: String,
        /// Profile name or full profile id
        #[arg(long)]
        profile: String,
    },
    /// List auth profiles
    List,
    /// Show auth status with active profile and token expiry info
    Status,
}

#[derive(Subcommand, Debug)]
enum MigrateCommands {
    /// Show schema migration status for the memory database
    Status,
    /// Verify applied schema migration checksums
    Verify,
    /// Preview pending schema migrations without writing
    DryRun,
    /// Plan migrations up to a target version (dry-run diff, writes nothing)
    Plan {
        /// Target schema version to plan up to (inclusive). Pending migrations
        /// with a version greater than this are excluded from the plan.
        #[arg(long)]
        target_version: String,
    },
    /// Record the current schema as the migration baseline
    Baseline,
    /// Import memory from an `OpenClaw` workspace into this `OpenPRX` workspace
    Openclaw {
        /// Optional path to `OpenClaw` workspace (defaults to ~/.openclaw/workspace)
        #[arg(long)]
        source: Option<std::path::PathBuf>,

        /// Validate and preview migration without writing any data
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
enum CronCommands {
    /// List all scheduled tasks
    List,
    /// Add a new scheduled task
    Add {
        /// Cron expression
        expression: String,
        /// Optional IANA timezone (e.g. America/Los_Angeles)
        #[arg(long)]
        tz: Option<String>,
        /// Command to run
        command: String,
    },
    /// Add a one-shot scheduled task at an RFC3339 timestamp
    AddAt {
        /// One-shot timestamp in RFC3339 format
        at: String,
        /// Command to run
        command: String,
    },
    /// Add a fixed-interval scheduled task
    AddEvery {
        /// Interval in milliseconds
        every_ms: u64,
        /// Command to run
        command: String,
    },
    /// Add a one-shot delayed task (e.g. "30m", "2h", "1d")
    Once {
        /// Delay duration
        delay: String,
        /// Command to run
        command: String,
    },
    /// Remove a scheduled task
    Remove {
        /// Task ID
        id: String,
    },
    /// Update a scheduled task
    Update {
        /// Task ID
        id: String,
        /// New cron expression
        #[arg(long)]
        expression: Option<String>,
        /// New IANA timezone
        #[arg(long)]
        tz: Option<String>,
        /// New command to run
        #[arg(long)]
        command: Option<String>,
        /// New job name
        #[arg(long)]
        name: Option<String>,
    },
    /// Pause a scheduled task
    Pause {
        /// Task ID
        id: String,
    },
    /// Resume a paused task
    Resume {
        /// Task ID
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ModelCommands {
    /// List model catalogs
    List {
        /// Provider name (defaults to configured default provider)
        #[arg(long)]
        provider: Option<String>,
    },
    /// Refresh and cache provider models
    Refresh {
        /// Provider name (defaults to configured default provider)
        #[arg(long)]
        provider: Option<String>,

        /// Force live refresh and ignore fresh cache
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum DoctorCommands {
    /// Probe model catalogs across providers and report availability
    Models {
        /// Probe a specific provider only (default: all known providers)
        #[arg(long)]
        provider: Option<String>,

        /// Prefer cached catalogs when available (skip forced live refresh)
        #[arg(long)]
        use_cache: bool,
    },
    /// Diagnose memory backend and embedding/vector configuration
    Memory,
    /// Report live runtime validation matrix readiness
    Runtime,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ChannelCommands {
    /// List configured channels
    List,
    /// Start all configured channels (Telegram, Discord, Slack)
    Start,
    /// Run health checks for configured channels
    Doctor,
    /// Add a new channel
    Add {
        /// Channel type
        channel_type: String,
        /// Configuration JSON
        config: String,
    },
    /// Remove a channel
    Remove {
        /// Channel name
        name: String,
    },
    /// Bind a Telegram identity (username or numeric user ID) into allowlist
    BindTelegram {
        /// Telegram identity to allow (username without '@' or numeric user ID)
        identity: String,
    },
}

#[derive(Subcommand, Debug)]
enum SkillCommands {
    /// List installed skills
    List,
    /// Install a skill from a git URL (HTTPS/SSH) or local path
    Install {
        /// Git URL (HTTPS/SSH) or local path
        source: String,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum IntegrationCommands {
    /// List integrations
    List,
    /// Show details about a specific integration
    Info {
        /// Integration name
        name: String,
    },
}

/// Process-wide bound on how long the runtime drop will wait for stuck
/// blocking threads (notably the reedline `read_line` task that parks in
/// `spawn_blocking` reading from stdin).
///
/// Without this bound, `#[tokio::main]`'s default runtime drop would
/// `join()` blocking threads forever; reedline's blocking stdin read does
/// not observe `CancellationToken`s, so e.g. the double-Ctrl-C exit path
/// (`shutdown.cancel()` → `chat::run` returns) would hang the process
/// until the user manually killed it. Capping the wait at 2 seconds gives
/// well-behaved shutdowns time to finish while preventing the foot-gun.
const RUNTIME_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[allow(unsafe_code)]
fn main() -> Result<()> {
    // Build the runtime explicitly (instead of using `#[tokio::main]`) so
    // that we can bound how long runtime drop waits for blocking threads.
    // See `RUNTIME_SHUTDOWN_TIMEOUT` above for the rationale.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    let result = runtime.block_on(async_main());
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

#[allow(clippy::too_many_lines)]
#[allow(unsafe_code)]
async fn async_main() -> Result<()> {
    // Install default crypto provider for Rustls TLS.
    // This prevents the error: "could not automatically determine the process-level CryptoProvider"
    // when both aws-lc-rs and ring features are available (or neither is explicitly selected).
    if let Err(e) = rustls::crypto::ring::default_provider().install_default() {
        eprintln!("Warning: Failed to install default crypto provider: {e:?}");
    }

    let cli = Cli::parse();

    if let Some(config_dir) = &cli.config_dir {
        if config_dir.trim().is_empty() {
            bail!("--config-dir cannot be empty");
        }
    }

    // session-worker must stay stdout-clean for IPC JSON.
    if let Commands::SessionWorker {
        task,
        workspace,
        memory_db,
        tools,
        timeout,
    } = &cli.command
    {
        return session_worker::runner::run_from_stdin(
            task.clone(),
            workspace.clone(),
            memory_db.clone(),
            tools.clone(),
            *timeout,
        )
        .await;
    }

    // Completions must remain stdout-only and should not load config or initialize logging.
    // This avoids warnings/log lines corrupting sourced completion scripts.
    if let Commands::Completions { shell } = &cli.command {
        let mut stdout = std::io::stdout().lock();
        write_shell_completion(*shell, &mut stdout)?;
        return Ok(());
    }

    // Init generates a fresh workspace — no existing config needed.
    if let Commands::Init { spec, dir, force } = &cli.command {
        let target = match dir {
            Some(d) => std::path::PathBuf::from(d),
            None => directories::UserDirs::new()
                .map(|u| u.home_dir().to_path_buf())
                .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
                .join(".openprx"),
        };
        return spec.generate(&target, *force);
    }

    // Initialize logging - respects RUST_LOG env var, defaults to INFO.
    // For `chat` subcommand, we build a reloadable subscriber so that the
    // chat handler can redirect tracing to ~/.openprx/chat.log once the TUI
    // takes over the terminal. Until then logs still go to stderr so startup
    // diagnostics (config errors, etc.) remain visible to the user.
    let use_stderr = matches!(
        cli.command,
        Commands::Chat { .. }
            | Commands::Config { .. }
            | Commands::Models { .. }
            | Commands::Integrations { .. }
            | Commands::Audit { .. }
    );
    if use_stderr {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;

        let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let stderr_writer = tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr);
        let fmt_layer: ChatFmtLayer = tracing_subscriber::fmt::Layer::default().with_writer(stderr_writer);
        let (reload_layer, reload_handle) = tracing_subscriber::reload::Layer::new(fmt_layer);
        if CHAT_TRACING_RELOAD.set(reload_handle).is_err() {
            eprintln!("BUG: CHAT_TRACING_RELOAD already initialized");
        }
        if let Err(e) = tracing_subscriber::registry()
            .with(env_filter)
            .with(reload_layer)
            .try_init()
        {
            eprintln!("failed to set default tracing subscriber: {e}");
        }
    } else {
        let subscriber = fmt::Subscriber::builder()
            .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .finish();
        if let Err(e) = tracing::subscriber::set_global_default(subscriber) {
            eprintln!("failed to set default tracing subscriber: {e}");
        }
    }

    // Onboard runs quick setup by default, or the interactive wizard with --interactive.
    // The onboard wizard uses reqwest::blocking internally, which creates its own
    // Tokio runtime. To avoid "Cannot drop a runtime in a context where blocking is
    // not allowed", we run the wizard on a blocking thread via spawn_blocking.
    if let Commands::Onboard {
        interactive,
        channels_only,
        api_key,
        provider,
        model,
        memory,
    } = &cli.command
    {
        let interactive = *interactive;
        let channels_only = *channels_only;
        let api_key = api_key.clone();
        let provider = provider.clone();
        let model = model.clone();
        let memory = memory.clone();

        if interactive && channels_only {
            bail!("Use either --interactive or --channels-only, not both");
        }
        if channels_only && (api_key.is_some() || provider.is_some() || model.is_some() || memory.is_some()) {
            bail!("--channels-only does not accept --api-key, --provider, --model, or --memory");
        }
        let config_dir = cli.config_dir.as_deref();
        let autostart_config = if channels_only {
            let (config, autostart) = onboard::wizard::run_channels_repair_wizard(config_dir).await?;
            if autostart { Some(config) } else { None }
        } else if interactive {
            let (config, autostart) = onboard::wizard::run_wizard(config_dir).await?;
            if autostart { Some(config) } else { None }
        } else {
            onboard::wizard::run_quick_setup(
                api_key.as_deref(),
                provider.as_deref(),
                model.as_deref(),
                memory.as_deref(),
                config_dir,
            )
            .await?;
            None
        };
        if let Some(config) = autostart_config {
            channels::start_channels(config).await?;
        }
        return Ok(());
    }

    // `prx go` — zero-config instant start. No permanent files written.
    if let Commands::Go {
        api_key,
        provider,
        model,
        message,
    } = &cli.command
    {
        let (detected_provider, detected_key, detected_model) = onboard::auto_detect::detect_credentials(
            api_key.as_deref(),
            provider.as_deref(),
            model.as_deref(),
        )
        .context(
            "No API key found. Run `prx auth paste-token --provider anthropic`, or use: prx go -k <your-api-key>",
        )?;

        let tmpdir = tempfile::tempdir().context("failed to create temporary workspace for `prx go`")?;

        let mut config = Config::default();
        config.default_provider = Some(detected_provider);
        config.default_model = Some(detected_model);
        config.api_key = Some(detected_key);
        config.workspace_dir = tmpdir.path().to_path_buf();
        config.config_path = tmpdir.path().join("config.toml");

        if let Some(msg) = message {
            let result = agent::run(
                config,
                Some(msg.clone()),
                None, // provider already set in config
                None, // model already set in config
                0.7,
            )
            .await;
            // Keep tmpdir alive until agent finishes, then drop
            drop(tmpdir);
            return result.map(|_| ());
        }

        let result = chat::run(
            config, None, // provider already set in config
            None, // model already set in config
            0.7, false, // plain mode off
            None,  // no session resume
            false, // don't list sessions
        )
        .await;
        // Keep tmpdir alive until chat finishes, then drop
        drop(tmpdir);
        return result;
    }

    // All other commands need config loaded first
    let config = Config::load_or_init_with_config_dir(cli.config_dir.as_deref()).await?;

    match cli.command {
        Commands::Init { .. } => anyhow::bail!("BUG: Init command should have been handled earlier"),
        Commands::Onboard { .. } => anyhow::bail!("BUG: Onboard command should have been handled earlier"),
        Commands::Completions { .. } => anyhow::bail!("BUG: Completions command should have been handled earlier"),
        Commands::SessionWorker { .. } => anyhow::bail!("BUG: SessionWorker command should have been handled earlier"),
        Commands::Go { .. } => anyhow::bail!("BUG: Go command should have been handled earlier"),

        Commands::Agent {
            message,
            provider,
            model,
            temperature,
        } => agent::run(config, message, provider, model, temperature)
            .await
            .map(|_| ()),

        Commands::Chat {
            provider,
            model,
            temperature,
            plain,
            session,
            list_sessions,
        } => chat::run(config, provider, model, temperature, plain, session, list_sessions).await,

        Commands::Gateway { port, host } => {
            let port = port.unwrap_or(config.gateway.port);
            let host = host.unwrap_or_else(|| config.gateway.host.clone());
            if port == 0 {
                info!("🚀 Starting OpenPRX Gateway on {host} (random port)");
            } else {
                info!("🚀 Starting OpenPRX Gateway on {host}:{port}");
            }
            gateway::run_gateway(&host, port, config).await
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

async fn handle_memory_command(command: MemoryCommands, config: &Config) -> anyhow::Result<()> {
    match command {
        MemoryCommands::Reindex { json } => {
            if !config.modules.memory {
                bail!("memory module is disabled; enable [modules].memory before running memory maintenance");
            }
            let memory = memory::create_memory_with_storage_and_routes_with_acl(
                &config.memory,
                &config.embedding_routes,
                Some(&config.storage.provider.config),
                &config.workspace_dir,
                config.api_key.as_deref(),
                &config.identity_bindings,
                &config.user_policies,
            )?;
            let backend = memory.name().to_string();
            let repaired = memory.reindex().await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "backend": backend,
                        "reindexed": repaired,
                    })
                );
            } else {
                println!("Memory reindex complete for {backend}: {repaired} stale vectors rebuilt");
            }
            Ok(())
        }
    }
}

fn write_shell_completion<W: Write>(shell: CompletionShell, writer: &mut W) -> Result<()> {
    use clap_complete::generate;
    use clap_complete::shells;

    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();

    match shell {
        CompletionShell::Bash => generate(shells::Bash, &mut cmd, bin_name, writer),
        CompletionShell::Fish => generate(shells::Fish, &mut cmd, bin_name, writer),
        CompletionShell::Zsh => generate(shells::Zsh, &mut cmd, bin_name, writer),
        CompletionShell::PowerShell => {
            generate(shells::PowerShell, &mut cmd, bin_name, writer);
        }
        CompletionShell::Elvish => generate(shells::Elvish, &mut cmd, bin_name, writer),
    }

    writer.flush()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingOpenAiLogin {
    profile: String,
    code_verifier: String,
    state: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingOpenAiLoginFile {
    profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_verifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_code_verifier: Option<String>,
    state: String,
    created_at: String,
}

fn pending_openai_login_path(config: &Config) -> std::path::PathBuf {
    auth::state_dir_from_config(config).join("auth-openai-pending.json")
}

fn pending_openai_secret_store(config: &Config) -> security::secrets::SecretStore {
    security::secrets::SecretStore::new(&auth::state_dir_from_config(config), config.secrets.encrypt)
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn save_pending_openai_login(config: &Config, pending: &PendingOpenAiLogin) -> Result<()> {
    let path = pending_openai_login_path(config);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let secret_store = pending_openai_secret_store(config);
    let encrypted_code_verifier = secret_store.encrypt(&pending.code_verifier)?;
    let persisted = PendingOpenAiLoginFile {
        profile: pending.profile.clone(),
        code_verifier: None,
        encrypted_code_verifier: Some(encrypted_code_verifier),
        state: pending.state.clone(),
        created_at: pending.created_at.clone(),
    };
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let json = serde_json::to_vec_pretty(&persisted)?;
    std::fs::write(&tmp, json)?;
    set_owner_only_permissions(&tmp)?;
    std::fs::rename(tmp, &path)?;
    set_owner_only_permissions(&path)?;
    Ok(())
}

fn load_pending_openai_login(config: &Config) -> Result<Option<PendingOpenAiLogin>> {
    let path = pending_openai_login_path(config);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let persisted: PendingOpenAiLoginFile = serde_json::from_slice(&bytes)?;
    let secret_store = pending_openai_secret_store(config);
    let code_verifier = if let Some(encrypted) = persisted.encrypted_code_verifier {
        secret_store.decrypt(&encrypted)?
    } else if let Some(plaintext) = persisted.code_verifier {
        plaintext
    } else {
        bail!("Pending OpenAI login is missing code verifier");
    };
    Ok(Some(PendingOpenAiLogin {
        profile: persisted.profile,
        code_verifier,
        state: persisted.state,
        created_at: persisted.created_at,
    }))
}

fn clear_pending_openai_login(config: &Config) {
    let path = pending_openai_login_path(config);
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
        let _ = file.set_len(0);
        let _ = file.sync_all();
    }
    let _ = std::fs::remove_file(path);
}

fn read_auth_input(prompt: &str) -> Result<String> {
    let input = Password::new()
        .with_prompt(prompt)
        .allow_empty_password(false)
        .interact()?;
    Ok(input.trim().to_string())
}

fn read_plain_input(prompt: &str) -> Result<String> {
    let input: String = Input::new().with_prompt(prompt).interact_text()?;
    Ok(input.trim().to_string())
}

fn extract_openai_account_id_for_profile(access_token: &str) -> Option<String> {
    let account_id = auth::openai_oauth::extract_account_id_from_jwt(access_token);
    if account_id.is_none() {
        warn!(
            "Could not extract OpenAI account id from OAuth access token; \
             requests may fail until re-authentication."
        );
    }
    account_id
}

fn format_expiry(profile: &auth::profiles::AuthProfile) -> String {
    profile
        .token_set
        .as_ref()
        .and_then(|token_set| token_set.expires_at)
        .map_or_else(
            || "n/a".to_string(),
            |ts| {
                let now = chrono::Utc::now();
                if ts <= now {
                    format!("expired at {}", ts.to_rfc3339())
                } else {
                    let mins = (ts - now).num_minutes();
                    format!("expires in {mins}m ({})", ts.to_rfc3339())
                }
            },
        )
}

fn build_eu_ai_act_attestation(config: &Config) -> EuAiActAttestation {
    let checks = vec![
        EuAiActCheck {
            id: "L01",
            article: "Art.12",
            control: "Side-effect gate decisions are audit-loggable",
            status: "pass",
            evidence: "SideEffectGate emits ToolGate audit events through AuditLogger.",
            gap: "Retention policy still needs a configurable floor.",
        },
        EuAiActCheck {
            id: "L02",
            article: "Art.12",
            control: "Audit log retention floor",
            status: "warning",
            evidence: "security.audit has enablement and size settings.",
            gap: "No six-month minimum retention configuration is enforced yet.",
        },
        EuAiActCheck {
            id: "L03",
            article: "Art.12",
            control: "Control ladder trace rotation",
            status: "warning",
            evidence: "control_ladder_traces records runtime decisions.",
            gap: "Rotation and archival policy are still pending.",
        },
        EuAiActCheck {
            id: "L04",
            article: "Art.12",
            control: "Message timeline records router/provider events",
            status: "pass",
            evidence: "router.route_decision and provider.final_outcome message_events are persisted.",
            gap: "Provider policy URL metadata is not attached to every event.",
        },
        EuAiActCheck {
            id: "T01",
            article: "Art.13",
            control: "Provider routing transparency",
            status: "pass",
            evidence: "RouteDecision and ProviderExecutionOutcome capture selected provider and model.",
            gap: "User-facing provider transparency pages are still pending.",
        },
        EuAiActCheck {
            id: "T02",
            article: "Art.13",
            control: "Third-party LLM data policy disclosure",
            status: "warning",
            evidence: "Provider names are available in runtime configuration.",
            gap: "Policy URLs and data-use terms are not emitted by doctor/chat yet.",
        },
        EuAiActCheck {
            id: "T04",
            article: "Art.50",
            control: "AI interaction notice",
            status: "fail",
            evidence: "No universal first-message disclosure was found in runtime routing.",
            gap: "Channel adapters need a first-contact AI identity notice.",
        },
        EuAiActCheck {
            id: "H01",
            article: "Art.14",
            control: "ApprovalGrant v2 cryptographic witness",
            status: "pass",
            evidence: "ApprovalGrantV2 signs and verifies grants with Ed25519 witness signatures.",
            gap: "Persistence, revocation, and rotation policy remain future work.",
        },
        EuAiActCheck {
            id: "H02",
            article: "Art.14",
            control: "Side-effect tool gate coverage",
            status: "pass",
            evidence: "Fourteen side-effect tools call authorize_resource_operation.",
            gap: "Inbound channel listeners still need end-to-end supervised-mode coverage.",
        },
        EuAiActCheck {
            id: "H03",
            article: "Art.14",
            control: "High-risk operation blocking",
            status: "pass",
            evidence: "ResourceRiskLevel::High is blocked when policy disables high-risk operations.",
            gap: "Policy presets need operator-facing documentation.",
        },
        EuAiActCheck {
            id: "H04",
            article: "Art.14",
            control: "Inbound listener human oversight",
            status: "warning",
            evidence: "Tool execution paths are gated.",
            gap: "Native channel inbound paths still require explicit gate integration.",
        },
        EuAiActCheck {
            id: "A01",
            article: "Art.15",
            control: "Retrieval traceability",
            status: "pass",
            evidence: "retrieval_traces and fail-fast retrieval memory traits are present.",
            gap: "All context injection paths need trace coverage.",
        },
        EuAiActCheck {
            id: "A02",
            article: "Art.15",
            control: "Context compaction provenance",
            status: "pass",
            evidence: "compaction_runs records source events and compaction status.",
            gap: "Summary fidelity scoring is not yet enforced as a runtime gate.",
        },
        EuAiActCheck {
            id: "A03",
            article: "Art.15",
            control: "Prompt-injection content boundaries",
            status: "warning",
            evidence: "Document and retrieval layers are separated in memory traits.",
            gap: "Retrieved document chunks still need structured trust-boundary wrappers.",
        },
        EuAiActCheck {
            id: "A04",
            article: "Art.15",
            control: "Vector-store row-level isolation",
            status: "fail",
            evidence: "Owner-centric fields exist in memory models.",
            gap: "pgvector SQL row-level security policy is not implemented.",
        },
        EuAiActCheck {
            id: "Q01",
            article: "Art.16",
            control: "Dependency vulnerability gate",
            status: "pass",
            evidence: "cargo audit is part of the current milestone validation surface.",
            gap: "CI evidence should be attached to release attestations.",
        },
        EuAiActCheck {
            id: "Q02",
            article: "Art.16",
            control: "Rust quality gate",
            status: "pass",
            evidence: "fmt, clippy -D warnings, cargo check, and cargo test are local release gates.",
            gap: "Business E2E evidence remains deploy-gated.",
        },
        EuAiActCheck {
            id: "Q03",
            article: "Art.16",
            control: "Quality management documentation",
            status: "warning",
            evidence: "Reaudit and milestone reports define current acceptance gates.",
            gap: "A formal QMS document set is still pending.",
        },
        EuAiActCheck {
            id: "C01",
            article: "Art.17",
            control: "Conformity assessment attestation",
            status: "warning",
            evidence: "This command generates an implementation attestation.",
            gap: "A signed release-level conformity assessment document is still pending.",
        },
        EuAiActCheck {
            id: "C02",
            article: "Art.18",
            control: "Declaration of conformity template",
            status: "fail",
            evidence: "No declaration template is currently emitted.",
            gap: "DoC template requires product identity, version, operator, and scope fields.",
        },
        EuAiActCheck {
            id: "M01",
            article: "Art.19",
            control: "Runtime monitoring surfaces",
            status: "pass",
            evidence: "doctor runtime and control ladder traces expose runtime health and decision data.",
            gap: "Long-running trace rotation remains pending.",
        },
        EuAiActCheck {
            id: "M02",
            article: "Art.19",
            control: "Incident response runbook",
            status: "warning",
            evidence: "Audit events can capture denied side-effect attempts.",
            gap: "Operator incident triage and escalation runbooks are not documented.",
        },
        EuAiActCheck {
            id: "M03",
            article: "Art.19",
            control: "Post-market monitoring report cadence",
            status: "warning",
            evidence: "Runtime traces provide raw monitoring inputs.",
            gap: "Weekly/monthly monitoring report templates are still pending.",
        },
        EuAiActCheck {
            id: "M04",
            article: "Art.19",
            control: "Serious incident reporting workflow",
            status: "fail",
            evidence: "No dedicated serious-incident workflow is currently wired.",
            gap: "A 72-hour reporting workflow is needed for high-risk deployments.",
        },
    ];

    let passed_count = checks.iter().filter(|check| check.status == "pass").count();
    let warning_count = checks.iter().filter(|check| check.status == "warning").count();
    let failed_count = checks.iter().filter(|check| check.status == "fail").count();

    EuAiActAttestation {
        generated_at: chrono::Utc::now().to_rfc3339(),
        classification: "Art.50 Transparency Obligations System with high-risk-compatible controls".to_string(),
        default_provider: config.default_provider.clone(),
        total_checks: checks.len(),
        passed_count,
        warning_count,
        failed_count,
        checks,
    }
}

fn render_eu_ai_act_attestation_markdown(attestation: &EuAiActAttestation, verbose: bool) -> String {
    let mut output = String::new();
    output.push_str("# PRX EU AI Act Implementation Attestation\n\n");
    output.push_str(&format!("- Generated at: {}\n", attestation.generated_at));
    output.push_str(&format!("- Classification: {}\n", attestation.classification));
    output.push_str(&format!(
        "- Checks: {} total, {} pass, {} warning, {} fail\n\n",
        attestation.total_checks, attestation.passed_count, attestation.warning_count, attestation.failed_count
    ));
    output.push_str("| ID | Article | Status | Control |\n");
    output.push_str("|----|---------|--------|---------|\n");
    for check in &attestation.checks {
        output.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            check.id, check.article, check.status, check.control
        ));
        if verbose {
            output.push_str(&format!(
                "\n{} evidence: {}\n\n{} gap: {}\n\n",
                check.id, check.evidence, check.id, check.gap
            ));
        }
    }
    output
}

fn render_eu_ai_act_attestation_json(attestation: &EuAiActAttestation) -> Result<String> {
    serde_json::to_string_pretty(attestation).context("Failed to serialize EU AI Act attestation")
}

fn handle_audit_command(audit_command: AuditCommands, config: &Config) -> Result<()> {
    match audit_command {
        AuditCommands::AttestEuAiAct {
            json,
            format,
            output,
            verbose,
        } => {
            let attestation = build_eu_ai_act_attestation(config);
            let stdout_json = json || format == AuditOutputFormat::Json;
            let stdout_rendered = if stdout_json {
                render_eu_ai_act_attestation_json(&attestation)?
            } else {
                render_eu_ai_act_attestation_markdown(&attestation, verbose)
            };

            if let Some(path) = output {
                let write_json = path.extension().and_then(|ext| ext.to_str()) == Some("json") || stdout_json;
                let file_rendered = if write_json {
                    render_eu_ai_act_attestation_json(&attestation)?
                } else {
                    render_eu_ai_act_attestation_markdown(&attestation, verbose)
                };
                fs::write(&path, file_rendered)
                    .with_context(|| format!("Failed to write EU AI Act attestation to {}", path.display()))?;
            }

            println!("{stdout_rendered}");
            Ok(())
        }
    }
}

fn handle_approval_command(approval_command: ApprovalCommands, config: &Config) -> Result<()> {
    let conn = open_approval_ledger(config)?;
    match approval_command {
        ApprovalCommands::List { version } => {
            let version = version.unwrap_or(2);
            if version == acl::approval_grant::ApprovalGrantV2::VERSION {
                let mut stmt = conn.prepare(
                    "SELECT grant_id, owner_id, principal_id, capability_op_id, expires_at, revoked_at
                     FROM approval_grants
                     WHERE version = ?1
                     ORDER BY issued_at DESC
                     LIMIT 50",
                )?;
                let rows = stmt.query_map([i64::from(version)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                })?;
                let grants = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                if grants.is_empty() {
                    println!("no approval grants found (version: 2)");
                } else {
                    for (grant_id, owner_id, principal_id, op_id, expires_at, revoked_at) in grants {
                        let status = if revoked_at.is_some() { "revoked" } else { "active" };
                        println!(
                            "{grant_id}\t{status}\towner={owner_id}\tprincipal={principal_id}\top={op_id}\texpires={expires_at}"
                        );
                    }
                }
                Ok(())
            } else {
                bail!("unsupported approval grant version: {version}")
            }
        }
        ApprovalCommands::Verify { all, grant_id } => {
            if all {
                let total: i64 = conn.query_row("SELECT COUNT(*) FROM approval_grants", [], |row| row.get(0))?;
                let malformed: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM approval_grants
                     WHERE grant_json = '' OR signature_alg = '' OR signed_payload_sha256 = ''",
                    [],
                    |row| row.get(0),
                )?;
                if total == 0 {
                    println!("no approval grants found to verify");
                } else if malformed == 0 {
                    println!("approval grant ledger metadata verified: {total} grants");
                } else {
                    bail!("approval grant ledger metadata verification failed: {malformed}/{total} malformed grants");
                }
                return Ok(());
            }
            let Some(grant_id) = grant_id else {
                bail!("provide a grant id or --all");
            };
            let found: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT grant_id, signature_alg, signed_payload_sha256
                     FROM approval_grants
                     WHERE grant_id = ?1",
                    [grant_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((grant_id, signature_alg, payload_sha)) = found else {
                bail!("approval grant not found: {grant_id}");
            };
            if signature_alg.trim().is_empty() || payload_sha.trim().is_empty() {
                bail!("approval grant metadata verification failed: {grant_id}");
            }
            println!("approval grant metadata verified: {grant_id} ({signature_alg})");
            Ok(())
        }
        ApprovalCommands::Revoke { grant_id, reason } => {
            let now = Utc::now().to_rfc3339();
            let updated = conn.execute(
                "UPDATE approval_grants
                 SET revoked_at = COALESCE(revoked_at, ?2),
                     revocation_reason = COALESCE(revocation_reason, ?3),
                     updated_at = ?2
                 WHERE grant_id = ?1",
                params![grant_id, now, reason],
            )?;
            if updated == 0 {
                bail!("approval grant not found: {grant_id}");
            }
            conn.execute(
                "INSERT INTO approval_grant_events (event_id, grant_id, event_type, actor, occurred_at, payload_json)
                 VALUES (?1, ?2, 'grant.revoked', 'cli', ?3, ?4)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    grant_id,
                    now,
                    serde_json::json!({ "reason": reason }).to_string()
                ],
            )?;
            println!("approval grant revoked: {grant_id}");
            Ok(())
        }
    }
}

fn open_approval_ledger(config: &Config) -> Result<Connection> {
    let db_path = config.workspace_dir.join("memory").join("brain.db");
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create approval ledger directory: {}", parent.display()))?;
    }
    let conn =
        Connection::open(&db_path).with_context(|| format!("Failed to open approval ledger: {}", db_path.display()))?;
    memory::sqlite::init_approval_grant_schema(&conn)?;
    Ok(conn)
}

#[allow(clippy::too_many_lines)]
async fn handle_auth_command(auth_command: AuthCommands, config: &Config) -> Result<()> {
    let auth_service = auth::AuthService::from_config(config);

    match auth_command {
        AuthCommands::Login {
            provider,
            profile,
            device_code,
        } => {
            let provider = auth::normalize_provider(&provider)?;
            if provider != "openai-codex" {
                bail!("`auth login` currently supports only --provider openai-codex");
            }

            let client = reqwest::Client::new();

            if device_code {
                match auth::openai_oauth::start_device_code_flow(&client).await {
                    Ok(device) => {
                        println!("OpenAI device-code login started.");
                        println!("Visit: {}", device.verification_uri);
                        println!("Code:  {}", device.user_code);
                        if let Some(uri_complete) = &device.verification_uri_complete {
                            println!("Fast link: {uri_complete}");
                        }
                        if let Some(message) = &device.message {
                            println!("{message}");
                        }

                        let token_set = auth::openai_oauth::poll_device_code_tokens(&client, &device).await?;
                        let account_id = extract_openai_account_id_for_profile(&token_set.access_token);

                        auth_service.store_openai_tokens(&profile, token_set, account_id, true)?;
                        clear_pending_openai_login(config);

                        println!("Saved profile {profile}");
                        println!("Active profile for openai-codex: {profile}");
                        return Ok(());
                    }
                    Err(e) => {
                        println!("Device-code flow unavailable: {e}. Falling back to browser/paste flow.");
                    }
                }
            }

            let pkce = auth::openai_oauth::generate_pkce_state();
            let pending = PendingOpenAiLogin {
                profile: profile.clone(),
                code_verifier: pkce.code_verifier.clone(),
                state: pkce.state.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            save_pending_openai_login(config, &pending)?;

            let authorize_url = auth::openai_oauth::build_authorize_url(&pkce);
            println!("Open this URL in your browser and authorize access:");
            println!("{authorize_url}");
            println!();
            println!("Waiting for callback at http://localhost:1455/auth/callback ...");

            let code = match auth::openai_oauth::receive_loopback_code(&pkce.state, std::time::Duration::from_secs(180))
                .await
            {
                Ok(code) => code,
                Err(e) => {
                    println!("Callback capture failed: {e}");
                    println!("Run `prx auth paste-redirect --provider openai-codex --profile {profile}`");
                    return Ok(());
                }
            };

            let token_set = auth::openai_oauth::exchange_code_for_tokens(&client, &code, &pkce).await?;
            let account_id = extract_openai_account_id_for_profile(&token_set.access_token);

            auth_service.store_openai_tokens(&profile, token_set, account_id, true)?;
            clear_pending_openai_login(config);

            println!("Saved profile {profile}");
            println!("Active profile for openai-codex: {profile}");
            Ok(())
        }

        AuthCommands::PasteRedirect {
            provider,
            profile,
            input,
        } => {
            let provider = auth::normalize_provider(&provider)?;
            if provider != "openai-codex" {
                bail!("`auth paste-redirect` currently supports only --provider openai-codex");
            }

            let pending = load_pending_openai_login(config)?.ok_or_else(|| {
                anyhow::anyhow!("No pending OpenAI login found. Run `prx auth login --provider openai-codex` first.")
            })?;

            if pending.profile != profile {
                bail!(
                    "Pending login profile mismatch: pending={}, requested={}",
                    pending.profile,
                    profile
                );
            }

            let redirect_input = match input {
                Some(value) => value,
                None => read_plain_input("Paste redirect URL or OAuth code")?,
            };

            let code = auth::openai_oauth::parse_code_from_redirect(&redirect_input, Some(&pending.state))?;

            let pkce = auth::openai_oauth::PkceState {
                code_verifier: pending.code_verifier.clone(),
                code_challenge: String::new(),
                state: pending.state.clone(),
            };

            let client = reqwest::Client::new();
            let token_set = auth::openai_oauth::exchange_code_for_tokens(&client, &code, &pkce).await?;
            let account_id = extract_openai_account_id_for_profile(&token_set.access_token);

            auth_service.store_openai_tokens(&profile, token_set, account_id, true)?;
            clear_pending_openai_login(config);

            println!("Saved profile {profile}");
            println!("Active profile for openai-codex: {profile}");
            Ok(())
        }

        AuthCommands::PasteToken {
            provider,
            profile,
            token,
            auth_kind,
        } => {
            let provider = auth::normalize_provider(&provider)?;
            let token = match token {
                Some(token) => token.trim().to_string(),
                None => read_auth_input("Paste token")?,
            };
            if token.is_empty() {
                bail!("Token cannot be empty");
            }

            let kind = auth::anthropic_token::detect_auth_kind(&token, auth_kind.as_deref());
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("auth_kind".to_string(), kind.as_metadata_value().to_string());

            auth_service.store_provider_token(&provider, &profile, &token, metadata, true)?;
            println!("Saved profile {profile}");
            println!("Active profile for {provider}: {profile}");
            Ok(())
        }

        AuthCommands::SetupToken { provider, profile } => {
            let provider = auth::normalize_provider(&provider)?;
            let token = read_auth_input("Paste token")?;
            if token.is_empty() {
                bail!("Token cannot be empty");
            }

            let kind = auth::anthropic_token::detect_auth_kind(&token, Some("authorization"));
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("auth_kind".to_string(), kind.as_metadata_value().to_string());

            auth_service.store_provider_token(&provider, &profile, &token, metadata, true)?;
            println!("Saved profile {profile}");
            println!("Active profile for {provider}: {profile}");
            Ok(())
        }

        AuthCommands::Refresh { provider, profile } => {
            let provider = auth::normalize_provider(&provider)?;
            if provider != "openai-codex" {
                bail!("`auth refresh` currently supports only --provider openai-codex");
            }

            match auth_service.get_valid_openai_access_token(profile.as_deref()).await? {
                Some(_) => {
                    println!("OpenAI Codex token is valid (refresh completed if needed).");
                    Ok(())
                }
                None => {
                    bail!("No OpenAI Codex auth profile found. Run `prx auth login --provider openai-codex`.")
                }
            }
        }

        AuthCommands::Logout { provider, profile } => {
            let provider = auth::normalize_provider(&provider)?;
            let removed = auth_service.remove_profile(&provider, &profile)?;
            if removed {
                println!("Removed auth profile {provider}:{profile}");
            } else {
                println!("Auth profile not found: {provider}:{profile}");
            }
            Ok(())
        }

        AuthCommands::Use { provider, profile } => {
            let provider = auth::normalize_provider(&provider)?;
            auth_service.set_active_profile(&provider, &profile)?;
            println!("Active profile for {provider}: {profile}");
            Ok(())
        }

        AuthCommands::List => {
            let data = auth_service.load_profiles()?;
            if data.profiles.is_empty() {
                println!("No auth profiles configured.");
                return Ok(());
            }

            for (id, profile) in &data.profiles {
                let active = data
                    .active_profiles
                    .get(&profile.provider)
                    .is_some_and(|active_id| active_id == id);
                let marker = if active { "*" } else { " " };
                println!("{marker} {id}");
            }

            Ok(())
        }

        AuthCommands::Status => {
            let data = auth_service.load_profiles()?;
            if data.profiles.is_empty() {
                println!("No auth profiles configured.");
                return Ok(());
            }

            for (id, profile) in &data.profiles {
                let active = data
                    .active_profiles
                    .get(&profile.provider)
                    .is_some_and(|active_id| active_id == id);
                let marker = if active { "*" } else { " " };
                println!(
                    "{} {} kind={:?} account={} expires={}",
                    marker,
                    id,
                    profile.kind,
                    crate::security::redact(profile.account_id.as_deref().unwrap_or("unknown")),
                    format_expiry(profile)
                );
            }

            println!();
            println!("Active profiles:");
            for (provider, profile_id) in &data.active_profiles {
                println!("  {provider}: {profile_id}");
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_definition_has_no_flag_conflicts() {
        Cli::command().debug_assert();
    }

    #[test]
    fn onboard_help_includes_model_flag() {
        let cmd = Cli::command();
        let onboard = cmd
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == "onboard")
            .expect("onboard subcommand must exist");

        let has_model_flag = onboard
            .get_arguments()
            .any(|arg| arg.get_id().as_str() == "model" && arg.get_long() == Some("model"));

        assert!(
            has_model_flag,
            "onboard help should include --model for quick setup overrides"
        );
    }

    #[test]
    fn onboard_cli_accepts_model_provider_and_api_key_in_quick_mode() {
        let cli = Cli::try_parse_from([
            "prx",
            "onboard",
            "--provider",
            "openrouter",
            "--model",
            "custom-model-946",
            "--api-key",
            "sk-issue946",
        ])
        .expect("quick onboard invocation should parse");

        match cli.command {
            Commands::Onboard {
                interactive,
                channels_only,
                api_key,
                provider,
                model,
                ..
            } => {
                assert!(!interactive);
                assert!(!channels_only);
                assert_eq!(provider.as_deref(), Some("openrouter"));
                assert_eq!(model.as_deref(), Some("custom-model-946"));
                assert_eq!(api_key.as_deref(), Some("sk-issue946"));
            }
            other => panic!("expected onboard command, got {other:?}"),
        }
    }

    #[test]
    fn completions_cli_parses_supported_shells() {
        for shell in ["bash", "fish", "zsh", "powershell", "elvish"] {
            let cli = Cli::try_parse_from(["prx", "completions", shell]).expect("completions invocation should parse");
            match cli.command {
                Commands::Completions { .. } => {}
                other => panic!("expected completions command, got {other:?}"),
            }
        }
    }

    #[test]
    fn config_show_cli_parses_default_and_json_format() {
        let cli = Cli::try_parse_from(["prx", "config", "show"]).expect("config show should parse");
        match cli.command {
            Commands::Config {
                config_command: ConfigCommands::Show { format },
            } => assert_eq!(format, ConfigShowFormat::Toml),
            other => panic!("expected config show command, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["prx", "config", "show", "--format", "json"])
            .expect("config show --format json should parse");
        match cli.command {
            Commands::Config {
                config_command: ConfigCommands::Show { format },
            } => assert_eq!(format, ConfigShowFormat::Json),
            other => panic!("expected config show json command, got {other:?}"),
        }
    }

    #[test]
    fn memory_reindex_cli_parses() {
        let cli = Cli::try_parse_from(["prx", "memory", "reindex"]).expect("memory reindex should parse");
        match cli.command {
            Commands::Memory {
                memory_command: MemoryCommands::Reindex { json },
            } => assert!(!json),
            other => panic!("expected memory reindex command, got {other:?}"),
        }

        let cli =
            Cli::try_parse_from(["prx", "memory", "reindex", "--json"]).expect("memory reindex --json should parse");
        match cli.command {
            Commands::Memory {
                memory_command: MemoryCommands::Reindex { json },
            } => assert!(json),
            other => panic!("expected memory reindex command, got {other:?}"),
        }
    }

    #[test]
    fn approval_revoke_cli_parses() {
        let cli = Cli::try_parse_from([
            "prx",
            "approval",
            "revoke",
            "grant-test",
            "--reason",
            "operator requested",
        ])
        .expect("approval revoke should parse");
        match cli.command {
            Commands::Approval {
                approval_command: ApprovalCommands::Revoke { grant_id, reason },
            } => {
                assert_eq!(grant_id, "grant-test");
                assert_eq!(reason, "operator requested");
            }
            other => panic!("expected approval revoke command, got {other:?}"),
        }
    }

    #[test]
    fn eu_ai_act_audit_cli_parses_json_and_output() {
        let cli = Cli::try_parse_from([
            "prx",
            "audit",
            "attest-eu-ai-act",
            "--json",
            "--output",
            "/tmp/eu-ai-act-test.json",
        ])
        .expect("eu ai act attestation should parse");

        match cli.command {
            Commands::Audit {
                audit_command:
                    AuditCommands::AttestEuAiAct {
                        json,
                        format,
                        output,
                        verbose,
                    },
            } => {
                assert!(json);
                assert_eq!(format, AuditOutputFormat::Markdown);
                assert_eq!(
                    output.as_deref(),
                    Some(std::path::Path::new("/tmp/eu-ai-act-test.json"))
                );
                assert!(!verbose);
            }
            other => panic!("expected audit attest-eu-ai-act command, got {other:?}"),
        }
    }

    #[test]
    fn eu_ai_act_attestation_has_required_check_counts() {
        let attestation = build_eu_ai_act_attestation(&Config::default());

        assert_eq!(attestation.total_checks, 24);
        assert_eq!(attestation.checks.len(), 24);
        assert!(attestation.passed_count >= 8);
        assert_eq!(
            attestation.total_checks,
            attestation.passed_count + attestation.warning_count + attestation.failed_count
        );

        let json = render_eu_ai_act_attestation_json(&attestation).expect("attestation should serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("attestation json should parse");
        assert_eq!(value.get("total_checks").and_then(serde_json::Value::as_u64), Some(24));
        assert!(
            value
                .get("passed_count")
                .and_then(serde_json::Value::as_u64)
                .expect("passed_count should be numeric")
                >= 8
        );
    }

    #[tokio::test]
    async fn memory_reindex_respects_memory_module_gate() {
        let mut config = Config::default();
        config.modules.memory = false;

        let error = handle_memory_command(MemoryCommands::Reindex { json: false }, &config)
            .await
            .expect_err("disabled memory module should block reindex");

        assert!(error.to_string().contains("memory module is disabled"));
    }

    #[test]
    fn doctor_memory_cli_parses() {
        let cli = Cli::try_parse_from(["prx", "doctor", "memory"]).expect("doctor memory should parse");
        match cli.command {
            Commands::Doctor {
                doctor_command: Some(DoctorCommands::Memory),
            } => {}
            other => panic!("expected doctor memory command, got {other:?}"),
        }
    }

    #[test]
    fn doctor_runtime_cli_parses() {
        let cli = Cli::try_parse_from(["prx", "doctor", "runtime"]).expect("doctor runtime should parse");
        match cli.command {
            Commands::Doctor {
                doctor_command: Some(DoctorCommands::Runtime),
            } => {}
            other => panic!("expected doctor runtime command, got {other:?}"),
        }
    }

    #[test]
    fn config_show_redacts_sensitive_toml_values() {
        let mut value = toml::Value::Table(toml::toml! {
            default_provider = "kimi-code"
            api_key = "enc2:encrypted-root"

            [storage.provider.config]
            db_url = "postgres://user:secret@example/prx"

            [agents.worker]
            api_key = "sk-agent"
            model = "kimi-k2"

            [channels.slack]
            bot_token = "xoxb-token"
            allowed_users = ["alice"]

            [nested]
            api_keys = ["sk-one", "sk-two"]
            credential = { username = "user", password = "pass" }
        });

        redact_config_show_value(&mut value);

        let root = value.as_table().expect("redacted value should remain a table");
        assert_eq!(root.get("api_key").and_then(toml::Value::as_str), Some("***"));
        assert_eq!(
            root.get("default_provider").and_then(toml::Value::as_str),
            Some("kimi-code")
        );

        let nested = root
            .get("nested")
            .and_then(toml::Value::as_table)
            .expect("nested table should remain");
        assert_eq!(
            nested
                .get("api_keys")
                .and_then(toml::Value::as_array)
                .expect("api_keys should remain an array")
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["***", "***"]
        );

        let rendered = toml::to_string_pretty(&value).expect("redacted config should serialize");
        assert!(rendered.contains("default_provider = \"kimi-code\""));
        assert!(rendered.contains("model = \"kimi-k2\""));
        assert!(rendered.contains("allowed_users = [\"alice\"]"));
        assert!(rendered.contains("api_key = \"***\""));
        assert!(rendered.contains("db_url = \"***\""));
        assert!(rendered.contains("bot_token = \"***\""));
        assert!(!rendered.contains("enc2:encrypted-root"));
        assert!(!rendered.contains("postgres://user:secret@example/prx"));
        assert!(!rendered.contains("sk-agent"));
        assert!(!rendered.contains("xoxb-token"));
        assert!(!rendered.contains("sk-one"));
        assert!(!rendered.contains("password = \"pass\""));
    }

    #[test]
    fn models_and_integrations_accept_bare_and_list_commands() {
        let cli = Cli::try_parse_from(["prx", "models"]).expect("bare models should parse");
        match cli.command {
            Commands::Models { model_command } => assert!(model_command.is_none()),
            other => panic!("expected bare models command, got {other:?}"),
        }

        let cli =
            Cli::try_parse_from(["prx", "models", "list", "--provider", "openai"]).expect("models list should parse");
        match cli.command {
            Commands::Models {
                model_command: Some(ModelCommands::List { provider }),
            } => assert_eq!(provider.as_deref(), Some("openai")),
            other => panic!("expected models list command, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["prx", "integrations"]).expect("bare integrations should parse");
        match cli.command {
            Commands::Integrations { integration_command } => assert!(integration_command.is_none()),
            other => panic!("expected bare integrations command, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["prx", "integrations", "list"]).expect("integrations list should parse");
        match cli.command {
            Commands::Integrations {
                integration_command: Some(IntegrationCommands::List),
            } => {}
            other => panic!("expected integrations list command, got {other:?}"),
        }
    }

    #[test]
    fn completion_generation_mentions_binary_name() {
        let mut output = Vec::new();
        write_shell_completion(CompletionShell::Bash, &mut output).expect("completion generation should succeed");
        let script = String::from_utf8(output).expect("completion output should be valid utf-8");
        assert!(script.contains("prx"), "completion script should reference binary name");
    }
}
