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
//! # Persistence
//!
//! The mailbox **survives a daemon restart.** Every accepted mutation is written
//! through to `{workspace}/state/chat-mailbox.db` before the caller is answered
//! — registrations, queued assignments, acknowledgements, deregistrations and
//! results — and [`attach_store`] replays the whole thing on the way up. A chat
//! that was registered before the restart keeps pulling with the *same* session
//! id and the same token: only the token's SHA-256 is stored, so the comparison
//! that authenticates it is the comparison it always was.
//!
//! Two properties are worth stating because they are choices, not consequences:
//!
//! * **A failed write never fails an assignment.** The store is how work
//!   survives a restart; the mailbox is how a message on a phone reaches a
//!   terminal. Refusing live work because a disk is full would trade a
//!   recoverable durability gap for an outage, so a write that fails is logged
//!   at `error` and the in-memory queue carries on. `assign` answering "accepted"
//!   therefore means "queued, and durable unless the log says otherwise".
//! * **Work handed out and not acknowledged goes back to the head of the
//!   queue** on hydration, exactly as it does on the next pull (see [`pull`]).
//!   Work that *was* acknowledged stays acknowledged: the session that holds it
//!   may well have survived the daemon, and redelivering it would be this
//!   runtime deciding a live chat had lost work it is still doing.
//!
//! Three properties keep that last sentence true under concurrency, and each is
//! a rule rather than an accident:
//!
//! * **Writes are ordered by revision, not by arrival.** A mutation is planned
//!   while a session's state lock is held and written after it is released, so
//!   two overlapping pulls can reach the file in the opposite order to the
//!   transitions that produced them. Every write therefore carries the
//!   monotonic revision it was planned at, and the store refuses one that is
//!   older than the row already holds. Without it a delivery planned before an
//!   acknowledgement could land after it, and the next restart would hand
//!   acknowledged work back out.
//! * **Retirement and result are one commit**, as are a deregistration and the
//!   cancellations that cascade with it. A failure between two separate writes
//!   would take work out of every queue while leaving no record of it.
//! * **A report is idempotent.** A chat whose report lost its response retries
//!   it, and the retry is answered with the sequence the first one recorded
//!   rather than refused as unknown work.
//!
//! # What is deliberately not here
//!
//! * **Delivery to the origin.** This module records *who asked* and *what came
//!   back*; it never sends anything. Routing a result back to a channel is an
//!   outbound action and must go through the outbound gates, which is a
//!   deliberate separation — see [`AssignmentResult::origin`].
//! * **Multi-host.** Chat and daemon share a machine and talk over the loopback
//!   gateway.

use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, MutexGuard, RwLock};
use sha2::{Digest, Sha256};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::runtime::registry::{WorkGuard, WorkId};
use crate::runtime::sqlite_pool::{BOUNDED_BUSY_WAIT, BoundedBusyBudget, bounded_busy_remaining};
use crate::security::op_id::ref_for_channel_recipient;
use crate::security::policy::SecurityPolicy;

mod reporting;
mod store;

