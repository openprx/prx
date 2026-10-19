//! Background/managed session glue: local session commands, daemon and assignment requests, exit shutdown.

#[cfg(feature = "terminal-tui")]
use super::input_queue::QueuedInputMessage;
use super::output::print_fallback_chat_output;
#[cfg(feature = "terminal-tui")]
use super::provider_turns::provider_turn_visible_admission;
#[cfg(feature = "terminal-tui")]
use super::tui;
use super::{SESSION_LOGS_MAX_LINES, dispatcher, session};
use crate::config::Config;
use crate::tools::Tool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Format the recap of child sessions restored from a reloaded chat
/// session (v4). Pure string builder (no I/O, no lock) so it is trivially
/// unit-testable. Each line shows the kind, display seq `#N`, terminal status,
/// title/command, and (when present) the completion summary. The header makes
/// it explicit that these are historical results — nothing has been revived.
pub(super) fn format_reloaded_background_sessions(
    sessions: &[crate::chat::sessions::PersistedSessionSummary],
) -> String {
    let mut out = String::new();
    out.push_str("[previous session — background task results (not resumed)]");
    for s in sessions {
        let summary = s.summary.trim();
        // v5: tag model-spawned sessions so the recap distinguishes them from
        // operator-initiated ones. User-initiated sessions stay untagged to keep
        // the common case quiet (origin defaults to "user" for legacy blobs).
        let origin_tag = if s.origin == "model" { " [model]" } else { "" };
        let usage = crate::chat::session::summarize_session_token_usage(&s.token_usage_records)
            .and_then(crate::chat::session::format_session_token_usage_inline);
        let usage = usage.map_or_else(String::new, |usage| format!(" {usage}"));
        if summary.is_empty() {
            out.push_str(&format!(
                "\n  · {} #{}{} {}{} — {}",
                s.kind, s.seq, origin_tag, s.status, usage, s.title
            ));
        } else {
            out.push_str(&format!(
                "\n  · {} #{}{} {}{} — {}: {}",
                s.kind, s.seq, origin_tag, s.status, usage, s.title, summary
            ));
        }
    }
    out
}

pub(super) fn format_managed_sessions_list(views: &[crate::chat::sessions::model::ManagedSessionView]) -> String {
    if views.is_empty() {
        return "No child TUI sessions.".to_string();
    }
    let mut out = String::from("Background sessions:\n");
    for v in views {
        let usage = crate::chat::session::summarize_session_token_usage(&v.token_usage_records)
            .and_then(crate::chat::session::format_session_token_usage_inline);
        let usage = usage.map_or_else(String::new, |usage| format!(" {usage}"));
        out.push_str(&format!(
            "  #{} {} {} {} {}{} {}\n",
            v.seq,
            v.kind.as_str(),
            v.origin.as_str(),
            v.status.as_str(),
            crate::chat::sessions::model::session_elapsed_label(v),
            usage,
            v.title
        ));
    }
    out.trim_end().to_string()
}

/// Append this chat's in-flight daemon requests to a `/sessions` listing.
fn append_in_flight_daemon_requests(
    listing: String,
    registry: &crate::chat::sessions::daemon::InFlightRegistry,
) -> String {
    match crate::chat::sessions::daemon::in_flight_report(registry) {
        Some(report) => format!("{listing}\n\n{report}"),
        None => listing,
    }
}

pub(super) async fn handle_local_session_command(
    action: &crate::chat::sessions::SessionCommand,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
    tools_registry: &[Box<dyn Tool>],
) -> Option<String> {
    use crate::chat::sessions::SessionCommand;

    match action {
        SessionCommand::Sessions => {
            let views = chat_sessions.snapshot().await;
            // A daemon request is not a child session — nothing local is
            // running for it — but it is in-flight work this chat is waiting
            // on, and the whole point of listing it is that a daemon which
            // accepted the connection and never answered must not be able to
            // hide here. Appended rather than merged so the two remain
            // visibly different kinds of thing.
            Some(append_in_flight_daemon_requests(
                format_managed_sessions_list(&views),
                &chat_sessions.daemon_requests(),
            ))
        }
        SessionCommand::Logs { seq } => {
            Some(format_local_session_logs(*seq, chat_sessions, session_rings, reaped_log_archive, reap_policy).await)
        }
        SessionCommand::Kill { seq } => Some(kill_local_session(*seq, chat_sessions, tools_registry).await),
        _ => None,
    }
}

