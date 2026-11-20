//! Line-mode terminal approval prompt for `prx chat --plain` and
//! `prx chat --message` (CR-2).
//!
//! The prompt `Approve <tool> <summary>? [y/N/a] ` goes to stderr and one
//! line is read from the controlling terminal: `y` approves this call, `a`
//! approves it and later calls of the same tool, anything else or EOF denies.
//! Without a controlling terminal only `OPENPRX_APPROVAL_OVERRIDE` can allow;
//! otherwise the call is denied.
#![allow(clippy::print_stderr)]

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::tools::ApprovalDenialCause;

/// Upper bound on the bytes kept from one answer line.
const ANSWER_MAX_BYTES: usize = 256;
/// How often a waiting prompt checks whether its approval is still pending.
const PENDING_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// Pause between non-blocking reads of the terminal device.
const TTY_READ_PAUSE: Duration = Duration::from_millis(50);
/// Characters of the argument summary shown in the prompt.
const SUMMARY_MAX_CHARS: usize = 160;

/// The operator's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalApproval {
    /// `y`: this call only.
    Once,
    /// `a`: this call and later calls of the same tool.
    Always,
    /// Refused: `UserRejected` for any other answer; otherwise EOF or a
    /// stopped wait (`NoAnswer`), no terminal without an allowing override
    /// (`NoTerminal`), or a busy or failed reader (`PromptUnavailable`).
    Deny(ApprovalDenialCause),
}

impl TerminalApproval {
    /// `(approved, always)`.
    pub(crate) const fn flags(self) -> (bool, bool) {
        match self {
            Self::Once => (true, false),
            Self::Always => (true, true),
            Self::Deny(_) => (false, false),
        }
    }

    fn parse(line: &str) -> Self {
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Self::Once,
            "a" | "always" => Self::Always,
            _ => Self::Deny(ApprovalDenialCause::UserRejected),
        }
    }
}

/// What one approval prompt read from its answer line source.
#[derive(Debug)]
pub(crate) enum AnswerRead {
    /// There is no controlling terminal; only `OPENPRX_APPROVAL_OVERRIDE` can
    /// allow.
    NoTerminal(std::io::Error),
    /// A terminal exists but no line can be read from it (another prompt
    /// holds it, or its reader could not start).
    Unavailable,
    /// The source finished: the answer line, or `None` on EOF.
    Line(Option<String>),
    /// The approval stopped waiting before an answer arrived.
    Abandoned,
}

/// Where an approval prompt reads its one answer line. Production reads the
/// controlling terminal ([`ControllingTerminal`]); the line-mode tests inject
/// the lines an operator would type.
#[async_trait::async_trait]
pub(crate) trait AnswerLineSource: Send + Sync {
    /// Show the prompt for `tool` and read one answer line while
    /// `still_waiting` holds.
    async fn read_answer(&self, tool: &str, args: &str, still_waiting: &(dyn Fn() -> bool + Send + Sync))
    -> AnswerRead;
}

/// The controlling terminal (`/dev/tty`), or the chat input thread's next
/// line while that thread owns the terminal stdin.
pub(crate) struct ControllingTerminal;

#[async_trait::async_trait]
impl AnswerLineSource for ControllingTerminal {
    async fn read_answer(
        &self,
        tool: &str,
        args: &str,
        still_waiting: &(dyn Fn() -> bool + Send + Sync),
    ) -> AnswerRead {
        let tty = match open_tty() {
            Ok(tty) => tty,
            Err(error) => return AnswerRead::NoTerminal(error),
        };
        let show_prompt = || {
            eprint!("Approve {tool} {}? [y/N/a] ", summary(args));
            let _ = std::io::stderr().flush();
        };

        let answer = if crate::channels::terminal::prompt_line::tty_input_loop_active() {
            // The chat input thread owns the terminal stdin; take its next line.
            // The claim comes before the prompt, so only a line typed after
            // the prompt appeared can answer it.
            drop(tty);
            let claimed = crate::channels::terminal::prompt_line::claim_next_line();
            show_prompt();
            let Some(mut claimed) = claimed else {
                tracing::warn!(tool = %crate::agent::diagnostics::text_class(tool), "terminal approval prompt already active; denying");
                return AnswerRead::Unavailable;
            };
            wait_for_answer(claimed.line(), still_waiting).await
        } else {
            show_prompt();
            match TtyReader::spawn(tty) {
                Ok(mut reader) => {
                    let answer = wait_for_answer(reader.line(), still_waiting).await;
                    reader.stop();
                    answer
                }
                Err(error) => {
                    tracing::warn!(tool = %crate::agent::diagnostics::text_class(tool), error = %crate::agent::diagnostics::error_class(&error), "terminal approval reader unavailable; denying");
                    return AnswerRead::Unavailable;
                }
            }
        };
        match answer {
            Some(Some(line)) => AnswerRead::Line(Some(line)),
            Some(None) => {
                eprintln!();
                AnswerRead::Line(None)
            }
            None => {
                eprintln!("\napproval cancelled");
                AnswerRead::Abandoned
            }
        }
    }
}

/// Ask the operator on the terminal. `still_waiting` reports whether the
/// approval is still pending; once it is not (the turn was cancelled, for
/// example by Ctrl-C), the prompt stops reading and returns `Deny(NoAnswer)`.
pub(crate) async fn prompt<F>(tool: &str, args: &str, still_waiting: F) -> TerminalApproval
where
    F: Fn() -> bool + Send + Sync,
{
    prompt_from(&ControllingTerminal, tool, args, &still_waiting).await
}