pub use reporting::{report, results_after};
pub use store::{
    MailboxStore, Queue, STORED_RESULT_CAPACITY, SqliteMailboxStore, StoredAssignment, StoredResult, StoredSession,
    StoredState,
};

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
    /// The assignment was ended from outside — an operator killed its work row
    /// — before this report arrived. The cancellation is already in the result
    /// feed, so the report is refused rather than recorded as a second, and
    /// contradictory, completion.
    AssignmentCancelled { summary: String },
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
            Self::AssignmentCancelled { .. } => "assignment_cancelled",
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
            Self::AssignmentCancelled { summary } => write!(
                f,
                "that assignment was cancelled before this report arrived ({summary}); the cancellation is already \
                 in the result feed and this report was not recorded"
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

/// Evidence of liveness: when the session last actually pulled.
///
/// Two variants because the evidence has two possible provenances and they are
/// not interchangeable. A pull this process saw is a monotonic reading, so a
/// clock adjustment cannot make a live session look silent. A pull recorded by
/// the *previous* process cannot be an `Instant` at all — a monotonic clock has
/// no name for a moment before this process started — so it is read back as the
/// wall-clock stamp it was stored as.
///
/// What must never happen is a restored session reading as fresh: it would claim
/// liveness for a chat that has not been heard from since before the restart.
/// Hence a stamp rather than `None`, which would say `never_polled` and, worse,
/// `Instant::now()`, which would say `polling`.
#[derive(Debug, Clone, Copy)]
enum LastPoll {
    /// A pull this process handled.
    Observed(Instant),
    /// A pull the store remembers from before this process started.
    Restored(u64),
}

impl LastPoll {
    /// How long ago this pull happened, as the listing reports it.
    fn age(self, now: Instant, now_unix_ms: u64) -> Duration {
        match self {
            Self::Observed(last) => now.saturating_duration_since(last),
            Self::Restored(at) => Duration::from_millis(now_unix_ms.saturating_sub(at)),
        }
    }
}

struct SessionState {
    /// When the session last actually pulled, or `None` if it never has.
    last_poll: Option<LastPoll>,
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

/// What [`ResultLog::insert`] did with an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
enum Admission {
    /// On the feed.
    Added,
    /// That sequence is already on the feed.
    Duplicate,
    /// Below the feed's floor: treated as already trimmed.
    BelowFloor,
}

struct ResultLog {
    entries: VecDeque<AssignmentResult>,
    next_seq: u64,
    oldest_seq: u64,
}

impl ResultLog {
    const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            // Sequence 0 is never used, so `after_seq=0` means "from the start"
            // without a sentinel that a real entry could collide with.
            next_seq: 1,
            oldest_seq: 1,
        }
    }

    /// Reserve the next sequence.
    ///
    /// Taken before the store is asked and published after, so this log's lock
    /// is never held across a database wait. The caller holds
    /// `ChatSessions::publication` over that whole span, which is what makes
    /// publications arrive in sequence order; [`Self::insert`] merges by
    /// sequence for the replay path, where the same record can come twice.
    const fn mint(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// Merge one result into the feed by its sequence.
    ///
    /// A *merge*, not an append, because the feed is written from two places
    /// that can both offer the same record: a live completion, and a replay of
    /// the stored feed when a store is attached. The gateway supervisor
    /// re-attaches on every in-process gateway restart, so appending blindly
    /// duplicated every persisted result — and duplicates break exactly what
    /// [`ResultsPage`] promises, an ordered feed whose sequences a consumer can
    /// use as a cursor.
    ///
    /// Says what became of the entry. A replay is expected to be refused
    /// often; a live publication never should be, and its caller logs it.
    fn insert(&mut self, result: AssignmentResult) -> Admission {
        if result.seq < self.oldest_seq {
            // Already trimmed. Re-admitting it would move the feed's floor
            // backwards and tell a consumer it had *not* missed entries it had.
            return Admission::BelowFloor;
        }
        let seq = result.seq;
        match self.entries.binary_search_by(|entry| entry.seq.cmp(&seq)) {
            Ok(_) => return Admission::Duplicate,
            Err(index) => self.entries.insert(index, result),
        }
        // The sequence keeps climbing across restarts. Restarting it at 1 would
        // hand a consumer that cached a cursor a feed whose new entries look
        // older than the ones it already saw.
        self.next_seq = self.next_seq.max(seq.saturating_add(1));
        self.trim();
        Admission::Added
    }

    /// Drop the oldest entries past the buffer's capacity and move the floor
    /// with them, so a consumer that fell behind can tell.
    fn trim(&mut self) {
        while self.entries.len() > RESULT_LOG_CAPACITY {
            if let Some(dropped) = self.entries.pop_front() {
                self.oldest_seq = dropped.seq.saturating_add(1);
            }
        }
        if let Some(first) = self.entries.front() {
            self.oldest_seq = self.oldest_seq.max(first.seq);
        }
    }
}

/// The right to mint and publish results: held from a sequence's mint until
/// that result is on the feed.
///
/// [`ResultLog`] merges by sequence and the store write between mint and
/// publish can take any time, so without a turn two concurrent reports could
/// publish in the opposite order to their sequences — see
/// `ChatSessions::publication` for what that cost. The ordering is carried by
/// the types: [`Self::mint`] hands back a [`Minted`] that borrows this turn,
/// and only [`Self::publish`] consumes it, so a result minted under a turn
/// cannot be published after that turn has ended.
///
/// # Bounded by the contention budget
///
/// The turn opens the thread's [`BoundedBusyBudget`] *before* it queues for
/// the lock, so the wait for the lock and the store write share one budget.
/// A writer that waits the whole budget out still asks the store, with no
/// budget left: the write fails fast while the database is busy — counted by
/// the store as the failure it is — and lands when the database has come
/// free. It still mints and publishes in order. Whoever holds the lock is
/// inside its own budget, so K writers behind a stuck database give up in
/// about one budget rather than K, and the store counts K failures, not one.
struct PublicationTurn<'a> {
    sessions: &'a ChatSessions,
    _order: MutexGuard<'a, ()>,
    /// False when waiting for the turn spent the whole budget. The store is
    /// still asked; this only selects the warning that a write made with no
    /// budget left may not be durable.
    durable: bool,
    /// Declared after the guard so it is dropped after it: the budget
    /// outlives the turn it bounds.
    _budget: BoundedBusyBudget,
}

/// A result minted under a [`PublicationTurn`], publishable only through it.
struct Minted<'turn> {
    entry: AssignmentResult,
    _turn: PhantomData<&'turn ()>,
}

