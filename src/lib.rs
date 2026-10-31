#![warn(clippy::all, clippy::pedantic)]
#![allow(
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
    clippy::manual_pattern_char_comparison,
    clippy::manual_string_new,
    clippy::manual_let_else,
    clippy::match_same_arms,
    clippy::match_wildcard_for_single_variants,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_continue,
    clippy::needless_lifetimes,
    clippy::needless_return,
    clippy::needless_pass_by_value,
    clippy::needless_raw_string_hashes,
    clippy::overly_complex_bool_expr,
    clippy::question_mark,
    clippy::ref_option,
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
    clippy::unnecessary_cast,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_literal_bound,
    clippy::unnecessary_map_or,
    clippy::bind_instead_of_map,
    clippy::cast_lossless,
    clippy::clone_on_copy,
    clippy::comparison_chain,
    clippy::elidable_lifetime_names,
    clippy::manual_contains,
    clippy::manual_is_multiple_of,
    clippy::needless_borrow,
    clippy::needless_update,
    clippy::redundant_closure,
    clippy::unwrap_or_default,
    clippy::unnecessary_semicolon,
    clippy::unused_self,
    clippy::cast_precision_loss,
    clippy::unnecessary_wraps,
    clippy::assertions_on_constants,
    clippy::excessive_nesting,
    clippy::single_option_map,
    clippy::trait_duplication_in_bounds,
    clippy::large_stack_frames,
    clippy::too_long_first_doc_paragraph
)]

use clap::Subcommand;
use serde::{Deserialize, Serialize};

/// Identifying user agent on provider auth and API requests.
pub const PRX_USER_AGENT: &str = concat!("prx/", env!("CARGO_PKG_VERSION"));

/// HTTP client that sends [`PRX_USER_AGENT`] on every request.
pub fn prx_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(PRX_USER_AGENT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Blocking HTTP client that sends [`PRX_USER_AGENT`] on every request.
pub fn prx_blocking_http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(PRX_USER_AGENT)
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

pub mod acl;
pub mod agent;
pub mod approval;
pub mod auth;
pub mod capability;
pub mod causal_cli;
pub(crate) mod causal_tree;
pub mod channels;
#[allow(private_interfaces)]
pub mod chat;
pub mod compliance;
pub mod config;
pub mod cost;
pub mod cron;
pub mod daemon;
pub mod decision;
pub mod doctor;
pub mod evolution_cli;
pub mod fitness_cli;
pub mod gateway;
pub(crate) mod health;
pub mod heartbeat;
pub(crate) mod hooks;
pub mod identity;
pub mod initialization;
pub mod integrations;
pub mod llm;
pub(crate) mod markdown;
pub mod media;
pub mod memory;
pub mod migration;
pub(crate) mod multimodal;
pub mod nodes;
pub mod observability;
#[cfg(feature = "wasm-plugins")]
pub mod plugins;
pub mod providers;
pub mod recovery;
pub mod router;
pub mod runtime;
pub mod schema_migration;
pub mod security;
pub mod self_system;
pub mod service;
pub mod session_worker;
pub mod skillforge;
pub mod skills;
pub mod tools;
pub mod tunnel;
pub(crate) mod util;
pub mod webhook;
pub(crate) mod xin;

pub use xin::handle_command as handle_xin_command;

/// Subscriber stack wrapped by the reloadable chat tracing layer.
pub type ChatSubscriber =
    tracing_subscriber::layer::Layered<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;
/// Type-erased tracing writer used while chat takes ownership of the terminal.
pub type ChatWriter = tracing_subscriber::fmt::writer::BoxMakeWriter;
/// Reloadable formatting layer shared by the binary bootstrap and chat runtime.
pub type ChatFmtLayer = tracing_subscriber::fmt::Layer<
    ChatSubscriber,
    tracing_subscriber::fmt::format::DefaultFields,
    tracing_subscriber::fmt::format::Format<tracing_subscriber::fmt::format::Full>,
    ChatWriter,
>;
/// Single process-global tracing reload registry shared by lib and bin.
pub static CHAT_TRACING_RELOAD: std::sync::OnceLock<tracing_subscriber::reload::Handle<ChatFmtLayer, ChatSubscriber>> =
    std::sync::OnceLock::new();

pub use config::Config;

// Re-export security types for integration tests.
pub use security::pairing::{PairingGuard, constant_time_eq};
pub use security::policy::{AutonomyLevel, SecurityPolicy};

// Re-export HookManager for integration tests (gateway AppState requires it).
pub use hooks::HookManager;

/// Service management subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServiceCommands {
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

/// Channel management subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChannelCommands {
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

/// Skills management subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillCommands {
    /// List installed skills
    List,
    /// Explicitly synchronize enabled community skill repositories
    Sync,
    /// Install a skill from a git URL (HTTPS/SSH) or local path
    Install {
        /// Git URL (HTTPS/SSH) or local path
        source: String,
    },
    /// Update an installed remote skill, or revalidate a linked local skill
    Update {
        /// Skill name
        name: String,
    },
    /// Enable an installed workspace skill
    Enable {
        /// Skill name
        name: String,
    },
    /// Disable an installed workspace skill without deleting it
    Disable {
        /// Skill name
        name: String,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name
        name: String,
    },
    /// Inspect and operate the skill lifecycle (`[skills_lifecycle]`)
    Lifecycle {
        #[command(subcommand)]
        command: SkillLifecycleCommands,
    },
}

