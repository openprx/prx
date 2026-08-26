//! Daemon-assigned work: how `prx chat` enrols, takes work, and answers for it.
//!
//! The other direction of the control link. `daemon.rs` is chat asking the
//! daemon to do something; this module is the daemon asking *chat* to. A
//! message can arrive on WhatsApp naming a chat session and a task, and that
//! task has to run in **this** process — the daemon's own
//! `POST /api/sessions/{id}/message` starts a turn inside the daemon and has
//! nothing to do with a separate `prx chat`.
//!
//! Four decisions shape everything below.
//!
//! - **Chat pulls; nothing pushes.** Chat already speaks to the daemon's HTTP
//!   control API, so taking work over the same client needs no listener, no
//!   port and no second transport in a process whose whole job is a terminal.
//!   The poll interval is a *sampling rate*, not a deadline: nothing here
//!   measures elapsed time, caps iterations, or abandons a request.
//! - **Acknowledge on the next pull, never on receipt.** The mailbox is
//!   at-least-once. A client that acknowledges the moment bytes arrive and then
//!   dies has destroyed the work; a client that acknowledges on its *next* pull
//!   has proved it survived long enough to queue it. The cost is redelivery,
//!   which [`AssignmentClient::accept`] absorbs by treating `assignment_id` as
//!   an idempotency key.
//! - **Enrolment never blocks startup.** A daemon that is down, unreachable or
//!   refusing credentials must cost this chat exactly one capability — being
//!   assignable — and nothing else. The failure is stated once, in the
//!   transcript, and chat carries on.
//! - **Assigned work is never silent.** Somebody else is driving this terminal.
//!   Every arrival is announced with where it came from and how it was
//!   dispatched, and every result reported back is announced too, so the person
//!   sitting in front of it can always account for what ran.
//!
//! What the three dispositions mean here is deliberately *not* new machinery:
//! the daemon only records a disposition and puts the item at the head of the
//! queue, and chat maps each one onto a path it already had.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::runtime::tasks_cli::{self, AssignmentPull, AssignmentReceipt, PulledAssignment, TasksEndpoint};

use super::daemon::{InFlightGuard, InFlightRegistry, register_in_flight_labeled};

/// How often chat asks the daemon whether it has been given work.
///
/// A sampling interval, in the same sense as `sessions_spawn`'s
/// `JOIN_POLL_INTERVAL`: it decides how often a question is asked, never how
/// long an answer may take. No call in this module carries a timeout, and
/// nothing here compares an elapsed duration against a limit.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1_500);

/// How many assignments to take in one pull.
///
/// A response-size bound, not a concurrency limit: whatever is left stays at
/// the head of the daemon's queue and comes on the next pull. Chat runs turns
/// one queue at a time regardless, so asking for more would only make a single
/// HTTP body larger.
const PULL_BATCH: usize = 8;

/// Upper bound the daemon enforces on a reported summary, in bytes.
const MAX_SUMMARY_BYTES: usize = 16_384;

/// Upper bound the daemon enforces on a registered label, in bytes.
const MAX_LABEL_BYTES: usize = 64;

/// How much of an assigned task is echoed into the transcript on arrival.
const TASK_PREVIEW_CHARS: usize = 120;

/// Marker prefix on the synthetic input message carrying an assignment.
///
/// The sender field is the same place `prx-ui` marks synthetic slash commands,
/// and for the same reason: the input queue is typed as channel messages, so a
/// message that is not a person typing has to say so somewhere the queue
/// already carries. Nothing outside this module parses it.
const SENDER_PREFIX: &str = "prx-assignment:";

/// Channel an assignment is delivered on.
///
/// `terminal`, exactly like a typed line: the whole point is that assigned work
/// runs through the ordinary turn machinery rather than a parallel path with
/// its own bugs.
const ASSIGNMENT_CHANNEL: &str = "terminal";

/// Reply target on the synthetic message, matching a typed line.
const ASSIGNMENT_REPLY_TARGET: &str = "user";

// ── Dispositions ────────────────────────────────────────────────────────────

/// What the assigning side asked chat to do about work already in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Run it when the current work is done. The default, and the least
    /// disruptive: an assignment is somebody else's priority, not necessarily
    /// this terminal's.
    Queue,
    /// Put it in front of whatever else is queued, without stopping the turn in
    /// flight.
    Steer,
    /// Stop the turn in flight first, then run it.
    Interrupt,
}

impl Disposition {
    /// Read the daemon's word for a disposition.
    ///
    /// An unrecognised word becomes [`Disposition::Queue`], never
    /// [`Disposition::Interrupt`]: a newer daemon inventing a stronger
    /// disposition must not be able to make an older chat destroy a turn it
    /// does not understand the reason for. The caller is told, so silence is
    /// not how it degrades.
    #[must_use]
    pub fn from_wire(raw: &str) -> Self {
        match raw.trim() {
            "steer" => Self::Steer,
            "interrupt" => Self::Interrupt,
            _ => Self::Queue,
        }
    }

    /// Whether the daemon's word was one this chat actually knows.
    #[must_use]
    pub fn is_known_wire(raw: &str) -> bool {
        matches!(raw.trim(), "queue" | "steer" | "interrupt")
    }

    /// The wire word for this disposition.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
            Self::Interrupt => "interrupt",
        }
    }

    /// Whether this disposition jumps the input queue.
    ///
    /// `steer` and `interrupt` both do; they differ only in whether the turn in
    /// flight is stopped first.
    #[must_use]
    pub const fn jumps_the_queue(self) -> bool {
        matches!(self, Self::Steer | Self::Interrupt)
    }

    /// Whether this disposition ends the turn in flight before delivering.
    #[must_use]
    pub const fn ends_the_turn_in_flight(self) -> bool {
        matches!(self, Self::Interrupt)
    }
}

/// What chat may report back about an assignment.
///
/// `cancelled` is deliberately absent. It is the daemon's own verdict about
/// work *it* ended, and a client able to claim it would make "an operator
/// killed this" indistinguishable from "the client gave up". The daemon refuses
/// it at the boundary; this enum makes it unrepresentable on the way there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    /// The turn ran and produced a reply.
    Completed,
    /// The turn ran and did not produce a reply, or was ended before it could.
    Failed,
    /// Chat declined to run it at all.
    Rejected,
}

impl ResultStatus {
    /// The wire word for this status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
        }
    }

    /// Every status a chat session can report, for tests and documentation.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::Completed, Self::Failed, Self::Rejected]
    }
}

// ── The mailbox, behind a trait ─────────────────────────────────────────────

/// Boxed future returned by [`AssignmentMailbox`] calls.
pub type MailboxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The two calls the poll loop and the result guard make.
///
/// A trait rather than a concrete client so the loop's ordering guarantees —
/// acknowledge on the *next* pull, run a redelivery once — can be asserted
/// against a recording double instead of against a live daemon. The HTTP
/// implementation is [`AssignmentClient`].
pub trait AssignmentMailbox: Send + Sync {
    /// Redacted handle for this session, for anything an operator will read.
    fn session_ref(&self) -> String;

    /// Where this mailbox lives, for the in-flight listing.
    fn base_url(&self) -> String;