impl<'a> PublicationTurn<'a> {
    fn take(sessions: &'a ChatSessions) -> Self {
        let budget = BoundedBusyBudget::start();
        let wait = bounded_busy_remaining();
        if let Some(order) = sessions.publication.try_lock_for(wait) {
            return Self {
                sessions,
                _order: order,
                durable: true,
                _budget: budget,
            };
        }
        // The budget is gone. Still take the turn — the order is what keeps a
        // result from being refused as already trimmed — but no longer wait
        // on the database: this wait now lasts only as long as the holders'
        // own budgets, each of which started before this one expired. Nothing
        // is logged yet: the caller may turn out to owe the store nothing (a
        // replayed report), and [`Self::write`] warns when it does.
        Self {
            sessions,
            _order: sessions.publication.lock(),
            durable: false,
            _budget: budget,
        }
    }

    /// Reserve a sequence and build the two forms of one finished assignment:
    /// the row to persist, and the entry to publish once the store has been
    /// asked.
    ///
    /// The result log's own lock is taken only for the mint and again for the
    /// publish, never across the database wait, so feed readers never wait on
    /// the store.
    fn mint(
        &self,
        session: &Session,
        assignment: &Assignment,
        status: ResultStatus,
        summary: String,
    ) -> (StoredResult, Minted<'_>) {
        let seq = self.sessions.results.lock().mint();
        let completed_at_unix_ms = unix_ms();
        let stored = StoredResult {
            seq,
            assignment_id: assignment.id.clone(),
            session_id: session.id.clone(),
            session_label: session.label.clone(),
            disposition: assignment.disposition,
            status,
            summary: summary.clone(),
            completed_at_unix_ms,
            origin: assignment.origin.clone(),
        };
        let entry = AssignmentResult {
            seq,
            assignment_id: assignment.id.clone(),
            session_id: session.id.clone(),
            session_label: session.label.clone(),
            disposition: assignment.disposition,
            status,
            summary,
            completed_at_unix_ms,
            origin: assignment.origin.clone(),
        };
        (
            stored,
            Minted {
                entry,
                _turn: PhantomData,
            },
        )
    }

    /// Write through to the store inside this turn's budget.
    ///
    /// Always asks the store, even when the budget was spent waiting for the
    /// turn. The nested budget then joins the expired one, so the store's
    /// connection and busy waits give up at once: the write costs
    /// milliseconds, fails with the store's own counted error while the
    /// database is busy, and lands when it has come free. Skipping the store
    /// instead would leave its failure counters — and the `durable` flag the
    /// operator reads off the pool report — claiming no result was lost.
    fn write(&self, operation: &'static str, write: impl FnOnce(&dyn MailboxStore) -> anyhow::Result<()>) {
        if !self.durable && self.sessions.store().is_some() {
            tracing::warn!(
                operation,
                budget_ms = u64::try_from(BOUNDED_BUSY_WAIT.as_millis()).unwrap_or(u64::MAX),
                "chat assignment results are queued behind a slow mailbox store; this mutation may not be durable"
            );
        }
        self.sessions.write_through(operation, write);
    }

    /// Put one finished assignment on the live feed. Dropping the assignment
    /// afterwards retires its registry row.
    ///
    /// A live result is never expected to be refused: under the turn every
    /// sequence lands above the floor and exactly once. If one is, the
    /// return trip waiting for it will report it lost, so the refusal is
    /// logged — by sequence and assignment, never by recipient or body.
    fn publish(&self, minted: Minted<'_>) -> u64 {
        let seq = minted.entry.seq;
        let assignment_id = minted.entry.assignment_id.clone();
        let admission = self.sessions.results.lock().insert(minted.entry);
        if admission != Admission::Added {
            tracing::warn!(
                seq,
                assignment = %assignment_id,
                admission = ?admission,
                "a live chat assignment result was refused by the result feed; its return trip will report it lost"
            );
        }
        seq
    }
}

/// One registry and one mailbox.
///
/// A type with methods rather than a bag of free functions over a global,
/// because persistence has to be testable: "restart the daemon" is expressed as
/// *a second instance opened on the same database*, which is only possible if a
/// second instance can exist at all. The process still has exactly one — the
/// free functions below all delegate to [`SESSIONS`] — and nothing outside this
/// module can build another.
struct ChatSessions {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    results: Mutex<ResultLog>,
    /// Held from minting a result's sequence until that result is on the feed.
    ///
    /// [`ResultLog`] is a merge by sequence, and the store write between mint
    /// and publish can take any time, so without this two concurrent reports
    /// could publish in the opposite order to their sequences. A later
    /// sequence landing first moved the feed's floor past the earlier one,
    /// which was then refused as already trimmed — a result silently gone from
    /// the live feed while its assignment row retired, which the return trip
    /// reads as an eviction — and it walked any cursor past the earlier one
    /// for good. Holding this across the write keeps publication in sequence
    /// order and every result readable the moment its report returns.
    ///
    /// Taken through [`PublicationTurn`], which bounds the wait by the
    /// store's contention budget and ties every minted result to the turn
    /// that must publish it — except by `hydrate`, which runs once at attach
    /// and takes it directly so a replayed floor cannot land between a live
    /// report's mint and its publish.
    ///
    /// Separate from `results` on purpose: readers of the feed never wait on
    /// a database, only other result writers do.
    publication: Mutex<()>,
    /// Monotonic revision stamped on every store write.
    ///
    /// Writes are planned while a session's state lock is held and executed
    /// after it is released — deliberately, so a contended database cannot
    /// freeze a session — which means two overlapping pulls can reach the file
    /// in the opposite order to the transitions that produced them. The
    /// revision is what the store compares to refuse the older one; see
    /// [`MailboxStore`].
    revision: AtomicU64,
    /// Where mutations are written through. `None` until a gateway attaches one,
    /// which is what keeps every unit test that never asks for persistence on
    /// the in-memory behaviour it was written against.
    store: RwLock<Option<Arc<dyn MailboxStore>>>,
}