async fn format_local_session_logs(
    seq: u64,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
) -> String {
    match chat_sessions.resolve_run_id(seq).await {
        Ok(run_id) => {
            let sid = crate::chat::sessions::id::SessionId::from_run_id(&run_id);
            session_rings.get(&sid).map_or_else(
                || format!("Session #{seq} has no buffered output yet."),
                |ring| {
                    // Replay the retained window without disturbing live-follow cursors.
                    let lines = ring.recent_lines(SESSION_LOGS_MAX_LINES);
                    if lines.is_empty() {
                        format!("Session #{seq} has no buffered output yet.")
                    } else {
                        format_session_logs(seq, &lines, ring.is_truncated())
                    }
                },
            )
        }
        Err(e) => {
            let now = chrono::Utc::now();
            reaped_log_archive
                .logs_message(seq, reap_policy, now)
                .unwrap_or_else(|| {
                    chat_sessions.reaped_session(seq).map_or_else(
                        || format!("Logs failed: {e}"),
                        |reaped| format_reaped_session_notice(seq, Some(&reaped.summary)),
                    )
                })
        }
    }
}

async fn kill_local_session(
    seq: u64,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    tools_registry: &[Box<dyn Tool>],
) -> String {
    match chat_sessions.kind_for_seq(seq).await {
        Ok(crate::chat::sessions::model::ManagedKind::Shell) => {
            return match chat_sessions.kill_shell(seq).await {
                Ok(()) => format!("Killed background shell #{seq} (process group terminated)."),
                Err(e) => format!("Kill failed: {e}"),
            };
        }
        Ok(crate::chat::sessions::model::ManagedKind::Pty) => {
            #[cfg(feature = "terminal-tui")]
            {
                return match chat_sessions.kill_pty(seq).await {
                    Ok(()) => {
                        format!("Killed interactive PTY session #{seq} (process group terminated).")
                    }
                    Err(e) => format!("Kill failed: {e}"),
                };
            }
            #[cfg(not(feature = "terminal-tui"))]
            {
                return "Interactive PTY sessions are only available in the terminal TUI.".to_string();
            }
        }
        Ok(crate::chat::sessions::model::ManagedKind::Agent) => {}
        Ok(crate::chat::sessions::model::ManagedKind::Transcript) => {
            return "Transcript is a read-only viewer, not a killable child session.".to_string();
        }
        Ok(crate::chat::sessions::model::ManagedKind::Approval) => {
            return "Tool approval is a foreground prompt, not a killable child session.".to_string();
        }
        Ok(crate::chat::sessions::model::ManagedKind::Diff) => {
            return "Diff is a read-only viewer, not a killable child session.".to_string();
        }
        Ok(crate::chat::sessions::model::ManagedKind::Worker) => {
            return "Provider worker detail is a read-only viewer, not a killable child session.".to_string();
        }
        Err(e) => return format!("Kill failed: {e}"),
    }

    // Agent path: resolve `#N` -> run UUID and delegate to sessions_spawn `kill`
    // so side-effect grants, terminal-state checks, steer cleanup, and task
    // events remain identical to the normal `/kill` path.
    let run_id = match chat_sessions.resolve_run_id(seq).await {
        Ok(id) => id,
        Err(e) => return format!("Kill failed: {e}"),
    };
    let Some(tool) = tools_registry.iter().find(|t| t.supports_name("sessions_spawn")) else {
        return "Background sessions are not available in this session.".to_string();
    };

    let operation_name = format!("sessions_spawn:kill:{run_id}");
    let grant = crate::security::policy::ApprovalGrant::for_resource_operation(
        "sessions_spawn",
        &operation_name,
        "chat-operator",
        None,
    );
    let mut args = serde_json::json!({
        "action": "kill",
        "run_id": run_id,
    });
    match serde_json::to_value(&grant) {
        Ok(grant_value) => {
            if let Some(obj) = args.as_object_mut() {
                obj.insert(
                    crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                    grant_value,
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to serialize kill approval grant; proceeding without it"
            );
        }
    }
    match tool.execute_named("sessions_spawn", args).await {
        Ok(result) => {
            if result.output.is_empty() {
                result
                    .error
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| "(no output)".to_string())
            } else {
                result.output
            }
        }
        Err(e) => format!("Kill failed: {e}"),
    }
}