    /// Take queued work, acknowledging `ack` — the ids from the previous pull —
    /// on the way in.
    fn pull(&self, ack: Vec<String>) -> MailboxFuture<'_, AssignmentPull>;

    /// Say what this session made of one assignment.
    fn report(
        &self,
        assignment_id: String,
        status: ResultStatus,
        summary: String,
    ) -> MailboxFuture<'_, AssignmentReceipt>;

    /// Claim one assignment id, returning `false` when it was already taken.
    ///
    /// The mailbox is at-least-once, so this is what makes a redelivery cost
    /// nothing: `false` means "work I already hold", which must still be
    /// acknowledged and must not be run a second time.
    fn accept(&self, assignment_id: &str) -> bool;
}

/// Chat's enrolment with one daemon, and the state that keeps it honest.
pub struct AssignmentClient {
    endpoint: TasksEndpoint,
    label: String,
    pid: Option<u32>,
    enrolment: Mutex<Option<Enrolment>>,
    /// Assignment ids this chat has already taken into its own queue.
    ///
    /// The mailbox is at-least-once, so the same id can arrive again whenever a
    /// response was lost on the way back. Running it twice would be the client
    /// duplicating somebody's work; this set is what makes redelivery
    /// harmless.
    taken: Mutex<HashSet<String>>,
}

/// The identity and credential one registration yielded.
#[derive(Debug, Clone)]
struct Enrolment {
    session_id: String,
    session_ref: String,
    /// Returned once by the daemon, which keeps only its hash. Held in memory
    /// for the life of the process and never written anywhere.
    token: String,
}

impl AssignmentClient {
    /// Build a client for the daemon this chat is configured to talk to.
    #[must_use]
    pub fn new(config: &Config) -> Arc<Self> {
        Arc::new(Self {
            endpoint: super::daemon::endpoint(config),
            label: label_for(config),
            pid: Some(std::process::id()),
            enrolment: Mutex::new(None),
            taken: Mutex::new(HashSet::new()),
        })
    }

    /// The label this chat registers under.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Register with the daemon, storing the session id and one-time token.
    ///
    /// Returns the redacted session handle on success. The caller decides what
    /// to do with a failure; nothing here retries, because a chat that cannot
    /// enrol has a perfectly usable degraded mode and hammering a daemon that
    /// refused it would only bury the reason.
    pub async fn enrol(&self) -> Result<String> {
        let registration = tasks_cli::register_chat_session(&self.endpoint, &self.label, self.pid).await?;
        let session_ref = registration.session_ref.clone();
        *self.enrolment.lock() = Some(Enrolment {
            session_id: registration.session_id,
            session_ref: registration.session_ref,
            token: registration.token,
        });
        Ok(session_ref)
    }

    /// Whether this chat is currently enrolled.
    #[must_use]
    pub fn is_enrolled(&self) -> bool {
        self.enrolment.lock().is_some()
    }

    /// The daemon-assigned session id, once enrolled.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.enrolment.lock().as_ref().map(|e| e.session_id.clone())
    }

    fn credentials(&self) -> Result<(String, String)> {
        let held = self.enrolment.lock().clone();
        let enrolment = held.ok_or_else(|| anyhow::anyhow!("this chat session is not enrolled with a daemon"))?;
        Ok((enrolment.session_id, enrolment.token))
    }

    /// Withdraw from the daemon's registry.
    ///
    /// Nothing on the daemon side removes a session on a clock — a session that
    /// stopped pulling is reported as silent, never evicted — so an exit that
    /// skips this leaves a row that assignments can still be addressed to, with
    /// nobody left to run them.
    pub async fn deregister(&self) -> Result<usize> {
        let (session_id, _) = self.credentials()?;
        let report = tasks_cli::deregister_chat_session(&self.endpoint, &session_id).await?;
        *self.enrolment.lock() = None;
        Ok(report.discarded)
    }
}

impl AssignmentMailbox for AssignmentClient {
    fn session_ref(&self) -> String {
        self.enrolment
            .lock()
            .as_ref()
            .map_or_else(|| "unenrolled".to_string(), |e| e.session_ref.clone())
    }

    fn base_url(&self) -> String {
        self.endpoint.base_url().to_string()
    }

    fn pull(&self, ack: Vec<String>) -> MailboxFuture<'_, AssignmentPull> {
        Box::pin(async move {
            let (session_id, token) = self.credentials()?;
            tasks_cli::pull_chat_assignments(&self.endpoint, &session_id, &token, &ack, Some(PULL_BATCH)).await
        })
    }

    fn report(
        &self,
        assignment_id: String,
        status: ResultStatus,
        summary: String,
    ) -> MailboxFuture<'_, AssignmentReceipt> {
        Box::pin(async move {
            let (session_id, token) = self.credentials()?;
            tasks_cli::report_chat_assignment(
                &self.endpoint,
                &session_id,
                &token,
                &assignment_id,
                status.as_str(),
                &summary,
            )
            .await
        })
    }

    fn accept(&self, assignment_id: &str) -> bool {
        self.taken.lock().insert(assignment_id.to_string())
    }
}

// ── Whether to enrol at all ─────────────────────────────────────────────────

/// Whether this configuration means chat to enrol with a daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enrolable {
    /// A daemon was named, or a credential for one exists: enrol.
    Yes,
    /// Nothing points this chat at a daemon. The reason is carried so it can be
    /// shown to anyone who asks why they cannot be assigned work.
    No(&'static str),
}

/// Decide whether to enrol, from configuration alone.
///
/// Enrolment is opt-in by evidence rather than by a flag, because there is no
/// safe default guess: the fallback daemon address is `[gateway]`'s own bind,
/// which every chat has whether or not a daemon is listening on it, and
/// `gateway.require_pairing` is on by default, so an unconfigured chat would
/// announce a 401 on every single start. Two things count as evidence that an
/// operator meant chat to talk to a daemon:
///
/// 1. `chat.daemon.url` is set — chat has been pointed at one explicitly;
/// 2. a credential resolves — `chat.daemon.token`, or a plaintext entry in
///    `gateway.paired_tokens` when chat and the daemon share a config dir.
///
/// Neither of those is a promise that enrolment will succeed. It is only the
/// difference between a failure worth reporting and a question nobody asked.
#[must_use]
pub fn enrolable(config: &Config) -> Enrolable {
    if !config.chat.daemon.url.trim().is_empty() {
        return Enrolable::Yes;
    }
    if super::daemon::operator_token(config).is_some() {
        return Enrolable::Yes;
    }
    Enrolable::No(
        "no daemon is configured for this chat: set `chat.daemon.url` to the daemon's gateway, or \
         `chat.daemon.token` to a token it accepts",
    )
}

/// The label this chat registers under.
///
/// The workspace directory name: stable across restarts, which is what an
/// `assign_allow` rule needs, and already how an operator thinks about which
/// chat is which. The daemon treats it as self-declared and does not verify it.
fn label_for(config: &Config) -> String {
    let raw = config
        .workspace_dir
        .file_name()
        .map(|name| name.to_string_lossy().trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "chat".to_string());
    truncate_bytes(&raw, MAX_LABEL_BYTES)
}