static SESSIONS: LazyLock<Arc<ChatSessions>> = LazyLock::new(|| Arc::new(ChatSessions::new()));

impl ChatSessions {
    fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            results: Mutex::new(ResultLog::new()),
            publication: Mutex::new(()),
            revision: AtomicU64::new(0),
            store: RwLock::new(None),
        }
    }

    /// Stamp for the next store write. Minted while the state lock that
    /// produced the transition is held, so revision order is transition order.
    fn next_revision(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::SeqCst).saturating_add(1)
    }

    fn store(&self) -> Option<Arc<dyn MailboxStore>> {
        self.store.read().clone()
    }

    /// Run one write-through, and let the mailbox carry on if it fails.
    ///
    /// The whole availability trade-off lives in these four lines: a store that
    /// refuses a write costs durability for that mutation and nothing else. It
    /// is `error` rather than `warn` because an operator who has lost durability
    /// has lost the property this module advertises, and must be able to find
    /// out from the log rather than from a restart.
    fn write_through(&self, operation: &'static str, write: impl FnOnce(&dyn MailboxStore) -> anyhow::Result<()>) {
        let Some(store) = self.store() else {
            return;
        };
        // One contention budget for this mutation, or — when the caller already
        // opened one for a whole request — a share of that one. Without it the
        // bound is per statement, and a request that owes the database four
        // writes waits out the bound four times.
        let _budget = BoundedBusyBudget::start();
        if let Err(error) = write(store.as_ref()) {
            write_through_failed(operation, &error);
        }
    }

    fn find(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions.read().get(session_id.trim()).map(Arc::clone)
    }
}

/// The one log line for a mutation that did not reach the store.
fn write_through_failed(operation: &'static str, error: &anyhow::Error) {
    tracing::error!(operation, error = %crate::security::redaction::redact_provider_text(&format!("{error:#}")), "chat mailbox write-through failed");
}

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

// ── Persistence ─────────────────────────────────────────────────────────────

/// Give the process mailbox a store and replay what it holds.
///
/// Called from the gateway's startup path, which is the one place both launch
/// modes (`prx gateway` and the daemon's supervised gateway) pass through. It is
/// **idempotent**: the daemon's supervisor restarts the gateway in-process on
/// failure, and a second call must not duplicate sessions that are still live in
/// memory, so hydration skips any id already registered.
///
/// Attaching a store that cannot be read is not fatal. The mailbox runs from
/// memory in that case, exactly as it did before there was a store at all.
pub fn attach_store(store: Arc<dyn MailboxStore>) {
    SESSIONS.attach_store(store);
}

/// Put a store on the process mailbox and hand back the one it had.
///
/// Test-only, and deliberately not a hydration: it exists so a test that has to
/// exercise the *process* registry — the gateway handlers address it through
/// [`SESSIONS`] and have no other registry to address — can put back exactly
/// what it found and leave the global as it was for every other test in the
/// binary. A test that attaches a store under a temporary directory and walks
/// away leaves every later test writing into a file that no longer exists.
#[cfg(test)]
pub(crate) fn swap_store(store: Option<Arc<dyn MailboxStore>>) -> Option<Arc<dyn MailboxStore>> {
    std::mem::replace(&mut *SESSIONS.store.write(), store)
}

/// Open the mailbox database under a workspace and attach it.
///
/// Returns the path it attached, or `None` when the database could not be
/// opened — reported here rather than propagated, because a gateway that
/// refused to start over a mailbox file would fail *every* capability to
/// preserve one.
pub fn attach_workspace_store(workspace_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let path = workspace_dir.join("state").join("chat-mailbox.db");
    match SqliteMailboxStore::open(&path) {
        Ok(store) => {
            let store = Arc::new(store);
            // Durability is allowed to degrade here without failing live work,
            // which is only tolerable while an operator can see that it has.
            // `prx tasks pools` and `GET /api/runtime/pools` already report
            // every other database this process keeps; the registry holds only
            // a `Weak`, so the mailbox drops out of the report by itself when a
            // later attachment replaces it.
            let stats: Arc<dyn crate::runtime::registry::PoolStatsSource> = Arc::clone(&store) as _;
            crate::runtime::registry::register_pool(&stats);
            attach_store(store);
            Some(path)
        }
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                error = %crate::security::redaction::redact_provider_text(&format!("{error:#}")),
                "chat mailbox persistence unavailable, running from memory only"
            );
            None
        }
    }
}

