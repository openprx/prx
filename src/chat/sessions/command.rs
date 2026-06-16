//! Parsing of chat session slash commands (`/bg`, `/sessions`, `/kill`, …).
//!
//! [`parse_session_command`] is a pure function: it recognises the session
//! command family and returns a [`SessionCommand`] action for the chat main loop
//! to execute (the loop owns the mutable runtime state; the command dispatcher
//! only holds immutable borrows, so it cannot run these directly).
//!
//! v1a implements `Bg` / `Sessions` / `Kill`; v1b adds `Steer` / `Attach`. The
//! remaining variants are parsed into shape for later stages (v1.1: `Detach`;
//! v2: `Shell` / `Logs`) so the surface is stable, but the chat main loop only
//! executes the v1a+v1b subset.

/// A parsed chat session command, handed back to the main loop for execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    /// `/bg <task>` — spawn a background agent session.
    Bg { task: String },
    /// `/sessions` — list background sessions.
    Sessions,
    /// `/kill <seq>` — abort the session with the given display sequence `#N`.
    Kill { seq: u64 },
    /// `/steer <seq> <message>` — inject a steering message (v1b).
    Steer { seq: u64, message: String },
    /// `/attach <seq>` — read-only tail of recent output (v1b).
    Attach { seq: u64 },
    /// `/detach` — return focus to main (v1.1).
    Detach,
    /// `/shell <command>` — background shell session (v2).
    Shell { command: String },
    /// `/logs <seq>` — show a session log (v2).
    Logs { seq: u64 },
    /// `/pty <command>` — interactive PTY shell with full terminal handoff (v3).
    Pty { command: String },
}

/// Parse a chat session command from raw input.
///
/// Returns `None` for anything that is not a recognised session command (the
/// caller falls through to other slash handling). This must be invoked **before**
/// the generic unknown-slash fallback so `/bg`/`/sessions`/`/kill` are not
/// swallowed as "unknown command".
#[must_use]
pub fn parse_session_command(input: &str) -> Option<SessionCommand> {
    let trimmed = input.trim();

    // Bare commands first (exact match).
    match trimmed {
        "/sessions" => return Some(SessionCommand::Sessions),
        "/detach" => return Some(SessionCommand::Detach),
        _ => {}
    }

    // `/bg <task>` — everything after the command word is the task.
    if let Some(rest) = trimmed.strip_prefix("/bg") {
        // Require a separator so `/bgsomething` is not matched.
        let task = rest.strip_prefix(char::is_whitespace)?.trim();
        if task.is_empty() {
            return None;
        }
        return Some(SessionCommand::Bg { task: task.to_string() });
    }

    // `/shell <command>` (v2; parsed for surface stability).
    if let Some(rest) = trimmed.strip_prefix("/shell") {
        let command = rest.strip_prefix(char::is_whitespace)?.trim();
        if command.is_empty() {
            return None;
        }
        return Some(SessionCommand::Shell {
            command: command.to_string(),
        });
    }

    // `/pty <command>` — interactive PTY shell (v3). Everything after the command
    // word is the command line run inside the pseudo-terminal.
    if let Some(rest) = trimmed.strip_prefix("/pty") {
        let command = rest.strip_prefix(char::is_whitespace)?.trim();
        if command.is_empty() {
            return None;
        }
        return Some(SessionCommand::Pty {
            command: command.to_string(),
        });
    }

    // `/kill <seq>`
    if let Some(rest) = trimmed.strip_prefix("/kill") {
        let arg = rest.strip_prefix(char::is_whitespace)?.trim();
        let seq = arg.parse::<u64>().ok()?;
        return Some(SessionCommand::Kill { seq });
    }

    // `/attach <seq>` (v1b)
    if let Some(rest) = trimmed.strip_prefix("/attach") {
        let arg = rest.strip_prefix(char::is_whitespace)?.trim();
        let seq = arg.parse::<u64>().ok()?;
        return Some(SessionCommand::Attach { seq });
    }

    // `/logs <seq>` (v2)
    if let Some(rest) = trimmed.strip_prefix("/logs") {
        let arg = rest.strip_prefix(char::is_whitespace)?.trim();
        let seq = arg.parse::<u64>().ok()?;
        return Some(SessionCommand::Logs { seq });
    }

    // `/steer <seq> <message>` (v1b)
    if let Some(rest) = trimmed.strip_prefix("/steer") {
        let rest = rest.strip_prefix(char::is_whitespace)?.trim_start();
        let (seq_str, message) = rest.split_once(char::is_whitespace)?;
        let seq = seq_str.parse::<u64>().ok()?;
        let message = message.trim();
        if message.is_empty() {
            return None;
        }
        return Some(SessionCommand::Steer {
            seq,
            message: message.to_string(),
        });
    }

    None
}