/// Individual bounded experiment operations, under the existing lifecycle policy.
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillExperimentCommands {
    /// Crate-controlled preregistered V01 trials; no qualification or publication.
    Study {
        #[command(subcommand)]
        command: SkillExperimentStudyCommands,
    },
    /// Typed product flow: exact final publication, annotation and honest status.
    Product {
        #[command(subcommand)]
        command: SkillExperimentProductCommands,
    },
    /// Compile one bounded general/domain/compose cycle, without model work.
    PlanCycle { source: String },
    /// Run or continue a complete admitted plan without replaying uncertain work.
    RunPlan { source: String },
    /// Resolve authoritative pins in a spec file without starting model or tool work.
    Plan { source: String },
    /// Freeze a fully pinned specification in the owner store.
    Admit { source: String },
    /// Inspect immutable plan, usage liabilities and stage records.
    Inspect { id: String, digest: String },
    /// Prevent further stage work; in-flight liabilities remain accounted.
    Cancel { id: String, digest: String },
    /// Reconcile interrupted handoffs conservatively, without retrying effects.
    Recover { id: String, digest: String },
    /// Execute exactly one frozen stage using the supplied operation JSON.
    Run {
        id: String,
        digest: String,
        stage: String,
        source: String,
    },
}

/// Local V01 transport over the admitted study owner, never a direct executor.
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillExperimentStudyCommands {
    Plan {
        source: String,
    },
    Admit {
        source: String,
    },
    Run {
        id: String,
        digest: String,
        study_digest: String,
    },
    Inspect {
        id: String,
        digest: String,
        study_digest: String,
    },
    Report {
        id: String,
        digest: String,
        study_digest: String,
    },
    Export {
        id: String,
        digest: String,
        study_digest: String,
    },
}

/// Explicit local operator inputs; files contain intent and pins, never grants.
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillExperimentProductCommands {
    /// Discover bounded candidate skill/pattern IDs; does not grant eligibility.
    Catalog {
        #[arg(long)]
        after_skill: Option<String>,
    },
    /// Plan one complete cycle without model/tool execution.
    Plan { source: String },
    /// Admit the complete typed plan, including all exact operation hashes.
    Admit { source: String },
    /// Run or safely continue an already admitted complete plan.
    Run { source: String },
    /// Inspect owner records without recovery, repair or model work.
    Inspect { id: String, digest: String },
    /// Persist cancel intent; cleanup and termination remain separate observations.
    Cancel { id: String, digest: String },
    /// Reconcile only the existing experiment; never retry uncertain effects.
    Recover { id: String, digest: String },
    /// Review the exact final expectation with the existing source reviewer.
    Review { source: String },
    /// Publish the sealed exact final or return its original Committed transaction.
    Publish { source: String },
    /// Annotate one authentic already-Committed publication; never publish.
    Annotate { source: String },
    /// Query eligible lessons using actual current owner/model/policy pins.
    QueryLessons { source: String },
    /// Roll back this experiment's skill through the original generation-CAS owner.
    Rollback { source: String },
    /// Set monotonic flags on an exact owner/experiment adoption reference.
    SetAdoptionStatus { source: String },
}