impl ChatSessions {
    /// Attach a store and replay it.
    ///
    /// Idempotent in both halves: attaching replaces whatever store was there,
    /// and hydration skips every session id already live in memory, so the
    /// gateway supervisor restarting the gateway inside one process cannot
    /// duplicate a session or its queue.
    fn attach_store(self: &Arc<Self>, store: Arc<dyn MailboxStore>) {
        *self.store.write() = Some(Arc::clone(&store));
        match store.hydrate() {
            Ok(state) => self.hydrate(state),
            Err(error) => {
                // Not fatal, and deliberately not a reason to detach: new
                // mutations may still be writable even when a read failed, and
                // a mailbox that silently stopped persisting would be worse
                // than one that lost a replay.
                tracing::error!(error = %crate::security::redaction::redact_provider_text(&format!("{error:#}")), "chat mailbox could not be replayed");
            }
        }
    }

    /// Rebuild sessions, queues and the result feed from stored state.
    ///
    /// # Ordering
    ///
    /// Each session's inbox is rebuilt as *delivered-but-unacknowledged first,
    /// then queued*, which is the same reclaim rule [`pull`] applies on every
    /// poll: an assignment that was handed out and never acknowledged can only
    /// be one that never arrived, and it must not end up behind work that was
    /// queued after it. Acknowledged assignments are restored as acknowledged —
    /// the chat holding them may have outlived the daemon, and redelivering
    /// would be this runtime deciding for it that it had lost them.
    ///
    /// Work-registry rows are minted fresh: a `WorkId` is a process-local
    /// address, so the alternative to a new row is an assignment nobody can
    /// list or kill.
    fn hydrate(self: &Arc<Self>, state: StoredState) {
        // Past every revision the file already holds, so a write this process
        // plans can never be mistaken for one an earlier process planned.
        let restored_revision = state
            .sessions
            .iter()
            .map(|session| session.revision)
            .chain(state.assignments.iter().map(|assignment| assignment.revision))
            .max()
            .unwrap_or(0);
        self.revision.fetch_max(restored_revision, Ordering::SeqCst);

        let mut queues: HashMap<String, Vec<StoredAssignment>> = HashMap::new();
        for assignment in state.assignments {
            queues
                .entry(assignment.session_id.clone())
                .or_default()
                .push(assignment);
        }

        let mut restored_sessions = 0_usize;
        let mut restored_assignments = 0_usize;
        for stored in state.sessions {
            if self.sessions.read().contains_key(&stored.session_id) {
                continue;
            }
            let session = Arc::new(Session {
                id: stored.session_id.clone(),
                label: stored.label,
                pid: stored.pid,
                token_hash: stored.token_hash,
                registered_at_unix_ms: stored.registered_at_unix_ms,
                polls: AtomicU64::new(stored.polls),
                state: Mutex::new(SessionState {
                    last_poll: stored.last_poll_at_unix_ms.map(LastPoll::Restored),
                    inbox: VecDeque::new(),
                    delivered: Vec::new(),
                    accepted: Vec::new(),
                }),
            });

            let stored_queue = queues.remove(&stored.session_id).unwrap_or_default();
            let mut reclaimed = Vec::new();
            let mut queued = Vec::new();
            let mut accepted = Vec::new();
            for assignment in stored_queue {
                match assignment.queue {
                    Queue::Delivered => reclaimed.push(assignment),
                    Queue::Inbox => queued.push(assignment),
                    Queue::Accepted => accepted.push(assignment),
                }
            }

            // Move the reclaimed rows to the head of the *stored* queue too,
            // walking backwards exactly as `pull` does, so the order this
            // replay produces is the order a second replay would produce. Left
            // alone, their old positions would interleave with the queued ones
            // and the two replays would disagree.
            for stored_assignment in reclaimed.iter().rev() {
                let revision = self.next_revision();
                self.write_through("hydrate_reclaim", |store| {
                    store.move_to_queue(
                        &stored_assignment.assignment_id,
                        Queue::Inbox,
                        stored_assignment.deliveries,
                        true,
                        revision,
                    )
                });
            }

            {
                let mut live = session.state.lock();
                for stored_assignment in reclaimed.into_iter().chain(queued) {
                    let restored = self.restore_assignment(&session, &stored_assignment);
                    live.inbox.push_back(restored);
                    restored_assignments += 1;
                }
                for stored_assignment in accepted {
                    let restored = self.restore_assignment(&session, &stored_assignment);
                    live.accepted.push(restored);
                    restored_assignments += 1;
                }
            }

            self.sessions.write().insert(stored.session_id, session);
            restored_sessions += 1;
        }

        if !state.results.is_empty() {
            // In order with live reports: a replayed floor must not land
            // between a live report's mint and its publish. See `publication`.
            let _in_order = self.publication.lock();
            let mut log = self.results.lock();
            for result in state.results {
                // Merged by sequence, never appended: this replay runs again on
                // every in-process gateway restart, and the same record offered
                // twice must stay one entry. See [`ResultLog::insert`].
                // A replay re-offers entries the feed may already hold or have
                // trimmed; refusing those is the merge working, not a loss.
                let _replayed = log.insert(AssignmentResult {
                    seq: result.seq,
                    assignment_id: result.assignment_id,
                    session_id: result.session_id,
                    session_label: result.session_label,
                    disposition: result.disposition,
                    status: result.status,
                    summary: result.summary,
                    completed_at_unix_ms: result.completed_at_unix_ms,
                    origin: result.origin,
                });
            }
        }

        if restored_sessions > 0 || restored_assignments > 0 {
            tracing::info!(
                sessions = restored_sessions,
                assignments = restored_assignments,
                "restored chat session mailboxes from disk"
            );
        }
    }

