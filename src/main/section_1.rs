use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use dialoguer::{Input, Password};
use openprx::{
    CHAT_TRACING_RELOAD, ChannelCommands, ChatFmtLayer, Config, CronCommands, EvolutionCommands, FitnessCommands,
    IntegrationCommands, MigrateCommands, ServiceCommands, SkillCommands, XinCommands, acl, agent, auth, channels,
    chat, cli_tracing, config, cron, daemon, doctor, evolution_cli, fitness_cli, gateway, handle_xin_command,
    integrations, memory, migration, providers, runtime, security, service, session_worker, skills,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf};
use tracing::warn;
use tracing_subscriber::{EnvFilter, fmt};

fn parse_workdir(value: &str) -> Result<std::path::PathBuf, String> {
    openprx::config::RuntimeConfig::validate_workdir(std::path::Path::new(value)).map_err(|error| error.to_string())
}

fn parse_temperature(s: &str) -> std::result::Result<f64, String> {
    let t: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if !(0.0..=2.0).contains(&t) {
        return Err("temperature must be between 0.0 and 2.0".to_string());
    }
    Ok(t)
}

const fn cli_tracing_command(command: &Commands) -> cli_tracing::CliCommand {
    match command {
        Commands::Init { .. } | Commands::Token { .. } => cli_tracing::CliCommand::Init,
        Commands::Go { .. } => cli_tracing::CliCommand::Go,
        Commands::Chat { message: Some(_), .. } => cli_tracing::CliCommand::ChatMessage,
        Commands::Chat { .. } => cli_tracing::CliCommand::Chat,
        Commands::Gateway { .. } => cli_tracing::CliCommand::Gateway,
        Commands::Daemon { .. } => cli_tracing::CliCommand::Daemon,
        Commands::Service { .. } => cli_tracing::CliCommand::Service,
        Commands::Doctor { .. } => cli_tracing::CliCommand::Doctor,
        Commands::A2a { .. } => cli_tracing::CliCommand::Status,
        Commands::Status { .. } => cli_tracing::CliCommand::Status,
        Commands::Tasks { .. } => cli_tracing::CliCommand::Tasks,
        Commands::Memory { .. } => cli_tracing::CliCommand::Memory,
        Commands::Evolution { .. } => cli_tracing::CliCommand::Evolution,
        #[cfg(feature = "wasm-plugins")]
        Commands::Plugin { .. } => cli_tracing::CliCommand::Plugin,
        Commands::Cost { .. } => cli_tracing::CliCommand::Cost,
        Commands::Fitness { .. } => cli_tracing::CliCommand::Fitness,
        Commands::Cron { .. } => cli_tracing::CliCommand::Cron,
        Commands::Xin { .. } => cli_tracing::CliCommand::Xin,
        Commands::Causal { .. } => cli_tracing::CliCommand::Causal,
        Commands::DecisionEngine { .. } => cli_tracing::CliCommand::DecisionEngine,
        Commands::Models { .. } => cli_tracing::CliCommand::Models,
        Commands::Providers { .. } => cli_tracing::CliCommand::Providers,
        Commands::Routes { .. } => cli_tracing::CliCommand::Routes,
        Commands::Channel {
            channel_command: ChannelCommands::Start,
        } => cli_tracing::CliCommand::ChannelStart,
        Commands::Channel { .. } => cli_tracing::CliCommand::ChannelManage,
        Commands::Integrations { .. } => cli_tracing::CliCommand::Integrations,
        Commands::Skills { .. } => cli_tracing::CliCommand::Skills,
        Commands::Migrate { .. } => cli_tracing::CliCommand::Migrate,
        Commands::Auth { .. } => cli_tracing::CliCommand::Auth,
        Commands::Approval { .. } => cli_tracing::CliCommand::Approval,
        Commands::Audit { .. } => cli_tracing::CliCommand::Audit,
        Commands::Config { .. } => cli_tracing::CliCommand::Config,
        #[cfg(feature = "test-mock")]
        Commands::MailboxHarness { .. } => cli_tracing::CliCommand::SessionWorker,
        Commands::SessionWorker { .. } => cli_tracing::CliCommand::SessionWorker,
        Commands::Completions { .. } => cli_tracing::CliCommand::Completions,
    }
}

// CLI dispatch remains binary-owned; every runtime subsystem it calls is owned
// and compiled once by the openprx library.



// `prx providers|models|routes|config` management handlers: binary-owned,
// every operation calls the library's `config::manage` service in-process.


/// `OpenPRX` - 100% Rust. 100% Agnostic. Your AI, your rules.
#[derive(Parser, Debug)]
#[command(name = "prx")]
#[command(author = "g1e2x87")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "The fastest, smallest AI assistant.", long_about = None)]
struct Cli {
    #[arg(long, global = true)]
    config_dir: Option<String>,

