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

/// Shortest gap between two attempts by this poller to register again.
///
/// Re-enrolment is idempotent, so a repeat costs the daemon little — but every
/// successful one puts a line on the operator's screen, and a daemon that keeps
/// forgetting this session would otherwise write one every [`POLL_INTERVAL`].
/// A floor of a minute keeps the recovery automatic while bounding what a
/// pathological daemon can print, at the price of up to a minute of extra
/// downtime on the pathological path only: a single restart still recovers on
/// the first unregistered answer.
pub const RE_ENROLMENT_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_mins(1);

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

    /// Which enrolment the caller is looking at. `0` means "not enrolled".
    ///
    /// Read before a call that may fail, and handed back to [`Self::re_enrol`]
    /// afterwards: that is what makes re-enrolment idempotent without a lock
    /// held across the network. Two callers that saw the same dead enrolment
    /// pass the same number, and only the first of them registers.
    fn enrolment_generation(&self) -> u64;

    /// Register again, because the enrolment `stale` names is gone.
    ///
    /// Registering is the entire recovery: the daemon keeps its session
    /// registry in memory and rebuilds nothing on start, so a restart leaves
    /// this chat holding a session id nothing answers to. A new registration
    /// yields a new id and a new token, and the old row does not need
    /// withdrawing because a refusal that says "no such session" is the daemon
    /// saying the row is already gone.
    fn re_enrol(&self, stale: u64) -> MailboxFuture<'_, ReEnrolment>;
}

/// What one re-enrolment attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReEnrolment {
    /// This call registered again. The session id and token are new, and every
    /// id from the old enrolment is meaningless to the daemon now.
    Registered { session_ref: String },
    /// The enrolment the caller saw had already been replaced — or withdrawn on
    /// the way out — so this call registered nothing. Idempotency, stated
    /// rather than assumed.
    Superseded { session_ref: String },
}

/// Chat's enrolment with one daemon, and the state that keeps it honest.
pub struct AssignmentClient {
    message_target: Mutex<Option<crate::messaging::SessionAddr>>,
    endpoint: TasksEndpoint,
    label: String,
    pid: Option<u32>,
    enrolment: Mutex<Option<Enrolment>>,
    /// Serialises re-enrolment, so two callers that raced the same dead
    /// enrolment produce one registration rather than two sessions the daemon
    /// then has to be told apart. `tokio::sync::Mutex` because the section it
    /// guards contains an HTTP round trip; the `parking_lot` mutexes here are
    /// never held across an await.
    re_enrolling: tokio::sync::Mutex<()>,
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
    /// Which registration this is: 1 for the first, one more for each
    /// replacement. The version stamp re-enrolment checks so a caller acting on
    /// an enrolment that has already been replaced replaces nothing.
    generation: u64,
}

impl AssignmentClient {
    /// Build a client for the daemon this chat is configured to talk to.
    #[must_use]
    pub fn new(config: &Config) -> Arc<Self> {
        Arc::new(Self {
            message_target: Mutex::new(None),
            endpoint: super::daemon::endpoint(config),
            label: label_for(config),
            pid: Some(std::process::id()),
            enrolment: Mutex::new(None),
            re_enrolling: tokio::sync::Mutex::new(()),
            taken: Mutex::new(HashSet::new()),
        })
    }