    /// Rebuild one assignment, with a fresh registry row and kill watcher.
    fn restore_assignment(self: &Arc<Self>, session: &Arc<Session>, stored: &StoredAssignment) -> Assignment {
        let cancel = CancellationToken::new();
        let finished = CancellationToken::new();
        let work_name = work_row_name(stored.disposition, &session.reference(), &stored.origin);
        let work = crate::runtime::registry::register_sub_agent(
            &work_name,
            &stored.assignment_id,
            None,
            None,
            Some(cancel.clone()),
        );
        let work_id = work.id();
        self.watch_for_kill(session, stored.assignment_id.clone(), cancel, finished.clone());
        Assignment {
            id: stored.assignment_id.clone(),
            session_id: session.id.clone(),
            task: stored.task.clone(),
            disposition: stored.disposition,
            origin: stored.origin.clone(),
            created_at_unix_ms: stored.created_at_unix_ms,
            deliveries: stored.deliveries,
            _work: work,
            work_id,
            _finished: finished.drop_guard(),
        }
    }
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
    SESSIONS.register(label, pid)
}

/// Remove a session and cancel everything still outstanding for it.
///
/// Returns the number of assignments discarded, or `None` if the id was not
/// registered. Each discarded assignment is recorded in the result feed as
/// [`ResultStatus::Cancelled`], so an assigner waiting on an answer is told the
/// work went away instead of waiting on it forever.
pub fn deregister(session_id: &str) -> Option<usize> {
    SESSIONS.deregister(session_id)
}

/// Every registered session, oldest registration first.
#[must_use]
pub fn list() -> Vec<SessionView> {
    SESSIONS.list()
}

impl ChatSessions {
    fn register(&self, label: &str, pid: Option<u32>) -> Result<Registration, ChatSessionError> {
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
        let revision = self.next_revision();
        self.write_through("insert_session", |store| {
            store.insert_session(&StoredSession {
                session_id: session_id.clone(),
                label: label.clone(),
                pid,
                // The hash, never the token. A stolen database file can say a
                // session existed; it cannot drain that session's mailbox.
                token_hash: session.token_hash.clone(),
                registered_at_unix_ms,
                polls: 0,
                last_poll_at_unix_ms: None,
                revision,
            })
        });
        self.sessions.write().insert(session_id.clone(), session);
        Ok(Registration {
            session_id,
            session_ref,
            token,
            label,
            registered_at_unix_ms,
        })
    }

    fn deregister(&self, session_id: &str) -> Option<usize> {
        let session = self.sessions.write().remove(session_id.trim())?;
        let outstanding = {
            let mut state = session.state.lock();
            let mut outstanding = Vec::new();
            outstanding.extend(state.inbox.drain(..));
            outstanding.append(&mut state.delivered);
            outstanding.append(&mut state.accepted);
            outstanding
        };
        let count = outstanding.len();

        // The session cascade takes the queued rows with it, so the discarded
        // assignments need no individual delete — but the cascade and the
        // cancellations that replace them have to be **one** commit. Written
        // separately, a failure between them leaves an assigner with neither
        // outstanding work nor an answer about it.
        if outstanding.is_empty() {
            // Nothing to record, so nothing to order: the cascade need not
            // queue behind result writers.
            self.write_through("cascade_session", |store| store.cascade_session(&session.id, &[]));
            return Some(count);
        }
        let turn = PublicationTurn::take(self);
        {
            let mut stored = Vec::with_capacity(count);
            let mut minted = Vec::with_capacity(count);
            for assignment in &outstanding {
                let (record, entry) = turn.mint(
                    &session,
                    assignment,
                    ResultStatus::Cancelled,
                    "the chat session deregistered while this assignment was still outstanding".to_string(),
                );
                stored.push(record);
                minted.push(entry);
            }
            turn.write("cascade_session", |store| store.cascade_session(&session.id, &stored));
            for entry in minted {
                turn.publish(entry);
            }
        }
        drop(turn);
        Some(count)
    }