/// Skill lifecycle subcommands. Every mutation goes through the lifecycle
/// service under the `[skills_lifecycle]` settings.
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillLifecycleCommands {
    /// Operate a frozen bounded instruction experiment. Scheduling is explicit.
    Experiment {
        #[command(subcommand)]
        command: SkillExperimentCommands,
    },
    /// Validate a frozen evaluation suite and print its canonical digest.
    SuiteDigest { source: String },
    /// Install a suite whose digest is already registered by operator configuration.
    InstallSuite { source: String },
    /// Review candidate source deterministically without publishing it.
    Review { proposal_id: String },
    /// Run paired validation and holdout through the real agent, then record the gate.
    /// Publication remains a separate command.
    Evaluate { proposal_id: String },

    /// Generate a private script draft from an operator-supplied skill template
    ProposeScript {
        /// Template directory with a declared script tool
        source: String,
        /// Exact bundle-relative script path already declared by the template
        #[arg(long)]
        script: String,
        /// Desired behavior, treated as untrusted generation input
        #[arg(long)]
        instructions: String,
    },
    /// Import a complete executable bundle after reviewing its exact hash
    ImportExecutable {
        source: String,
        #[arg(long)]
        reviewed: String,
    },
    /// Show the lifecycle mode, policy, proposal counts and startup recovery
    Status,
    /// List proposals, oldest first
    List {
        /// Only the proposals of this skill id
        #[arg(long)]
        skill: Option<String>,
    },
    /// Show one proposal
    Inspect {
        /// Proposal id
        proposal_id: String,
    },
    /// Publish an eligible non-experiment proposal (H2 uses experiment product publish)
    Publish {
        /// Proposal id
        proposal_id: String,
    },
    /// Cancel a proposal that has not been published
    Cancel {
        /// Proposal id
        proposal_id: String,
        /// Why it is cancelled
        #[arg(long)]
        reason: Option<String>,
    },
    /// Re-activate an earlier revision (needs `mode = "auto"` and a valid policy)
    Rollback {
        /// Skill id
        #[arg(long)]
        skill: String,
        /// Revision id to re-activate
        #[arg(long)]
        revision: String,
        /// Active generation you observed
        #[arg(long)]
        expected_generation: u64,
    },
    /// Close an authorized proposal stuck on a transaction conflict, without publishing
    ForceClose {
        /// Proposal id
        proposal_id: String,
        /// Why it is closed (recorded on the proposal)
        #[arg(long)]
        reason: String,
        /// Final state
        #[arg(long = "as", value_enum, default_value = "failed")]
        outcome: ForceCloseArg,
    },
}

/// Final state of a force-closed proposal.
#[derive(Copy, Clone, Debug, Eq, PartialEq, clap::ValueEnum, Serialize, Deserialize)]
pub enum ForceCloseArg {
    Failed,
    Stale,
}

/// Migration subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MigrateCommands {
    /// Show schema migration status for the memory database
    Status,
    /// Verify applied schema migration checksums
    Verify,
    /// Preview pending schema migrations without writing
    DryRun,
    /// Plan migrations up to a target version (dry-run diff, writes nothing)
    Plan {
        /// Known numeric backend migration version to plan through (inclusive).
        /// Pending migrations above this version are excluded.
        #[arg(long)]
        target_version: String,
    },
    /// Deprecated compatibility command; never writes a synthetic baseline
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

/// Cron subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CronCommands {
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
    /// Mark a scheduled task as the operator's own (created before PRX
    /// recorded who creates jobs): its shell command may then run git,
    /// hardened. Jobs the agent created stay refused.
    MarkOperator {
        /// Task ID
        id: String,
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

/// Xin autonomous-task subcommands.
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum XinCommands {
    /// Show task, goal, and step counts.
    Status,
    /// List legacy and recurring tasks.
    List,
    /// Show one task.
    Get { id: String },
    /// Add a task.
    Add {
        name: String,
        payload: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, default_value = "agent_session", value_parser = ["agent_session", "shell"])]
        execution_mode: String,
        #[arg(long, default_value = "normal", value_parser = ["low", "normal", "high", "critical"])]
        priority: String,
        #[arg(long)]
        recurring: bool,
        #[arg(long, default_value_t = 0)]
        interval_secs: u64,
    },
    /// Mark a task as the operator's own (created before PRX recorded who
    /// creates tasks): its shell command may then run git, hardened.
    MarkOperator { id: String },
    /// Update editable task fields.
    Update {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, value_parser = ["low", "normal", "high", "critical"])]
        priority: Option<String>,
        #[arg(long)]
        payload: Option<String>,
        #[arg(long)]
        interval_secs: Option<u64>,
    },
    /// Delete a task and its retained run records.
    Remove { id: String },
    /// Pause future task scheduling.
    Pause { id: String },
    /// Resume a paused or cancelled task.
    Resume { id: String },
    /// Cancel a task and revoke an active execution lease.
    Cancel { id: String },
    /// Execute a task immediately through the normal runner path.
    Run { id: String },
    /// Show newest task execution attempts.
    Runs {
        id: String,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=50))]
        limit: u32,
    },
    /// Show task or goal lifecycle events.
    Events { id: String },
    /// Manage durable goals.
    Goals {
        #[command(subcommand)]
        goal_command: XinGoalCommands,
    },
    /// Manage ordered goal steps.
    Steps {
        #[command(subcommand)]
        step_command: XinStepCommands,
    },
}

#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum XinGoalCommands {
    List,
    Get {
        id: String,
    },
    Add {
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, default_value = "normal", value_parser = ["low", "normal", "high", "critical"])]
        priority: String,
        #[arg(long)]
        target_completion_at: Option<String>,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    Cancel {
        id: String,
    },
    Remove {
        id: String,
    },
}

