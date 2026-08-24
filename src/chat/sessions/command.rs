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
    /// `/sessions` — list child TUI sessions.
    Sessions,
    /// `/kill <seq>` — abort the session with the given display sequence `#N`.
    Kill { seq: u64 },
    /// `/steer <seq> <message>` — inject a steering message (v1b).
    Steer { seq: u64, message: String },
    /// `/attach <seq>` — read-only tail of recent output (v1b).
    Attach { seq: u64 },
    /// `/detach` — return focus to main (v1.1).
    Detach,
    /// `/transcript` — open the read-only transcript child TUI (P6b1).
    Transcript,
    /// `/diff` / `/diff --cached` — open a read-only workspace diff child TUI (P6c2).
    Diff { cached: bool },
    /// `/shell <command>` — background shell session (v2).
    Shell { command: String },
    /// `/logs <seq>` — show a session log (v2).
    Logs { seq: u64 },
    /// `/pty <command>` — interactive PTY shell with full terminal handoff (v3).
    Pty { command: String },
    /// `/approve <seq>` — approve a child session suspended on the approval
    /// gate (NeedsInput), injecting a runtime grant so the gated tool can proceed.
    Approve { seq: u64 },
    /// `/deny <seq>` — deny a child session suspended on the approval gate
    /// (NeedsInput); the gated tool is reported as denied to the sub-agent.
    Deny { seq: u64 },
    /// `/sessions --daemon` — list the work a daemon process is holding (a5).
    ///
    /// The three `Daemon*` variants are separate from their local counterparts
    /// rather than a scope field on them, because the two scopes do not share
    /// an address space: a local session is a display sequence (`#3`) minted by
    /// this process, while a daemon address is a run id (portable) or the
    /// daemon's own work id (`w42`). Keeping them apart also keeps the local
    /// commands byte-for-byte what they were.
    DaemonSessions,
    /// `/kill --daemon <address>` — terminate one daemon work item (a5).
    DaemonKill { address: String },
    /// `/steer --daemon <address> <message>` — hand a message to a running
    /// daemon task, redirecting it mid-flight (a5).
    DaemonSteer { address: String, message: String },
}