pub(super) fn format_observed_session_announcement(view: &crate::chat::sessions::model::ManagedSessionView) -> String {
    let elapsed = crate::chat::sessions::model::session_elapsed_label(view);
    let title = clamp_chars(&view.title, CHILD_STARTED_TITLE_MAX_CHARS);
    let verb = if matches!(
        view.status,
        crate::chat::sessions::model::ManagedStatus::Running | crate::chat::sessions::model::ManagedStatus::NeedsInput
    ) {
        "started"
    } else {
        "observed"
    };
    if title.is_empty() {
        format!(
            "[{} #{} {} {} {elapsed}] {verb}",
            view.kind.as_str(),
            view.seq,
            view.origin.as_str(),
            view.status.as_str(),
        )
    } else {
        format!(
            "[{} #{} {} {} {elapsed}] {verb}: {title}",
            view.kind.as_str(),
            view.seq,
            view.origin.as_str(),
            view.status.as_str(),
        )
    }
}

pub(super) fn format_finished_session_announcement(fin: &crate::chat::sessions::runtime::FinishedSession) -> String {
    let kind = fin.kind.as_str();
    let marker = match fin.status {
        crate::chat::sessions::model::ManagedStatus::Completed => "✓",
        crate::chat::sessions::model::ManagedStatus::Failed
        | crate::chat::sessions::model::ManagedStatus::Cancelled => "✗",
        crate::chat::sessions::model::ManagedStatus::Running
        | crate::chat::sessions::model::ManagedStatus::NeedsInput => "•",
    };
    let elapsed = crate::chat::sessions::model::format_elapsed_compact(
        crate::chat::sessions::model::elapsed_seconds_between(fin.created_at, fin.updated_at),
    );
    let usage = if fin.kind == crate::chat::sessions::model::ManagedKind::Agent {
        crate::chat::session::summarize_session_token_usage(&fin.token_usage_records)
            .and_then(crate::chat::session::format_session_token_usage_inline)
    } else {
        None
    };
    let suffix = usage.map_or_else(|| elapsed.clone(), |usage| format!("{elapsed} · {usage}"));
    let summary = compact_child_completion_summary(&fin.summary);
    if summary.is_empty() {
        format!("[{kind} #{} {marker} {suffix}]", fin.seq)
    } else {
        format!("[{kind} #{} {marker} {suffix}] {summary}", fin.seq)
    }
}

pub(super) const CHILD_COMPLETION_SUMMARY_MAX_CHARS: usize = 120;

const CHILD_STARTED_TITLE_MAX_CHARS: usize = 96;

pub(super) fn compact_child_completion_summary(text: &str) -> String {
    let Some(line) = text.lines().map(str::trim).find(|line| is_useful_completion_line(line)) else {
        return String::new();
    };
    clamp_chars(line, CHILD_COMPLETION_SUMMARY_MAX_CHARS)
}

#[cfg(feature = "terminal-tui")]
pub(super) fn refresh_sessions_cache(mirror: &mut tui::TuiState, entries: Vec<crate::chat::sessions::SwitcherEntry>) {
    mirror.sessions_cache = entries;
}

fn is_useful_completion_line(line: &str) -> bool {
    !line.is_empty() && !line.starts_with("```") && !line.starts_with("~~~")
}