/// Cut a string to at most `limit` bytes, on a character boundary.
fn truncate_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_string()
}

// ── Carrying an assignment through the ordinary input queue ─────────────────

/// One assignment recognised on an input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    pub assignment_id: String,
    pub disposition: Disposition,
}

/// Build the input message that carries one assignment into the chat queue.
///
/// The content is the task verbatim for [`Disposition::Queue`], and prefixed
/// with `/now ` for the two that jump the queue — the same prefix a person
/// types to put their own input in front of the backlog, so priority ordering
/// stays one mechanism with one set of tests rather than two that can drift.
/// The prefix is consumed by `classify_input_priority`, so the model sees the
/// task and nothing else.
#[must_use]
pub fn assignment_message(
    assignment: &PulledAssignment,
    disposition: Disposition,
) -> crate::channels::traits::ChannelMessage {
    let content = if disposition.jumps_the_queue() {
        format!("/now {}", assignment.task)
    } else {
        assignment.task.clone()
    };
    crate::channels::traits::ChannelMessage {
        id: uuid::Uuid::new_v4().to_string(),
        sender: format!("{SENDER_PREFIX}{}:{}", disposition.as_str(), assignment.assignment_id),
        reply_target: ASSIGNMENT_REPLY_TARGET.to_string(),
        content,
        channel: ASSIGNMENT_CHANNEL.to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}

/// Recognise an assignment on an input message, or `None` for ordinary input.
#[must_use]
pub fn delivered(msg: &crate::channels::traits::ChannelMessage) -> Option<Delivered> {
    let rest = msg.sender.strip_prefix(SENDER_PREFIX)?;
    let (disposition, assignment_id) = rest.split_once(':')?;
    if assignment_id.is_empty() {
        return None;
    }
    Some(Delivered {
        assignment_id: assignment_id.to_string(),
        disposition: Disposition::from_wire(disposition),
    })
}

// ── What the operator sees ──────────────────────────────────────────────────

/// The line shown when an assignment arrives.
///
/// Someone other than the person at the keyboard just put work into this
/// terminal. Where it came from, how it was dispatched, and enough of the task
/// to recognise it all have to be on screen; the origin is already a
/// fingerprint by the time it reaches here, and is passed through unchanged.
#[must_use]
pub fn arrival_notice(assignment: &PulledAssignment, disposition: Disposition) -> String {
    let origin = origin_label(assignment);
    let preview = preview_of(&assignment.task);
    let redelivery = if assignment.deliveries > 1 {
        format!(" (redelivery #{})", assignment.deliveries)
    } else {
        String::new()
    };
    let dispatched = match disposition {
        Disposition::Queue => "queued behind whatever this chat is already doing",
        Disposition::Steer => "put in front of the queue; the turn in flight keeps running",
        Disposition::Interrupt => "the turn in flight is being ended first",
    };
    format!(
        "Assigned by {origin}{redelivery}: {preview}\n  Disposition {} — {dispatched}.",
        disposition.as_str()
    )
}

/// The line shown when a disposition this chat does not know arrives.
#[must_use]
pub fn unknown_disposition_notice(raw: &str) -> String {
    format!(
        "Daemon assignment carried disposition {raw:?}, which this chat does not know; running it as \
         `queue`. A word this chat cannot read is never treated as permission to end a turn."
    )
}

/// The line shown when a result was handed back to the daemon.
#[must_use]
pub fn reported_notice(assignment_id: &str, status: ResultStatus, seq: u64) -> String {
    format!(
        "Reported {} for daemon assignment {} (result #{seq}).",
        status.as_str(),
        short_id(assignment_id)
    )
}

/// The line shown when a result could not be handed back.
#[must_use]
pub fn report_failure_notice(assignment_id: &str, status: ResultStatus, error: &anyhow::Error) -> String {
    format!(
        "Could not report {} for daemon assignment {}: {error:#}. The daemon still has it outstanding; \
         whoever assigned it is waiting.",
        status.as_str(),
        short_id(assignment_id)
    )
}

/// The line shown when this chat could not enrol.
#[must_use]
pub fn enrolment_failure_notice(base_url: &str, error: &anyhow::Error) -> String {
    format!(
        "This chat could not enrol with the daemon at {base_url}, so it cannot be assigned work: {error:#}\n\
         Everything else in this chat is unaffected. Restart chat once the daemon accepts it to become \
         assignable again."
    )
}

/// The line shown once this chat is assignable.
#[must_use]
pub fn enrolled_notice(label: &str, session_ref: &str, base_url: &str) -> String {
    format!("This chat is assignable at {base_url} as {label} ({session_ref}).")
}

fn origin_label(assignment: &PulledAssignment) -> String {
    let channel = if assignment.origin_channel.trim().is_empty() {
        "unknown"
    } else {
        assignment.origin_channel.trim()
    };
    assignment
        .origin_ref
        .as_deref()
        .map(str::trim)
        .filter(|origin_ref| !origin_ref.is_empty())
        .map_or_else(|| channel.to_string(), |origin_ref| format!("{channel} {origin_ref}"))
}

fn preview_of(task: &str) -> String {
    let first_line = task.lines().next().unwrap_or("").trim();
    if first_line.chars().count() <= TASK_PREVIEW_CHARS {
        return first_line.to_string();
    }
    let head: String = first_line.chars().take(TASK_PREVIEW_CHARS).collect();
    format!("{head}…")
}

fn short_id(assignment_id: &str) -> String {
    let head: String = assignment_id.chars().take(8).collect();
    if head.is_empty() { "?".to_string() } else { head }
}

/// Fit a summary into what the daemon accepts: never empty, never oversized.
///
/// The daemon refuses both, and a refusal here would lose the only account of
/// what happened. Truncation says so in the text rather than silently handing
/// back a sentence that stops mid-word.
#[must_use]
pub fn bound_summary(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "(this chat produced no text for the assignment)".to_string();
    }
    if trimmed.len() <= MAX_SUMMARY_BYTES {
        return trimmed.to_string();
    }
    const MARKER: &str = "\n… (truncated by chat to fit the result field)";
    truncate_bytes(trimmed, MAX_SUMMARY_BYTES.saturating_sub(MARKER.len())) + MARKER
}

// ── Reporting one assignment's turn ─────────────────────────────────────────

/// Default account when a turn ends without a reply to report.
const NO_REPLY_SUMMARY: &str = "This chat ended the turn for the assignment without producing a reply. \
                               The input may have been handled as a local chat command, or the turn was \
                               ended before it answered.";

/// Reports one assignment's outcome when the turn that ran it ends, whichever
/// way it ends.
///
/// A guard rather than a call at the end of the turn because the turn body has
/// many exits: a slash command short-circuits it, a cancellation unwinds it, a
/// quit breaks out of the loop entirely. Anything that only reported on the
/// straight-line path would leave the assigner waiting forever on exactly the
/// cases they most need to hear about. Dropping is the one thing every exit has
/// in common.
///
/// The one exit that must *not* report is a requeue: the message goes back into
/// the backlog and will run later, so [`AssignmentTurnGuard::disarm`] is called
/// there and the next attempt takes over.
pub struct AssignmentTurnGuard {
    mailbox: Arc<dyn AssignmentMailbox>,
    in_flight: InFlightRegistry,
    notify: Arc<dyn Fn(&str) + Send + Sync>,
    assignment_id: String,
    outcome: Option<(ResultStatus, String)>,
    armed: bool,
}