/// Strip the `--daemon` scope flag from a command's argument text.
///
/// Returns the remaining argument text when the flag is present (possibly
/// empty), and `None` when it is not. The flag sits directly after the command
/// word, mirroring `/diff --cached` — the only other flag on the chat command
/// surface — so the scope is visible before the argument it applies to.
/// `--daemonize` and friends are rejected rather than treated as the flag.
fn strip_daemon_scope(arg: &str) -> Option<&str> {
    let rest = arg.trim_start().strip_prefix("--daemon")?;
    if rest.is_empty() {
        return Some("");
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

/// Parse a daemon work address: a run id, or the daemon's own `w42` / `42`.
///
/// Deliberately not `parse_seq_arg`: a daemon address is opaque to chat and is
/// forwarded verbatim, so nothing here may rewrite it. A leading `-` is
/// rejected so a mistyped flag becomes "unknown command" instead of an address
/// the daemon will simply fail to resolve.
fn parse_daemon_address(arg: &str) -> Option<String> {
    let token = arg.split_whitespace().next()?;
    if token.starts_with('-') {
        return None;
    }
    Some(token.to_string())
}

/// Parse a session display sequence argument (`N`) robustly.
///
/// v3b-a minor fix: `/kill N` was occasionally reported as "Unknown command"
/// because the argument seen by the parser was not a bare integer. The two
/// realistic causes are (a) the operator typing the displayed form `/kill #3`
/// (sessions are shown as `#N`), and (b) stray surrounding whitespace from how
/// the input line is assembled. Neither is a hard parser bug, but both are easy
/// to tolerate: we trim whitespace, accept an optional leading `#`, and take the
/// first whitespace-delimited token so a trailing comment / accidental extra
/// token does not break the command. Returns `None` if no integer is present
/// (so a genuinely malformed command still falls through cleanly).
fn parse_seq_arg(arg: &str) -> Option<u64> {
    let token = arg.split_whitespace().next()?;
    let digits = token.strip_prefix('#').unwrap_or(token);
    digits.parse::<u64>().ok().filter(|seq| *seq > 0)
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
        "/transcript" => return Some(SessionCommand::Transcript),
        "/diff" => return Some(SessionCommand::Diff { cached: false }),
        _ => {}
    }

    if let Some(rest) = trimmed.strip_prefix("/diff") {
        let rest = rest.strip_prefix(char::is_whitespace)?.trim();
        if rest == "--cached" {
            return Some(SessionCommand::Diff { cached: true });
        }
        return None;
    }

    // `/sessions --daemon` — the bare form was matched above, so anything left
    // here carries an argument, and the only argument is the scope flag.
    if let Some(rest) = trimmed.strip_prefix("/sessions") {
        let arg = rest.strip_prefix(char::is_whitespace)?;
        if !strip_daemon_scope(arg)?.is_empty() {
            return None;
        }
        return Some(SessionCommand::DaemonSessions);
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

    // `/kill [--daemon] <id>`
    if let Some(rest) = trimmed.strip_prefix("/kill") {
        let arg = rest.strip_prefix(char::is_whitespace)?;
        if let Some(scoped) = strip_daemon_scope(arg) {
            let address = parse_daemon_address(scoped)?;
            return Some(SessionCommand::DaemonKill { address });
        }
        let seq = parse_seq_arg(arg)?;
        return Some(SessionCommand::Kill { seq });
    }

    // `/attach <seq>` (v1b)
    if let Some(rest) = trimmed.strip_prefix("/attach") {
        let arg = rest.strip_prefix(char::is_whitespace)?;
        let seq = parse_seq_arg(arg)?;
        return Some(SessionCommand::Attach { seq });
    }

    // `/logs <seq>` (v2)
    if let Some(rest) = trimmed.strip_prefix("/logs") {
        let arg = rest.strip_prefix(char::is_whitespace)?;
        let seq = parse_seq_arg(arg)?;
        return Some(SessionCommand::Logs { seq });
    }

    // `/approve <seq>` (NeedsInput)
    if let Some(rest) = trimmed.strip_prefix("/approve") {
        let arg = rest.strip_prefix(char::is_whitespace)?;
        let seq = parse_seq_arg(arg)?;
        return Some(SessionCommand::Approve { seq });
    }

    // `/deny <seq>` (NeedsInput)
    if let Some(rest) = trimmed.strip_prefix("/deny") {
        let arg = rest.strip_prefix(char::is_whitespace)?;
        let seq = parse_seq_arg(arg)?;
        return Some(SessionCommand::Deny { seq });
    }

    // `/steer [--daemon] <id> <message>` (v1b; `--daemon` in a5)
    if let Some(rest) = trimmed.strip_prefix("/steer") {
        let rest = rest.strip_prefix(char::is_whitespace)?.trim_start();
        if let Some(scoped) = strip_daemon_scope(rest) {
            let (address, message) = scoped.split_once(char::is_whitespace)?;
            let address = parse_daemon_address(address)?;
            let message = message.trim();
            if message.is_empty() {
                return None;
            }
            return Some(SessionCommand::DaemonSteer {
                address,
                message: message.to_string(),
            });
        }
        let (seq_str, message) = rest.split_once(char::is_whitespace)?;
        let seq = parse_seq_arg(seq_str)?;
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
        ManagedKind::Transcript => Some(
            "Steer is not supported for the read-only transcript viewer. Close it with Esc to return to main chat."
                .to_string(),
        ),
        ManagedKind::Approval => Some(
            "Steer is not supported for the foreground tool approval prompt. Decide with y, n, or Esc.".to_string(),
        ),
        ManagedKind::Diff => Some(
            "Steer is not supported for the read-only diff viewer. Close it with Esc to return to main chat."
                .to_string(),
        ),
        ManagedKind::Worker => Some(
            "Steer is not supported for the read-only provider worker viewer. Close it with Esc to return to main chat."
                .to_string(),
        ),
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

    /// Evidence 3, at the parser level: without `--daemon` every command
    /// parses to exactly the variant it parsed to before a5.
    #[test]
    fn the_local_scope_is_untouched_by_the_daemon_flag() {
        assert_eq!(parse_session_command("/sessions"), Some(SessionCommand::Sessions));
        assert_eq!(parse_session_command("/kill 3"), Some(SessionCommand::Kill { seq: 3 }));
        assert_eq!(parse_session_command("/kill #3"), Some(SessionCommand::Kill { seq: 3 }));
        assert_eq!(
            parse_session_command("/steer 3 do the other thing"),
            Some(SessionCommand::Steer {
                seq: 3,
                message: "do the other thing".to_string()
            })
        );
    }

    #[test]
    fn parses_daemon_scoped_sessions() {
        assert_eq!(
            parse_session_command("/sessions --daemon"),
            Some(SessionCommand::DaemonSessions)
        );
        assert_eq!(
            parse_session_command("  /sessions   --daemon  "),
            Some(SessionCommand::DaemonSessions)
        );
    }

    #[test]
    fn sessions_rejects_anything_that_is_not_the_scope_flag() {
        assert_eq!(parse_session_command("/sessions --all"), None);
        assert_eq!(parse_session_command("/sessions --daemonize"), None);
        assert_eq!(parse_session_command("/sessions --daemon extra"), None);
        assert_eq!(parse_session_command("/sessionsx"), None);
    }

    /// Evidence 4, at the parser level: a run id and a `w42` are both carried
    /// through verbatim, because chat does not own that address space.
    #[test]
    fn daemon_addresses_pass_through_in_both_id_spaces() {
        assert_eq!(
            parse_session_command("/kill --daemon w3"),
            Some(SessionCommand::DaemonKill {
                address: "w3".to_string()
            })
        );
        assert_eq!(
            parse_session_command("/kill --daemon d9671848-1111-2222-3333-444444444444"),
            Some(SessionCommand::DaemonKill {
                address: "d9671848-1111-2222-3333-444444444444".to_string()
            })
        );
        assert_eq!(
            parse_session_command("/steer --daemon w3 also check Y"),
            Some(SessionCommand::DaemonSteer {
                address: "w3".to_string(),
                message: "also check Y".to_string()
            })
        );
        assert_eq!(
            parse_session_command("/steer --daemon d9671848-1111-2222-3333-444444444444 also check Y"),
            Some(SessionCommand::DaemonSteer {
                address: "d9671848-1111-2222-3333-444444444444".to_string(),
                message: "also check Y".to_string()
            })
        );
    }

    #[test]
    fn a_daemon_address_is_never_rewritten_the_way_a_local_seq_is() {
        // `#3` is a local display form; the daemon knows nothing about it, so
        // it is forwarded as typed rather than silently turned into `3`.
        assert_eq!(
            parse_session_command("/kill --daemon #3"),
            Some(SessionCommand::DaemonKill {
                address: "#3".to_string()
            })
        );
    }

    #[test]
    fn daemon_commands_reject_missing_or_flag_shaped_arguments() {
        assert_eq!(parse_session_command("/kill --daemon"), None);
        assert_eq!(parse_session_command("/kill --daemon   "), None);
        assert_eq!(parse_session_command("/kill --daemon --force"), None);
        assert_eq!(parse_session_command("/steer --daemon w3"), None);
        assert_eq!(parse_session_command("/steer --daemon w3    "), None);
        assert_eq!(parse_session_command("/steer --daemon --force go"), None);
        assert_eq!(parse_session_command("/kill --daemonize w3"), None);
    }

    #[test]
    fn parses_sessions() {
        assert_eq!(parse_session_command("/sessions"), Some(SessionCommand::Sessions));
        assert_eq!(parse_session_command("  /sessions  "), Some(SessionCommand::Sessions));
    }

    #[test]
    fn parses_transcript() {
        assert_eq!(parse_session_command("/transcript"), Some(SessionCommand::Transcript));
        assert_eq!(
            parse_session_command("  /transcript  "),
            Some(SessionCommand::Transcript)
        );
    }

    #[test]
    fn parses_diff_workspace_and_cached_only() {
        assert_eq!(
            parse_session_command("/diff"),
            Some(SessionCommand::Diff { cached: false })
        );
        assert_eq!(
            parse_session_command("  /diff --cached  "),
            Some(SessionCommand::Diff { cached: true })
        );
        assert_eq!(parse_session_command("/diff src/main.rs"), None);
        assert_eq!(parse_session_command("/difffoo"), None);
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
    fn kill_tolerates_hash_and_whitespace() {
        // v3b-a minor fix: the displayed form `#N` and stray surrounding
        // whitespace must parse, not fall through to "Unknown command".
        assert_eq!(parse_session_command("/kill #2"), Some(SessionCommand::Kill { seq: 2 }));
        assert_eq!(
            parse_session_command("/kill   3"),
            Some(SessionCommand::Kill { seq: 3 })
        );
        assert_eq!(
            parse_session_command("  /kill 4  "),
            Some(SessionCommand::Kill { seq: 4 })
        );
        // A trailing extra token is ignored (first token wins) rather than
        // rejecting the whole command.
        assert_eq!(
            parse_session_command("/kill 5 extra"),
            Some(SessionCommand::Kill { seq: 5 })
        );
        // The same tolerance applies to the sibling seq commands.
        assert_eq!(
            parse_session_command("/attach #7"),
            Some(SessionCommand::Attach { seq: 7 })
        );
        assert_eq!(
            parse_session_command("/attach 0"),
            None,
            "seq 0 is reserved for synthetic child views and must not route to /attach"
        );
        assert_eq!(
            parse_session_command("/attach #0"),
            None,
            "display-form seq 0 is also rejected"
        );
        assert_eq!(parse_session_command("/logs #9"), Some(SessionCommand::Logs { seq: 9 }));
        assert_eq!(
            parse_session_command("/steer #3 do it"),
            Some(SessionCommand::Steer {
                seq: 3,
                message: "do it".to_string()
            })
        );
        // A bare `#` with no digits is still rejected.
        assert_eq!(parse_session_command("/kill #"), None);
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
    fn steer_unsupported_message_shell_pty_and_transcript_are_clear() {
        use super::super::model::ManagedKind;
        let shell = steer_unsupported_message(ManagedKind::Shell, 2).expect("test: shell msg");
        assert!(shell.contains("shell #2"), "names the shell: {shell}");
        assert!(shell.to_lowercase().contains("not supported"), "states it: {shell}");

        let pty = steer_unsupported_message(ManagedKind::Pty, 4).expect("test: pty msg");
        assert!(pty.contains("PTY session #4"), "names the pty: {pty}");
        assert!(pty.to_lowercase().contains("not supported"), "states it: {pty}");

        let transcript = steer_unsupported_message(ManagedKind::Transcript, 0).expect("test: transcript msg");
        assert!(
            transcript.contains("read-only transcript"),
            "names transcript: {transcript}"
        );
        assert!(
            transcript.to_lowercase().contains("not supported"),
            "states it: {transcript}"
        );
    }
}
