//! Live `prx chat` sessions, and the mailbox the daemon hands them work through.
//!
//! # The direction this closes
//!
//! Three of the four legs of "a message in WhatsApp makes a `prx chat` session
//! do something, and the answer comes back to the phone" already existed: the
//! channel delivers inbound messages to the daemon, `prx chat` already talks to
//! the daemon over the authenticated HTTP control plane
//! ([`crate::chat::sessions::daemon`]), and the daemon already sends outbound
//! messages through the gates in [`crate::tools::message_send`]. The missing leg
//! was the daemon handing work *to* a chat session, and the reason it was
//! missing is that a chat session has no listener: it is a terminal program, not
//! a server.
//!
//! This module supplies the leg without giving it one. Chat already makes
//! outbound HTTP calls to the daemon, so the daemon keeps a mailbox per session
//! and chat **pulls** from it. No new transport, no new port, no new credential
//! shape — the same bearer-authenticated `/api` surface everything else uses.
//!
//! # No wall clock, on purpose
//!
//! Nothing here expires, and no number in this file is a timeout:
//!
//! * A session is **never evicted for being quiet.** A chat that has not polled
//!   in an hour may be dead, or may be sitting inside a six-hour tool call — and
//!   this runtime exists to allow the second one. So quiet is *reported*
//!   ([`ChatLiveness::Silent`]) and never acted upon. Deleting the session would
//!   silently discard queued work on a guess about duration; saying "this one
//!   has not polled for 3714s" hands the judgement to the operator, who can
//!   `DELETE` it or kill individual assignments.
//! * An assignment is **never dropped for waiting too long.** It waits in the
//!   mailbox until it is pulled, or until an operator kills its work-registry
//!   row. That row is the whole point: once nothing expires by itself, being
//!   seen is the precondition for being ended.
//! * The mailbox has **no depth limit and no concurrency limit.** A wedged chat
//!   accumulates rows, and every one of them is individually listed and
//!   killable, which is this runtime's stated substitute for a cap.
//!
//! The one duration constant here, [`SILENT_AFTER`], is the counterpart of
//! `[runtime] long_task_warn_secs` rather than of any timeout: it changes a word
//! in a report and terminates nothing. Read [`crate::agent::idle`] for the full
//! statement of the distinction; the short form is that liveness here is
//! evidence-based (the last pull actually happened) rather than clock-based.
//!
//! # What is deliberately not here
//!
//! * **Persistence.** The mailbox is process memory. A daemon restart forgets
//!   every session and every queued assignment, and chat sessions re-register on
//!   their next start. Callers must not treat an accepted assignment as durable.
//! * **Delivery to the origin.** This module records *who asked* and *what came
//!   back*; it never sends anything. Routing a result back to a channel is an
//!   outbound action and must go through the outbound gates, which is a
//!   deliberate separation — see [`AssignmentResult::origin`].
//! * **Multi-host.** Chat and daemon share a machine and talk over the loopback
//!   gateway.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::runtime::registry::{WorkGuard, WorkId};
use crate::security::op_id::ref_for_channel_recipient;
use crate::security::policy::SecurityPolicy;

/// Longest session label accepted at registration.
pub const MAX_LABEL_LEN: usize = 64;
/// Longest assignment body accepted. Well past any instruction a human types,
/// and far short of anything that would make the mailbox a file store.
pub const MAX_TASK_LEN: usize = 32 * 1024;
/// Longest result summary accepted back from a chat session.
pub const MAX_SUMMARY_LEN: usize = 16 * 1024;
/// Assignments handed over in one pull when the caller does not say.
pub const DEFAULT_PULL_BATCH: usize = 8;
/// Ceiling on one pull response.
///
/// A response-size bound, not a limit on how much work may be outstanding: what
/// does not fit stays at the head of the queue and comes back on the next pull,
/// and `queued_remaining` tells the caller there is more.
pub const MAX_PULL_BATCH: usize = 64;
/// How many finished assignments the cross-session result feed keeps.
///
/// A report buffer, not a quota. When it overflows the oldest entries are
/// dropped and [`ResultsPage::oldest_seq`] moves past the consumer's cursor, so
/// a consumer that fell behind can *tell* it missed results instead of silently
/// believing it saw them all.
pub const RESULT_LOG_CAPACITY: usize = 256;

/// How long a registered session may go without pulling before the listing
/// calls it [`ChatLiveness::Silent`].
///
/// **This is not a timeout and must never become one.** Crossing it changes one
/// word in a report; it evicts nothing, discards nothing and cancels nothing.
/// It exists because "last pulled 3714s ago" is a number an operator has to
/// interpret, and "silent" is the interpretation for the common case.
///
/// The value is chosen to be many multiples of a sane chat poll interval, so an
/// ordinary in-flight pull can never make a healthy session look silent.
pub const SILENT_AFTER: Duration = Duration::from_mins(2);

/// Channel name recorded for an assignment that came from the operator plane.
///
/// Matches the name the outbound side uses for the same plane
/// (`crate::gateway::api::channels::OPERATOR_PLANE_CHANNEL`), so one origin
/// reads the same wherever this runtime mentions it.
pub const OPERATOR_PLANE_CHANNEL: &str = "api";

/// Channel name under which a chat session is fingerprinted.
///
/// The session is the *recipient* of an assignment, so it is redacted with the
/// same `op_id::ref_for_channel_recipient` every other recipient goes through
/// and under a stable channel name, which is what makes the fingerprint in a
/// work-registry row and the one in the session listing the same string.
pub const CHAT_SESSION_CHANNEL: &str = "chat";

// ── Value types ─────────────────────────────────────────────────────────────

/// What the assigner wants done with a turn the chat session may already be
/// running.
///
/// Every variant maps onto a path `prx chat` already has; none of them is
/// triggered by a clock. `Interrupt` in particular is an explicit operator
/// decision, not a timeout firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Wait for the session to be free. The default, because it is the variant
    /// that cannot destroy work in progress.
    Queue,
    /// Hand the task to the turn that is running right now, as additional
    /// input.
    Steer,
    /// Stop the turn that is running right now, then deliver the task.
    Interrupt,
}

impl Disposition {
    /// Stable lowercase tag used by the CLI and the HTTP API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
            Self::Interrupt => "interrupt",
        }
    }

    /// Parse the wire form. Unknown values are rejected rather than folded into
    /// the default: a caller that asked to interrupt and got a queue would be
    /// told nothing while its intent quietly evaporated.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "queue" => Some(Self::Queue),
            "steer" => Some(Self::Steer),
            "interrupt" => Some(Self::Interrupt),
            _ => None,
        }
    }

    /// Whether this disposition is meant to reach a turn that is already
    /// running, and therefore jumps the queue.
    const fn is_immediate(self) -> bool {
        matches!(self, Self::Steer | Self::Interrupt)
    }
}

/// How an assignment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    /// The chat session ran the task and reports success.
    Completed,
    /// The chat session ran the task and reports failure.
    Failed,
    /// The chat session declined the task without running it.
    Rejected,
    /// The assignment was ended from outside — an operator kill of its work
    /// row, or the session deregistering with it still outstanding. Recorded
    /// rather than dropped so whoever asked for the work can be told it will
    /// not arrive.
    Cancelled,
}

impl ResultStatus {
    /// Stable lowercase tag used by the CLI and the HTTP API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse the wire form. `cancelled` is deliberately **not** accepted: it is
    /// this runtime's own verdict about work it ended, and a session claiming
    /// it would make an operator kill indistinguishable from a client giving up.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

/// Whether a registered session is still pulling its mailbox.
///
/// Derived from evidence — the last pull that actually happened — not from a
/// heartbeat a client could keep sending while wedged, and never used to evict
/// anything. See the module note on the absence of a wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatLiveness {
    /// Registered, but has not pulled once yet.
    NeverPolled,
    /// Pulled recently enough that nothing is worth reporting.
    Polling,
    /// Has not pulled for longer than [`SILENT_AFTER`]. **Possibly dead** — the
    /// registry does not claim to know, and does not act on it.
    Silent,
}