/// [`prompt`] reading its answer from `source`.
pub(crate) async fn prompt_from(
    source: &dyn AnswerLineSource,
    tool: &str,
    args: &str,
    still_waiting: &(dyn Fn() -> bool + Send + Sync),
) -> TerminalApproval {
    match source.read_answer(tool, args, still_waiting).await {
        AnswerRead::NoTerminal(error) => override_decision(tool, &error),
        AnswerRead::Unavailable => TerminalApproval::Deny(ApprovalDenialCause::PromptUnavailable),
        AnswerRead::Line(None) => TerminalApproval::Deny(ApprovalDenialCause::NoAnswer),
        AnswerRead::Line(Some(line)) => TerminalApproval::parse(&line),
        AnswerRead::Abandoned => {
            tracing::info!(tool = %crate::agent::diagnostics::text_class(tool), "terminal approval abandoned: the approval is no longer pending");
            TerminalApproval::Deny(ApprovalDenialCause::NoAnswer)
        }
    }
}

/// `Some(answer)` when the line source finished (`None` inside is EOF);
/// `None` when the approval stopped waiting first.
async fn wait_for_answer<L>(line: L, still_waiting: &(dyn Fn() -> bool + Send + Sync)) -> Option<Option<String>>
where
    L: std::future::Future<Output = Option<String>>,
{
    tokio::pin!(line);
    let mut check = tokio::time::interval(PENDING_CHECK_INTERVAL);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            answer = &mut line => return Some(answer),
            _ = check.tick() => {
                if !still_waiting() {
                    return None;
                }
            }
        }
    }
}

fn override_decision(tool: &str, error: &std::io::Error) -> TerminalApproval {
    let raw = std::env::var("OPENPRX_APPROVAL_OVERRIDE").ok();
    let approved: bool = crate::chat::dispatcher::resolve_supervised_approval_override(raw.as_deref());
    if raw.is_none() {
        tracing::warn!(tool = %crate::agent::diagnostics::text_class(tool), error = %crate::agent::diagnostics::error_class(&error), "no controlling terminal and no approval override; denying");
    } else {
        tracing::info!(
            security_event = "openprx_approval_override_applied",
            tool = %crate::agent::diagnostics::text_class(tool),
            approved,
            "no controlling terminal; OPENPRX_APPROVAL_OVERRIDE applied"
        );
    }
    if approved {
        TerminalApproval::Once
    } else {
        TerminalApproval::Deny(ApprovalDenialCause::NoTerminal)
    }
}

/// Tell a line-mode operator that a tool call was refused and did not run.
pub(crate) fn announce_denied(tool: &str, reason: &str) {
    eprintln!("Denied {tool}: {reason} — the call did not run.");
}

/// A one-line argument summary with control characters removed.
fn summary(args: &str) -> String {
    let printable: String = args.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).collect();
    crate::util::truncate_with_ellipsis(&printable, SUMMARY_MAX_CHARS)
}

#[cfg(unix)]
fn open_tty() -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    // Non-blocking so the reader can stop when the approval stops waiting.
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/tty")
}

#[cfg(not(unix))]
fn open_tty() -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no controlling terminal on this platform",
    ))
}

/// Reads one line from the terminal device on its own thread.
struct TtyReader {
    stop: Arc<AtomicBool>,
    receiver: tokio::sync::oneshot::Receiver<Option<String>>,
}

impl TtyReader {
    fn spawn(tty: std::fs::File) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let thread_stop = Arc::clone(&stop);
        crate::runtime::blocking::spawn_detached_thread("prx-approval-tty", move || {
            let _ = sender.send(read_line(tty, &thread_stop));
        })?;
        Ok(Self { stop, receiver })
    }

    async fn line(&mut self) -> Option<String> {
        (&mut self.receiver).await.ok().flatten()
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for TtyReader {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One line (without the newline), `None` on EOF, a read error or a stop.
fn read_line(mut tty: std::fs::File, stop: &AtomicBool) -> Option<String> {
    use std::io::Read as _;
    let mut line = Vec::new();
    let mut buffer = [0_u8; 64];
    while !stop.load(Ordering::Acquire) {
        match tty.read(&mut buffer) {
            Ok(0) => return None,
            Ok(read) => {
                let chunk = buffer.get(..read).unwrap_or_default();
                if let Some(end) = chunk.iter().position(|byte| *byte == b'\n') {
                    line.extend_from_slice(chunk.get(..end).unwrap_or_default());
                    return Some(String::from_utf8_lossy(&line).into_owned());
                }
                line.extend_from_slice(chunk);
                if line.len() >= ANSWER_MAX_BYTES {
                    return Some(String::from_utf8_lossy(&line).into_owned());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(TTY_READ_PAUSE),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::TerminalApproval;
    use crate::tools::ApprovalDenialCause;

    #[test]
    fn answers_parse_to_once_always_or_deny() {
        for (line, expected) in [
            ("y", TerminalApproval::Once),
            (" YES \r", TerminalApproval::Once),
            ("a", TerminalApproval::Always),
            ("always", TerminalApproval::Always),
            ("n", TerminalApproval::Deny(ApprovalDenialCause::UserRejected)),
            ("", TerminalApproval::Deny(ApprovalDenialCause::UserRejected)),
            ("yy", TerminalApproval::Deny(ApprovalDenialCause::UserRejected)),
        ] {
            assert_eq!(TerminalApproval::parse(line), expected, "{line:?}");
        }
    }
}