impl AssignmentTurnGuard {
    /// Arm a report for one assignment.
    #[must_use]
    pub fn new(
        mailbox: Arc<dyn AssignmentMailbox>,
        in_flight: InFlightRegistry,
        notify: Arc<dyn Fn(&str) + Send + Sync>,
        assignment_id: String,
    ) -> Self {
        Self {
            mailbox,
            in_flight,
            notify,
            assignment_id,
            outcome: None,
            armed: true,
        }
    }

    /// Record the reply this turn produced.
    pub fn completed(&mut self, reply: &str) {
        self.outcome = Some((ResultStatus::Completed, bound_summary(reply)));
    }

    /// Record that the turn ran and failed, with the reason.
    pub fn failed(&mut self, reason: &str) {
        self.outcome = Some((ResultStatus::Failed, bound_summary(reason)));
    }

    /// Give up the report because this assignment has not actually run yet.
    ///
    /// Used where the input is put back on the backlog: reporting there would
    /// answer for a turn that is still going to happen, and the daemon would
    /// then refuse the real report as unknown.
    pub const fn disarm(&mut self) {
        self.armed = false;
    }

    /// Whether this guard will still report when dropped.
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        self.armed
    }

    /// The status and summary this guard would report right now.
    #[must_use]
    pub fn pending_outcome(&self) -> (ResultStatus, String) {
        self.outcome
            .clone()
            .unwrap_or_else(|| (ResultStatus::Failed, NO_REPLY_SUMMARY.to_string()))
    }
}

impl Drop for AssignmentTurnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (status, summary) = self.pending_outcome();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // Outside a runtime there is nothing to spawn onto. Say so where it
            // can be found rather than pretending the report went out.
            tracing::warn!(
                assignment = %self.assignment_id,
                status = status.as_str(),
                "no tokio runtime to report a daemon assignment result on"
            );
            return;
        };
        let mailbox = Arc::clone(&self.mailbox);
        let notify = Arc::clone(&self.notify);
        let assignment_id = self.assignment_id.clone();
        // The report is a request to the daemon like any other, so it is listed
        // and endable while it runs: a daemon that accepts the connection and
        // never answers must not leave an invisible task behind.
        let cancel = CancellationToken::new();
        let guard = register_in_flight_labeled(
            &self.in_flight,
            &format!("assignment result {}", short_id(&assignment_id)),
            &mailbox.base_url(),
            cancel.clone(),
        );
        handle.spawn(async move {
            let _row = guard;
            let outcome = tokio::select! {
                () = cancel.cancelled() => None,
                outcome = mailbox.report(assignment_id.clone(), status, summary) => Some(outcome),
            };
            match outcome {
                Some(Ok(receipt)) => notify(&reported_notice(&assignment_id, status, receipt.seq)),
                Some(Err(error)) => notify(&report_failure_notice(&assignment_id, status, &error)),
                None => notify(&format!(
                    "Stopped waiting on the daemon's acknowledgement for assignment {}. The result may or \
                     may not have landed.",
                    short_id(&assignment_id)
                )),
            }
        });
    }
}

// ── How often a failing pull is allowed to say so ───────────────────────────

/// How many distinct failure reasons this policy will ever announce.
///
/// The reason text comes from the daemon, so a peer whose message varies (an
/// id, a counter, a clock) could otherwise grow this set without bound. Past
/// the cap nothing new is announced and nothing new is remembered; the log
/// still has every occurrence.
const MAX_TRACKED_REASONS: usize = 16;

/// One unbroken run of failing pulls, all with the same reason.
#[derive(Debug, Clone)]
struct FailingEpisode {
    reason: String,
    /// Failing pulls in this episode, announced or not — the number the
    /// recovery line quotes.
    pulls: u64,
    /// Whether this episode's reason reached the screen. A recovery is only
    /// announced for an episode that was, or the operator is told something
    /// healed that they were never told had broken.
    announced: bool,
}

/// Decides what a failing — and a recovering — pull is allowed to say.
///
/// # Why the obvious version was not enough
///
/// The first version kept the *last* failure text, skipped a repeat of it, and
/// cleared it on any successful pull. That holds only while failure is
/// sustained. The failure it was written against was not: the daemon rate
/// limited the mailbox, so pulls alternated success, refusal, success, refusal
/// — and every success cleared the key, so every refusal was "new" and printed
/// again. The dedup was real and the flood was real at the same time.
///
/// So the key here is the reason itself and it is kept **across** recoveries:
/// one distinct reason is stated once for the life of one poller, and its
/// recovery once. Alternating failures collapse onto that one statement instead
/// of resetting it, which is the property the flood needed and the previous
/// version did not have. The ceiling is total, not per-interval: no failure
/// mode, however it recurs, can cost more than two lines per distinct reason.
///
/// What that trades away is being told a second time that a reason that has
/// already been announced and recovered is back. That is the deliberate side to
/// err on: a stale silence costs an operator one `/sessions --daemon`, and a
/// screen full of the same sentence costs them the chat they were using.
#[derive(Debug, Default)]
pub struct FailureNotices {
    /// Reasons already put on screen. Kept across recoveries on purpose.
    announced: std::collections::HashSet<String>,
    /// Reasons whose recovery has already been put on screen.
    recovered: std::collections::HashSet<String>,
    episode: Option<FailingEpisode>,
}

impl FailureNotices {
    /// Record a failing pull, returning the line to show — or `None` when this
    /// reason has already said everything it is allowed to say.
    pub fn failed(&mut self, reason: &str) -> Option<String> {
        if self.episode.as_ref().is_none_or(|episode| episode.reason != reason) {
            self.episode = Some(FailingEpisode {
                reason: reason.to_string(),
                pulls: 0,
                announced: false,
            });
        }
        let episode = self.episode.as_mut()?;
        episode.pulls = episode.pulls.saturating_add(1);
        if self.announced.contains(reason) || self.announced.len() >= MAX_TRACKED_REASONS {
            tracing::debug!(reason, pulls = episode.pulls, "chat assignment pull is still failing");
            return None;
        }
        self.announced.insert(reason.to_string());
        episode.announced = true;
        Some(pull_failure_notice(reason))
    }

    /// Record a successful pull, returning the line to show when it ends an
    /// episode the operator was told about.
    pub fn recovered(&mut self) -> Option<String> {
        let episode = self.episode.take()?;
        if !episode.announced || !self.recovered.insert(episode.reason.clone()) {
            return None;
        }
        Some(pull_recovery_notice(&episode.reason, episode.pulls))
    }

    /// Whether a failing episode is currently open.
    #[must_use]
    pub const fn is_failing(&self) -> bool {
        self.episode.is_some()
    }
}