impl ChatLiveness {
    /// Stable lowercase tag used by the CLI and the HTTP API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NeverPolled => "never_polled",
            Self::Polling => "polling",
            Self::Silent => "silent",
        }
    }
}

/// Who is asking for an assignment.
///
/// This type is the identity boundary. It has exactly two constructors and
/// neither of them reads a caller-supplied identity claim:
/// [`AssignPrincipal::operator_plane`] is minted by a handler that has already
/// passed the gateway's bearer check, and
/// [`AssignPrincipal::from_trusted_scope`] reads only the `_zc_*` block the
/// runtime writes immediately before a tool executes
/// (`crate::tools::execution::RUNTIME_ONLY_ARG_PREFIXES` strips any forged copy
/// first). A model, an MCP body or an HTTP payload therefore cannot name itself
/// into this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignPrincipal {
    /// An authenticated operator-plane request. Authorized by construction:
    /// holding the gateway token already means being the local operator, and
    /// this runtime deliberately does not mint a second credential.
    OperatorPlane,
    /// A correspondent on a messaging channel, as the runtime recorded them.
    Correspondent {
        channel: String,
        sender: String,
        chat_type: String,
    },
}

impl AssignPrincipal {
    /// The operator plane, for a handler that has passed the gateway's auth
    /// middleware.
    #[must_use]
    pub const fn operator_plane() -> Self {
        Self::OperatorPlane
    }

    /// Build a correspondent principal from runtime-injected tool arguments.
    ///
    /// Returns `None` unless `_zc_scope_trusted` is literally `true` and the
    /// `_zc_scope` block carries both a sender and a channel. Failing closed
    /// here is what keeps an untrusted call from becoming an anonymous one that
    /// some later rule might match.
    #[must_use]
    pub fn from_trusted_scope(args: &serde_json::Value) -> Option<Self> {
        if !args
            .get("_zc_scope_trusted")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return None;
        }
        let scope = args.get("_zc_scope")?.as_object()?;
        let sender = scope.get("sender").and_then(serde_json::Value::as_str)?.trim();
        let channel = scope.get("channel").and_then(serde_json::Value::as_str)?.trim();
        if sender.is_empty() || channel.is_empty() {
            return None;
        }
        Some(Self::Correspondent {
            channel: channel.to_string(),
            sender: sender.to_string(),
            // Absent narrows rather than widens: a rule that names a chat_type
            // simply will not match, so a scope block missing the field cannot
            // borrow a permission written for `direct`.
            chat_type: scope
                .get("chat_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
        })
    }

    /// The channel this origin sits on. Channel names are not sensitive and are
    /// kept readable everywhere this runtime records an origin.
    #[must_use]
    pub fn channel(&self) -> &str {
        match self {
            Self::OperatorPlane => OPERATOR_PLANE_CHANNEL,
            Self::Correspondent { channel, .. } => channel,
        }
    }

    /// The redacted reference for this origin: `None` for the operator plane,
    /// which is not a person and has no identifier to protect, and the shared
    /// `op_id` fingerprint for a correspondent.
    #[must_use]
    pub fn reference(&self) -> Option<String> {
        match self {
            Self::OperatorPlane => None,
            Self::Correspondent { channel, sender, .. } => Some(ref_for_channel_recipient(channel, sender)),
        }
    }

    /// One human-readable, redaction-safe label for logs, registry rows and
    /// refusals — `api`, or `wacli 9b2e0f…`.
    #[must_use]
    pub fn label(&self) -> String {
        self.reference().map_or_else(
            || self.channel().to_string(),
            |reference| format!("{} {reference}", self.channel()),
        )
    }
}

/// Why a mailbox operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatSessionError {
    /// No session with that id is registered.
    UnknownSession,
    /// The per-session token was missing or wrong.
    BadToken,
    /// The principal may not assign work to this session.
    NotAuthorized { principal: String, session_ref: String },
    /// No such assignment is outstanding for this session.
    UnknownAssignment,
    /// The request itself was malformed.
    Invalid(String),
}

impl ChatSessionError {
    /// Stable machine tag for this refusal.
    ///
    /// The prose above is written for a person and will be reworded; a client
    /// that has to *act* on one particular refusal — the chat poller
    /// re-registering when its session is gone — branches on this instead, so
    /// improving a sentence can never quietly break a recovery path. Sent as
    /// `code` alongside `error` in every HTTP refusal, and mirrored by
    /// `crate::runtime::tasks_cli::ControlApiRefusal` on the client side.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownSession => "unknown_session",
            Self::BadToken => "bad_token",
            Self::NotAuthorized { .. } => "not_authorized",
            Self::UnknownAssignment => "unknown_assignment",
            Self::Invalid(_) => "invalid_request",
        }
    }
}

impl std::fmt::Display for ChatSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSession => write!(
                f,
                "no chat session with that id is registered; `GET /api/chat-sessions` lists the live ones"
            ),
            Self::BadToken => write!(
                f,
                "the session token is missing or does not match; it is issued once, at registration"
            ),
            // Neither half is written out in the clear: the sender is the same
            // fingerprint the outbound refusals and the work registry use for
            // that account, and the session is fingerprinted the same way its
            // listing row is. A refusal is the last place an identifier should
            // start leaking.
            Self::NotAuthorized { principal, session_ref } => write!(
                f,
                "principal {principal} may not assign work to chat session {session_ref}: assignment is \
                 default-deny. Name the origin in `autonomy.scopes.assign_owners`, or give it an \
                 `assign_allow` entry for this session in `autonomy.scopes.rules`"
            ),
            Self::UnknownAssignment => write!(
                f,
                "that assignment is not outstanding for this session; it was already reported, killed, or never existed"
            ),
            Self::Invalid(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ChatSessionError {}

// ── Stored state ────────────────────────────────────────────────────────────

/// One unit of work handed to a chat session.
///
/// Owns its work-registry row for its whole life: created when the assignment
/// is accepted, dropped when the result comes back or the row is killed. So an
/// assignment queued for a session nobody is draining is visible in
/// `prx tasks list` the entire time it sits there.
pub struct Assignment {
    id: String,
    session_id: String,
    task: String,
    disposition: Disposition,
    origin: AssignPrincipal,
    created_at_unix_ms: u64,
    deliveries: u32,
    /// Registry row. Dropping this retires the row.
    _work: WorkGuard,
    work_id: WorkId,
    /// Cancelled when this assignment is dropped, which is what lets the kill
    /// watcher retire itself instead of parking on a token that will never
    /// fire.
    _finished: DropGuard,
}

impl Assignment {
    fn view(&self) -> AssignmentView {
        AssignmentView {
            assignment_id: self.id.clone(),
            session_id: self.session_id.clone(),
            task: self.task.clone(),
            disposition: self.disposition,
            deliveries: self.deliveries,
            created_at_unix_ms: self.created_at_unix_ms,
            work_id: self.work_id,
            origin_channel: self.origin.channel().to_string(),
            origin_ref: self.origin.reference(),
        }
    }
}

/// A queued assignment as handed to the chat session that pulls it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentView {
    pub assignment_id: String,
    pub session_id: String,
    pub task: String,
    pub disposition: Disposition,
    /// How many times this assignment has been handed out. Greater than 1 means
    /// an earlier delivery was never acknowledged; the puller should treat the
    /// assignment id as an idempotency key.
    pub deliveries: u32,
    pub created_at_unix_ms: u64,
    /// Address of this assignment's work-registry row, for `prx tasks kill`.
    pub work_id: WorkId,
    pub origin_channel: String,
    /// Redacted origin; `None` when the assignment came from the operator plane.
    pub origin_ref: Option<String>,
}