    pub(crate) fn set_message_target(&self, target: crate::messaging::SessionAddr) {
        *self.message_target.lock() = Some(target);
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
    /// enrol at all has a perfectly usable degraded mode and hammering a daemon
    /// that refused it would only bury the reason. Losing an enrolment that
    /// once worked is the different case, and [`AssignmentClient::re_enrol_now`]
    /// is what answers it.
    pub async fn enrol(&self) -> Result<String> {
        let registration = tasks_cli::register_chat_session(&self.endpoint, &self.label, self.pid).await?;
        let session_ref = registration.session_ref.clone();
        self.install(registration, 1);
        Ok(session_ref)
    }

    /// Replace the held enrolment with a fresh registration.
    ///
    /// Everything the old one meant goes with it: the id, the token, and the
    /// set of assignment ids already taken, which belonged to a mailbox that no
    /// longer exists. Keeping the last of those would let an id the *new*
    /// daemon hands out collide with a dead one and be silently discarded as a
    /// redelivery.
    fn install(&self, registration: tasks_cli::ChatSessionRegistration, generation: u64) {
        *self.enrolment.lock() = Some(Enrolment {
            session_id: registration.session_id,
            session_ref: registration.session_ref,
            token: registration.token,
            generation,
        });
        self.taken.lock().clear();
    }

    /// The enrolment version this client currently holds; `0` when unenrolled.
    fn generation(&self) -> u64 {
        self.enrolment.lock().as_ref().map_or(0, |held| held.generation)
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

    /// Register again after the daemon stopped recognising this session.
    ///
    /// Idempotent by version stamp rather than by trust: the generation the
    /// caller saw is compared with the one held now, under a lock that is also
    /// held across the registration call, so concurrent callers that saw the
    /// same dead enrolment yield exactly one new session. A caller holding an
    /// older stamp is told it was superseded and registers nothing — which is
    /// also what stops a poll that raced the exit path from re-registering a
    /// chat that has just withdrawn.
    async fn re_enrol_now(&self, stale: u64) -> Result<ReEnrolment> {
        let _serialised = self.re_enrolling.lock().await;
        let held = self.enrolment.lock().clone();
        let current = held.as_ref().map_or(0, |enrolment| enrolment.generation);
        if current != stale {
            return Ok(ReEnrolment::Superseded {
                session_ref: held.map_or_else(|| "unenrolled".to_string(), |enrolment| enrolment.session_ref),
            });
        }
        let registration = tasks_cli::register_chat_session(&self.endpoint, &self.label, self.pid).await?;
        let session_ref = registration.session_ref.clone();
        self.install(registration, stale.saturating_add(1));
        Ok(ReEnrolment::Registered { session_ref })
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
            let target = self.message_target.lock().clone();
            tasks_cli::pull_chat_inbox(
                &self.endpoint,
                &session_id,
                &token,
                &ack,
                Some(PULL_BATCH),
                target.as_ref(),
            )
            .await
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

    fn enrolment_generation(&self) -> u64 {
        self.generation()
    }

    fn re_enrol(&self, stale: u64) -> MailboxFuture<'_, ReEnrolment> {
        Box::pin(self.re_enrol_now(stale))
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
    let error = crate::security::redaction::redact_provider_text(&format!("{error:#}"));
    format!(
        "Could not report {} for daemon assignment {}: {error}. The daemon still has it outstanding; \
         whoever assigned it is waiting.",
        status.as_str(),
        short_id(assignment_id)
    )
}

/// The line shown when this chat could not enrol.
#[must_use]
pub fn enrolment_failure_notice(base_url: &str, error: &anyhow::Error) -> String {
    let base_url = crate::security::redaction::redact_provider_text(base_url);
    let error = crate::security::redaction::redact_provider_text(&format!("{error:#}"));
    format!(
        "This chat could not enrol with the daemon at {base_url}, so it cannot be assigned work: {error}\n\
         Everything else in this chat is unaffected. Restart chat once the daemon accepts it to become \
         assignable again."
    )
}

/// The line shown when this chat had to register with the daemon again.
///
/// Not silent, and not a warning either: the session id and the mailbox behind
/// it are new, so anything an operator wrote down about the old one — a
/// `--session` argument, an `assign_allow` rule keyed on the id — is stale, and
/// the only place they can learn that is here. Printed once per re-enrolment,
/// which is once per daemon restart rather than once per poll.
#[must_use]
pub fn re_enrolled_notice(base_url: &str, session_ref: &str) -> String {
    format!(
        "The daemon at {base_url} no longer had this chat's registration — it keeps them in memory, so a \
         restart forgets every one — and this chat registered again as {session_ref}. It is assignable \
         again; anything queued for the old registration is gone with it."
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
    ///
    /// The daemon stores the report as the assignment's result, so the reply
    /// is redacted like the terminal commit first (D-053).
    pub fn completed(&mut self, reply: &str) {
        self.outcome = Some((
            ResultStatus::Completed,
            bound_summary(&crate::agent::terminal::redact_persisted_text(reply)),
        ));
    }

    /// Record that the turn ran and failed, with the reason (redacted like
    /// the reply).
    pub fn failed(&mut self, reason: &str) {
        self.outcome = Some((
            ResultStatus::Failed,
            bound_summary(&crate::agent::terminal::redact_persisted_text(reason)),
        ));
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
            let status_label: &'static str = status.as_str();
            tracing::warn!(
                assignment = %crate::agent::diagnostics::text_class(&self.assignment_id),
                status = status_label,
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
        let reason = crate::security::redaction::redact_provider_text(reason);
        let reason = reason.as_str();
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
            let failed_pulls: u64 = episode.pulls;
            tracing::debug!(reason = %crate::agent::diagnostics::text_class(reason), failed_pulls, "chat assignment pull is still failing");
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

    /// End the failing episode without a recovery line, for a caller that has
    /// already said something more specific about the same recovery.
    ///
    /// The re-enrolment notice is exactly that: it names what was wrong and
    /// says this chat is assignable again, so adding the generic line after it
    /// would say the same thing twice.
    pub fn settled(&mut self) {
        self.episode = None;
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
    input: &crate::runtime::h2_entry::ChatInputSender,
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
    input: crate::runtime::h2_entry::ChatInputSender,
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
///
/// MUTATION GUARD: drop the re-enrolment arm — treat a refusal that says this
/// session is unregistered like any other failure — and
/// `a_daemon_that_forgot_this_session_is_answered_by_registering_again` fails:
/// the poller keeps asking with credentials nothing recognises until a human
/// restarts the chat.
pub async fn run_poller(
    mailbox: Arc<dyn AssignmentMailbox>,
    in_flight: InFlightRegistry,
    input: crate::runtime::h2_entry::ChatInputSender,
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
    // When this poller last tried to register again, so a daemon that forgets
    // this session on every pull cannot buy one notice per poll interval. A
    // clock reading, not a timer: nothing here waits on it.
    let mut last_re_enrolment: Option<tokio::time::Instant> = None;
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
                if batch
                    .messages
                    .iter()
                    .any(|value| value.get("kind").and_then(|v| v.as_str()) == Some("message"))
                {
                    let wake = crate::channels::traits::ChannelMessage {
                        id: "mailbox-wake".into(),
                        sender: "session-message".into(),
                        sender_is_bot: true,
                        channel: "terminal".into(),
                        ..crate::channels::traits::ChannelMessage::default()
                    };
                    if input.send(wake).await.is_err() {
                        break;
                    }
                }
                if let Some(line) = notices.recovered() {
                    notify(&line);
                }
                let outcome = take_batch(mailbox.as_ref(), batch.assignments, &input, notify.as_ref()).await;
                pending_acks = outcome.acks;
                if outcome.input_closed {
                    break;
                }
            }
            Err(error) if tasks_cli::session_is_unregistered(&error) => {
                // The daemon holds no row for this session: it restarted — the
                // registry is process memory and rebuilds nothing — or an
                // operator removed the row. Either way this chat is
                // unassignable until it registers again, and requiring a human
                // to restart every chat after every daemon restart is not a
                // recovery. Registering again is, and the next pull is the
                // natural retry for a registration that also fails: this loop
                // is already periodic, so nothing here needs a timer.
                //
                // Whatever happens next, the acknowledgements were for a mailbox
                // that no longer exists. Sending them to a new session would
                // acknowledge ids it never handed out.
                pending_acks.clear();
                // At most one attempt per [`RE_ENROLMENT_MIN_INTERVAL`]: a daemon
                // stuck in "who are you?" answers every pull, and re-registering
                // on each one would put a notice on screen every poll interval.
                if last_re_enrolment.is_some_and(|at| at.elapsed() < RE_ENROLMENT_MIN_INTERVAL) {
                    tracing::debug!(
                        session = %crate::agent::diagnostics::text_class(&session_ref),
                        "daemon still does not know this chat session; re-enrolment is rate limited"
                    );
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        () = tokio::time::sleep(POLL_INTERVAL) => {}
                    }
                    continue;
                }
                last_re_enrolment = Some(tokio::time::Instant::now());
                let stale = mailbox.enrolment_generation();
                let re_enrolled = tokio::select! {
                    () = cancel.cancelled() => break,
                    outcome = mailbox.re_enrol(stale) => outcome,
                };
                match re_enrolled {
                    Ok(ReEnrolment::Registered { session_ref }) => {
                        notices.settled();
                        notify(&re_enrolled_notice(&mailbox.base_url(), &session_ref));
                    }
                    // Someone else had already replaced this enrolment. Saying
                    // so again would be one line per poller for one event.
                    Ok(ReEnrolment::Superseded { .. }) => notices.settled(),
                    Err(error) => {
                        if let Some(line) = notices.failed(&format!("{error:#}")) {
                            notify(&line);
                        }
                    }
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