/// The line shown when this chat's mailbox cannot be reached.
#[must_use]
pub fn pull_failure_notice(reason: &str) -> String {
    format!(
        "Daemon assignments are not being received: {reason}\nThis chat keeps asking, and nothing else \
         is affected. This reason will not be repeated here; further occurrences of it go to the log."
    )
}

/// The line shown when the mailbox answers again after a run of failures.
#[must_use]
pub fn pull_recovery_notice(reason: &str, failed_pulls: u64) -> String {
    let cause = reason.lines().next().unwrap_or(reason).trim();
    let pulls = if failed_pulls == 1 { "pull" } else { "pulls" };
    format!(
        "Daemon assignments are being received again after {failed_pulls} failed {pulls} ({cause}). \
         This chat can be assigned work again."
    )
}

// ── The poll loop ───────────────────────────────────────────────────────────

/// Where one pulled batch went, so the loop's ordering can be asserted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BatchOutcome {
    /// Ids handed to the local input queue by this batch, in order.
    pub delivered: Vec<String>,
    /// Ids already held from an earlier delivery, taken again and not re-run.
    pub duplicates: Vec<String>,
    /// Ids to acknowledge on the *next* pull.
    pub acks: Vec<String>,
    /// True when the input queue is gone and the loop should stop.
    pub input_closed: bool,
}

/// Hand one pulled batch to the chat input queue.
///
/// The ordering here is the whole point, and it is the reverse of the obvious
/// one: an id joins `acks` only **after** the message it carries has been
/// accepted by the local queue. Acknowledging first would tell the daemon the
/// work is safe at the moment it is least safe — still in flight, owned by
/// nobody — and a process that died there would take it with it. Paying for
/// that with an occasional redelivery is the trade the mailbox is designed
/// around, and `taken` is what makes a redelivery cost nothing.
///
/// MUTATION GUARD: move the `acks.push` above the `send` and
/// `a_batch_is_acknowledged_only_after_it_reaches_the_local_queue` fails.
pub async fn take_batch(
    mailbox: &dyn AssignmentMailbox,
    batch: Vec<PulledAssignment>,
    input: &tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
    notify: &(dyn Fn(&str) + Send + Sync),
) -> BatchOutcome {
    let mut outcome = BatchOutcome::default();
    for assignment in batch {
        let id = assignment.assignment_id.clone();
        if !mailbox.accept(&id) {
            // A redelivery of work already queued here. Acknowledge it so the
            // daemon stops resending, and do not run it a second time.
            outcome.duplicates.push(id.clone());
            outcome.acks.push(id);
            continue;
        }
        if !Disposition::is_known_wire(&assignment.disposition) {
            notify(&unknown_disposition_notice(&assignment.disposition));
        }
        let disposition = Disposition::from_wire(&assignment.disposition);
        let message = assignment_message(&assignment, disposition);
        // Announced before it is handed over, not after: the turn can start,
        // finish and print its answer before this task is scheduled again, and
        // an announcement that lands after the answer explains nothing.
        notify(&arrival_notice(&assignment, disposition));
        if input.send(message).await.is_err() {
            // The chat loop is gone. The id is deliberately *not* acknowledged:
            // nothing here will ever run it, so the daemon should keep it.
            outcome.input_closed = true;
            return outcome;
        }
        outcome.delivered.push(id.clone());
        outcome.acks.push(id);
    }
    outcome
}

/// Enrol with the daemon, then pull this chat's mailbox until it is ended.
///
/// Enrolment runs **here**, inside the spawned task, rather than on the startup
/// path: reaching a daemon can take as long as a TCP connect takes, and a chat
/// that will not show a prompt until some other process answers has made a
/// convenience into a dependency. A refusal costs exactly one capability, is
/// stated once, and everything else about this chat is unchanged.
///
/// The enrolment call is itself listed and endable while it runs, for the same
/// reason every other daemon call in this process is.
pub async fn enrol_and_poll(
    client: Arc<AssignmentClient>,
    in_flight: InFlightRegistry,
    input: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
    notify: Arc<dyn Fn(&str) + Send + Sync>,
    cancel: CancellationToken,
) {
    let base_url = client.base_url();
    let enrolling = register_in_flight_labeled(&in_flight, "assignment enrolment", &base_url, cancel.clone());
    let enrolled = tokio::select! {
        () = cancel.cancelled() => return,
        result = client.enrol() => result,
    };
    drop(enrolling);
    match enrolled {
        Ok(session_ref) => notify(&enrolled_notice(client.label(), &session_ref, &base_url)),
        Err(error) => {
            notify(&enrolment_failure_notice(&base_url, &error));
            return;
        }
    }
    run_poller(client, in_flight, input, notify, cancel).await;
}