/// One finished assignment, as recorded in the cross-session result feed.
pub struct AssignmentResult {
    pub seq: u64,
    pub assignment_id: String,
    pub session_id: String,
    pub session_label: String,
    pub disposition: Disposition,
    pub status: ResultStatus,
    pub summary: String,
    pub completed_at_unix_ms: u64,
    /// **Carries the origin in the clear, and must not be serialized as-is.**
    ///
    /// It is here for exactly one consumer: an in-process caller that has to
    /// route this result back to whoever asked for the work, which it cannot do
    /// from a fingerprint. Everything that leaves the process — the HTTP feed,
    /// logs, work-registry rows — publishes [`AssignPrincipal::channel`] and
    /// [`AssignPrincipal::reference`] instead, and the delivery itself still has
    /// to pass the outbound gates like any other message.
    pub origin: AssignPrincipal,
}

/// A page of the result feed.
pub struct ResultsPage {
    pub results: Vec<AssignmentResult>,
    /// Cursor to pass as `after_seq` next time.
    pub next_seq: u64,
    /// Lowest sequence still retained. A consumer whose cursor is below this
    /// missed results to the buffer's capacity and should say so rather than
    /// assume it is up to date.
    pub oldest_seq: u64,
}

/// A registered chat session, as reported by the listing.
pub struct SessionView {
    pub session_id: String,
    /// Redacted reference, identical to the one in this session's assignment
    /// rows in the work registry.
    pub session_ref: String,
    pub label: String,
    pub pid: Option<u32>,
    pub registered_at_unix_ms: u64,
    pub liveness: ChatLiveness,
    /// Seconds since the last pull, or `None` if it has never pulled. Reported
    /// as the raw number as well as the [`ChatLiveness`] word so an operator can
    /// draw their own line.
    pub last_poll_age_secs: Option<u64>,
    pub polls: u64,
    pub queued: usize,
    /// Handed out and not yet acknowledged.
    pub delivered: usize,
    /// Acknowledged and being worked on.
    pub accepted: usize,
}

/// What a registration hands back to the chat process.
pub struct Registration {
    pub session_id: String,
    pub session_ref: String,
    /// Shown exactly once. Only its SHA-256 is kept, so a lost token means
    /// re-registering, never recovering.
    pub token: String,
    pub label: String,
    pub registered_at_unix_ms: u64,
}

/// What an accepted assignment hands back to the assigner.
#[derive(Debug)]
pub struct AssignmentReceipt {
    pub assignment_id: String,
    pub session_id: String,
    pub session_ref: String,
    pub disposition: Disposition,
    /// Address of the work-registry row, so the assigner can report something
    /// killable rather than something opaque.
    pub work_id: WorkId,
    /// How many assignments are queued ahead of this one. Zero for an immediate
    /// disposition, which goes to the head of the queue.
    pub queued_ahead: usize,
}

struct SessionState {
    /// Evidence of liveness: when the session last actually pulled. Monotonic,
    /// so a clock adjustment cannot make a live session look silent.
    last_poll: Option<Instant>,
    /// Waiting to be handed out.
    inbox: VecDeque<Assignment>,
    /// Handed out, not yet acknowledged. Returned to the head of the inbox on
    /// the next pull — see [`pull`].
    delivered: Vec<Assignment>,
    /// Acknowledged; the session says it has them and is working.
    accepted: Vec<Assignment>,
}

struct Session {
    id: String,
    label: String,
    pid: Option<u32>,
    token_hash: String,
    registered_at_unix_ms: u64,
    polls: AtomicU64,
    state: Mutex<SessionState>,
}

impl Session {
    fn reference(&self) -> String {
        ref_for_channel_recipient(CHAT_SESSION_CHANNEL, &self.id)
    }
}

struct ResultLog {
    entries: VecDeque<AssignmentResult>,
    next_seq: u64,
    oldest_seq: u64,
}

struct ChatSessions {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    results: Mutex<ResultLog>,
}

static SESSIONS: LazyLock<ChatSessions> = LazyLock::new(|| ChatSessions {
    sessions: RwLock::new(HashMap::new()),
    results: Mutex::new(ResultLog {
        entries: VecDeque::new(),
        // Sequence 0 is never used, so `after_seq=0` means "from the start"
        // without a sentinel that a real entry could collide with.
        next_seq: 1,
        oldest_seq: 1,
    }),
});

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Bound and trim one free-text field from the request boundary.
fn bounded(value: &str, field: &str, limit: usize) -> Result<String, ChatSessionError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ChatSessionError::Invalid(format!("{field} must not be empty")));
    }
    if trimmed.len() > limit {
        return Err(ChatSessionError::Invalid(format!(
            "{field} must be at most {limit} bytes"
        )));
    }
    Ok(trimmed.to_string())
}

fn find(session_id: &str) -> Option<Arc<Session>> {
    SESSIONS.sessions.read().get(session_id.trim()).map(Arc::clone)
}

// ── Registration ────────────────────────────────────────────────────────────

/// Register a `prx chat` session and mint its mailbox token.
///
/// The **daemon** picks the id, not the caller: an id a client chose would be a
/// name it could also guess for somebody else's mailbox, and the assignment ACL
/// matches on it.
///
/// # Errors
///
/// [`ChatSessionError::Invalid`] when the label is empty or too long.
pub fn register(label: &str, pid: Option<u32>) -> Result<Registration, ChatSessionError> {
    let label = bounded(label, "label", MAX_LABEL_LEN)?;
    // Two v4s: the id is public and correlates a session with its rows, the
    // token is the secret and is 256 bits of the same CSPRNG.
    let session_id = uuid::Uuid::new_v4().to_string();
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let registered_at_unix_ms = unix_ms();
    let session = Arc::new(Session {
        id: session_id.clone(),
        label: label.clone(),
        pid,
        token_hash: hash_token(&token),
        registered_at_unix_ms,
        polls: AtomicU64::new(0),
        state: Mutex::new(SessionState {
            last_poll: None,
            inbox: VecDeque::new(),
            delivered: Vec::new(),
            accepted: Vec::new(),
        }),
    });
    let session_ref = session.reference();
    SESSIONS.sessions.write().insert(session_id.clone(), session);
    Ok(Registration {
        session_id,
        session_ref,
        token,
        label,
        registered_at_unix_ms,
    })
}

/// Remove a session and cancel everything still outstanding for it.
///
/// Returns the number of assignments discarded, or `None` if the id was not
/// registered. Each discarded assignment is recorded in the result feed as
/// [`ResultStatus::Cancelled`], so an assigner waiting on an answer is told the
/// work went away instead of waiting on it forever.
pub fn deregister(session_id: &str) -> Option<usize> {
    let session = SESSIONS.sessions.write().remove(session_id.trim())?;
    let outstanding = {
        let mut state = session.state.lock();
        let mut outstanding = Vec::new();
        outstanding.extend(state.inbox.drain(..));
        outstanding.append(&mut state.delivered);
        outstanding.append(&mut state.accepted);
        outstanding
    };
    let count = outstanding.len();
    for assignment in outstanding {
        record_result(
            &session,
            &assignment,
            ResultStatus::Cancelled,
            "the chat session deregistered while this assignment was still outstanding".to_string(),
        );
    }
    Some(count)
}

