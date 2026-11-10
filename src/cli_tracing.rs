//! Default tracing filter and sink for each `prx` CLI command.
//!
//! Foreground commands whose stdout is the user-facing product default to
//! `warn` on stderr when `RUST_LOG` is unset. Long-running services keep
//! `info` on the process default writer (stdout). `RUST_LOG` always wins
//! when it is set.

/// CLI command family used to pick the default tracing filter and sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliCommand {
    Init,
    Go,
    Agent,
    Chat,
    Gateway,
    Daemon,
    Service,
    Doctor,
    Status,
    Tasks,
    Memory,
    Evolution,
    Plugin,
    Cost,
    Fitness,
    Cron,
    Xin,
    Causal,
    Models,
    Providers,
    Routes,
    /// `prx channel start` — the long-running channel server.
    ChannelStart,
    /// Other `prx channel` subcommands (list, doctor, add, …).
    ChannelManage,
    Integrations,
    Skills,
    Migrate,
    Auth,
    Approval,
    Audit,
    Config,
    SessionWorker,
    Completions,
}

/// Default tracing subscriber settings when `RUST_LOG` is unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliTracingDefaults {
    /// `EnvFilter` directive used when `RUST_LOG` is unset.
    pub filter: &'static str,
    /// When true, the fmt layer writes to stderr so stdout stays the product.
    pub write_stderr: bool,
}

impl CliCommand {
    /// Daemon, gateway, channel start, and the session-worker process.
    #[must_use]
    pub const fn is_long_running_service(self) -> bool {
        matches!(
            self,
            Self::Daemon | Self::Gateway | Self::ChannelStart | Self::SessionWorker
        )
    }

    /// Filter and sink for this command when `RUST_LOG` is unset.
    #[must_use]
    pub const fn defaults(self) -> CliTracingDefaults {
        if self.is_long_running_service() {
            CliTracingDefaults {
                filter: "info",
                write_stderr: false,
            }
        } else {
            CliTracingDefaults {
                filter: "warn",
                write_stderr: true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CliCommand, CliTracingDefaults};

    fn all_commands() -> [CliCommand; 32] {
        [
            CliCommand::Init,
            CliCommand::Go,
            CliCommand::Agent,
            CliCommand::Chat,
            CliCommand::Gateway,
            CliCommand::Daemon,
            CliCommand::Service,
            CliCommand::Doctor,
            CliCommand::Status,
            CliCommand::Tasks,
            CliCommand::Memory,
            CliCommand::Evolution,
            CliCommand::Plugin,
            CliCommand::Cost,
            CliCommand::Fitness,
            CliCommand::Cron,
            CliCommand::Xin,
            CliCommand::Causal,
            CliCommand::Models,
            CliCommand::Providers,
            CliCommand::Routes,
            CliCommand::ChannelStart,
            CliCommand::ChannelManage,
            CliCommand::Integrations,
            CliCommand::Skills,
            CliCommand::Migrate,
            CliCommand::Auth,
            CliCommand::Approval,
            CliCommand::Audit,
            CliCommand::Config,
            CliCommand::SessionWorker,
            CliCommand::Completions,
        ]
    }

    fn classify(command: CliCommand) -> (&'static str, bool) {
        match command {
            CliCommand::Daemon | CliCommand::Gateway | CliCommand::ChannelStart | CliCommand::SessionWorker => {
                ("info", false)
            }
            CliCommand::Init
            | CliCommand::Go
            | CliCommand::Agent
            | CliCommand::Chat
            | CliCommand::Service
            | CliCommand::Doctor
            | CliCommand::Status
            | CliCommand::Tasks
            | CliCommand::Memory
            | CliCommand::Evolution
            | CliCommand::Plugin
            | CliCommand::Cost
            | CliCommand::Fitness
            | CliCommand::Cron
            | CliCommand::Xin
            | CliCommand::Causal
            | CliCommand::Models
            | CliCommand::Providers
            | CliCommand::Routes
            | CliCommand::ChannelManage
            | CliCommand::Integrations
            | CliCommand::Skills
            | CliCommand::Migrate
            | CliCommand::Auth
            | CliCommand::Approval
            | CliCommand::Audit
            | CliCommand::Config
            | CliCommand::Completions => ("warn", true),
        }
    }

    #[test]
    fn default_filter_selection_per_command() {
        for command in all_commands() {
            let CliTracingDefaults { filter, write_stderr } = command.defaults();
            let (expected_filter, expected_stderr) = classify(command);
            assert_eq!(filter, expected_filter, "{command:?}");
            assert_eq!(write_stderr, expected_stderr, "{command:?}");
        }
        assert_eq!(CliCommand::Agent.defaults().filter, "warn");
        assert!(CliCommand::Agent.defaults().write_stderr);
        assert_eq!(CliCommand::Chat.defaults().filter, "warn");
        assert_eq!(CliCommand::Go.defaults().filter, "warn");
        assert_eq!(CliCommand::Daemon.defaults().filter, "info");
        assert!(!CliCommand::Daemon.defaults().write_stderr);
        assert_eq!(CliCommand::Gateway.defaults().filter, "info");
        assert_eq!(CliCommand::ChannelStart.defaults().filter, "info");
        assert_eq!(CliCommand::ChannelManage.defaults().filter, "warn");
        assert_eq!(CliCommand::SessionWorker.defaults().filter, "info");
    }
}