    fn list(&self) -> Vec<SessionView> {
        let sessions = self.sessions.read().values().map(Arc::clone).collect::<Vec<_>>();
        let now = Instant::now();
        let now_unix_ms = unix_ms();
        let mut views = sessions
            .iter()
            .map(|session| {
                let state = session.state.lock();
                let age = state.last_poll.map(|last| last.age(now, now_unix_ms));
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
    SESSIONS.assign(policy, principal, session_id, task, disposition)
}

impl ChatSessions {
    fn assign(
        self: &Arc<Self>,
        policy: &SecurityPolicy,
        principal: &AssignPrincipal,
        session_id: &str,
        task: &str,
        disposition: Disposition,
    ) -> Result<AssignmentReceipt, ChatSessionError> {
        let session_id = session_id.trim();
        let task = bounded(task, "task", MAX_TASK_LEN)?;
        let session = self.find(session_id);
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
        let work_name = work_row_name(disposition, &session.reference(), principal);
        let work =
            crate::runtime::registry::register_sub_agent(&work_name, &assignment_id, None, None, Some(cancel.clone()));
        let work_id = work.id();

        let created_at_unix_ms = unix_ms();
        let assignment = Assignment {
            id: assignment_id.clone(),
            session_id: session.id.clone(),
            task: task.clone(),
            disposition,
            origin: principal.clone(),
            created_at_unix_ms,
            deliveries: 0,
            _work: work,
            work_id,
            _finished: finished.clone().drop_guard(),
        };

        // Written before the receipt is handed back, so "accepted" and "durable"
        // are the same instant for every caller that reads one.
        let revision = self.next_revision();
        self.write_through("enqueue", |store| {
            store.enqueue(
                &StoredAssignment {
                    assignment_id: assignment_id.clone(),
                    session_id: session.id.clone(),
                    task,
                    disposition,
                    queue: Queue::Inbox,
                    deliveries: 0,
                    created_at_unix_ms,
                    origin: principal.clone(),
                    revision,
                },
                disposition.is_immediate(),
            )
        });

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

        self.watch_for_kill(&session, assignment_id.clone(), cancel, finished);

        Ok(AssignmentReceipt {
            assignment_id,
            session_id: session.id.clone(),
            session_ref: session.reference(),
            disposition,
            work_id,
            queued_ahead,
        })
    }
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
/// The work-registry row name for one assignment.
///
/// One function so the row a restart mints reads exactly like the row the
/// original assignment had: same channel in the clear, same two fingerprints,
/// same absent task body.
fn work_row_name(disposition: Disposition, session_ref: &str, origin: &AssignPrincipal) -> String {
    format!(
        "chat_assign {} → {CHAT_SESSION_CHANNEL} {session_ref} from {}",
        disposition.as_str(),
        origin.label(),
    )
}

impl ChatSessions {
    fn watch_for_kill(
        self: &Arc<Self>,
        session: &Arc<Session>,
        assignment_id: String,
        cancel: CancellationToken,
        finished: CancellationToken,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let session = Arc::clone(session);
        // Weak, not strong. The watcher outlives nothing: it holds the registry
        // only for the instant it records the cancellation, and a strong handle
        // would close a cycle (registry → session → assignment → drop guard →
        // this task) that keeps a registry alive precisely while it still has
        // queued work to release.
        let registry = Arc::downgrade(self);
        handle.spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {
                    if let Some(registry) = registry.upgrade() {
                        // Off the async worker: recording the kill waits for
                        // the publication turn and writes to the store, and a
                        // worker parked on either stalls every task it runs.
                        let recorded = crate::runtime::blocking::spawn_blocking(move || {
                            registry.discard(&session, &assignment_id);
                        })
                        .await;
                        if let Err(error) = recorded {
                            tracing::error!("recording an operator kill of a chat assignment failed: {error}");
                        }
                    }
                }
                () = finished.cancelled() => {}
            }
        });
    }

    /// Take one assignment out of every queue it could be in and record it as
    /// cancelled. Dropping it retires its registry row, which is what confirms
    /// the kill.
    ///
    /// Blocking: it waits for the publication turn and writes to the store.
    /// The kill watcher calls it on the blocking pool for that reason.
    fn discard(&self, session: &Arc<Session>, assignment_id: &str) {
        let turn = PublicationTurn::take(self);
        let removed = {
            let mut state = session.state.lock();
            take_assignment(&mut state, assignment_id)
        };
        if let Some(assignment) = removed {
            {
                let (record, minted) = turn.mint(
                    session,
                    &assignment,
                    ResultStatus::Cancelled,
                    "the assignment's work item was killed by an operator".to_string(),
                );
                turn.write("complete_assignment", |store| {
                    store.complete_assignment(&assignment.id, &record)
                });
                turn.publish(minted);
            }
            drop(turn);
        }
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
    SESSIONS.pull(session_id, token, ack, max)
}

impl ChatSessions {
    fn pull(
        &self,
        session_id: &str,
        token: &str,
        ack: &[String],
        max: Option<usize>,
    ) -> Result<Pulled, ChatSessionError> {
        let session = self.find(session_id).ok_or(ChatSessionError::UnknownSession)?;
        authenticate(&session, token)?;
        let max = max.unwrap_or(DEFAULT_PULL_BATCH).clamp(1, MAX_PULL_BATCH);

        // Every write this pull owes the store, planned under the lock and
        // executed after it. See [`PendingWrite`].
        let mut pending: Vec<PendingWrite> = Vec::new();
        let pulled = {
            let mut state = session.state.lock();
            // Counted and clocked **inside** the lock that mints the revision,
            // because the revision is what orders the write. Sampled outside
            // it, two overlapping pulls can pair the lower count with the
            // higher revision — the pull that incremented first takes the lock
            // second — and the store's revision guard then keeps the lower
            // count, rolling the persisted liveness backwards. Value order and
            // revision order have to be the same order.
            let polls = session.polls.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            let now_unix_ms = unix_ms();
            state.last_poll = Some(LastPoll::Observed(Instant::now()));
            // The evidence of liveness is persisted with the pull that produced it,
            // so a session restored after a restart is as old as its last real pull
            // rather than as young as the restart. See [`LastPoll`].
            pending.push(PendingWrite::RecordPoll {
                polls,
                now_unix_ms,
                revision: self.next_revision(),
            });

            let mut acked = 0;
            for id in ack {
                if let Some(index) = state.delivered.iter().position(|entry| &entry.id == id) {
                    let assignment = state.delivered.remove(index);
                    pending.push(PendingWrite::move_to(
                        "ack",
                        &assignment,
                        Queue::Accepted,
                        false,
                        self.next_revision(),
                    ));
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
                pending.push(PendingWrite::move_to(
                    "reclaim",
                    &assignment,
                    Queue::Inbox,
                    true,
                    self.next_revision(),
                ));
                state.inbox.push_front(assignment);
            }

            let mut assignments = Vec::new();
            for _ in 0..max {
                let Some(mut assignment) = state.inbox.pop_front() else {
                    break;
                };
                assignment.deliveries = assignment.deliveries.saturating_add(1);
                pending.push(PendingWrite::move_to(
                    "deliver",
                    &assignment,
                    Queue::Delivered,
                    false,
                    self.next_revision(),
                ));
                assignments.push(assignment.view());
                state.delivered.push(assignment);
            }

            Pulled {
                assignments,
                acked,
                requeued,
                queued_remaining: state.inbox.len(),
            }
        };

        // Out of the lock, still before the caller is answered: write-through is
        // preserved, but a contended database can no longer freeze this session.
        self.flush_pending(&session.id, pending);
        Ok(pulled)
    }

    /// Run the writes a pull planned, in the order it planned them.
    ///
    /// One contention budget covers the whole flush: the pull is a single
    /// request, and a database another process is holding must cost it one
    /// bounded wait rather than one per row it touches. Each write still
    /// carries the revision it was planned at, so a flush that loses the race
    /// to a later pull writes nothing instead of writing something stale.
    fn flush_pending(&self, session_id: &str, pending: Vec<PendingWrite>) {
        let _budget = BoundedBusyBudget::start();
        for write in pending {
            match write {
                PendingWrite::RecordPoll {
                    polls,
                    now_unix_ms,
                    revision,
                } => {
                    self.write_through("record_poll", |store| {
                        store.record_poll(session_id, polls, now_unix_ms, revision)
                    });
                }
                PendingWrite::MoveToQueue {
                    operation,
                    assignment_id,
                    queue,
                    deliveries,
                    to_front,
                    revision,
                } => {
                    self.write_through(operation, |store| {
                        store.move_to_queue(&assignment_id, queue, deliveries, to_front, revision)
                    });
                }
            }
        }
    }
}

/// One store write a pull owes, held until the session state lock is released.
///
/// A pull mutates three in-memory queues under `session.state.lock()` and the
/// SQLite mirror of each mutation is a blocking write. Performing them inside
/// the lock puts a session's whole state behind a file lock this process does
/// not own — a second `prx` on the same workspace, or an operator's `sqlite3`
/// write transaction — which is exactly the stall the mailbox's availability
/// trade-off exists to prevent. Planning under the lock and writing after it
/// keeps the order (and the write-through promise: the caller is still answered
/// only once the writes have been attempted) without holding the session
/// hostage.
enum PendingWrite {
    RecordPoll {
        polls: u64,
        now_unix_ms: u64,
        revision: u64,
    },
    MoveToQueue {
        operation: &'static str,
        assignment_id: String,
        queue: Queue,
        deliveries: u32,
        to_front: bool,
        revision: u64,
    },
}

impl PendingWrite {
    fn move_to(operation: &'static str, assignment: &Assignment, queue: Queue, to_front: bool, revision: u64) -> Self {
        Self::MoveToQueue {
            operation,
            assignment_id: assignment.id.clone(),
            queue,
            deliveries: assignment.deliveries,
            to_front,
            revision,
        }
    }
}

fn authenticate(session: &Session, token: &str) -> Result<(), ChatSessionError> {
    let token = token.trim();
    if token.is_empty() || hash_token(token) != session.token_hash {
        return Err(ChatSessionError::BadToken);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
