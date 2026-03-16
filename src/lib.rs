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
    dead_code
)]

use clap::Subcommand;
use serde::{Deserialize, Serialize};

pub mod agent;
pub(crate) mod approval;
pub(crate) mod auth;
pub mod channels;
pub mod config;
pub(crate) mod cost;
pub(crate) mod cron;
pub(crate) mod daemon;
pub(crate) mod doctor;
pub mod gateway;
pub(crate) mod hardware;
pub(crate) mod health;
pub(crate) mod heartbeat;
pub(crate) mod hooks;
pub(crate) mod identity;
pub(crate) mod integrations;
pub mod media;
pub mod memory;
pub(crate) mod migration;
pub(crate) mod multimodal;
pub mod nodes;
pub mod observability;
pub(crate) mod onboard;
#[cfg(feature = "wasm-plugins")]
pub mod plugins;
pub mod providers;
pub mod rag;
#[cfg(feature = "llm-router")]
pub mod router;
pub mod runtime;
pub(crate) mod security;
pub mod self_system;
pub(crate) mod service;
pub mod session_worker;
pub(crate) mod skillforge;
pub(crate) mod skills;
pub mod tools;
pub(crate) mod tunnel;
pub(crate) mod util;
pub mod webhook;

pub use config::Config;

/// Service management subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum ServiceCommands {
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
pub(crate) enum ChannelCommands {
    /// List all configured channels
    List,
    /// Start all configured channels (handled in main.rs for async)
    Start,
    /// Run health checks for configured channels (handled in main.rs for async)
    Doctor,
    /// Add a new channel configuration
    #[command(long_about = "\
Add a new channel configuration.

Provide the channel type and a JSON object with the required \
configuration keys for that channel type.

Supported types: telegram, discord, slack, whatsapp, matrix, imessage, email.

Examples:
  openprx channel add telegram '{\"bot_token\":\"...\",\"name\":\"my-bot\"}'
  openprx channel add discord '{\"bot_token\":\"...\",\"name\":\"my-discord\"}'")]
    Add {
        /// Channel type (telegram, discord, slack, whatsapp, matrix, imessage, email)
        channel_type: String,
        /// Optional configuration as JSON
        config: String,
    },
    /// Remove a channel configuration
    Remove {
        /// Channel name to remove
        name: String,
    },
    /// Bind a Telegram identity (username or numeric user ID) into allowlist
    #[command(long_about = "\
Bind a Telegram identity into the allowlist.

Adds a Telegram username (without the '@' prefix) or numeric user \
ID to the channel allowlist so the agent will respond to messages \
from that identity.

Examples:
  openprx channel bind-telegram openprx_user
  openprx channel bind-telegram 123456789")]
    BindTelegram {
        /// Telegram identity to allow (username without '@' or numeric user ID)
        identity: String,
    },
}

/// Skills management subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum SkillCommands {
    /// List all installed skills
    List,
    /// Install a new skill from a git URL (HTTPS/SSH) or local path
    Install {
        /// Source git URL (HTTPS/SSH) or local path
        source: String,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name to remove
        name: String,
    },
}

/// Migration subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum MigrateCommands {
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
pub(crate) enum CronCommands {
    /// List all scheduled tasks
    List,
    /// Add a new scheduled task
    #[command(long_about = "\
Add a new recurring scheduled task.

Uses standard 5-field cron syntax: 'min hour day month weekday'. \
Times are evaluated in UTC by default; use --tz with an IANA \
timezone name to override.

Examples:
  openprx cron add '0 9 * * 1-5' 'Good morning' --tz America/New_York
  openprx cron add '*/30 * * * *' 'Check system health'")]
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
    #[command(long_about = "\
Add a one-shot task that fires at a specific UTC timestamp.

The timestamp must be in RFC 3339 format (e.g. 2025-01-15T14:00:00Z).

Examples:
  openprx cron add-at 2025-01-15T14:00:00Z 'Send reminder'
  openprx cron add-at 2025-12-31T23:59:00Z 'Happy New Year!'")]
    AddAt {
        /// One-shot timestamp in RFC3339 format
        at: String,
        /// Command to run
        command: String,
    },
    /// Add a fixed-interval scheduled task
    #[command(long_about = "\
Add a task that repeats at a fixed interval.

Interval is specified in milliseconds. For example, 60000 = 1 minute.

Examples:
  openprx cron add-every 60000 'Ping heartbeat'     # every minute
  openprx cron add-every 3600000 'Hourly report'    # every hour")]
    AddEvery {
        /// Interval in milliseconds
        every_ms: u64,
        /// Command to run
        command: String,
    },
    /// Add a one-shot delayed task (e.g. "30m", "2h", "1d")
    #[command(long_about = "\
Add a one-shot task that fires after a delay from now.

Accepts human-readable durations: s (seconds), m (minutes), \
h (hours), d (days).

Examples:
  openprx cron once 30m 'Run backup in 30 minutes'
  openprx cron once 2h 'Follow up on deployment'
  openprx cron once 1d 'Daily check'")]
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
    #[command(long_about = "\
Update one or more fields of an existing scheduled task.

Only the fields you specify are changed; others remain unchanged.

Examples:
  openprx cron update <task-id> --expression '0 8 * * *'
  openprx cron update <task-id> --tz Europe/London --name 'Morning check'
  openprx cron update <task-id> --command 'Updated message'")]
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

/// Integration subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum IntegrationCommands {
    /// Show details about a specific integration
    Info {
        /// Integration name
        name: String,
    },
}

/// Hardware discovery subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HardwareCommands {
    /// Enumerate USB devices (VID/PID) and show known boards
    #[command(long_about = "\
Enumerate USB devices and show known boards.

Scans connected USB devices by VID/PID and matches them against \
known development boards (STM32 Nucleo, Arduino, ESP32).

Examples:
  openprx hardware discover")]
    Discover,
    /// Introspect a device by path (e.g. /dev/ttyACM0)
    #[command(long_about = "\
Introspect a device by its serial or device path.

Opens the specified device path and queries for board information, \
firmware version, and supported capabilities.

Examples:
  openprx hardware introspect /dev/ttyACM0
  openprx hardware introspect COM3")]
    Introspect {
        /// Serial or device path
        path: String,
    },
    /// Get chip info via USB (probe-rs over ST-Link). No firmware needed on target.
    #[command(long_about = "\
Get chip info via USB using probe-rs over ST-Link.

Queries the target MCU directly through the debug probe without \
requiring any firmware on the target board.

Examples:
  openprx hardware info
  openprx hardware info --chip STM32F401RETx")]
    Info {
        /// Chip name (e.g. STM32F401RETx). Default: STM32F401RETx for Nucleo-F401RE
        #[arg(long, default_value = "STM32F401RETx")]
        chip: String,
    },
}