#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum XinStepCommands {
    List {
        goal_id: String,
    },
    Get {
        id: String,
    },
    Add {
        goal_id: String,
        name: String,
        payload: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        sequence: Option<u32>,
        #[arg(long, default_value = "agent_session", value_parser = ["agent_session", "shell"])]
        execution_mode: String,
        #[arg(long, default_value_t = 0)]
        lease_ttl_secs: u64,
    },
    Retry {
        id: String,
    },
}

#[cfg(test)]
mod module_ownership_tests {
    /// `prx chat` sends tracing to `chat.log` before the configuration is even
    /// loaded, so the inherited-embedding-default warning that `dispatch` logs
    /// is invisible to exactly the users who pay for it every turn. The binary
    /// has to write it to the real stderr instead, before dispatch hands the
    /// screen to the TUI.
    #[test]
    fn the_binary_prints_the_embedding_upgrade_notice_for_chat() {
        let main = include_str!("main.rs");
        let call = "chat::print_embedding_upgrade_notice_to_stderr(&config);";
        let notice_at = main
            .find(call)
            .expect("main.rs must print the inherited embedding default for chat");
        let dispatch_at = main
            .find("mode::dispatch(cli.command, config).await")
            .expect("main.rs must dispatch");
        assert!(
            notice_at < dispatch_at,
            "the notice must be printed before dispatch redirects nothing and the TUI takes the screen"
        );
    }

    #[test]
    fn binary_imports_the_library_module_graph() {
        let main = include_str!("main.rs");
        for duplicate in [
            "mod acl;",
            "mod agent;",
            "mod approval;",
            "mod auth;",
            "mod causal_cli;",
            "mod causal_tree;",
            "mod channels;",
            "mod chat;",
            "mod config;",
            "mod cost;",
            "mod cron;",
            "mod daemon;",
            "mod doctor;",
            "mod evolution_cli;",
            "mod fitness_cli;",
            "mod gateway;",
            "mod health;",
            "mod heartbeat;",
            "mod hooks;",
            "mod identity;",
            "mod integrations;",
            "mod llm;",
            "mod media;",
            "mod memory;",
            "mod migration;",
            "mod multimodal;",
            "mod nodes;",
            "mod observability;",
            "mod initialization;",
            "mod plugins;",
            "mod providers;",
            "mod recovery;",
            "mod router;",
            "mod runtime;",
            "mod schema_migration;",
            "mod security;",
            "mod self_system;",
            "mod service;",
            "mod session_worker;",
            "mod skillforge;",
            "mod skills;",
            "mod tools;",
            "mod tunnel;",
            "mod util;",
            "mod webhook;",
            "mod xin;",
        ] {
            assert!(
                !main.lines().any(|line| line.trim() == duplicate),
                "binary must import library-owned module instead of declaring `{duplicate}`"
            );
        }

        for duplicate in [
            "enum ServiceCommands",
            "enum ChannelCommands",
            "enum SkillCommands",
            "enum MigrateCommands",
            "enum CronCommands",
            "enum XinCommands",
            "enum XinGoalCommands",
            "enum XinStepCommands",
            "enum EvolutionCommands",
            "enum FitnessCommands",
            "enum EvolutionLayerArg",
            "enum IntegrationCommands",
        ] {
            assert!(
                !main.contains(duplicate),
                "binary must import library-owned command DTO `{duplicate}`"
            );
        }

        assert!(
            !main.contains("static CHAT_TRACING_RELOAD"),
            "process-global chat tracing registry must be library-owned"
        );
    }
}

/// Integration subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IntegrationCommands {
    /// List integrations
    List,
    /// Show details about a specific integration
    Info {
        /// Integration name
        name: String,
    },
}

/// Evolution dashboard and operation subcommands.
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EvolutionCommands {
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

/// Fitness snapshot and migration subcommands.
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FitnessCommands {
    /// Show the active schedule, store path, and latest report.
    Status,
    /// Score the most recently closed calendar day.
    Run,
    /// Show v2 daily reports, newest first.
    History {
        /// Maximum reports to display.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Export and remove legacy memory-backed fitness rows.
    MigrateLegacy {
        /// Apply the migration. Without this flag the command is read-only.
        #[arg(long)]
        apply: bool,
    },
}

/// Evolution layer selected by the CLI.
#[derive(Copy, Clone, Debug, Eq, PartialEq, clap::ValueEnum, Serialize, Deserialize)]
pub enum EvolutionLayerArg {
    #[value(name = "L1")]
    L1,
    #[value(name = "L2")]
    L2,
    #[value(name = "L3")]
    L3,
}