    #[command(subcommand)]
    command: Commands,
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
        /// Turn the report into a release gate at the selected severity
        #[arg(long, value_enum)]
        fail_on: Option<AuditFailOn>,
    },
    /// Generate a versioned Article 47 / Annex V declaration artifact from explicit operator inputs
    #[command(name = "generate-eu-declaration")]
    GenerateEuDeclaration {
        /// Output path; defaults to compliance.eu_ai_act.declaration.artifact_path
        #[arg(long)]
        output: Option<PathBuf>,
        /// Reference to an operator-controlled external signature or signing receipt
        #[arg(long)]
        signature_reference: String,
    },
    /// Create a durable Article 73 serious-incident record from a JSON input file
    #[command(name = "incident-create")]
    IncidentCreate {
        /// JSON file containing the incident classification and ownership fields
        #[arg(long)]
        input: PathBuf,
    },
    /// Record or replace the initial report for an open serious incident
    #[command(name = "incident-initial-report")]
    IncidentInitialReport {
        incident_id: String,
        /// UTF-8 file containing the initial report
        #[arg(long)]
        input: PathBuf,
    },
    /// Append supplemental facts to a serious incident
    #[command(name = "incident-supplement")]
    IncidentSupplement {
        incident_id: String,
        /// UTF-8 file containing the supplement
        #[arg(long)]
        input: PathBuf,
    },
    /// Close a serious incident with an explicit timestamp and summary
    #[command(name = "incident-close")]
    IncidentClose {
        incident_id: String,
        /// RFC 3339 closure timestamp
        #[arg(long)]
        closed_at: String,
        /// UTF-8 file containing the closure summary
        #[arg(long)]
        summary: PathBuf,
    },
    /// Export an incident, supplements, and immutable audit-event hashes
    #[command(name = "incident-export")]
    IncidentExport {
        incident_id: String,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum AuditOutputFormat {
    Markdown,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum AuditFailOn {
    Fail,
    Warning,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ControlStatus {
    Pass,
    Warning,
    Fail,
    NotApplicable,
    Unknown,
}

impl ControlStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warning => "warning",
            Self::Fail => "fail",
            Self::NotApplicable => "not_applicable",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Serialize)]
struct EvidenceReference {
    kind: &'static str,
    reference: String,
}

#[derive(Debug, Serialize)]
struct ApplicabilityDecision {
    status: ControlStatus,
    classification: String,
    owner: Option<String>,
    reviewed_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct EuAiActAttestation {
    report_type: &'static str,
    evaluator_version: &'static str,
    product_version: &'static str,
    source_identity: Option<String>,
    config_generation: Option<u64>,
    config_source_revision: Option<String>,
    generated_at: String,
    applicability: ApplicabilityDecision,
    default_provider: Option<String>,
    total_checks: usize,
    passed_count: usize,
    warning_count: usize,
    failed_count: usize,
    unknown_count: usize,
    not_applicable_count: usize,
    checks: Vec<EuAiActCheck>,
}

#[derive(Debug, Serialize)]
struct EuAiActCheck {
    id: &'static str,
    framework_reference: &'static str,
    control: &'static str,
    applicability: &'static str,
    severity: &'static str,
    status: ControlStatus,
    observed_at: String,
    evidence: Vec<EvidenceReference>,
    explanation: String,
    remediation: String,
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
enum TokenCommands {
    Issue {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage pinned mTLS messaging identities and peers.
    A2a {
        #[command(subcommand)]
        command: openprx::messaging::a2a::cli::A2aCommand,
    },
    /// Issue an install-owner bearer into a private output file
    Token {
        #[command(subcommand)]
        command: TokenCommands,
    },
    /// Initialize or repair configuration, channels and agent identity
    Init {
        /// Configuration preset (default: minimal)
        #[arg(long, value_enum)]
        spec: Option<crate::config::init::Spec>,
        /// Target configuration directory
        #[arg(long)]
        dir: Option<String>,
        /// Repair existing configuration while preserving explicit operator settings
        #[arg(long)]
        force: bool,

        /// Run the full interactive wizard (default is quick setup)
        #[arg(long)]
        interactive: bool,

        /// Reconfigure channels only (fast repair flow)
        #[arg(long)]
        channels_only: bool,

        /// API key (quick mode only). Deprecated: an
        /// argument is visible in the process list; use --from-stdin or --from-env
        #[arg(long)]
        api_key: Option<String>,

        /// Read the API key from standard input (quick mode)
        #[arg(long, conflicts_with_all = ["api_key", "from_env"])]
        from_stdin: bool,

        /// Read the API key from this environment variable (quick mode)
        #[arg(long, value_name = "VAR", conflicts_with = "api_key")]
        from_env: Option<String>,

        /// Provider name (quick mode; otherwise preserve the preset or existing selection)
        #[arg(long)]
        provider: Option<String>,
        /// Model ID override (used in quick mode)
        #[arg(short = 'm', long)]
        model: Option<String>,
        /// Declare native tool support for a model outside the release catalog (quick mode)
        #[arg(long, conflicts_with = "no_native_tools")]
        native_tools: bool,
        /// Declare that a model outside the release catalog has no native tools (quick mode)
        #[arg(long, conflicts_with = "native_tools")]
        no_native_tools: bool,
        /// Memory backend (sqlite, lucid, markdown, none) - used in quick mode, default: sqlite
        #[arg(long)]
        memory: Option<String>,
        /// The decision backend (quick mode): builtin or compatible_http; a
        /// hosted service also needs --decision-credential-stdin
        #[arg(long, value_enum)]
        decision_backend: Option<openprx::decision_engine_cli::BackendKindArg>,
        /// The decision endpoint (quick mode, compatible_http)
        #[arg(long, requires = "decision_backend")]
        decision_endpoint: Option<String>,
        /// The decision model, one exact version (quick mode)
        #[arg(long, requires = "decision_backend")]
        decision_model: Option<String>,
        /// Enable the advisory decision engine (quick mode): sets decision_engine.enabled = true
        #[arg(long, requires = "decision_backend")]
        decision_enable: bool,
        /// Read the hosted decision backend's credential (typesafe or
        /// openrouter) from standard input, store it as `cred-decision-<kind>`
        /// and reference it (quick mode). Remote processing stays off until
        /// `backend.remote_enabled` is set with `prx decision-engine config set`
        #[arg(long, requires = "decision_backend", conflicts_with = "from_stdin")]
        decision_credential_stdin: bool,
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
        /// API key (required on first use, or use existing auth-profiles). Deprecated:
        /// an argument is visible in the process list; use --from-env
        #[arg(short = 'k', long = "key")]
        api_key: Option<String>,

        /// Read the API key from this environment variable
        #[arg(long, value_name = "VAR", conflicts_with = "api_key")]
        from_env: Option<String>,

        /// Read the API key from standard input (then the session reads no further input from
        /// it; use with --message, or --from-env for an interactive session)
        #[arg(long, conflicts_with_all = ["api_key", "from_env"])]
        from_stdin: bool,

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

    /// Start an interactive chat session with streaming output
    #[command(long_about = "\
Start an interactive chat session with the fullscreen terminal UI.

Features streaming responses, tool execution display, and session history.
Uses the full Agent pipeline: memory recall, LLM routing, built-in tools,
and all configured providers.

The fullscreen UI keeps transcript history inside PRX rather than in native
terminal scrollback. Use in-app scrolling while chatting and `/export [md|json]`
to save a transcript. Use `--plain` or `PRX_TUI=0` for the non-TUI reedline
fallback.

Examples:
  prx chat                              # fullscreen interactive session
  prx chat -p ollama -m llama3.3        # use local model
  prx chat -p anthropic                 # use Anthropic
  PRX_TUI=0 prx chat                    # plain reedline fallback
  prx chat --plain                      # plain text fallback
  prx chat --continue                   # resume last session
  prx chat --session last               # resume last session
  prx chat --session abc123             # resume specific session
  prx chat --list-sessions              # list saved sessions
  prx chat --message 'What is 2+2?'     # one turn: the reply on stdout, then exit
  prx chat -q 'Summarize x' --save      # one turn, saved as a resumable session
  prx chat --session abc123 -q 'More?'  # one turn in a saved session

With --message (-q) chat runs exactly one turn and prints only the reply on \
stdout. The message is literal model input; slash text is not a command. \
Exit status: 0 answered, 2 refused by policy (the refusal reply is still \
printed), 1 any other failure (`Error: ...` on stderr). The turn is not saved \
as a session unless --save, --session or --continue is given; --session or \
--continue naming no saved session is an error. In Supervised mode tool \
approvals are asked on the terminal ([y/N/a]); without a terminal only \
OPENPRX_APPROVAL_OVERRIDE can allow.")]
    Chat {
        /// Controlled H2 root ceilings JSON (aggregate resources, per-root time/concurrency).
        #[arg(long = "h2-controlled-limits", requires = "h2_authority_seconds",
            conflicts_with_all = ["plain", "list_sessions", "provider", "model", "temperature"])]
        h2_limits: Option<String>,

        /// Lifetime of this foreground OS-launched H2 authority, in seconds.
        #[arg(long, requires = "h2_limits")]
        h2_authority_seconds: Option<u64>,

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

        /// Resume the most recent saved session. Explicit --session wins if both are provided.
        #[arg(long = "continue")]
        continue_last: bool,

        /// List all saved sessions and exit
        #[arg(long)]
        list_sessions: bool,

        /// Run one turn with this message, print only the reply and exit
        #[arg(short = 'q', long, conflicts_with_all = ["list_sessions", "h2_limits", "h2_authority_seconds"])]
        message: Option<String>,

        /// Save the --message turn as a resumable chat session
        #[arg(long, requires = "message")]
        save: bool,

        /// Absolute existing working directory for shell, file tools and git.
        #[arg(long, value_parser = parse_workdir)]
        workdir: Option<std::path::PathBuf>,
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
        /// Delegate only this exact authenticated principal to the existing owner.
        #[arg(long, requires_all = ["h2_limits", "h2_authority_seconds"])]
        h2_principal: Option<String>,

        /// Controlled H2 root ceilings JSON; excludes ordinary conversational spending.
        #[arg(long = "h2-controlled-limits", requires_all = ["h2_principal", "h2_authority_seconds"])]
        h2_limits: Option<String>,

        /// Lifetime of the foreground local-review authority, in seconds.
        #[arg(long, requires_all = ["h2_principal", "h2_limits"])]
        h2_authority_seconds: Option<u64>,

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
    Status {
        #[arg(long)]
        json: bool,
    },

    /// Inspect and terminate work running inside a live PRX process
    #[command(long_about = "\
Inspect and terminate work running inside a live PRX process.

PRX does not cut work off on a clock: a long agent run is normal business, so
nothing expires by itself. That makes this command the operator's backstop —
it lists every tool call, sub-agent run and child process the runtime is
currently holding, together with parent/child lineage and elapsed time, and
ends any of them on request.

The registry lives inside the process that owns the work, so these subcommands
talk to a running gateway over HTTP rather than inspecting this invocation.

Examples:
  prx tasks list                      # what is running right now
  prx tasks list --json               # machine-readable, includes unreaped children
  prx tasks kill w42                  # kill w42 and everything it started
  prx tasks kill w42 --no-cascade     # kill only w42
  prx tasks kill <run-id>             # same, addressed by run id
  prx tasks kill <batch-id>           # end every member of a spawn_batch fan-out
  prx tasks send <run-id> \"also check Y\"  # steer a running task mid-flight
  prx tasks pools                     # connection-pool saturation")]
    Tasks {
        #[command(subcommand)]
        tasks_command: TasksCommands,
    },

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

    /// Inspect, authorize, and reload trusted WASM plugin artifacts.
    #[cfg(feature = "wasm-plugins")]
    Plugin {
        #[command(subcommand)]
        command: openprx::plugins::cli::PluginCommand,
    },

    /// Read workspace costs and outstanding reservations without creating state.
    Cost {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Inspect, run, and migrate daily self-system fitness snapshots.
    Fitness {
        /// Output machine-readable JSON.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        fitness_command: FitnessCommands,
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

    /// Manage Xin autonomous tasks and durable goal workflows.
    #[command(long_about = "\
Manage Xin autonomous tasks, execution history, and durable Goal/Step workflows.

Examples:
  prx xin status
  prx xin list
  prx xin add nightly-check 'Inspect service health' --recurring --interval-secs 3600
  prx xin update <task-id> --priority high
  prx xin run <task-id>
  prx xin cancel <task-id>
  prx xin goals add release-checklist
  prx xin steps add <goal-id> verify 'Verify the release artifacts'")]
    Xin {
        #[command(subcommand)]
        xin_command: XinCommands,
    },

    /// Retired: prints one line pointing at `prx decision-engine`
    #[command(hide = true)]
    Causal {
        /// Ignored arguments of the retired command.
        #[arg(num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Inspect and configure the advisory decision engine
    DecisionEngine {
        /// Output machine-readable JSON.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: openprx::decision_engine_cli::DecisionEngineCommands,
    },

    /// Manage provider model catalogs
    Models {
        #[command(subcommand)]
        model_command: Option<ModelCommands>,
    },

    /// List, configure and test AI providers and their credentials
    #[command(long_about = "\
List, configure and test AI providers and their credentials.

With no subcommand, lists the supported providers and the configured ones. \
Every change prints its plan first and writes nothing without --yes. \
A credential is never taken on the command line: it is read from a hidden \
prompt, standard input (--from-stdin) or an environment variable (--from-env).

Examples:
  prx providers
  prx providers show anthropic
  prx providers add anthropic --fallback --yes
  prx providers credential status openai   # which key PRX uses today
  prx auth paste-token --provider openai --from-stdin
  prx providers test openrouter --live")]
    Providers {
        #[command(subcommand)]
        providers_command: Option<ProvidersCommands>,
    },

    /// Manage model routes (`hint:<name>` selections)
    Routes {
        #[command(subcommand)]
        routes_command: RoutesCommands,
    },

    /// Manage channels (telegram, discord, slack, wacli)
    #[command(long_about = "\
Manage communication channels.

Add, remove, list, and health-check channels that connect OpenPRX \
to messaging platforms. Supported channel types: telegram, discord, \
slack, whatsapp, wacli, imessage, and email.

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

    #[cfg(feature = "test-mock")]
    // Bash completion uses `__` as its path separator. Keep the contracted
    // fixture spelling as a hidden alias, outside completion's command tree.
    #[command(name = "mailbox-harness", alias = "__mailbox-harness", hide = true)]
    MailboxHarness {
        #[arg(value_parser=["accept-adopt"])]
        action: String,
        #[arg(long)]
        workspace: std::path::PathBuf,
        #[arg(long)]
        worker: bool,
        #[arg(long, default_value_t = 1)]
        turns: usize,
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
enum TasksCommands {
    /// List work items the running process is currently holding
    List {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Gateway base URL (default: the configured gateway bind address)
        #[arg(long)]
        url: Option<String>,
        /// Bearer token, when the gateway requires pairing
        #[arg(long)]
        token: Option<String>,
    },
    /// Terminate one work item by id
    Kill {
        /// Run id, work item id as printed by `prx tasks list` (`w42` or `42`),
        /// or the batch id of a `spawn_batch` fan-out (ends the whole batch)
        id: String,
        /// Kill only this item, leaving the tools and processes it started
        /// running. Cascading is the default because nothing else will clean
        /// those up.
        #[arg(long)]
        no_cascade: bool,
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Gateway base URL (default: the configured gateway bind address)
        #[arg(long)]
        url: Option<String>,
        /// Bearer token, when the gateway requires pairing
        #[arg(long)]
        token: Option<String>,
    },
    /// Send a message to a running task, redirecting it mid-flight
    ///
    /// Addresses a run started from any entry point — a channel message, the
    /// chat TUI, another `prx` — because the work registry is process-wide.
    /// Use the run id from `prx tasks list`; `w42` works too but means nothing
    /// outside this machine.
    Send {
        /// Run id (portable) or work item id as printed by `prx tasks list`
        id: String,
        /// The instruction to hand to the running task
        message: String,
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Gateway base URL (default: the configured gateway bind address)
        #[arg(long)]
        url: Option<String>,
        /// Bearer token, when the gateway requires pairing
        #[arg(long)]
        token: Option<String>,
    },
    /// Show connection-pool occupancy and saturation counters
    Pools {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Gateway base URL (default: the configured gateway bind address)
        #[arg(long)]
        url: Option<String>,
        /// Bearer token, when the gateway requires pairing
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommands {
    /// Set the runtime autonomy level through a configuration transaction; restart running chats.
    Autonomy {
        #[arg(value_parser = ["readonly", "supervised", "full"])]
        level: String,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Set the Brave Search credential from stdin without an argument or echo
    BraveKey {
        #[arg(long, required = true)]
        from_stdin: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
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
    /// Show the plan of writing a configuration file; nothing is written
    Plan {
        /// The new content of the file
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
        /// Which configuration file it replaces (`config.toml` or a `config.d` fragment name)
        #[arg(long, default_value = "config.toml")]
        target: String,
        /// Permit this raw replacement to omit or empty stored secret slots
        #[arg(long)]
        allow_credential_clear: bool,
    },
    /// Write a configuration file through the configuration transaction
    Apply {
        /// The new content of the file
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
        /// Which configuration file it replaces (`config.toml` or a `config.d` fragment name)
        #[arg(long, default_value = "config.toml")]
        target: String,
        /// Permit this raw replacement to omit or empty stored secret slots
        #[arg(long)]
        allow_credential_clear: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// List, show or purge the recorded configuration changes
    History {
        #[command(subcommand)]
        history_command: Option<HistoryCommands>,
    },
    /// Restore the configuration from before a recorded change
    Rollback {
        /// The history record to undo (see `prx config history`)
        #[arg(required_unless_present = "last", conflicts_with = "last")]
        id: Option<String>,
        /// Undo the most recent recorded change
        #[arg(long)]
        last: bool,
        /// Apply the rollback (without it, only what it would do is shown)
        #[arg(long)]
        yes: bool,
        /// Allow restoring a credential the current configuration no longer holds
        #[arg(long)]
        restore_credentials: bool,
        /// Allow a rollback that turns `secrets.encrypt` off
        #[arg(long)]
        disable_encryption: bool,
    },
    /// Show the configuration revision, the default model's status and pending changes
    Status,
    /// Manage the model bindings of delegate agents
    Agents {
        #[command(subcommand)]
        agents_command: AgentsCommands,
    },
}

/// How a mutating management command applies its plan.
#[derive(Args, Debug, Clone, Default)]
struct ApplyArgs {
    /// Apply the plan (without it, the plan is shown and nothing is written)
    #[arg(long)]
    yes: bool,
    /// Confirm a high-risk plan; names the revision the plan reported
    #[arg(long, value_name = "REVISION", requires = "yes")]
    confirm_high_risk: Option<String>,
}

/// Which operation a default or a route serves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum OperationArg {
    #[default]
    Chat,
    Embedding,
}

#[derive(Subcommand, Debug)]
enum HistoryCommands {
    /// List the recorded changes, newest first
    List,
    /// Show one recorded change
    Show {
        /// The history record id
        id: String,
    },
    /// Remove every history record (after a leaked credential was rotated)
    Purge {
        /// Remove them (required)
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AgentsCommands {
    /// Create a role with a system-managed identity directory
    New {
        agent: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// List the delegate agents and their model bindings
    List,
    /// Bind an existing agent to a provider and model
    Set {
        /// The agent name (`[agents.<name>]`)
        agent: String,
        /// The provider
        provider: String,
        /// The model
        model: String,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Subcommand, Debug)]
enum ProvidersCommands {
    /// List the supported providers and the configured ones
    List,
    /// Show one provider: identity, roles, uses, declarations and credential status
    Show {
        /// The provider (`openai`, `custom:<url>`, ...)
        provider: String,
    },
    /// Add a provider to the configured set in a role; today the only role is the failover chain
    /// (`--fallback`, `reliability.fallback_providers`)
    Add {
        /// The provider (`anthropic`, `custom:<url>`, ...)
        provider: String,
        /// Add it to the failover chain (`reliability.fallback_providers`); required, so a later
        /// role cannot change what an existing script does
        #[arg(long, required = true)]
        fallback: bool,
        /// Append a model fallback for the current default model (shared by the provider chain)
        #[arg(long)]
        model: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Change the endpoint of the default provider (`api_url`)
    Update {
        /// The default provider
        provider: String,
        /// The new endpoint URL
        #[arg(long, conflicts_with = "clear_api_url", required_unless_present = "clear_api_url")]
        api_url: Option<String>,
        /// Remove the endpoint override
        #[arg(long)]
        clear_api_url: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Remove a provider (refused while any selection still uses it)
    Remove {
        /// The provider
        provider: String,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Check that a provider and model resolve; `--live` also requests the model list once
    Test {
        /// The provider (default: the configured default provider)
        provider: Option<String>,
        /// The model (default: the configured default model)
        #[arg(long)]
        model: Option<String>,
        /// Also send one model-list request to the provider (never a generation)
        #[arg(long)]
        live: bool,
    },
    /// Manage the credential references of a provider
    Credential {
        #[command(subcommand)]
        credential_command: CredentialCommands,
    },
    /// Report legacy provider configuration (inline route, embedding-route and agent keys, the
    /// `[[providers.tool_dialects]]` table, unverified selections, tokens stored under an alias).
    /// With --apply, plan moving the inline keys into credential references and the dialect rows
    /// into `[[providers.models]]`; --yes writes it through the configuration transaction
    /// (`prx config rollback` undoes it)
    Migrate {
        /// Plan the migration instead of only reporting
        #[arg(long)]
        apply: bool,
        #[command(flatten)]
        write: ApplyArgs,
    },
}

/// Management commands that only read: they load an existing configuration
/// read-only, so neither loading nor the legacy-secret upgrade writes
/// anything (D-101 F-5).
const fn read_only_management_command(command: &Commands) -> bool {
    match command {
        Commands::Providers { providers_command } => {
            matches!(
                providers_command,
                None | Some(
                    ProvidersCommands::List
                        | ProvidersCommands::Show { .. }
                        | ProvidersCommands::Test { .. }
                        | ProvidersCommands::Credential {
                            credential_command: CredentialCommands::Status { .. },
                        }
                )
            ) || matches!(
                providers_command,
                Some(ProvidersCommands::Migrate {
                    write: ApplyArgs { yes: false, .. },
                    ..
                })
            )
        }
        Commands::Config { config_command } => matches!(
            config_command,
            ConfigCommands::Status
                | ConfigCommands::Show { .. }
                | ConfigCommands::Schema
                | ConfigCommands::Plan { .. }
                | ConfigCommands::Agents {
                    agents_command: AgentsCommands::List,
                }
                | ConfigCommands::History {
                    history_command: None | Some(HistoryCommands::List | HistoryCommands::Show { .. }),
                }
        ),
        Commands::Routes { routes_command } => matches!(routes_command, RoutesCommands::List { .. }),
        Commands::Models { model_command } => {
            matches!(
                model_command,
                None | Some(ModelCommands::List { .. } | ModelCommands::Show { .. })
            )
        }
        _ => false,
    }
}

#[derive(Subcommand, Debug)]
enum CredentialCommands {
    /// Store a credential reference for a provider; a model route, embedding route or agent of that
    /// provider uses it when its `credential_ref` names it. Read from a hidden prompt, --from-stdin
    /// or --from-env; never an argument
    Set {
        /// The provider the credential is issued for
        provider: String,
        /// The reference name (stored as `cred-<name>`; default: the catalog provider's name)
        #[arg(long)]
        name: Option<String>,
        /// Read the credential from standard input
        #[arg(long, conflicts_with = "from_env")]
        from_stdin: bool,
        /// Read the credential from this environment variable
        #[arg(long, value_name = "VAR")]
        from_env: Option<String>,
    },
    /// Remove a credential reference (refused while the configuration names it)
    Remove {
        /// The reference (`cred-<name>` or `<name>`)
        reference: String,
    },
    /// Show credential status (never a value): every reference, or one provider's credentials
    Status {
        /// The provider (default: every credential reference)
        provider: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum RoutesCommands {
    /// List the routes
    List {
        /// Which route table
        #[arg(long, value_enum, default_value_t = OperationArg::Chat)]
        operation: OperationArg,
    },
    /// Add a route
    Add {
        /// The route hint (`hint:<name>` selects it)
        hint: String,
        /// The provider
        provider: String,
        /// The model
        model: String,
        /// Which route table
        #[arg(long, value_enum, default_value_t = OperationArg::Chat)]
        operation: OperationArg,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Change a route's provider or model
    Update {
        /// The route hint
        hint: String,
        /// The new provider
        #[arg(long)]
        provider: Option<String>,
        /// The new model
        #[arg(long)]
        model: Option<String>,
        /// Which route table
        #[arg(long, value_enum, default_value_t = OperationArg::Chat)]
        operation: OperationArg,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Remove a route (refused while a selection names it)
    Remove {
        /// The route hint
        hint: String,
        /// Which route table
        #[arg(long, value_enum, default_value_t = OperationArg::Chat)]
        operation: OperationArg,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ConfigShowFormat {
    Toml,
    Json,
}

#[derive(Subcommand, Debug)]
enum AuthCommands {
    /// Login with a subscription OAuth provider (`openai-codex` or `xai`)
    Login {
        /// Provider (`openai-codex` or `xai`)
        #[arg(long)]
        provider: String,
        /// Profile name (default: default)
        #[arg(long, default_value = "default")]
        profile: String,
        /// Use OAuth device-code flow
        #[arg(long)]
        device_code: bool,
    },
    /// Complete OAuth by pasting redirect URL or auth code (`openai-codex` or `xai`)
    PasteRedirect {
        /// Provider (`openai-codex` or `xai`)
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
        /// Token value (if omitted, read interactively). Deprecated: an argument is
        /// visible in the process list; use --from-stdin or the prompt
        #[arg(long)]
        token: Option<String>,
        /// Read the token from standard input
        #[arg(long, conflicts_with = "token")]
        from_stdin: bool,
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
    /// Refresh a subscription access token (`openai-codex` or `xai`)
    Refresh {
        /// Provider (`openai-codex` or `xai`)
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
enum ModelCommands {
    /// Inspect the embedded release catalog
    Catalog {
        #[command(subcommand)]
        catalog_command: CatalogCommands,
    },
    /// Plan published model replacements; --yes applies one PMM transaction
    MigrateRetired {
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Show declared model levels and parameters from the release catalog
    Levels { model: String },
    /// Show or update persisted model settings through a configuration transaction
    Settings {
        model: String,
        #[arg(long, value_parser = ["low", "medium", "high"])]
        level: Option<String>,
        /// Explicit override, repeated as --set name=JSON-value
        #[arg(long = "set")]
        overrides: Vec<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// List model catalogs; with --operation or --executable, every known model with what the
    /// evidence says it can do
    List {
        /// Provider name (defaults to configured default provider)
        #[arg(long)]
        provider: Option<String>,
        /// Only models the evidence does not rule out for this operation (`chat`, `embedding`, ...)
        #[arg(long)]
        operation: Option<String>,
        /// Only models the evidence proves can execute the operation (chat when none is named)
        #[arg(long)]
        executable: bool,
    },
    /// Show what the evidence says one model can do
    Show {
        /// The provider
        provider: String,
        /// The model
        model: String,
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
    /// Declare what a model can do (`[[providers.models]]`)
    Add(ModelDeclarationArgs),
    /// Change a model declaration (only the facts given are replaced)
    Update(ModelDeclarationArgs),
    /// Remove a model declaration
    Remove {
        /// The provider
        #[arg(long)]
        provider: String,
        /// The model (omit for the provider-wide declaration)
        #[arg(long)]
        model: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Show or set the default model of an operation
    Default {
        #[command(subcommand)]
        default_command: DefaultCommands,
    },
}

#[derive(Subcommand, Debug)]
enum CatalogCommands {
    /// Show the embedded revision, content digest and lifecycle counts
    Status,
}

/// A `[[providers.models]]` declaration on the command line.
#[derive(Args, Debug)]
struct ModelDeclarationArgs {
    /// The provider
    #[arg(long)]
    provider: String,
    /// The model (omit to declare every model of the provider)
    #[arg(long)]
    model: Option<String>,
    /// The complete list of operations the model executes (comma-separated)
    #[arg(long, value_delimiter = ',', value_parser = parse_setting::<config::schema::ModelOperationSetting>)]
    operations: Option<Vec<config::schema::ModelOperationSetting>>,
    /// Whether the model accepts native tool calls
    #[arg(long)]
    native_tools: Option<bool>,
    /// Whether the model accepts image input
    #[arg(long)]
    vision_input: Option<bool>,
    /// Whether the model streams responses
    #[arg(long)]
    streaming: Option<bool>,
    /// Whether the model honours a JSON-schema response format
    #[arg(long)]
    structured_output: Option<bool>,
    /// The tool-schema dialect the endpoint accepts (`raw`, `no_root_combinators`)
    #[arg(long, value_parser = parse_setting::<config::schema::ToolSchemaDialectSetting>)]
    tool_schema_dialect: Option<config::schema::ToolSchemaDialectSetting>,
    /// The tool-call grammar the endpoint emits (`native`, `xml_function_tags`)
    #[arg(long, value_parser = parse_setting::<config::schema::ToolCallDialectSetting>)]
    tool_call_dialect: Option<config::schema::ToolCallDialectSetting>,
    /// The context window in tokens
    #[arg(long)]
    context_window: Option<u32>,
    #[command(flatten)]
    apply: ApplyArgs,
}

#[derive(Subcommand, Debug)]
enum DefaultCommands {
    /// Show the default chat and embedding models and how they resolve
    Show,
    /// Set the default model of an operation
    Set {
        /// The provider
        provider: String,
        /// The model
        model: String,
        #[arg(long, value_parser = ["low", "medium", "high"])]
        level: Option<String>,
        /// Explicit model parameter override, repeated as --set name=JSON-value
        #[arg(long = "set")]
        overrides: Vec<String>,
        /// Which default
        #[arg(long, value_enum, default_value_t = OperationArg::Chat)]
        operation: OperationArg,
        /// The root connection (`api_key`, `api_url`, `reliability.api_keys`) belongs to the new
        /// default provider (a high-risk change: the key is sent to it); chat only
        #[arg(long, conflicts_with = "clear_root_credential")]
        keep_root_credential: bool,
        /// Clear the root connection (`api_key`, `api_url`, `reliability.api_keys`) with the switch;
        /// chat only
        #[arg(long)]
        clear_root_credential: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

/// Parse a snake_case configuration setting (`no_root_combinators`) the way
/// the configuration file spells it.
fn parse_setting<T: serde::de::DeserializeOwned>(text: &str) -> std::result::Result<T, String> {
    serde_json::from_value(serde_json::Value::String(text.trim().to_ascii_lowercase()))
        .map_err(|_| format!("'{}' is not one of the accepted values", text.escape_default()))
}

#[derive(Subcommand, Debug)]
enum DoctorCommands {
    /// Probe model catalogs across providers and report availability
    Models {
        /// Probe a specific provider only (default: the providers configured in this install)
        #[arg(long)]
        provider: Option<String>,

        /// Probe every catalog provider, including ones not configured here.
        /// A provider that is not configured never receives a key taken
        /// from the process environment.
        #[arg(long, conflicts_with = "provider")]
        all_providers: bool,

        /// Prefer cached catalogs when available (skip forced live refresh)
        #[arg(long)]
        use_cache: bool,
    },
    /// Diagnose memory backend and embedding/vector configuration
    Memory,
    /// Report live runtime validation matrix readiness
    Runtime,
    /// List durable ingress rows that are not reconciled (read-only)
    Ingress,
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

/// Resolve the blocking-pool cap before the runtime exists.
///
/// The pool size is fixed at `Builder` time, so it cannot come from the normal
/// config load (which is async and needs a runtime). We therefore do a
/// synchronous, read-only pre-read of `[runtime] max_blocking_threads`, falling
/// back to a cgroup-aware hardware-derived default.
///
fn resolve_max_blocking_threads(cli: &Cli) -> usize {
    openprx::config::bootstrap_max_blocking_threads(cli.config_dir.as_deref())
        .unwrap_or_else(openprx::runtime::blocking::default_max_blocking_threads)
}

#[allow(unsafe_code)]
fn main() -> std::process::ExitCode {
    // Build the runtime explicitly (instead of using `#[tokio::main]`) so
    // that we can bound how long runtime drop waits for blocking threads.
    // See `RUNTIME_SHUTDOWN_TIMEOUT` above for the rationale.
    //
    // The blocking pool is sized explicitly too. Tokio's default is a
    // hardware-agnostic 512 threads, which is the process's last implicit
    // concurrency gate now that turns, sub-agents and sessions are unbounded —
    // and it fails by deadlock, not rejection: once every slot is held, further
    // `spawn_blocking` calls queue forever with no timeout.
    let cli = Cli::parse();
    let max_blocking_threads = resolve_max_blocking_threads(&cli);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(max_blocking_threads)
        .build()
        .context("failed to build tokio runtime")
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("Error: {}", openprx::security::redaction::redacted_error_report(&err));
            return std::process::ExitCode::FAILURE;
        }
    };
    // Publish the real cap so blocking-pool saturation is measured against it.
    openprx::runtime::blocking::configure(max_blocking_threads);
    let result = runtime.block_on(async {
        let result = Box::pin(async_main(cli)).await;
        // Every exit path (REPL /exit, one-shot end, Ctrl-C/SIGTERM through the
        // shutdown token, daemon shutdown, a process worker's own return) lands
        // here: stop and reap the process-mode children this process owns,
        // within the same bound, before the runtime drops their monitors.
        openprx::tools::sessions_spawn::stop_owned_process_runs(RUNTIME_SHUTDOWN_TIMEOUT).await;
        result
    });
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            // A refused `prx chat --message` turn already printed its reply.
            let code = openprx::chat::one_shot_exit_code(Some(&err));
            if code == openprx::chat::ONE_SHOT_REFUSAL_EXIT_CODE {
                return std::process::ExitCode::from(code);
            }
            eprintln!("Error: {}", openprx::security::redaction::redacted_error_report(&err));
            std::process::ExitCode::FAILURE
        }
    }
}