/// v5: the operator-facing message explaining why `/steer #N` does not apply to
/// a non-agent session, or `None` when the kind *is* steerable (agents).
///
/// Steer appends an instruction to a running sub-agent's steer channel; shells
/// run a fixed command and PTYs are interactive, so neither has a steer channel.
/// Returning a clear message (instead of letting the seq resolve to a non-agent
/// id the `sessions_spawn` tool can't address) keeps the failure legible. Pure
/// so the wording is unit-testable.
#[must_use]
pub fn steer_unsupported_message(kind: super::model::ManagedKind, seq: u64) -> Option<String> {
    use super::model::ManagedKind;
    match kind {
        ManagedKind::Agent => None,
        ManagedKind::Shell => Some(format!(
            "Steer is not supported for background shell #{seq}. \
             Shells run a fixed command — use /logs #{seq} to view output or /kill #{seq} to stop it."
        )),
        ManagedKind::Pty => Some(format!(
            "Steer is not supported for interactive PTY session #{seq}. \
             Re-enter it with /pty to type directly, or /kill #{seq} to stop it."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bg() {
        assert_eq!(
            parse_session_command("/bg do a thing"),
            Some(SessionCommand::Bg {
                task: "do a thing".to_string()
            })
        );
    }

    #[test]
    fn bg_requires_task() {
        assert_eq!(parse_session_command("/bg"), None);
        assert_eq!(parse_session_command("/bg   "), None);
    }

    #[test]
    fn bg_requires_separator() {
        // `/bgsomething` must not be treated as `/bg something`.
        assert_eq!(parse_session_command("/bgsomething"), None);
    }

    #[test]
    fn parses_sessions() {
        assert_eq!(parse_session_command("/sessions"), Some(SessionCommand::Sessions));
        assert_eq!(parse_session_command("  /sessions  "), Some(SessionCommand::Sessions));
    }

    #[test]
    fn parses_kill() {
        assert_eq!(parse_session_command("/kill 2"), Some(SessionCommand::Kill { seq: 2 }));
    }

    #[test]
    fn kill_requires_numeric_seq() {
        assert_eq!(parse_session_command("/kill abc"), None);
        assert_eq!(parse_session_command("/kill"), None);
    }

    #[test]
    fn parses_steer() {
        assert_eq!(
            parse_session_command("/steer 3 focus on tests"),
            Some(SessionCommand::Steer {
                seq: 3,
                message: "focus on tests".to_string()
            })
        );
    }

    #[test]
    fn parses_attach_detach_logs_shell() {
        assert_eq!(
            parse_session_command("/attach 1"),
            Some(SessionCommand::Attach { seq: 1 })
        );
        assert_eq!(parse_session_command("/detach"), Some(SessionCommand::Detach));
        assert_eq!(parse_session_command("/logs 4"), Some(SessionCommand::Logs { seq: 4 }));
        assert_eq!(
            parse_session_command("/shell echo hi"),
            Some(SessionCommand::Shell {
                command: "echo hi".to_string()
            })
        );
    }

    #[test]
    fn parses_pty() {
        assert_eq!(
            parse_session_command("/pty sh"),
            Some(SessionCommand::Pty {
                command: "sh".to_string()
            })
        );
        assert_eq!(
            parse_session_command("/pty python3 -i"),
            Some(SessionCommand::Pty {
                command: "python3 -i".to_string()
            })
        );
    }

    #[test]
    fn pty_requires_command_and_separator() {
        assert_eq!(parse_session_command("/pty"), None);
        assert_eq!(parse_session_command("/pty   "), None);
        // `/ptyfoo` must not be mistaken for `/pty foo`.
        assert_eq!(parse_session_command("/ptyfoo"), None);
    }

    #[test]
    fn ignores_non_session_commands() {
        for input in [
            "/help",
            "/clear",
            "/compact",
            "!ls",
            "hello",
            "/model gpt",
            "/provider x",
        ] {
            assert_eq!(parse_session_command(input), None, "input: {input}");
        }
    }

    #[test]
    fn steer_unsupported_message_agent_is_none() {
        use super::super::model::ManagedKind;
        assert!(steer_unsupported_message(ManagedKind::Agent, 1).is_none());
    }

    #[test]
    fn steer_unsupported_message_shell_and_pty_are_clear() {
        use super::super::model::ManagedKind;
        let shell = steer_unsupported_message(ManagedKind::Shell, 2).expect("test: shell msg");
        assert!(shell.contains("shell #2"), "names the shell: {shell}");
        assert!(shell.to_lowercase().contains("not supported"), "states it: {shell}");

        let pty = steer_unsupported_message(ManagedKind::Pty, 4).expect("test: pty msg");
        assert!(pty.contains("PTY session #4"), "names the pty: {pty}");
        assert!(pty.to_lowercase().contains("not supported"), "states it: {pty}");
    }
}