/// Every registered session, oldest registration first.
#[must_use]
pub fn list() -> Vec<SessionView> {
    let sessions = SESSIONS.sessions.read().values().map(Arc::clone).collect::<Vec<_>>();
    let now = Instant::now();
    let mut views = sessions
        .iter()
        .map(|session| {
            let state = session.state.lock();
            let age = state.last_poll.map(|last| now.saturating_duration_since(last));
            SessionView {
                session_id: session.id.clone(),
                session_ref: session.reference(),
                label: session.label.clone(),
                pid: session.pid,
                registered_at_unix_ms: session.registered_at_unix_ms,
                liveness: liveness_for(age),
                last_poll_age_secs: age.map(|age| age.as_secs()),
                polls: session.polls.load(Ordering::Relaxed),
                queued: state.inbox.len(),
                delivered: state.delivered.len(),
                accepted: state.accepted.len(),
            }
        })
        .collect::<Vec<_>>();
    views.sort_by(|left, right| {
        left.registered_at_unix_ms
            .cmp(&right.registered_at_unix_ms)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    views
}

/// Classify a session from how long ago it last pulled.
///
/// A pure function of the evidence, so the classification can be tested at the
/// boundary without waiting [`SILENT_AFTER`] out. `Silent` is a *word in a
/// report*: nothing in this module reads it back, and no caller may treat it as
/// permission to discard the session or its queue.
#[must_use]
pub fn liveness_for(last_poll_age: Option<Duration>) -> ChatLiveness {
    match last_poll_age {
        None => ChatLiveness::NeverPolled,
        Some(age) if age > SILENT_AFTER => ChatLiveness::Silent,
        Some(_) => ChatLiveness::Polling,
    }
}

// ── Assigning ───────────────────────────────────────────────────────────────

/// Authorize and queue one assignment for a chat session.
///
/// Authorization is performed **here**, not by the caller, so no entry point can
/// reach the mailbox without it. The decision itself is
/// [`SecurityPolicy::is_assignment_allowed`], which is default-deny; the
/// operator plane is the one principal that does not consult it, because a
/// request that satisfied the gateway's bearer check is already the local
/// operator.
///
/// An unknown session is authorized *before* it is reported as unknown, so a
/// principal that may not assign anywhere cannot use this endpoint to enumerate
/// which chat sessions exist.
///
/// # Errors
///
/// [`ChatSessionError::NotAuthorized`] when the principal may not assign to this
/// session, [`ChatSessionError::UnknownSession`] when the id is not registered,
/// and [`ChatSessionError::Invalid`] when the task body is empty or too long.
pub fn assign(
    policy: &SecurityPolicy,
    principal: &AssignPrincipal,
    session_id: &str,
    task: &str,
    disposition: Disposition,
) -> Result<AssignmentReceipt, ChatSessionError> {
    let session_id = session_id.trim();
    let task = bounded(task, "task", MAX_TASK_LEN)?;
    let session = find(session_id);
    let label = session.as_ref().map_or("", |session| session.label.as_str());

    if let AssignPrincipal::Correspondent {
        channel,
        sender,
        chat_type,
    } = principal
    {
        if !policy.is_assignment_allowed(sender, channel, chat_type, label, session_id) {
            return Err(ChatSessionError::NotAuthorized {
                principal: principal.label(),
                session_ref: ref_for_channel_recipient(CHAT_SESSION_CHANNEL, session_id),
            });
        }
    }

    let session = session.ok_or(ChatSessionError::UnknownSession)?;
    let assignment_id = uuid::Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    let finished = CancellationToken::new();

    // The row is a lineage **root** on purpose. An assignment outlives the turn
    // that requested it — that turn answers "queued" and ends — so hanging it
    // under `current_work_id()` would put it inside a cascade that kills it the
    // moment the requester finishes, and would leave it pointing at a parent row
    // that no longer exists.
    //
    // Its `run_id` is its own assignment id and nothing else's. Borrowing the
    // requester's would be actively harmful: `registry::resolve_address` answers
    // a run id with that run's lineage root, so an operator killing by the only
    // portable address would land on live work they never aimed at. Minting its
    // own also makes the assignment id a working `prx tasks kill` address.
    //
    // The name carries the disposition and the *channel* in the clear — neither
    // is sensitive, and without the channel the row could not say where the work
    // came from — while the correspondent and the target session are the
    // `op_id::ref_for_channel_recipient` fingerprints, the same function over
    // the same inputs that the refusal on this very assignment and the session
    // listing use. The task body is never in the row.
    //
    // MUTATION GUARD: drop the registration and
    // `a_queued_assignment_is_listed_and_a_kill_discards_it` fails; write either
    // identifier out in the clear and
    // `an_assignment_row_names_the_channel_but_fingerprints_the_parties` fails.
    let work_name = format!(
        "chat_assign {} → {CHAT_SESSION_CHANNEL} {} from {}",
        disposition.as_str(),
        session.reference(),
        principal.label(),
    );
    let work =
        crate::runtime::registry::register_sub_agent(&work_name, &assignment_id, None, None, Some(cancel.clone()));
    let work_id = work.id();

    let assignment = Assignment {
        id: assignment_id.clone(),
        session_id: session.id.clone(),
        task,
        disposition,
        origin: principal.clone(),
        created_at_unix_ms: unix_ms(),
        deliveries: 0,
        _work: work,
        work_id,
        _finished: finished.clone().drop_guard(),
    };

    let queued_ahead = {
        let mut state = session.state.lock();
        if disposition.is_immediate() {
            // Steer and interrupt exist to reach the turn running *now*, so they
            // go to the head. Queueing them behind unrelated work would turn
            // both into a delayed `queue` and silently discard the caller's
            // whole reason for choosing them.
            state.inbox.push_front(assignment);
            0
        } else {
            let ahead = state.inbox.len();
            state.inbox.push_back(assignment);
            ahead
        }
    };

    watch_for_kill(&session, assignment_id.clone(), cancel, finished);

    Ok(AssignmentReceipt {
        assignment_id,
        session_id: session.id.clone(),
        session_ref: session.reference(),
        disposition,
        work_id,
        queued_ahead,
    })
}

/// Retire an assignment as soon as an operator kills its work-registry row.
///
/// Without this the kill would cancel a token nobody listens to: the row only
/// leaves the registry when its `WorkGuard` drops, and the guard lives in the
/// mailbox, so `registry::kill` would report `Requested` forever and the
/// assignment would still be delivered.
///
/// The watcher also races the assignment's own completion, and loses cleanly:
/// dropping the assignment cancels `finished`, which ends the task. Watching
/// only `cancel` would leave one dormant task per assignment for the life of the
/// process.
fn watch_for_kill(
    session: &Arc<Session>,
    assignment_id: String,
    cancel: CancellationToken,
    finished: CancellationToken,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let session = Arc::clone(session);
    handle.spawn(async move {
        tokio::select! {
            () = cancel.cancelled() => {
                discard(&session, &assignment_id);
            }
            () = finished.cancelled() => {}
        }
    });
}

/// Take one assignment out of every queue it could be in and record it as
/// cancelled. Dropping it retires its registry row, which is what confirms the
/// kill.
fn discard(session: &Arc<Session>, assignment_id: &str) {
    let removed = {
        let mut state = session.state.lock();
        take_assignment(&mut state, assignment_id)
    };
    if let Some(assignment) = removed {
        record_result(
            session,
            &assignment,
            ResultStatus::Cancelled,
            "the assignment's work item was killed by an operator".to_string(),
        );
    }
}

fn take_assignment(state: &mut SessionState, assignment_id: &str) -> Option<Assignment> {
    if let Some(index) = state.inbox.iter().position(|entry| entry.id == assignment_id) {
        return state.inbox.remove(index);
    }
    if let Some(index) = state.delivered.iter().position(|entry| entry.id == assignment_id) {
        return Some(state.delivered.remove(index));
    }
    let index = state.accepted.iter().position(|entry| entry.id == assignment_id)?;
    Some(state.accepted.remove(index))
}

// ── Pulling ─────────────────────────────────────────────────────────────────

/// What one pull handed over.
#[derive(Debug)]
pub struct Pulled {
    pub assignments: Vec<AssignmentView>,
    /// How many of the caller's `ack` ids were actually outstanding.
    pub acked: usize,
    /// How many previously delivered but unacknowledged assignments were put
    /// back at the head of the queue by this pull.
    pub requeued: usize,
    /// Still waiting after this batch.
    pub queued_remaining: usize,
}

/// Hand a chat session its queued work, acknowledging the previous batch.
///
/// # Delivery guarantee
///
/// One round trip does three things, in this order:
///
/// 1. **Acknowledge.** Ids in `ack` move from delivered to accepted. An
///    acknowledgement means "I hold this and will report on it", and it is what
///    makes the assignment the session's responsibility.
/// 2. **Reclaim.** Anything handed out on an earlier pull and *not*
///    acknowledged goes back to the head of the queue. A pull is proof the
///    session is alive and asking for work, so an unacknowledged batch can only
///    be one that never arrived or was never processed — a dropped response, a
///    process that died between receiving and handling. Reclaiming on evidence
///    is what replaces the visibility timeout a queue would normally use here,
///    and it needs no clock.
/// 3. **Deliver.** Up to `max` assignments move from the head of the queue to
///    delivered, each with its delivery count incremented.
///
/// The result is at-least-once delivery. A response lost in flight is redelivered
/// on the next pull; a client that acknowledged and then crashed loses the work,
/// which is honest for a mailbox that is process memory to begin with. Because
/// redelivery is possible, `assignment_id` is an idempotency key and
/// [`AssignmentView::deliveries`] tells the puller when it is seeing a repeat.
///
/// The pull is also the session's liveness evidence: it is the only thing that
/// refreshes [`ChatLiveness`]. There is deliberately no separate heartbeat — a
/// client could keep beating while wedged and never draining, which is precisely
/// the state an operator needs to see.
///
/// # Errors
///
/// [`ChatSessionError::UnknownSession`] / [`ChatSessionError::BadToken`].
pub fn pull(session_id: &str, token: &str, ack: &[String], max: Option<usize>) -> Result<Pulled, ChatSessionError> {
    let session = find(session_id).ok_or(ChatSessionError::UnknownSession)?;
    authenticate(&session, token)?;
    let max = max.unwrap_or(DEFAULT_PULL_BATCH).clamp(1, MAX_PULL_BATCH);

    session.polls.fetch_add(1, Ordering::Relaxed);
    let mut state = session.state.lock();
    state.last_poll = Some(Instant::now());

    let mut acked = 0;
    for id in ack {
        if let Some(index) = state.delivered.iter().position(|entry| &entry.id == id) {
            let assignment = state.delivered.remove(index);
            state.accepted.push(assignment);
            acked += 1;
        }
    }

    // Back to the *head*, in their original order, so a redelivery cannot be
    // starved behind work queued while it was in flight. Popping from the back
    // and pushing to the front restores the order they were handed out in.
    let mut reclaimed = std::mem::take(&mut state.delivered);
    let requeued = reclaimed.len();
    while let Some(assignment) = reclaimed.pop() {
        state.inbox.push_front(assignment);
    }

    let mut assignments = Vec::new();
    for _ in 0..max {
        let Some(mut assignment) = state.inbox.pop_front() else {
            break;
        };
        assignment.deliveries = assignment.deliveries.saturating_add(1);
        assignments.push(assignment.view());
        state.delivered.push(assignment);
    }

    Ok(Pulled {
        assignments,
        acked,
        requeued,
        queued_remaining: state.inbox.len(),
    })
}

fn authenticate(session: &Session, token: &str) -> Result<(), ChatSessionError> {
    let token = token.trim();
    if token.is_empty() || hash_token(token) != session.token_hash {
        return Err(ChatSessionError::BadToken);
    }
    Ok(())
}

// ── Reporting back ──────────────────────────────────────────────────────────

/// Record the outcome of an assignment and retire its work-registry row.
///
/// Accepted from either the delivered or the accepted set, so a session that
/// finishes a task quickly and reports it before its next pull is not punished
/// for skipping the acknowledgement.
///
/// # Errors
///
/// [`ChatSessionError::UnknownSession`], [`ChatSessionError::BadToken`],
/// [`ChatSessionError::UnknownAssignment`], or [`ChatSessionError::Invalid`]
/// when the summary is empty or too long.
pub fn report(
    session_id: &str,
    token: &str,
    assignment_id: &str,
    status: ResultStatus,
    summary: &str,
) -> Result<u64, ChatSessionError> {
    let session = find(session_id).ok_or(ChatSessionError::UnknownSession)?;
    authenticate(&session, token)?;
    let summary = bounded(summary, "summary", MAX_SUMMARY_LEN)?;
    let assignment_id = assignment_id.trim();

    let assignment = {
        let mut state = session.state.lock();
        let index = state
            .accepted
            .iter()
            .position(|entry| entry.id == assignment_id)
            .map(|index| state.accepted.remove(index))
            .or_else(|| {
                state
                    .delivered
                    .iter()
                    .position(|entry| entry.id == assignment_id)
                    .map(|index| state.delivered.remove(index))
            });
        index.ok_or(ChatSessionError::UnknownAssignment)?
    };

    Ok(record_result(&session, &assignment, status, summary))
}

/// Append one finished assignment to the feed. Dropping the assignment
/// afterwards retires its registry row.
fn record_result(session: &Session, assignment: &Assignment, status: ResultStatus, summary: String) -> u64 {
    let mut log = SESSIONS.results.lock();
    let seq = log.next_seq;
    log.next_seq = log.next_seq.saturating_add(1);
    log.entries.push_back(AssignmentResult {
        seq,
        assignment_id: assignment.id.clone(),
        session_id: session.id.clone(),
        session_label: session.label.clone(),
        disposition: assignment.disposition,
        status,
        summary,
        completed_at_unix_ms: unix_ms(),
        origin: assignment.origin.clone(),
    });
    while log.entries.len() > RESULT_LOG_CAPACITY {
        if let Some(dropped) = log.entries.pop_front() {
            log.oldest_seq = dropped.seq.saturating_add(1);
        }
    }
    seq
}

/// Results with a sequence greater than `after_seq`, oldest first.
#[must_use]
pub fn results_after(after_seq: u64, limit: usize) -> ResultsPage {
    let log = SESSIONS.results.lock();
    let limit = limit.clamp(1, RESULT_LOG_CAPACITY);
    let results = log
        .entries
        .iter()
        .filter(|entry| entry.seq > after_seq)
        .take(limit)
        .map(|entry| AssignmentResult {
            seq: entry.seq,
            assignment_id: entry.assignment_id.clone(),
            session_id: entry.session_id.clone(),
            session_label: entry.session_label.clone(),
            disposition: entry.disposition,
            status: entry.status,
            summary: entry.summary.clone(),
            completed_at_unix_ms: entry.completed_at_unix_ms,
            origin: entry.origin.clone(),
        })
        .collect::<Vec<_>>();
    let next_seq = results.last().map_or(after_seq, |entry| entry.seq);
    ResultsPage {
        results,
        next_seq,
        oldest_seq: log.oldest_seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScopeRule;
    use crate::runtime::registry::{KillOutcome, WorkKind};

    const SENDER: &str = "+15550001111";
    const CHANNEL: &str = "wacli";
    const LABEL: &str = "workstation";

    /// Backdate a session's last pull, so the reporting boundary can be
    /// exercised without waiting [`SILENT_AFTER`] out in real time. Test-only:
    /// production has no way to move this clock, which is the point.
    fn backdate_last_poll(session_id: &str, by: Duration) {
        if let Some(session) = find(session_id) {
            let mut state = session.state.lock();
            state.last_poll = state.last_poll.and_then(|last| last.checked_sub(by));
        }
    }

    fn policy_with(rules: Vec<ScopeRule>, owners: &[&str]) -> SecurityPolicy {
        SecurityPolicy {
            scope_rules: rules,
            assign_owners: owners.iter().map(|entry| (*entry).to_string()).collect(),
            ..SecurityPolicy::default()
        }
    }

    fn correspondent() -> AssignPrincipal {
        AssignPrincipal::Correspondent {
            channel: CHANNEL.to_string(),
            sender: SENDER.to_string(),
            chat_type: "direct".to_string(),
        }
    }

    fn allow_all_rule() -> ScopeRule {
        ScopeRule {
            assign_allow: vec!["*".to_string()],
            ..ScopeRule::default()
        }
    }

    fn registered() -> Registration {
        register(LABEL, Some(4242)).expect("test: registration")
    }

    fn queue(policy: &SecurityPolicy, principal: &AssignPrincipal, session: &str, task: &str) -> AssignmentReceipt {
        assign(policy, principal, session, task, Disposition::Queue).expect("test: assignment accepted")
    }

    fn row_for(work_id: WorkId) -> Option<crate::runtime::registry::WorkSnapshot> {
        crate::runtime::registry::snapshot(work_id)
    }

    fn results_for(assignment_id: &str) -> Vec<(ResultStatus, String)> {
        results_after(0, RESULT_LOG_CAPACITY)
            .results
            .into_iter()
            .filter(|result| result.assignment_id == assignment_id)
            .map(|result| (result.status, result.summary))
            .collect()
    }

    // ── Authorization ───────────────────────────────────────────────────────

    /// Default-deny, and a refusal that names nobody.
    ///
    /// Both halves matter. The deny is the policy; the redaction is what stops
    /// this from becoming the one place an identifier leaks in plaintext, which
    /// is exactly the regression T23 had to clean up on the outbound side.
    ///
    /// MUTATION GUARD: skip the `is_assignment_allowed` call in `assign` and the
    /// deny assertion fails; interpolate the sender or the session id into
    /// `ChatSessionError::NotAuthorized` and the redaction assertions fail.
    #[tokio::test]
    async fn an_unauthorized_correspondent_is_refused_and_the_refusal_names_nobody() {
        let session = registered();
        let policy = policy_with(vec![], &[]);

        let error = assign(
            &policy,
            &correspondent(),
            &session.session_id,
            "do a thing",
            Disposition::Queue,
        )
        .expect_err("test: assignment must be denied by default");

        let ChatSessionError::NotAuthorized { .. } = error else {
            panic!("test: expected a NotAuthorized refusal, got {error:?}");
        };
        let text = error.to_string();
        assert!(
            !text.contains(SENDER),
            "the refusal must not echo the plaintext sender: {text}"
        );
        assert!(
            !text.contains(&session.session_id),
            "the refusal must not echo the plaintext session id: {text}"
        );
        // The fingerprints it does carry are the shared ones, so this refusal,
        // the work-registry row and the session listing all speak of the same
        // parties in the same string.
        assert!(
            text.contains(&ref_for_channel_recipient(CHANNEL, SENDER)),
            "test: {text}"
        );
        assert!(text.contains(&session.session_ref), "test: {text}");
        assert!(
            text.contains(CHANNEL),
            "the channel is not sensitive and stays readable: {text}"
        );

        // Nothing reached the mailbox.
        let pulled = pull(&session.session_id, &session.token, &[], None).expect("test: pull");
        assert!(pulled.assignments.is_empty());
        deregister(&session.session_id);
    }

    /// The owner path.
    #[tokio::test]
    async fn an_owner_may_fill_the_mailbox() {
        let session = registered();
        let policy = policy_with(vec![], &[&format!("{CHANNEL}:{SENDER}")]);

        let receipt = queue(&policy, &correspondent(), &session.session_id, "owner task");
        assert_eq!(receipt.queued_ahead, 0);

        let pulled = pull(&session.session_id, &session.token, &[], None).expect("test: pull");
        assert_eq!(pulled.assignments.len(), 1);
        assert_eq!(pulled.assignments.first().map(|a| a.task.as_str()), Some("owner task"));
        deregister(&session.session_id);
    }

    /// The scope-rule path, and that it really is the rule doing the work: the
    /// same principal against a session the rule does not name is refused.
    #[tokio::test]
    async fn an_assign_allow_rule_may_fill_the_mailbox() {
        let named = registered();
        let other = register("laptop", None).expect("test: registration");
        let policy = policy_with(
            vec![ScopeRule {
                channel: Some(CHANNEL.to_string()),
                assign_allow: vec![format!("{LABEL}:*")],
                ..ScopeRule::default()
            }],
            &[],
        );

        queue(&policy, &correspondent(), &named.session_id, "granted");
        let refused = assign(
            &policy,
            &correspondent(),
            &other.session_id,
            "not granted",
            Disposition::Queue,
        )
        .expect_err("test: a session the rule does not name stays denied");
        assert!(matches!(refused, ChatSessionError::NotAuthorized { .. }));

        assert_eq!(
            pull(&named.session_id, &named.token, &[], None)
                .expect("test: pull")
                .assignments
                .len(),
            1
        );
        assert!(
            pull(&other.session_id, &other.token, &[], None)
                .expect("test: pull")
                .assignments
                .is_empty()
        );
        deregister(&named.session_id);
        deregister(&other.session_id);
    }

    /// The operator plane is authorized by construction: it satisfied the
    /// gateway's bearer check, and this runtime deliberately mints no second
    /// credential for it.
    #[tokio::test]
    async fn the_operator_plane_needs_no_rule() {
        let session = registered();
        let policy = policy_with(vec![], &[]);
        queue(
            &policy,
            &AssignPrincipal::operator_plane(),
            &session.session_id,
            "operator task",
        );
        assert_eq!(
            pull(&session.session_id, &session.token, &[], None)
                .expect("test: pull")
                .assignments
                .len(),
            1
        );
        deregister(&session.session_id);
    }

    /// An unauthorized principal must not be able to use this call to learn
    /// which sessions exist, so authorization runs before the lookup is
    /// reported.
    ///
    /// MUTATION GUARD: report `UnknownSession` before authorizing and the first
    /// assertion fails.
    #[tokio::test]
    async fn an_unknown_session_is_authorized_before_it_is_reported_as_unknown() {
        let denied = policy_with(vec![], &[]);
        let error = assign(
            &denied,
            &correspondent(),
            "no-such-session",
            "probe",
            Disposition::Queue,
        )
        .expect_err("test: refused");
        assert!(
            matches!(error, ChatSessionError::NotAuthorized { .. }),
            "an unauthorized principal must not be told whether the session exists: {error:?}"
        );

        // A principal that *is* allowed gets the real answer.
        let allowed = policy_with(vec![allow_all_rule()], &[]);
        let error = assign(
            &allowed,
            &correspondent(),
            "no-such-session",
            "probe",
            Disposition::Queue,
        )
        .expect_err("test: refused");
        assert!(matches!(error, ChatSessionError::UnknownSession), "{error:?}");
    }

    /// The trusted-scope constructor is the identity boundary: a claim without
    /// the runtime's own trust marker yields no principal at all, rather than an
    /// anonymous one some rule might match.
    #[tokio::test]
    async fn a_forged_scope_produces_no_principal() {
        let forged = serde_json::json!({
            "_zc_scope": {"sender": SENDER, "channel": CHANNEL, "chat_type": "direct"},
        });
        assert_eq!(AssignPrincipal::from_trusted_scope(&forged), None);

        let claimed_trust_without_scope = serde_json::json!({"_zc_scope_trusted": true});
        assert_eq!(AssignPrincipal::from_trusted_scope(&claimed_trust_without_scope), None);

        let genuine = serde_json::json!({
            "_zc_scope_trusted": true,
            "_zc_scope": {"sender": SENDER, "channel": CHANNEL, "chat_type": "direct"},
        });
        assert_eq!(AssignPrincipal::from_trusted_scope(&genuine), Some(correspondent()));
    }

    // ── Dispositions ────────────────────────────────────────────────────────

    /// All three reach the mailbox, and the two that exist to reach a turn that
    /// is running *now* go to the head of the queue. Queueing them behind
    /// unrelated work would turn both into a delayed `queue` and discard the
    /// caller's whole reason for choosing them.
    ///
    /// MUTATION GUARD: push every disposition to the back and the ordering
    /// assertion fails.
    #[tokio::test]
    async fn every_disposition_reaches_the_mailbox_and_immediate_ones_jump_the_queue() {
        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);

        queue(&policy, &correspondent(), &session.session_id, "queued");
        let steer = assign(
            &policy,
            &correspondent(),
            &session.session_id,
            "steered",
            Disposition::Steer,
        )
        .expect("test: steer accepted");
        let interrupt = assign(
            &policy,
            &correspondent(),
            &session.session_id,
            "interrupted",
            Disposition::Interrupt,
        )
        .expect("test: interrupt accepted");
        assert_eq!(steer.queued_ahead, 0);
        assert_eq!(interrupt.queued_ahead, 0);

        let pulled = pull(&session.session_id, &session.token, &[], Some(8)).expect("test: pull");
        let order = pulled
            .assignments
            .iter()
            .map(|assignment| (assignment.disposition, assignment.task.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                (Disposition::Interrupt, "interrupted"),
                (Disposition::Steer, "steered"),
                (Disposition::Queue, "queued"),
            ]
        );
        deregister(&session.session_id);
    }

    #[tokio::test]
    async fn an_unknown_disposition_is_rejected_rather_than_defaulted() {
        assert_eq!(Disposition::parse("queue"), Some(Disposition::Queue));
        assert_eq!(Disposition::parse("steer"), Some(Disposition::Steer));
        assert_eq!(Disposition::parse("interrupt"), Some(Disposition::Interrupt));
        assert_eq!(Disposition::parse("kill"), None);
        assert_eq!(Disposition::parse(""), None);
    }

    // ── Delivery guarantee ──────────────────────────────────────────────────

    /// A pull that the caller never acknowledged is redelivered, not lost.
    ///
    /// This is the whole ack contract: the response may be dropped in flight or
    /// the puller may die between receiving and handling it, and neither may
    /// consume the work. Reclaiming on the *next pull* is what replaces the
    /// visibility timeout a queue would normally use — evidence, not a clock.
    ///
    /// MUTATION GUARD: retire an assignment when it is handed out instead of
    /// when it is acknowledged, and the redelivery assertion fails.
    #[tokio::test]
    async fn an_unacknowledged_pull_is_redelivered_rather_than_lost() {
        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);
        let receipt = queue(&policy, &correspondent(), &session.session_id, "must survive");

        let first = pull(&session.session_id, &session.token, &[], None).expect("test: first pull");
        assert_eq!(first.assignments.len(), 1);
        assert_eq!(first.assignments.first().map(|a| a.deliveries), Some(1));
        assert_eq!(first.requeued, 0);

        // The caller vanished without acknowledging. The next pull hands the
        // same assignment back, flagged as a redelivery.
        let second = pull(&session.session_id, &session.token, &[], None).expect("test: second pull");
        assert_eq!(second.requeued, 1);
        assert_eq!(second.assignments.len(), 1);
        assert_eq!(
            second.assignments.first().map(|a| a.assignment_id.as_str()),
            Some(receipt.assignment_id.as_str())
        );
        assert_eq!(
            second.assignments.first().map(|a| a.deliveries),
            Some(2),
            "a redelivery must announce itself so the puller can dedupe on the assignment id"
        );

        // Once acknowledged it stops coming back, and it is still outstanding
        // rather than forgotten — the session owes a result for it.
        let third = pull(
            &session.session_id,
            &session.token,
            std::slice::from_ref(&receipt.assignment_id),
            None,
        )
        .expect("test: third pull");
        assert_eq!(third.acked, 1);
        assert_eq!(third.requeued, 0);
        assert!(third.assignments.is_empty());

        let fourth = pull(&session.session_id, &session.token, &[], None).expect("test: fourth pull");
        assert!(
            fourth.assignments.is_empty(),
            "an acknowledged assignment must not be redelivered"
        );
        assert!(row_for(receipt.work_id).is_some(), "it is still outstanding work");
        deregister(&session.session_id);
    }

    /// A batch bigger than the ceiling is not lost: the remainder stays at the
    /// head of the queue and `queued_remaining` says so.
    #[tokio::test]
    async fn a_pull_hands_over_at_most_the_requested_batch() {
        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);
        for index in 0..3 {
            queue(&policy, &correspondent(), &session.session_id, &format!("task {index}"));
        }

        let first = pull(&session.session_id, &session.token, &[], Some(2)).expect("test: pull");
        assert_eq!(first.assignments.len(), 2);
        assert_eq!(first.queued_remaining, 1);
        assert_eq!(first.assignments.first().map(|a| a.task.as_str()), Some("task 0"));
        deregister(&session.session_id);
    }

    #[tokio::test]
    async fn a_wrong_session_token_cannot_drain_the_mailbox() {
        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);
        queue(&policy, &correspondent(), &session.session_id, "private work");

        assert_eq!(
            pull(&session.session_id, "not-the-token", &[], None).expect_err("test: refused"),
            ChatSessionError::BadToken
        );
        assert_eq!(
            pull(&session.session_id, "", &[], None).expect_err("test: refused"),
            ChatSessionError::BadToken
        );
        // The rightful holder still gets it.
        assert_eq!(
            pull(&session.session_id, &session.token, &[], None)
                .expect("test: pull")
                .assignments
                .len(),
            1
        );
        deregister(&session.session_id);
    }

    // ── Visibility and the kill path ────────────────────────────────────────

    /// A queued assignment is listed in the work registry the whole time it
    /// waits, and killing that row really removes it from the mailbox.
    ///
    /// Once nothing expires on its own, being *seen* is the precondition for
    /// being ended — and a row that can be retired while the work stays queued
    /// would be a visible lie rather than an invisible hang.
    ///
    /// MUTATION GUARD: drop the `register_sub_agent` call and the listing
    /// assertion fails; drop `watch_for_kill` and the kill is reported as
    /// merely requested while the assignment is still delivered.
    #[tokio::test]
    async fn a_queued_assignment_is_listed_and_a_kill_discards_it() {
        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);
        let receipt = queue(&policy, &correspondent(), &session.session_id, "kill me");

        let row = row_for(receipt.work_id).expect("test: the assignment must be listed while it waits");
        assert_eq!(row.kind, WorkKind::SubAgent);
        assert_eq!(
            row.parent, None,
            "an assignment outlives the turn that requested it, so it is a lineage root"
        );
        assert_eq!(
            row.run_id.as_deref(),
            Some(receipt.assignment_id.as_str()),
            "its run id is its own assignment id, never one borrowed from the requester"
        );

        let outcomes = crate::runtime::registry::kill(receipt.work_id, true).await;
        assert_eq!(
            outcomes.first().map(|result| result.outcome),
            Some(KillOutcome::Killed),
            "the kill must be confirmed by the row leaving the registry, not merely issued"
        );

        assert!(row_for(receipt.work_id).is_none());
        assert!(
            pull(&session.session_id, &session.token, &[], None)
                .expect("test: pull")
                .assignments
                .is_empty(),
            "a killed assignment must not still be delivered"
        );
        assert_eq!(
            results_for(&receipt.assignment_id),
            vec![(
                ResultStatus::Cancelled,
                "the assignment's work item was killed by an operator".to_string()
            )],
            "whoever asked for the work must be able to learn it will not arrive"
        );
        deregister(&session.session_id);
    }

    /// The row names the channel in the clear and both parties only as the
    /// shared fingerprint, and never carries the task body.
    ///
    /// MUTATION GUARD: interpolate the session id, the sender or the task into
    /// the work name and this test fails.
    #[tokio::test]
    async fn an_assignment_row_names_the_channel_but_fingerprints_the_parties() {
        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);
        let receipt = assign(
            &policy,
            &correspondent(),
            &session.session_id,
            "a secret instruction",
            Disposition::Interrupt,
        )
        .expect("test: assignment accepted");

        let row = row_for(receipt.work_id).expect("test: listed");
        let name = row.name.to_string();
        assert!(!name.contains(SENDER), "plaintext sender in a registry row: {name}");
        assert!(
            !name.contains(&session.session_id),
            "plaintext session id in a registry row: {name}"
        );
        assert!(
            !name.contains("a secret instruction"),
            "the task body must never enter the registry: {name}"
        );
        assert!(
            name.contains(&ref_for_channel_recipient(CHANNEL, SENDER)),
            "test: {name}"
        );
        assert!(name.contains(&session.session_ref), "test: {name}");
        assert!(name.contains(CHANNEL), "the channel stays readable: {name}");
        assert!(name.contains("interrupt"), "the disposition stays readable: {name}");

        deregister(&session.session_id);
    }

    /// An operator-plane assignment has no identity to protect, so its row says
    /// so plainly rather than fingerprinting a constant.
    #[tokio::test]
    async fn an_operator_plane_row_names_the_plane_itself() {
        let session = registered();
        let policy = policy_with(vec![], &[]);
        let receipt = queue(&policy, &AssignPrincipal::operator_plane(), &session.session_id, "task");
        let row = row_for(receipt.work_id).expect("test: listed");
        assert!(
            row.name.contains(&format!("from {OPERATOR_PLANE_CHANNEL}")),
            "{}",
            row.name
        );
        deregister(&session.session_id);
    }

    // ── Results ─────────────────────────────────────────────────────────────

    /// Reporting a result retires the registry row and puts the outcome on the
    /// feed, with the origin still recoverable in-process so it can be answered.
    #[tokio::test]
    async fn a_result_retires_the_row_and_reaches_the_feed() {
        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);
        let receipt = queue(&policy, &correspondent(), &session.session_id, "report me");
        pull(&session.session_id, &session.token, &[], None).expect("test: pull");

        let seq = report(
            &session.session_id,
            &session.token,
            &receipt.assignment_id,
            ResultStatus::Completed,
            "done",
        )
        .expect("test: result accepted");
        assert!(seq > 0);
        assert!(
            row_for(receipt.work_id).is_none(),
            "a finished assignment retires its row"
        );

        let entry = results_after(seq - 1, 1)
            .results
            .into_iter()
            .find(|result| result.assignment_id == receipt.assignment_id)
            .expect("test: the result must reach the feed");
        assert_eq!(entry.status, ResultStatus::Completed);
        assert_eq!(entry.summary, "done");
        assert_eq!(entry.session_label, LABEL);
        assert_eq!(
            entry.origin,
            correspondent(),
            "the in-process feed keeps the origin so the answer can be routed back"
        );

        // Reporting twice is refused rather than duplicated.
        assert_eq!(
            report(
                &session.session_id,
                &session.token,
                &receipt.assignment_id,
                ResultStatus::Completed,
                "again",
            )
            .expect_err("test: refused"),
            ChatSessionError::UnknownAssignment
        );
        deregister(&session.session_id);
    }

    /// `cancelled` is this runtime's own verdict about work it ended, so a
    /// session cannot claim it and make an operator kill look like a client
    /// giving up.
    #[tokio::test]
    async fn a_session_cannot_report_a_cancellation() {
        assert_eq!(ResultStatus::parse("completed"), Some(ResultStatus::Completed));
        assert_eq!(ResultStatus::parse("failed"), Some(ResultStatus::Failed));
        assert_eq!(ResultStatus::parse("rejected"), Some(ResultStatus::Rejected));
        assert_eq!(ResultStatus::parse("cancelled"), None);
    }

    /// Deregistering does not silently swallow outstanding work.
    #[tokio::test]
    async fn deregistering_cancels_outstanding_work_visibly() {
        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);
        let receipt = queue(&policy, &correspondent(), &session.session_id, "orphaned");

        assert_eq!(deregister(&session.session_id), Some(1));
        assert!(row_for(receipt.work_id).is_none());
        assert_eq!(
            results_for(&receipt.assignment_id)
                .into_iter()
                .map(|(status, _)| status)
                .collect::<Vec<_>>(),
            vec![ResultStatus::Cancelled]
        );
        assert_eq!(deregister(&session.session_id), None);
    }

    // ── Liveness reporting ──────────────────────────────────────────────────

    #[tokio::test]
    async fn liveness_is_classified_from_the_last_pull() {
        assert_eq!(liveness_for(None), ChatLiveness::NeverPolled);
        assert_eq!(liveness_for(Some(Duration::from_secs(1))), ChatLiveness::Polling);
        assert_eq!(liveness_for(Some(SILENT_AFTER)), ChatLiveness::Polling);
        assert_eq!(
            liveness_for(Some(SILENT_AFTER + Duration::from_secs(1))),
            ChatLiveness::Silent
        );
    }

    /// A session that has gone quiet is *reported* as quiet and left entirely
    /// alone: still registered, still holding its queue, still able to come
    /// back. Evicting it would discard queued work on a guess about duration,
    /// which is exactly the wall-clock mistake this runtime does not make.
    ///
    /// MUTATION GUARD: evict a silent session anywhere in `list` and the
    /// "still listed" or "its queue survived" assertion fails.
    #[tokio::test]
    async fn a_silent_session_is_reported_and_never_evicted() {
        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);
        queue(
            &policy,
            &correspondent(),
            &session.session_id,
            "waiting for a quiet chat",
        );

        let listed = |id: &str| {
            list()
                .into_iter()
                .find(|view| view.session_id == id)
                .expect("test: the session must stay listed")
        };
        assert_eq!(listed(&session.session_id).liveness, ChatLiveness::NeverPolled);
        assert_eq!(listed(&session.session_id).queued, 1);

        // One pull, then a long silence.
        pull(&session.session_id, &session.token, &[], Some(1)).expect("test: pull");
        assert_eq!(listed(&session.session_id).liveness, ChatLiveness::Polling);
        backdate_last_poll(&session.session_id, SILENT_AFTER + Duration::from_mins(1));

        let view = listed(&session.session_id);
        assert_eq!(view.liveness, ChatLiveness::Silent);
        assert!(
            view.last_poll_age_secs.is_some_and(|age| age > SILENT_AFTER.as_secs()),
            "the raw age is reported too, so an operator can draw their own line: {:?}",
            view.last_poll_age_secs
        );
        assert_eq!(view.delivered, 1, "its outstanding work is untouched");

        // And it can simply come back.
        pull(&session.session_id, &session.token, &[], None).expect("test: pull");
        assert_eq!(listed(&session.session_id).liveness, ChatLiveness::Polling);
        deregister(&session.session_id);
    }

    // ── Boundaries ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_and_oversized_fields_are_refused_at_the_boundary() {
        assert!(matches!(register("  ", None), Err(ChatSessionError::Invalid(_))));
        assert!(matches!(
            register(&"x".repeat(MAX_LABEL_LEN + 1), None),
            Err(ChatSessionError::Invalid(_))
        ));

        let session = registered();
        let policy = policy_with(vec![allow_all_rule()], &[]);
        assert!(matches!(
            assign(
                &policy,
                &correspondent(),
                &session.session_id,
                "   ",
                Disposition::Queue
            ),
            Err(ChatSessionError::Invalid(_))
        ));
        assert!(matches!(
            assign(
                &policy,
                &correspondent(),
                &session.session_id,
                &"x".repeat(MAX_TASK_LEN + 1),
                Disposition::Queue,
            ),
            Err(ChatSessionError::Invalid(_))
        ));
        deregister(&session.session_id);
    }

    #[tokio::test]
    async fn an_unknown_session_cannot_be_pulled_or_reported() {
        assert_eq!(
            pull("no-such-session", "token", &[], None).expect_err("test: refused"),
            ChatSessionError::UnknownSession
        );
        assert_eq!(
            report(
                "no-such-session",
                "token",
                "assignment",
                ResultStatus::Completed,
                "summary",
            )
            .expect_err("test: refused"),
            ChatSessionError::UnknownSession
        );
    }
}
