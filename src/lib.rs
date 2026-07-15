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

pub mod acl;
pub mod agent;
pub mod approval;
pub mod auth;
pub mod capability;
pub(crate) mod causal_tree;
pub mod channels;
#[allow(dead_code, private_interfaces)]
pub mod chat;
pub mod config;
pub mod cost;
pub mod cron;
pub mod daemon;
pub mod doctor;
pub mod evolution_cli;
pub mod gateway;
pub(crate) mod health;
pub mod heartbeat;
pub(crate) mod hooks;
pub mod identity;
pub mod integrations;
pub mod llm;
pub mod media;
pub mod memory;
pub mod migration;
pub(crate) mod multimodal;
pub mod nodes;
pub mod observability;
pub mod onboard;
#[cfg(feature = "wasm-plugins")]
pub mod plugins;
pub mod providers;
pub mod recovery;
pub mod router;
pub mod runtime;
pub mod schema_migration;
pub mod security;
pub mod self_system;
#[allow(dead_code)]
pub mod service;
#[allow(dead_code)]
pub mod session_worker;
pub mod skillforge;
pub mod skills;
pub mod tools;
pub mod tunnel;
pub(crate) mod util;
pub mod webhook;
pub(crate) mod xin;

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
pub use security::policy::{ActionTracker, AutonomyLevel, SecurityPolicy};

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

#[cfg(test)]
mod module_ownership_tests {
    #[test]
    fn binary_imports_the_library_module_graph() {
        let main = include_str!("main.rs");
        for duplicate in [
            "mod acl;",
            "mod agent;",
            "mod approval;",
            "mod auth;",
            "mod causal_tree;",
            "mod channels;",
            "mod chat;",
            "mod config;",
            "mod cost;",
            "mod cron;",
            "mod daemon;",
            "mod doctor;",
            "mod evolution_cli;",
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
            "mod onboard;",
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
            "enum EvolutionCommands",
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