fn clamp_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out = String::new();
    for _ in 0..max_chars {
        match chars.next() {
            Some(ch) => out.push(ch),
            None => return out,
        }
    }
    if chars.next().is_some() && max_chars > 0 {
        out.pop();
        out.push('…');
    }
    out
}

pub(super) fn format_session_logs(seq: u64, lines: &[String], truncated: bool) -> String {
    let mut out = format!("Session #{seq} logs (last {} lines):\n", lines.len());
    if truncated {
        out.push_str("  [output truncated]\n");
    }
    for line in lines {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[derive(Debug, Clone)]
pub(super) struct ArchivedReapedSessionLog {
    pub(super) seq: u64,
    pub(super) reaped_at: chrono::DateTime<chrono::Utc>,
    pub(super) summary: crate::chat::sessions::PersistedSessionSummary,
    pub(super) lines: Option<Vec<String>>,
    pub(super) truncated: bool,
}

#[derive(Debug, Default)]
pub(super) struct ReapedSessionLogArchive {
    pub(super) entries: Vec<ArchivedReapedSessionLog>,
}

impl ReapedSessionLogArchive {
    pub(super) fn archive_reaped(
        &mut self,
        reaped: &[crate::chat::sessions::runtime::ReapedSession],
        session_rings: &mut std::collections::HashMap<
            crate::chat::sessions::id::SessionId,
            crate::chat::sessions::SessionRing,
        >,
        policy: &crate::chat::sessions::runtime::ReapPolicy,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        for session in reaped {
            let (lines, truncated) = session_rings.remove(&session.id).map_or_else(
                || (Vec::new(), false),
                |ring| {
                    (
                        cap_archived_log_lines(
                            ring.recent_lines(policy.archive_max_lines),
                            policy.archive_max_lines,
                            policy.archive_max_bytes,
                        ),
                        ring.is_truncated(),
                    )
                },
            );
            self.entries.push(ArchivedReapedSessionLog {
                seq: session.seq,
                reaped_at: session.reaped_at,
                summary: session.summary.clone(),
                lines: Some(lines),
                truncated,
            });
        }
        self.prune(policy, now);
    }

    fn prune(&mut self, policy: &crate::chat::sessions::runtime::ReapPolicy, now: chrono::DateTime<chrono::Utc>) {
        self.entries
            .sort_by(|a, b| b.reaped_at.cmp(&a.reaped_at).then_with(|| b.seq.cmp(&a.seq)));
        for (idx, entry) in self.entries.iter_mut().enumerate() {
            let within_ttl = now.signed_duration_since(entry.reaped_at) <= policy.archive_ttl;
            if idx >= policy.archive_keep_last && !within_ttl {
                entry.lines = None;
            }
        }
    }

    pub(super) fn logs_message(
        &mut self,
        seq: u64,
        policy: &crate::chat::sessions::runtime::ReapPolicy,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<String> {
        self.prune(policy, now);
        let entry = self.entries.iter().rev().find(|entry| entry.seq == seq)?;
        match entry.lines.as_ref() {
            Some(lines) if !lines.is_empty() => Some(format_session_logs(seq, lines, entry.truncated)),
            Some(_) => Some(format_reaped_session_notice(seq, Some(&entry.summary))),
            None => Some(format_reaped_session_notice(seq, Some(&entry.summary))),
        }
    }
}

fn cap_archived_log_lines(mut lines: Vec<String>, max_lines: usize, max_bytes: usize) -> Vec<String> {
    if lines.len() > max_lines {
        let drop = lines.len().saturating_sub(max_lines);
        lines.drain(0..drop);
    }
    let mut total = lines.iter().map(|line| line.len()).sum::<usize>();
    while total > max_bytes && !lines.is_empty() {
        let first = lines.remove(0);
        total = total.saturating_sub(first.len());
    }
    lines
}

fn format_reaped_session_notice(seq: u64, summary: Option<&crate::chat::sessions::PersistedSessionSummary>) -> String {
    let mut msg = format!("Session #{seq} was reaped from live sessions; use the persisted summary.");
    if let Some(summary) = summary {
        let body = summary.summary.trim();
        if !body.is_empty() {
            msg.push_str(&format!(" Summary: {body}"));
        }
    }
    msg
}

pub(super) fn is_chat_quit_command(input: &str) -> bool {
    matches!(input, "/quit" | "/exit")
}

pub(super) async fn shutdown_child_sessions_for_exit(
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    chat_session: &mut session::ChatSession,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) -> crate::chat::sessions::runtime::ShutdownReport {
    let report = chat_sessions.shutdown_all("chat-exit").await;
    for persisted in report
        .summaries
        .iter()
        .filter(|summary| summary.status == crate::chat::sessions::model::STATUS_INTERRUPTED)
        .cloned()
    {
        chat_session.record_background_session(persisted.clone());
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::BackgroundSessionRecorded { summary: persisted },
            "chat.bg_session_recorded_exit",
        );
    }
    report
}

/// Surface a background-session system message into the chat (v1b reflow).
///
/// Standalone (not the in-loop `emit_chat_output` closure) so the main loop's
/// timer-tick branch — which runs inside the `select!` header, before the
/// closure is in scope — can use it. Behaviour mirrors `emit_chat_output`:
/// route through the dispatcher (single source, reaches both render paths) and
/// nudge the renderer on the TUI path; fall back to plain stdout otherwise.
#[cfg_attr(not(feature = "terminal-tui"), allow(unused_variables))]
pub(super) fn surface_session_message(
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    text: &str,
) {
    #[cfg(feature = "terminal-tui")]
    {
        let _ = dispatcher.dispatch_or_log(
            crate::chat::action::Action::SystemMessageAdded { text: text.to_string() },
            "chat.system_message_session",
        );
        if let Some(tx) = redraw_tx {
            let _ = tx.try_send(());
        } else {
            print_fallback_chat_output(text);
        }
    }
    #[cfg(not(feature = "terminal-tui"))]
    {
        print_fallback_chat_output(text);
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn surface_active_turn_message(
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    text: &str,
) {
    surface_session_message(dispatcher, redraw_tx, text);
}

/// Run one `--daemon`-scoped session command without holding the input loop (a5).
///
/// The runtime control API deliberately carries no request timeout — a running
/// task must not be abandoned because a clock expired — so awaiting it inline
/// would let an unresponsive daemon freeze the TUI, which is strictly worse
/// than the feature being missing. The request therefore runs in its own task
/// and reports back through the same system-message path background sessions
/// use; the loop returns to reading input immediately.
///
/// `local_fallback` is this chat's own `/sessions` listing, captured by the
/// caller *before* the task starts: `chat_sessions` is loop-owned mutable state
/// that cannot cross into a spawned task, so a listing that degrades on failure
/// has to bring its fallback with it.
///
/// The task is published in `in_flight` for as long as it runs. Without that a
/// daemon which accepts the connection and then answers nothing would leave a
/// task nobody can see and nobody can end — and the answer to a stall in this
/// runtime is being visible and killable, never a clock. The guard rides into
/// the task, so the row is retired by any ending: answered, failed, or ended by
/// an operator.
fn spawn_daemon_session_request(
    request: crate::chat::sessions::daemon::DaemonRequest,
    endpoint: crate::runtime::tasks_cli::TasksEndpoint,
    local_fallback: Option<String>,
    in_flight: &crate::chat::sessions::daemon::InFlightRegistry,
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
) {
    let cancel = CancellationToken::new();
    let guard =
        crate::chat::sessions::daemon::register_in_flight(in_flight, &request, endpoint.base_url(), cancel.clone());
    // Built before `request` is consumed by the call below.
    let cancelled = crate::chat::sessions::daemon::cancelled_notice(&request, &endpoint.shown_url());
    let dispatcher = dispatcher.clone();
    let redraw_tx = redraw_tx.cloned();
    tokio::spawn(async move {
        let _guard = guard;
        // `run` renders both outcomes, so there is no error path to drop here.
        // Cancellation drops the in-flight HTTP request with it, which is what
        // makes `/kill --daemon d<N>` an actual end rather than a hidden leak.
        let text = tokio::select! {
            () = cancel.cancelled() => cancelled,
            text = crate::chat::sessions::daemon::run(&endpoint, request, local_fallback.as_deref()) => text,
        };
        surface_session_message(&dispatcher, redraw_tx.as_ref(), &text);
    });
}

/// Everything a `--daemon`-scoped command needs to reach the daemon.
///
/// Bundled because the same three values have to reach both execution paths —
/// the idle input loop and the active-turn one — and the active-turn signature
/// is already long.
#[derive(Clone, Copy)]
pub(super) struct DaemonControl<'a> {
    pub(super) config: &'a Config,
    pub(super) dispatcher: &'a dispatcher::ChatDispatcher,
    pub(super) redraw_tx: Option<&'a mpsc::Sender<()>>,
}

/// Start one `--daemon`-scoped session command, returning the line to show now.
///
/// `None` for any action that is not daemon-scoped, so a caller can use this as
/// the daemon arm of its dispatch and fall through to the local handler.
///
/// This is the single execution point for the three `Daemon*` variants: chat
/// reaches them from two places (the idle loop and, while a provider turn is in
/// flight, the active-turn path), and a `--daemon` command that only worked in
/// one of them would be missing exactly when it is most wanted — a long turn is
/// running and the operator needs to look at, or end, what the daemon is doing.
///
/// `local_fallback` is only meaningful for a listing; the caller builds it
/// because only the caller holds `chat_sessions`.
pub(super) fn start_daemon_session_command(
    action: &crate::chat::sessions::SessionCommand,
    local_fallback: Option<String>,
    in_flight: &crate::chat::sessions::daemon::InFlightRegistry,
    control: DaemonControl<'_>,
) -> Option<String> {
    use crate::chat::sessions::SessionCommand;

    let request = match action {
        // Chat sees the daemon's work as a local *operator*, not as the user
        // whose sessions `/sessions` lists; the local listing rides along only
        // as the fallback to show if the daemon turns out to be unreachable.
        SessionCommand::DaemonSessions => crate::chat::sessions::daemon::DaemonRequest::List,
        SessionCommand::DaemonKill { address } => {
            // A `d<N>` address names one of *this chat's* own in-flight
            // requests, which is the only address the daemon could not act on:
            // it is this process that is waiting, not the daemon that is
            // working. Every other address space belongs to the daemon and is
            // forwarded verbatim.
            if let Some(text) = crate::chat::sessions::daemon::cancel_in_flight(in_flight, address) {
                return Some(text);
            }
            // No local fallback below: a kill either happened in the daemon or
            // it did not, and this chat's own sessions are not a partial answer
            // to that question.
            crate::chat::sessions::daemon::DaemonRequest::Kill {
                address: address.clone(),
            }
        }
        // The daemon resolves the address and decides whether the target can be
        // steered at all (a turn has no steer channel and is refused there), so
        // chat does no kind dispatch of its own — unlike the local `/steer`,
        // which owns its session kinds.
        SessionCommand::DaemonSteer { address, message } => crate::chat::sessions::daemon::DaemonRequest::Steer {
            address: address.clone(),
            message: message.clone(),
        },
        _ => return None,
    };
    let endpoint = crate::chat::sessions::daemon::endpoint(control.config);
    let notice = crate::chat::sessions::daemon::pending_notice(&request, &endpoint.shown_url());
    spawn_daemon_session_request(
        request,
        endpoint,
        local_fallback,
        in_flight,
        control.dispatcher,
        control.redraw_tx,
    );
    Some(notice)
}

/// One chat process's enrolment for daemon-assigned work.
///
/// Held for the life of the chat loop so the exit path can withdraw it: nothing
/// on the daemon side removes a chat session on a clock — a session that
/// stopped pulling is *reported* as silent, never evicted — so an exit that
/// forgets this leaves a row that work can still be addressed to with nobody
/// left to run it.
pub(super) struct AssignmentEnrolment {
    pub(super) client: std::sync::Arc<crate::chat::sessions::assignment::AssignmentClient>,
    /// Ends the poll loop. Also the token behind its `/kill --daemon d<N>` row.
    pub(super) cancel: CancellationToken,
}

/// The sink assignment machinery reports through.
///
/// The same system-message path background sessions use, so assigned work is
/// announced wherever the operator is already reading — TUI transcript on one
/// path, plain stdout on the other — instead of inventing a second surface.
fn assignment_notifier(
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
) -> std::sync::Arc<dyn Fn(&str) + Send + Sync> {
    let dispatcher = dispatcher.clone();
    let redraw_tx = redraw_tx.cloned();
    std::sync::Arc::new(move |text: &str| surface_session_message(&dispatcher, redraw_tx.as_ref(), text))
}

/// Start enrolling this chat for daemon-assigned work, without waiting for it.
///
/// Returns `None` when nothing in the configuration points this chat at a
/// daemon — the common case, and one that must stay silent: the fallback daemon
/// address is `[gateway]`'s own bind, which every chat has whether or not a
/// daemon listens there, so a chat that tried regardless would announce a
/// refusal on every single start.
///
/// Enrolment itself happens in the spawned task, so a daemon that is slow,
/// down, or refusing credentials never delays the prompt.
pub(super) fn start_assignment_enrolment(
    config: &Config,
    chat_sessions: &crate::chat::sessions::ChatSessionsHandle,
    input_tx: &mpsc::Sender<crate::channels::traits::ChannelMessage>,
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
) -> Option<AssignmentEnrolment> {
    use crate::chat::sessions::assignment::{self, Enrolable};

    match assignment::enrolable(config) {
        Enrolable::No(reason) => {
            tracing::debug!(reason, "chat is not enrolling for daemon assignments");
            return None;
        }
        Enrolable::Yes => {}
    }
    let client = assignment::AssignmentClient::new(config);
    let cancel = CancellationToken::new();
    tokio::spawn(assignment::enrol_and_poll(
        std::sync::Arc::clone(&client),
        chat_sessions.daemon_requests(),
        input_tx.clone(),
        assignment_notifier(dispatcher, redraw_tx),
        cancel.clone(),
    ));
    Some(AssignmentEnrolment { client, cancel })
}

/// Arm the report for an input message that turns out to be assigned work.
///
/// `None` for ordinary input and for a chat that never enrolled, so the turn
/// loop pays nothing for the feature it is not using.
pub(super) fn assignment_turn_guard(
    enrolment: Option<&AssignmentEnrolment>,
    chat_sessions: &crate::chat::sessions::ChatSessionsHandle,
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    msg: &crate::channels::traits::ChannelMessage,
) -> Option<crate::chat::sessions::assignment::AssignmentTurnGuard> {
    let delivery = crate::chat::sessions::assignment::delivered(msg)?;
    let enrolment = enrolment?;
    Some(crate::chat::sessions::assignment::AssignmentTurnGuard::new(
        std::sync::Arc::clone(&enrolment.client)
            as std::sync::Arc<dyn crate::chat::sessions::assignment::AssignmentMailbox>,
        chat_sessions.daemon_requests(),
        assignment_notifier(dispatcher, redraw_tx),
        delivery.assignment_id,
    ))
}

/// Withdraw this chat from the daemon's registry on the way out.
///
/// Ends the poll loop first so nothing takes new work between here and the
/// deregistration, then hands back whatever was still outstanding. The daemon
/// records a `cancelled` result for each of those, which is how the assigner
/// learns the work is not coming rather than waiting on a session that no
/// longer exists.
///
/// The final turn's result is reported from its own task, so it races this
/// call. That race is not closed by waiting on it: the control API carries no
/// request timeout by design, so a daemon that accepted the connection and then
/// said nothing would hold this chat's exit open with nothing left on screen to
/// end it with. The losing side of the race is a `cancelled` verdict instead of
/// a `completed` one — still true, and still delivered.
pub(super) async fn withdraw_assignment_enrolment(
    enrolment: Option<&AssignmentEnrolment>,
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
) {
    let Some(enrolment) = enrolment else {
        return;
    };
    enrolment.cancel.cancel();
    if !enrolment.client.is_enrolled() {
        return;
    }
    match enrolment.client.deregister().await {
        Ok(0) => tracing::info!("withdrew this chat session from the daemon assignment registry"),
        Ok(discarded) => surface_session_message(
            dispatcher,
            redraw_tx,
            &format!(
                "Withdrew this chat from the daemon; {discarded} assignment(s) it had not finished were \
                 handed back as cancelled."
            ),
        ),
        Err(error) => tracing::warn!(
            error = %error,
            "could not withdraw this chat session from the daemon assignment registry; it will be listed \
             as silent until an operator reaps it"
        ),
    }
}

/// Whether an action is one of the three `--daemon`-scoped variants.
#[cfg(feature = "terminal-tui")]
pub(super) const fn is_daemon_session_command(action: &crate::chat::sessions::SessionCommand) -> bool {
    matches!(
        action,
        crate::chat::sessions::SessionCommand::DaemonSessions
            | crate::chat::sessions::SessionCommand::DaemonKill { .. }
            | crate::chat::sessions::SessionCommand::DaemonSteer { .. }
    )
}

#[cfg(feature = "terminal-tui")]
pub(super) fn defer_resume_saved_session_if_provider_turn_pending(
    pending_turns: usize,
    deferred: &mut std::collections::VecDeque<String>,
    id: String,
) -> bool {
    if pending_turns == 0 {
        return false;
    }
    deferred.push_back(id);
    true
}

#[cfg(feature = "terminal-tui")]
pub(super) const fn should_continue_event_pump_after_input_closed(pending_turns: usize) -> bool {
    pending_turns > 0
}

#[cfg(feature = "terminal-tui")]
pub(super) fn should_drain_deferred_resume_after_visible_inputs(
    pending_turns: usize,
    backlog: &std::collections::VecDeque<QueuedInputMessage>,
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
) -> bool {
    pending_turns == 0
        && backlog.is_empty()
        && provider_turn_visible_admission(
            workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
            1,
        )
        .can_start_visible
}

pub(super) const fn consume_deferred_visible_input_pop(defer_visible_input_pop_once: &mut bool) -> bool {
    let should_defer = *defer_visible_input_pop_once;
    *defer_visible_input_pop_once = false;
    should_defer
}

pub(super) const fn surface_turn_elapsed_message(
    _dispatcher: &dispatcher::ChatDispatcher,
    _redraw_tx: Option<&mpsc::Sender<()>>,
    _status: &str,
    _started_at: chrono::DateTime<chrono::Utc>,
    _finished_at: chrono::DateTime<chrono::Utc>,
) {
    // Completion timing remains available in structured state and diagnostics,
    // but it is intentionally not emitted into the conversation transcript.
}

/// Session commands that stay answerable while a provider turn is in flight.
///
/// The three `Daemon*` variants are here for the same reason the local ones
/// are, only more so: a `--daemon` command exists to look at, or end, work in
/// another process, and the moment an operator needs that is precisely while a
/// long turn is running here. Leaving them out queued them behind the very turn
/// they were reached for. They cost the loop nothing — each one hands its
/// request to a task and returns the acceptance line (see
/// [`start_daemon_session_command`]).
#[cfg(feature = "terminal-tui")]
pub(super) fn local_session_command(input: &str) -> Option<crate::chat::sessions::SessionCommand> {
    let action = crate::chat::sessions::parse_session_command(input)?;
    match action {
        crate::chat::sessions::SessionCommand::Sessions
        | crate::chat::sessions::SessionCommand::Logs { .. }
        | crate::chat::sessions::SessionCommand::Kill { .. } => Some(action),
        _ if is_daemon_session_command(&action) => Some(action),
        _ => None,
    }
}