/// Pull this chat's mailbox until it is cancelled or the chat loop ends.
///
/// Registers itself in the chat's own in-flight list first, so `/sessions`
/// shows it and `/kill --daemon d<N>` ends it — the same answer to a stuck
/// daemon that every other request in this process gets, and the reason none of
/// this needs a clock.
pub async fn run_poller(
    mailbox: Arc<dyn AssignmentMailbox>,
    in_flight: InFlightRegistry,
    input: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
    notify: Arc<dyn Fn(&str) + Send + Sync>,
    cancel: CancellationToken,
) {
    let session_ref = mailbox.session_ref();
    let row: InFlightGuard = register_in_flight_labeled(
        &in_flight,
        &format!("assignment poll {session_ref}"),
        &mailbox.base_url(),
        cancel.clone(),
    );
    let _row = row;
    let mut pending_acks: Vec<String> = Vec::new();
    let mut notices = FailureNotices::default();
    loop {
        let pull = tokio::select! {
            () = cancel.cancelled() => break,
            pull = mailbox.pull(pending_acks.clone()) => pull,
        };
        match pull {
            Ok(batch) => {
                // The acknowledgements rode along with this request and the
                // daemon answered, so they are spent. Anything new takes their
                // place only once it is in the local queue.
                pending_acks.clear();
                if let Some(line) = notices.recovered() {
                    notify(&line);
                }
                let outcome = take_batch(mailbox.as_ref(), batch.assignments, &input, notify.as_ref()).await;
                pending_acks = outcome.acks;
                if outcome.input_closed {
                    break;
                }
            }
            Err(error) => {
                // What is allowed to reach the screen is [`FailureNotices`]'s
                // decision, not this loop's: a reason that recurs — including
                // one that alternates with success — must cost a bounded number
                // of lines, or the failure buries the chat somebody is using.
                if let Some(line) = notices.failed(&format!("{error:#}")) {
                    notify(&line);
                }
            }
        }
        tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
    // Nothing is announced on the way out. A poller ends for exactly two
    // reasons: an operator killed it, and `cancel_in_flight` already told them
    // so by name; or the chat loop is gone, and there is no longer anywhere for
    // a notice to be read.
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn assignment(id: &str, task: &str, disposition: &str) -> PulledAssignment {
        PulledAssignment {
            assignment_id: id.to_string(),
            session_id: "session".to_string(),
            task: task.to_string(),
            disposition: disposition.to_string(),
            deliveries: 1,
            created_at_unix_ms: 0,
            work_id: "w1".to_string(),
            origin_channel: "wacli".to_string(),
            origin_ref: Some("3d1a4f5b".to_string()),
        }
    }

    fn silent() -> Arc<dyn Fn(&str) + Send + Sync> {
        Arc::new(|_: &str| {})
    }

    fn client_for_tests() -> Arc<AssignmentClient> {
        let config = Config::default();
        AssignmentClient::new(&config)
    }

    /// One scripted answer to a pull.
    enum Answer {
        /// Hand these assignments over.
        Batch(Vec<PulledAssignment>),
        /// Refuse with this reason.
        Refuse(&'static str),
    }

    /// Records every pull's `ack` array and every report, and answers from a
    /// script. Enough to assert the loop's ordering without a live daemon.
    struct RecordingMailbox {
        base_url: String,
        pulls: Mutex<Vec<Vec<String>>>,
        reports: Mutex<Vec<(String, ResultStatus, String)>>,
        script: Mutex<Vec<Answer>>,
        taken: Mutex<HashSet<String>>,
        seq: AtomicUsize,
    }

    impl RecordingMailbox {
        fn new(script: Vec<Vec<PulledAssignment>>) -> Arc<Self> {
            Self::scripted(script.into_iter().map(Answer::Batch).collect())
        }

        fn scripted(script: Vec<Answer>) -> Arc<Self> {
            Arc::new(Self {
                base_url: "http://127.0.0.1:9".to_string(),
                pulls: Mutex::new(Vec::new()),
                reports: Mutex::new(Vec::new()),
                script: Mutex::new(script),
                taken: Mutex::new(HashSet::new()),
                seq: AtomicUsize::new(0),
            })
        }
    }

    impl AssignmentMailbox for RecordingMailbox {
        fn session_ref(&self) -> String {
            "9b2e0f7c".to_string()
        }

        fn base_url(&self) -> String {
            self.base_url.clone()
        }

        fn pull(&self, ack: Vec<String>) -> MailboxFuture<'_, AssignmentPull> {
            self.pulls.lock().push(ack);
            let answer = {
                let mut script = self.script.lock();
                if script.is_empty() {
                    Answer::Batch(Vec::new())
                } else {
                    script.remove(0)
                }
            };
            Box::pin(async move {
                match answer {
                    Answer::Batch(assignments) => Ok(AssignmentPull {
                        assignments,
                        acked: 0,
                        requeued: 0,
                        queued_remaining: 0,
                    }),
                    Answer::Refuse(reason) => Err(anyhow::anyhow!("{reason}")),
                }
            })
        }

        fn report(
            &self,
            assignment_id: String,
            status: ResultStatus,
            summary: String,
        ) -> MailboxFuture<'_, AssignmentReceipt> {
            self.reports.lock().push((assignment_id.clone(), status, summary));
            let seq = self.seq.fetch_add(1, Ordering::SeqCst) as u64 + 1;
            Box::pin(async move {
                Ok(AssignmentReceipt {
                    assignment_id,
                    status: status.as_str().to_string(),
                    seq,
                })
            })
        }

        fn accept(&self, assignment_id: &str) -> bool {
            self.taken.lock().insert(assignment_id.to_string())
        }
    }

    #[tokio::test]
    async fn a_batch_is_acknowledged_only_after_it_reaches_the_local_queue() {
        let client = client_for_tests();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let outcome = take_batch(
            client.as_ref(),
            vec![assignment("a-1", "summarise the repo", "queue")],
            &tx,
            silent().as_ref(),
        )
        .await;
        assert_eq!(outcome.delivered, vec!["a-1".to_string()]);
        assert_eq!(
            outcome.acks,
            vec!["a-1".to_string()],
            "an assignment safely in the local queue is acknowledged on the next pull"
        );
        assert!(rx.try_recv().is_ok(), "the assignment must reach the chat input queue");

        // The same call with nowhere to deliver must acknowledge nothing: work
        // this chat cannot run has to stay the daemon's.
        let client2 = client_for_tests();
        let (closed_tx, closed_rx) = tokio::sync::mpsc::channel(1);
        drop(closed_rx);
        let refused = take_batch(
            client2.as_ref(),
            vec![assignment("a-2", "do a thing", "queue")],
            &closed_tx,
            silent().as_ref(),
        )
        .await;
        assert!(refused.input_closed);
        assert!(
            refused.acks.is_empty(),
            "work that never reached the local queue must not be acknowledged: {:?}",
            refused.acks
        );
    }

    #[tokio::test]
    async fn a_redelivered_assignment_is_acknowledged_but_never_run_twice() {
        let client = client_for_tests();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let first = take_batch(
            client.as_ref(),
            vec![assignment("dup", "work", "queue")],
            &tx,
            silent().as_ref(),
        )
        .await;
        assert_eq!(first.delivered, vec!["dup".to_string()]);

        let mut again = assignment("dup", "work", "queue");
        again.deliveries = 2;
        let second = take_batch(client.as_ref(), vec![again], &tx, silent().as_ref()).await;
        assert!(
            second.delivered.is_empty(),
            "a redelivery of held work must not be queued again: {:?}",
            second.delivered
        );
        assert_eq!(second.duplicates, vec!["dup".to_string()]);
        assert_eq!(
            second.acks,
            vec!["dup".to_string()],
            "a redelivery must still be acknowledged, or the daemon keeps resending it"
        );
        assert!(rx.try_recv().is_ok());
        assert!(
            rx.try_recv().is_err(),
            "exactly one input message may exist for one assignment id"
        );
    }

    #[tokio::test]
    async fn the_first_pull_acknowledges_nothing_and_the_second_acknowledges_the_first_batch() {
        let mailbox = RecordingMailbox::new(vec![vec![assignment("a-1", "one", "queue")], vec![]]);
        let client = client_for_tests();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);

        // Drive the same two steps the loop drives, without its sleep.
        let mut pending: Vec<String> = Vec::new();
        for _ in 0..2u8 {
            let batch = mailbox.pull(pending.clone()).await.unwrap();
            pending.clear();
            let outcome = take_batch(client.as_ref(), batch.assignments, &tx, silent().as_ref()).await;
            pending = outcome.acks;
        }
        let pulls = mailbox.pulls.lock().clone();
        assert_eq!(pulls.len(), 2);
        assert!(
            pulls[0].is_empty(),
            "the first pull cannot acknowledge anything it has not received yet: {:?}",
            pulls[0]
        );
        assert_eq!(
            pulls[1],
            vec!["a-1".to_string()],
            "the batch taken by the first pull is acknowledged by the second"
        );
    }

    #[tokio::test]
    async fn a_dropped_turn_guard_reports_a_failure_rather_than_going_quiet() {
        let mailbox = RecordingMailbox::new(vec![]);
        let registry = super::super::daemon::new_in_flight_registry();
        {
            let _guard = AssignmentTurnGuard::new(
                Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
                Arc::clone(&registry),
                silent(),
                "a-1".to_string(),
            );
        }
        tokio::task::yield_now().await;
        let reports = mailbox.reports.lock().clone();
        assert_eq!(
            reports.len(),
            1,
            "a turn that ended without a reply still owes an answer"
        );
        assert_eq!(reports[0].0, "a-1");
        assert_eq!(reports[0].1, ResultStatus::Failed);
        assert!(!reports[0].2.trim().is_empty());
    }

    #[tokio::test]
    async fn a_completed_turn_reports_its_reply() {
        let mailbox = RecordingMailbox::new(vec![]);
        let registry = super::super::daemon::new_in_flight_registry();
        {
            let mut guard = AssignmentTurnGuard::new(
                Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
                Arc::clone(&registry),
                silent(),
                "a-2".to_string(),
            );
            guard.completed("37 TODOs");
        }
        tokio::task::yield_now().await;
        let reports = mailbox.reports.lock().clone();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].1, ResultStatus::Completed);
        assert_eq!(reports[0].2, "37 TODOs");
    }

    #[tokio::test]
    async fn a_disarmed_guard_reports_nothing_because_the_work_still_has_to_run() {
        let mailbox = RecordingMailbox::new(vec![]);
        let registry = super::super::daemon::new_in_flight_registry();
        {
            let mut guard = AssignmentTurnGuard::new(
                Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
                Arc::clone(&registry),
                silent(),
                "a-3".to_string(),
            );
            guard.disarm();
            assert!(!guard.is_armed());
        }
        tokio::task::yield_now().await;
        assert!(
            mailbox.reports.lock().is_empty(),
            "a requeued assignment must not be answered for before it runs"
        );
    }

    /// Collects every line the poller put on screen.
    fn recording_notifier() -> (Arc<dyn Fn(&str) + Send + Sync>, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let notify: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |text: &str| sink.lock().push(text.to_string()));
        (notify, lines)
    }

    /// Lines the poller printed to say the mailbox is failing, for a reason
    /// containing `marker`. The recovery line quotes the reason it is about, so
    /// counting occurrences of the reason alone would count that too.
    fn complaints_about(said: &[String], marker: &str) -> usize {
        said.iter()
            .filter(|line| line.starts_with("Daemon assignments are not being received") && line.contains(marker))
            .count()
    }

    /// Drive the poller over a script and hand back everything it said.
    ///
    /// `start_paused` because the loop sleeps a poll interval between pulls: the
    /// interval is a sampling rate, and simulating it is what keeps a test about
    /// twenty pulls from taking thirty seconds.
    async fn lines_from_script(script: Vec<Answer>) -> Vec<String> {
        let pulls_scripted = script.len();
        let mailbox = RecordingMailbox::scripted(script);
        let (notify, lines) = recording_notifier();
        let registry = super::super::daemon::new_in_flight_registry();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poller(
            Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
            registry,
            tx,
            notify,
            cancel.clone(),
        ));
        for _ in 0..(pulls_scripted * 4 + 8) {
            if mailbox.pulls.lock().len() > pulls_scripted {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        cancel.cancel();
        handle.await.expect("the poller must end when it is cancelled");
        assert!(
            mailbox.pulls.lock().len() > pulls_scripted,
            "the script must have been driven to the end"
        );
        let said = lines.lock().clone();
        said
    }

    const RATE_LIMITED: &str = "chat assignment pull failed with HTTP 429 Too Many Requests: \
                                Too many API requests. Please retry later.";
    const UNREACHABLE: &str = "cannot reach a running PRX process at http://127.0.0.1:9";

    /// The flood this was written against: a mailbox that refuses every other
    /// pull. The reason is identical every time, so it is worth exactly one
    /// line however many times it comes back.
    ///
    /// MUTATION GUARD: clear `FailureNotices::announced` on a successful pull —
    /// the shape of the dedup this replaced — and this fails with one line per
    /// refusal.
    #[tokio::test(start_paused = true)]
    async fn one_failure_reason_is_stated_once_however_often_it_recurs() {
        let said = lines_from_script(vec![
            Answer::Refuse(RATE_LIMITED),
            Answer::Batch(vec![]),
            Answer::Refuse(RATE_LIMITED),
            Answer::Batch(vec![]),
            Answer::Refuse(RATE_LIMITED),
            Answer::Refuse(RATE_LIMITED),
            Answer::Batch(vec![]),
            Answer::Refuse(RATE_LIMITED),
        ])
        .await;
        assert_eq!(
            complaints_about(&said, "429"),
            1,
            "one reason is worth one line, not one per failing pull; said:\n{said:#?}"
        );
        let recoveries = said.iter().filter(|line| line.contains("being received again")).count();
        assert_eq!(
            recoveries, 1,
            "recovery is announced, and it is announced once; said:\n{said:#?}"
        );
    }

    /// Going quiet about a failure is the other half of the bug: the operator
    /// has to be told, once, and told again when it clears.
    #[tokio::test(start_paused = true)]
    async fn a_failure_and_its_recovery_are_both_stated() {
        let said = lines_from_script(vec![
            Answer::Refuse(UNREACHABLE),
            Answer::Refuse(UNREACHABLE),
            Answer::Batch(vec![]),
        ])
        .await;
        assert!(
            said.iter().any(|line| line.contains("are not being received")),
            "a mailbox that cannot be reached must say so; said:\n{said:#?}"
        );
        let recovery = said
            .iter()
            .find(|line| line.contains("being received again"))
            .expect("recovery must be announced, or nobody knows when it healed");
        assert!(
            recovery.contains("2 failed pulls"),
            "the recovery line says how long it was broken: {recovery}"
        );
    }

    /// A different reason is a different fact, and is stated on its own.
    #[tokio::test(start_paused = true)]
    async fn a_changed_reason_is_stated_even_while_the_mailbox_is_still_failing() {
        let said = lines_from_script(vec![
            Answer::Refuse(RATE_LIMITED),
            Answer::Refuse(RATE_LIMITED),
            Answer::Refuse(UNREACHABLE),
            Answer::Refuse(UNREACHABLE),
        ])
        .await;
        assert_eq!(complaints_about(&said, "429"), 1, "said:\n{said:#?}");
        assert_eq!(
            complaints_about(&said, "cannot reach"),
            1,
            "a reason the operator has not seen must not be swallowed by the previous one; said:\n{said:#?}"
        );
    }

    #[test]
    fn a_recovery_is_never_announced_for_a_failure_that_was_never_announced() {
        let mut notices = FailureNotices::default();
        assert!(notices.recovered().is_none(), "nothing has failed yet");
        assert!(notices.failed("reason a").is_some());
        assert!(notices.failed("reason a").is_none());
        assert!(notices.recovered().is_some());
        // Second episode of the same reason: silent, so its recovery is silent
        // too. Announcing "it works again" for something never reported broken
        // is noise about a fact nobody was missing.
        assert!(notices.failed("reason a").is_none());
        assert!(notices.recovered().is_none());
        assert!(!notices.is_failing());
    }

    #[test]
    fn a_peer_whose_wording_keeps_changing_cannot_grow_the_policy_without_bound() {
        let mut notices = FailureNotices::default();
        let mut announced = 0;
        for attempt in 0..(MAX_TRACKED_REASONS * 4) {
            if notices.failed(&format!("refused, attempt {attempt}")).is_some() {
                announced += 1;
            }
        }
        assert_eq!(
            announced, MAX_TRACKED_REASONS,
            "a varying message must not buy unlimited screen space"
        );
    }

    #[tokio::test]
    async fn the_poller_is_listed_while_it_runs_and_a_kill_ends_it() {
        let registry = super::super::daemon::new_in_flight_registry();
        let client = client_for_tests();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poller(
            Arc::clone(&client) as Arc<dyn AssignmentMailbox>,
            Arc::clone(&registry),
            tx,
            silent(),
            cancel.clone(),
        ));
        // The row is published before the first request, so it is visible even
        // while the very first pull is outstanding.
        let mut listed = None;
        for _ in 0..100u16 {
            if let Some(report) = super::super::daemon::in_flight_report(&registry) {
                listed = Some(report);
                break;
            }
            tokio::task::yield_now().await;
        }
        let listed = listed.expect("the poller must be listed in /sessions while it runs");
        assert!(listed.contains("assignment poll"), "listing was: {listed}");
        let killed = super::super::daemon::cancel_in_flight(&registry, "d1").expect("d1 is this chat's address space");
        assert!(killed.contains("d1"), "kill notice was: {killed}");
        handle.await.expect("the poller must end when it is killed");
        assert!(
            super::super::daemon::in_flight_report(&registry).is_none(),
            "an ended poller must stop being listed"
        );
    }

    #[test]
    fn an_assignment_survives_a_round_trip_through_the_input_queue() {
        for (wire, disposition) in [
            ("queue", Disposition::Queue),
            ("steer", Disposition::Steer),
            ("interrupt", Disposition::Interrupt),
        ] {
            let msg = assignment_message(&assignment("a-1", "do the thing", wire), disposition);
            let seen = delivered(&msg).expect("an assignment message must be recognisable");
            assert_eq!(seen.assignment_id, "a-1");
            assert_eq!(seen.disposition, disposition);
            if disposition.jumps_the_queue() {
                assert!(
                    msg.content.starts_with("/now "),
                    "a queue-jumping disposition must use the existing priority prefix: {}",
                    msg.content
                );
            } else {
                assert_eq!(msg.content, "do the thing");
            }
        }
    }

    #[test]
    fn ordinary_input_is_never_mistaken_for_an_assignment() {
        let mut msg = assignment_message(&assignment("a-1", "task", "queue"), Disposition::Queue);
        msg.sender = "user".to_string();
        assert!(delivered(&msg).is_none());
        msg.sender = "prx-ui".to_string();
        assert!(delivered(&msg).is_none());
        msg.sender = format!("{SENDER_PREFIX}queue:");
        assert!(delivered(&msg).is_none(), "an empty assignment id is not an assignment");
    }

    #[test]
    fn an_unknown_disposition_degrades_to_queue_and_never_to_interrupt() {
        assert_eq!(Disposition::from_wire("obliterate"), Disposition::Queue);
        assert_eq!(Disposition::from_wire(""), Disposition::Queue);
        assert!(!Disposition::is_known_wire("obliterate"));
        assert!(Disposition::is_known_wire("interrupt"));
        assert!(!Disposition::from_wire("obliterate").ends_the_turn_in_flight());
    }

    #[test]
    fn only_interrupt_ends_the_turn_in_flight() {
        assert!(!Disposition::Queue.jumps_the_queue());
        assert!(Disposition::Steer.jumps_the_queue());
        assert!(Disposition::Interrupt.jumps_the_queue());
        assert!(!Disposition::Queue.ends_the_turn_in_flight());
        assert!(
            !Disposition::Steer.ends_the_turn_in_flight(),
            "steer inserts into the turn in flight; it does not destroy it"
        );
        assert!(Disposition::Interrupt.ends_the_turn_in_flight());
    }

    #[test]
    fn a_chat_session_can_never_report_the_daemons_own_cancelled_verdict() {
        let reportable: Vec<&str> = ResultStatus::all().iter().map(|status| status.as_str()).collect();
        assert_eq!(reportable, vec!["completed", "failed", "rejected"]);
        assert!(
            !reportable.contains(&"cancelled"),
            "`cancelled` is the daemon's verdict about work it ended, not something a client may claim"
        );
    }

    #[test]
    fn a_summary_is_fitted_to_what_the_daemon_accepts() {
        assert_eq!(bound_summary("  answer  "), "answer");
        assert!(
            !bound_summary("   ").is_empty(),
            "an empty summary is refused by the daemon"
        );
        let huge = "x".repeat(MAX_SUMMARY_BYTES * 2);
        let bounded = bound_summary(&huge);
        assert!(bounded.len() <= MAX_SUMMARY_BYTES, "bounded to {}", bounded.len());
        assert!(bounded.ends_with("to fit the result field)"));
        let wide = "漢".repeat(MAX_SUMMARY_BYTES);
        assert!(bound_summary(&wide).len() <= MAX_SUMMARY_BYTES);
    }

    #[test]
    fn enrolment_is_declined_when_nothing_points_this_chat_at_a_daemon() {
        let mut config = Config::default();
        config.chat.daemon.url = String::new();
        config.chat.daemon.token = String::new();
        config.gateway.paired_tokens = vec![];
        match enrolable(&config) {
            Enrolable::No(reason) => {
                assert!(reason.contains("chat.daemon.url"), "reason was: {reason}");
            }
            Enrolable::Yes => panic!("an unconfigured chat must not announce a daemon failure on every start"),
        }
        config.chat.daemon.url = "http://127.0.0.1:8765".to_string();
        assert_eq!(enrolable(&config), Enrolable::Yes);

        let mut with_token = Config::default();
        with_token.chat.daemon.url = String::new();
        with_token.chat.daemon.token = "plaintext-token".to_string();
        assert_eq!(enrolable(&with_token), Enrolable::Yes);
    }

    #[test]
    fn an_arrival_names_the_origin_and_the_disposition_without_going_quiet() {
        let notice = arrival_notice(
            &assignment("a-1", "summarise the repo", "interrupt"),
            Disposition::Interrupt,
        );
        assert!(notice.contains("wacli"), "{notice}");
        assert!(notice.contains("3d1a4f5b"), "{notice}");
        assert!(notice.contains("summarise the repo"), "{notice}");
        assert!(notice.contains("interrupt"), "{notice}");

        let mut redelivered = assignment("a-1", "task", "queue");
        redelivered.deliveries = 3;
        assert!(arrival_notice(&redelivered, Disposition::Queue).contains("redelivery #3"));

        let mut operator = assignment("a-2", "task", "queue");
        operator.origin_channel = "api".to_string();
        operator.origin_ref = None;
        let plane = arrival_notice(&operator, Disposition::Queue);
        assert!(plane.contains("api"), "{plane}");
    }

    #[test]
    fn a_label_is_stable_and_fits_what_the_daemon_accepts() {
        let mut config = Config::default();
        config.workspace_dir = std::path::PathBuf::from("/tmp/some/workstation");
        assert_eq!(label_for(&config), "workstation");
        config.workspace_dir = std::path::PathBuf::from(format!("/tmp/{}", "n".repeat(200)));
        assert!(label_for(&config).len() <= MAX_LABEL_BYTES);
        config.workspace_dir = std::path::PathBuf::from("/");
        assert_eq!(label_for(&config), "chat");
    }
}
