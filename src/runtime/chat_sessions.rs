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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::runtime::registry::{WorkGuard, WorkId};
use crate::security::op_id::ref_for_channel_recipient;
use crate::security::policy::SecurityPolicy;

mod store;

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
    /// Taken before the store is asked and published after, so no lock is held
    /// across a database wait; [`Self::insert`] puts the entry in sequence
    /// order whatever order the publications arrive in.
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
    fn insert(&mut self, result: AssignmentResult) {
        if result.seq < self.oldest_seq {
            // Already trimmed. Re-admitting it would move the feed's floor
            // backwards and tell a consumer it had *not* missed entries it had.
            return;
        }
        let seq = result.seq;
        match self.entries.binary_search_by(|entry| entry.seq.cmp(&seq)) {
            Ok(_) => return,
            Err(index) => self.entries.insert(index, result),
        }
        // The sequence keeps climbing across restarts. Restarting it at 1 would
        // hand a consumer that cached a cursor a feed whose new entries look
        // older than the ones it already saw.
        self.next_seq = self.next_seq.max(seq.saturating_add(1));
        self.trim();
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
        let _budget = crate::runtime::sqlite_pool::BoundedBusyBudget::start();
        if let Err(error) = write(store.as_ref()) {
            tracing::error!(operation, "chat mailbox write-through failed: {error:#}");
        }
    }

    fn find(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions.read().get(session_id.trim()).map(Arc::clone)
    }
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
                "chat mailbox persistence unavailable, running from memory only: {error:#}"
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
                tracing::error!("chat mailbox could not be replayed: {error:#}");
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
            let mut log = self.results.lock();
            for result in state.results {
                // Merged by sequence, never appended: this replay runs again on
                // every in-process gateway restart, and the same record offered
                // twice must stay one entry. See [`ResultLog::insert`].
                log.insert(AssignmentResult {
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
        let mut stored = Vec::with_capacity(count);
        let mut live = Vec::with_capacity(count);
        for assignment in &outstanding {
            let (record, entry) = self.mint_result(
                &session,
                assignment,
                ResultStatus::Cancelled,
                "the chat session deregistered while this assignment was still outstanding".to_string(),
            );
            stored.push(record);
            live.push(entry);
        }
        self.write_through("cascade_session", |store| store.cascade_session(&session.id, &stored));
        for entry in live {
            self.publish_result(entry);
        }
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
                        registry.discard(&session, &assignment_id);
                    }
                }
                () = finished.cancelled() => {}
            }
        });
    }

    /// Take one assignment out of every queue it could be in and record it as
    /// cancelled. Dropping it retires its registry row, which is what confirms
    /// the kill.
    fn discard(&self, session: &Arc<Session>, assignment_id: &str) {
        let removed = {
            let mut state = session.state.lock();
            take_assignment(&mut state, assignment_id)
        };
        if let Some(assignment) = removed {
            let (record, entry) = self.mint_result(
                session,
                &assignment,
                ResultStatus::Cancelled,
                "the assignment's work item was killed by an operator".to_string(),
            );
            self.write_through("complete_assignment", |store| {
                store.complete_assignment(&assignment.id, &record)
            });
            self.publish_result(entry);
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

        let polls = session.polls.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        let now_unix_ms = unix_ms();
        // Every write this pull owes the store, planned under the lock and
        // executed after it. See [`PendingWrite`].
        let mut pending: Vec<PendingWrite> = Vec::new();
        let pulled = {
            let mut state = session.state.lock();
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
        let _budget = crate::runtime::sqlite_pool::BoundedBusyBudget::start();
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
    SESSIONS.report(session_id, token, assignment_id, status, summary)
}

/// Results with a sequence greater than `after_seq`, oldest first.
#[must_use]
pub fn results_after(after_seq: u64, limit: usize) -> ResultsPage {
    SESSIONS.results_after(after_seq, limit)
}

impl ChatSessions {
    fn report(
        &self,
        session_id: &str,
        token: &str,
        assignment_id: &str,
        status: ResultStatus,
        summary: &str,
    ) -> Result<u64, ChatSessionError> {
        let session = self.find(session_id).ok_or(ChatSessionError::UnknownSession)?;
        authenticate(&session, token)?;
        let summary = bounded(summary, "summary", MAX_SUMMARY_LEN)?;
        let assignment_id = assignment_id.trim();

        let assignment = {
            let mut state = session.state.lock();
            state
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
                })
        };

        let Some(assignment) = assignment else {
            // Not outstanding. Either this session never held it — the refusal
            // it has always been — or this *is* the report, arriving a second
            // time because the first answer was lost in flight. A chat that is
            // told to retry a report it already made must not be told the work
            // is unknown, and must not mint a second completion sequence for
            // it, so the recorded result answers the replay.
            return self
                .replayed_result(&session.id, assignment_id)
                .ok_or(ChatSessionError::UnknownAssignment);
        };

        // Retirement and result are one commit. Written separately, a failure
        // between them takes the work out of every queue while leaving no
        // record of it, and the reporter cannot recover by retrying because the
        // assignment is no longer anywhere to retry against.
        let (record, entry) = self.mint_result(&session, &assignment, status, summary);
        self.write_through("complete_assignment", |store| {
            store.complete_assignment(&assignment.id, &record)
        });
        Ok(self.publish_result(entry))
    }

    /// The sequence a previous report of this assignment was recorded under.
    ///
    /// Bounded by the feed's retention: a replay that arrives after the record
    /// has been trimmed is still refused as unknown, which is honest — the
    /// answer it is asking for no longer exists to be handed back.
    fn replayed_result(&self, session_id: &str, assignment_id: &str) -> Option<u64> {
        let log = self.results.lock();
        log.entries
            .iter()
            .rev()
            .find(|entry| entry.assignment_id == assignment_id && entry.session_id == session_id)
            .map(|entry| entry.seq)
    }

    /// Reserve a sequence and build the two forms of one finished assignment:
    /// the row to persist, and the entry to publish once the store has been
    /// asked.
    ///
    /// Split from [`Self::publish_result`] so the result log's lock is never
    /// held across a database wait.
    fn mint_result(
        &self,
        session: &Session,
        assignment: &Assignment,
        status: ResultStatus,
        summary: String,
    ) -> (StoredResult, AssignmentResult) {
        let seq = self.results.lock().mint();
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
        let live = AssignmentResult {
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
        (stored, live)
    }

    /// Put one finished assignment on the live feed. Dropping the assignment
    /// afterwards retires its registry row.
    fn publish_result(&self, result: AssignmentResult) -> u64 {
        let seq = result.seq;
        self.results.lock().insert(result);
        seq
    }

    fn results_after(&self, after_seq: u64, limit: usize) -> ResultsPage {
        let log = self.results.lock();
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
        if let Some(session) = SESSIONS.find(session_id) {
            let mut state = session.state.lock();
            state.last_poll = state.last_poll.and_then(|last| match last {
                LastPoll::Observed(at) => at.checked_sub(by).map(LastPoll::Observed),
                LastPoll::Restored(at) => Some(LastPoll::Restored(
                    at.saturating_sub(u64::try_from(by.as_millis()).unwrap_or(u64::MAX)),
                )),
            });
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

        // Reporting twice is a *replay*, not a second completion: a chat whose
        // report lost its response retries it, and the recorded sequence is
        // what answers the retry.
        assert_eq!(
            report(
                &session.session_id,
                &session.token,
                &receipt.assignment_id,
                ResultStatus::Completed,
                "again",
            )
            .expect("test: a replayed report is answered, not refused"),
            seq,
            "a replay may not mint a second completion sequence"
        );
        assert_eq!(
            results_for(&receipt.assignment_id).len(),
            1,
            "and it may not duplicate the record either"
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

    // ── Persistence ─────────────────────────────────────────────────────────
    //
    // "Restart the daemon" is expressed here as *a second `ChatSessions`
    // instance opened on the same database file*, which is exactly what the
    // gateway does on the way up. These tests deliberately never touch the
    // process-wide `SESSIONS`: a global that every other test in this file
    // shares could not be restarted without destroying them, and a persistence
    // test that had to serialize against the whole module would be the first
    // thing to be quietly disabled.

    /// Open a fresh registry over `path` and replay whatever is there.
    fn mailbox_at(path: &std::path::Path) -> Arc<ChatSessions> {
        let store = SqliteMailboxStore::open(path).expect("test: open the mailbox store");
        let sessions = Arc::new(ChatSessions::new());
        sessions.attach_store(Arc::new(store));
        sessions
    }

    fn operator_policy() -> SecurityPolicy {
        policy_with(vec![], &[])
    }

    /// Everything a session would be handed on its next pull, as task strings.
    fn tasks_in_order(pulled: &Pulled) -> Vec<String> {
        pulled
            .assignments
            .iter()
            .map(|assignment| assignment.task.clone())
            .collect()
    }

    /// The whole durability claim in one flow: a restart keeps the queue, keeps
    /// its order, keeps the credential, and keeps the accounting.
    ///
    /// MUTATION GUARD: drop the "delivered goes back to the head" arm of
    /// `hydrate` and the order assertion fails; drop the `enqueue`
    /// write-through in `assign` and the count assertion fails (nothing is
    /// stored to replay); store the token instead of its hash and
    /// `the_mailbox_file_never_holds_a_plaintext_token` fails.
    #[tokio::test]
    async fn a_restart_replays_the_mailbox_and_the_old_token_still_pulls() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");
        let policy = operator_policy();
        let principal = AssignPrincipal::operator_plane();

        let registration = {
            let before = mailbox_at(&db);
            let registration = before.register(LABEL, Some(4242)).expect("test: registration");
            for task in ["first", "second", "third"] {
                before
                    .assign(&policy, &principal, &registration.session_id, task, Disposition::Queue)
                    .expect("test: assignment accepted");
            }
            // One handed out and deliberately left unacknowledged: it is the
            // assignment whose position after the restart is the whole point.
            let pulled = before
                .pull(&registration.session_id, &registration.token, &[], Some(1))
                .expect("test: pull");
            assert_eq!(tasks_in_order(&pulled), vec!["first".to_string()]);
            registration
        };

        // ── Restart ──
        let after = mailbox_at(&db);
        let pulled = after
            .pull(&registration.session_id, &registration.token, &[], None)
            .expect("test: the session id and token minted before the restart must still pull");
        assert_eq!(
            tasks_in_order(&pulled),
            vec!["first".to_string(), "second".to_string(), "third".to_string()],
            "nothing may be lost, and the unacknowledged delivery must come back at the head"
        );
        assert_eq!(
            pulled.assignments.first().map(|assignment| assignment.deliveries),
            Some(2),
            "the redelivered assignment must say it is a repeat, so its id can be used as an idempotency key"
        );

        // A wrong token is still a wrong token after a restart: the stored hash
        // is the same comparison the live registry made.
        assert_eq!(
            after
                .pull(&registration.session_id, "not-the-token", &[], None)
                .expect_err("test: refused"),
            ChatSessionError::BadToken
        );

        // ── Acknowledge one, restart again ──
        let acked = pulled
            .assignments
            .first()
            .map(|assignment| assignment.assignment_id.clone())
            .expect("test: an assignment to acknowledge");
        after
            .pull(
                &registration.session_id,
                &registration.token,
                std::slice::from_ref(&acked),
                Some(0),
            )
            .expect("test: acknowledging pull");

        let after_ack = mailbox_at(&db);
        let pulled = after_ack
            .pull(&registration.session_id, &registration.token, &[], None)
            .expect("test: pull");
        assert_eq!(
            tasks_in_order(&pulled),
            vec!["second".to_string(), "third".to_string()],
            "work the session acknowledged is work it still holds; redelivering it would be this runtime \
             deciding a live chat had lost it"
        );
        assert!(
            after_ack
                .list()
                .iter()
                .any(|session| session.session_id == registration.session_id && session.accepted == 1),
            "the acknowledged assignment must be restored as accepted, not silently dropped"
        );

        // ── Deregister, restart again ──
        after_ack.deregister(&registration.session_id);
        let after_exit = mailbox_at(&db);
        assert_eq!(
            after_exit
                .pull(&registration.session_id, &registration.token, &[], None)
                .expect_err("test: a withdrawn session must not come back"),
            ChatSessionError::UnknownSession
        );
        assert!(after_exit.list().is_empty(), "the session row itself must be gone");
    }

    /// The token is a secret that exists in the clear for exactly one response.
    ///
    /// Asserted against the file rather than against the code, because the way
    /// this regresses is a well-meant "store the token so a chat can recover
    /// it" — which the wording of `Registration` already forbids.
    #[tokio::test]
    async fn the_mailbox_file_never_holds_a_plaintext_token() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");
        let sessions = mailbox_at(&db);
        let registration = sessions.register(LABEL, None).expect("test: registration");
        sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                "do a thing",
                Disposition::Queue,
            )
            .expect("test: assignment accepted");

        let conn = rusqlite::Connection::open(&db).expect("test: open the mailbox file");
        let pattern = format!("%{}%", registration.token);
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chat_sessions
                 WHERE session_id LIKE ?1 OR label LIKE ?1 OR token_hash LIKE ?1",
                rusqlite::params![pattern],
                |row| row.get(0),
            )
            .expect("test: query the session rows");
        assert_eq!(hits, 0, "the mailbox database must never hold the token in the clear");

        // And the hash that *is* there is the one the live registry compares
        // against, so the check is not merely "some other string".
        let stored: String = conn
            .query_row(
                "SELECT token_hash FROM chat_sessions WHERE session_id = ?1",
                rusqlite::params![registration.session_id],
                |row| row.get(0),
            )
            .expect("test: read the stored hash");
        assert_eq!(stored, hash_token(&registration.token));

        // Belt and braces: the raw bytes do not contain it either, so a token
        // cannot hide in a table this query forgot to name. Scanned over the
        // `-wal` and `-shm` sidecars as well as the database file: under WAL
        // everything written since the last checkpoint is in the sidecar, so a
        // scan of the main file alone would pass over a 4 KiB header while the
        // data sat next to it.
        drop(conn);
        let mut scanned = 0usize;
        for file in mailbox_files(&db) {
            let Ok(bytes) = std::fs::read(&file) else {
                continue;
            };
            scanned += 1;
            assert!(
                !bytes
                    .windows(registration.token.len())
                    .any(|window| window == registration.token.as_bytes()),
                "the token must not appear anywhere in {}",
                file.display()
            );
        }
        assert!(
            scanned >= 2,
            "the scan is only meaningful if it saw the sidecars: {scanned} file(s) read"
        );
    }

    /// Every file the mailbox database consists of, sidecars included.
    fn mailbox_files(db: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = vec![db.to_path_buf()];
        for suffix in ["-wal", "-shm"] {
            let mut name = db.as_os_str().to_os_string();
            name.push(suffix);
            files.push(std::path::PathBuf::from(name));
        }
        files
    }

    /// The module promises `0600` for a file that holds assignment bodies and
    /// plaintext origins. Under WAL that promise is about three files: the
    /// `-wal` is where an un-checkpointed assignment actually lives, and it
    /// used to be created `0644` by SQLite while only the (empty) main file was
    /// restricted.
    ///
    /// MUTATION GUARD: chmod only the database file in `restrict_permissions`
    /// (the shape this replaced) and the second half — a database file that
    /// already exists with the mode an older build left behind — fails with a
    /// `0644` `-wal`.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_mailbox_database_and_its_sidecars_are_private_to_their_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");
        let sessions = mailbox_at(&db);
        let registration = sessions.register(LABEL, None).expect("test: registration");
        sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                "SECRETTASKMARKER",
                Disposition::Queue,
            )
            .expect("test: assignment accepted");

        let mut checked = 0usize;
        for file in mailbox_files(&db) {
            let Ok(metadata) = std::fs::metadata(&file) else {
                continue;
            };
            checked += 1;
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o600,
                "{} must be readable by its owner only",
                file.display()
            );
        }
        assert_eq!(
            checked, 3,
            "a WAL database is three files, and all three carry the assignment body"
        );

        // The body really is out there in the clear, which is what the mode is
        // protecting: a test that passed because nothing had been written yet
        // would be no test at all.
        let found = mailbox_files(&db).into_iter().any(|file| {
            std::fs::read(&file).is_ok_and(|bytes| {
                bytes
                    .windows("SECRETTASKMARKER".len())
                    .any(|window| window == b"SECRETTASKMARKER")
            })
        });
        assert!(
            found,
            "the assignment body must be on disk for this test to mean anything"
        );

        // A database file an older build left behind is `0644`, and SQLite gives
        // its sidecars the mode of the file it finds. Opening such a file has to
        // fix all three, not just the one this module used to name.
        let legacy_dir = tempfile::tempdir().expect("test: tempdir");
        let legacy = legacy_dir.path().join("state").join("chat-mailbox.db");
        std::fs::create_dir_all(legacy_dir.path().join("state")).expect("test: state directory");
        std::fs::write(&legacy, b"").expect("test: pre-existing database file");
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o644)).expect("test: legacy mode");
        let legacy_sessions = mailbox_at(&legacy);
        let legacy_registration = legacy_sessions.register(LABEL, None).expect("test: registration");
        legacy_sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &legacy_registration.session_id,
                "SECRETTASKMARKER",
                Disposition::Queue,
            )
            .expect("test: assignment accepted");
        for file in mailbox_files(&legacy) {
            let Ok(metadata) = std::fs::metadata(&file) else {
                continue;
            };
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o600,
                "opening a legacy world-readable mailbox must restrict {} too",
                file.display()
            );
        }
    }

    /// A store write is blocking, and SQLite can be made to block for as long as
    /// another process holds the file lock. Doing that write inside
    /// `session.state.lock()` would put the whole session — every `/sessions`
    /// listing, every new assignment for it — behind a lock this process does
    /// not own.
    ///
    /// MUTATION GUARD: move the `flush_pending` call back inside the
    /// `session.state.lock()` scope in `pull` and the probe below never gets an
    /// answer within its timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_blocked_mailbox_write_does_not_freeze_the_session_it_writes_for() {
        use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

        struct BlockingStore {
            entered: SyncSender<()>,
            release: Mutex<Receiver<()>>,
            blocked_once: std::sync::atomic::AtomicBool,
        }

        impl MailboxStore for BlockingStore {
            fn hydrate(&self) -> anyhow::Result<StoredState> {
                Ok(StoredState::default())
            }
            fn insert_session(&self, _session: &StoredSession) -> anyhow::Result<()> {
                Ok(())
            }
            fn record_poll(
                &self,
                _session_id: &str,
                _polls: u64,
                _at_unix_ms: u64,
                _revision: u64,
            ) -> anyhow::Result<()> {
                if !self.blocked_once.swap(true, Ordering::SeqCst) {
                    let _ = self.entered.send(());
                    // Stands in for a database file locked by another process.
                    let _ = self.release.lock().recv();
                }
                Ok(())
            }
            fn enqueue(&self, _assignment: &StoredAssignment, _at_front: bool) -> anyhow::Result<()> {
                Ok(())
            }
            fn move_to_queue(
                &self,
                _assignment_id: &str,
                _queue: Queue,
                _deliveries: u32,
                _to_front: bool,
                _revision: u64,
            ) -> anyhow::Result<()> {
                Ok(())
            }
            fn complete_assignment(&self, _assignment_id: &str, _result: &StoredResult) -> anyhow::Result<()> {
                Ok(())
            }
            fn cascade_session(&self, _session_id: &str, _results: &[StoredResult]) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let (entered_tx, entered_rx) = sync_channel::<()>(1);
        let (release_tx, release_rx) = sync_channel::<()>(1);
        let sessions = Arc::new(ChatSessions::new());
        sessions.attach_store(Arc::new(BlockingStore {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            blocked_once: std::sync::atomic::AtomicBool::new(false),
        }));
        let registration = sessions.register(LABEL, None).expect("test: registration");
        sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                "do a thing",
                Disposition::Queue,
            )
            .expect("test: assignment accepted");

        let puller = {
            let sessions = Arc::clone(&sessions);
            let session_id = registration.session_id.clone();
            let token = registration.token;
            std::thread::spawn(move || sessions.pull(&session_id, &token, &[], None))
        };
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("test: the pull must reach the store");

        let (probe_tx, probe_rx) = sync_channel::<usize>(1);
        let probe = {
            let sessions = Arc::clone(&sessions);
            std::thread::spawn(move || {
                let listed = sessions.list().len();
                let _ = probe_tx.send(listed);
            })
        };
        let answered = probe_rx.recv_timeout(std::time::Duration::from_secs(2));
        let _ = release_tx.send(());
        probe.join().expect("test: the probe thread");
        let pulled = puller
            .join()
            .expect("test: the pulling thread")
            .expect("test: the pull itself must still succeed");

        assert_eq!(
            answered.ok(),
            Some(1),
            "a mailbox write that is stuck on a foreign lock must not hold the session state lock"
        );
        assert_eq!(
            tasks_in_order(&pulled),
            vec!["do a thing".to_string()],
            "the write-through is still owed, and the work is still delivered"
        );
    }

    /// A store that refuses every write costs durability and nothing else.
    ///
    /// This is the availability trade-off stated in the module header, and it
    /// is only a claim until something asserts it: an assignment must still be
    /// accepted, delivered and reported while the disk is saying no.
    #[tokio::test]
    async fn a_store_that_fails_every_write_still_delivers_work() {
        struct RefusingStore;
        impl MailboxStore for RefusingStore {
            fn hydrate(&self) -> anyhow::Result<StoredState> {
                anyhow::bail!("no")
            }
            fn insert_session(&self, _session: &StoredSession) -> anyhow::Result<()> {
                anyhow::bail!("no")
            }
            fn record_poll(
                &self,
                _session_id: &str,
                _polls: u64,
                _at_unix_ms: u64,
                _revision: u64,
            ) -> anyhow::Result<()> {
                anyhow::bail!("no")
            }
            fn enqueue(&self, _assignment: &StoredAssignment, _at_front: bool) -> anyhow::Result<()> {
                anyhow::bail!("no")
            }
            fn move_to_queue(
                &self,
                _assignment_id: &str,
                _queue: Queue,
                _deliveries: u32,
                _to_front: bool,
                _revision: u64,
            ) -> anyhow::Result<()> {
                anyhow::bail!("no")
            }
            fn complete_assignment(&self, _assignment_id: &str, _result: &StoredResult) -> anyhow::Result<()> {
                anyhow::bail!("no")
            }
            fn cascade_session(&self, _session_id: &str, _results: &[StoredResult]) -> anyhow::Result<()> {
                anyhow::bail!("no")
            }
        }

        let sessions = Arc::new(ChatSessions::new());
        sessions.attach_store(Arc::new(RefusingStore));
        let registration = sessions
            .register(LABEL, None)
            .expect("test: registration must still succeed");
        sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                "do a thing",
                Disposition::Queue,
            )
            .expect("test: a failed write must not fail the assignment");
        let pulled = sessions
            .pull(&registration.session_id, &registration.token, &[], None)
            .expect("test: pull");
        assert_eq!(tasks_in_order(&pulled), vec!["do a thing".to_string()]);
        let assignment_id = pulled
            .assignments
            .first()
            .map(|assignment| assignment.assignment_id.clone())
            .expect("test: one assignment");
        sessions
            .report(
                &registration.session_id,
                &registration.token,
                &assignment_id,
                ResultStatus::Completed,
                "done",
            )
            .expect("test: reporting must still succeed");
    }

    /// A restored session is as old as its last real pull.
    ///
    /// The failure this forbids is a restart that makes every session look
    /// freshly alive: an operator would read `polling` for chats that died with
    /// the daemon, and the listing's whole job is to hand them evidence.
    #[tokio::test]
    async fn a_restored_session_is_stale_until_it_pulls_again() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");

        let registration = {
            let before = mailbox_at(&db);
            let registration = before.register(LABEL, None).expect("test: registration");
            before
                .pull(&registration.session_id, &registration.token, &[], None)
                .expect("test: pull");
            registration
        };

        // Backdate the stored evidence rather than the live one: the question
        // is what the *next* process believes about a pull it never saw.
        let conn = rusqlite::Connection::open(&db).expect("test: open the mailbox file");
        let silent_ms = u64::try_from(SILENT_AFTER.as_millis()).unwrap_or(u64::MAX);
        let long_ago = unix_ms().saturating_sub(silent_ms.saturating_mul(4));
        conn.execute(
            "UPDATE chat_sessions SET last_poll_at_unix_ms = ?1",
            rusqlite::params![i64::try_from(long_ago).unwrap_or(i64::MAX)],
        )
        .expect("test: backdate the stored pull");
        drop(conn);

        let after = mailbox_at(&db);
        let listed = after.list();
        let session = listed
            .iter()
            .find(|session| session.session_id == registration.session_id)
            .expect("test: the session must be restored");
        assert_eq!(
            session.liveness,
            ChatLiveness::Silent,
            "a session restored from disk must report the age of its last real pull, not the age of the restart"
        );
        assert_eq!(session.polls, 1, "the pull count survives the restart too");

        // And a real pull clears it, because the evidence is now first-hand.
        after
            .pull(&registration.session_id, &registration.token, &[], None)
            .expect("test: pull");
        let listed = after.list();
        let session = listed
            .iter()
            .find(|session| session.session_id == registration.session_id)
            .expect("test: the session is still there");
        assert_eq!(session.liveness, ChatLiveness::Polling);
    }

    /// Results outlive the process that recorded them, and their sequence keeps
    /// climbing so a consumer's cursor still means something.
    #[tokio::test]
    async fn the_result_feed_survives_a_restart_and_keeps_its_sequence() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");

        let first_seq = {
            let before = mailbox_at(&db);
            let registration = before.register(LABEL, None).expect("test: registration");
            before
                .assign(
                    &operator_policy(),
                    &AssignPrincipal::operator_plane(),
                    &registration.session_id,
                    "count the crates",
                    Disposition::Queue,
                )
                .expect("test: assignment accepted");
            let pulled = before
                .pull(&registration.session_id, &registration.token, &[], None)
                .expect("test: pull");
            let assignment_id = pulled
                .assignments
                .first()
                .map(|assignment| assignment.assignment_id.clone())
                .expect("test: one assignment");
            before
                .report(
                    &registration.session_id,
                    &registration.token,
                    &assignment_id,
                    ResultStatus::Completed,
                    "42 crates",
                )
                .expect("test: report")
        };

        let after = mailbox_at(&db);
        let page = after.results_after(0, RESULT_LOG_CAPACITY);
        let restored = page
            .results
            .iter()
            .find(|result| result.seq == first_seq)
            .expect("test: the result must survive the restart");
        assert_eq!(restored.status, ResultStatus::Completed);
        assert_eq!(restored.summary, "42 crates");
        assert!(
            after.results.lock().next_seq > first_seq,
            "the sequence must keep climbing across restarts, or a cached cursor starts lying"
        );
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

    // ── Ordering, idempotence and atomicity ─────────────────────────────────

    /// A pull whose persistence is still in flight must not be able to undo a
    /// later pull's acknowledgement once it finally lands.
    ///
    /// The schedule is the one the audit reproduced: the first pull mutates
    /// memory and then pauses at its first store write; a retry reclaims and
    /// redelivers the same assignment; a third pull acknowledges it; only then
    /// is the first pull's flush released. Every write *succeeds* — the defect
    /// is ordering, not durability — so the assertions are on what the file
    /// says afterwards and on what a fresh process makes of it.
    ///
    /// MUTATION GUARD: drop the revision guard from `move_to_queue` and the
    /// stored queue reads `delivered` again and the replay hands acknowledged
    /// work back out; drop it from `record_poll` and the stored poll count
    /// falls back to 1.
    #[tokio::test]
    async fn a_stale_flush_cannot_overwrite_a_newer_acknowledgement() {
        use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

        /// Delegates every call to the real store, and holds the *first*
        /// `record_poll` at the door until the test lets it through.
        struct GatedStore {
            inner: SqliteMailboxStore,
            armed: std::sync::atomic::AtomicBool,
            entered: SyncSender<()>,
            release: Mutex<Receiver<()>>,
        }

        impl MailboxStore for GatedStore {
            fn hydrate(&self) -> anyhow::Result<StoredState> {
                self.inner.hydrate()
            }
            fn insert_session(&self, session: &StoredSession) -> anyhow::Result<()> {
                self.inner.insert_session(session)
            }
            fn record_poll(&self, session_id: &str, polls: u64, at_unix_ms: u64, revision: u64) -> anyhow::Result<()> {
                if self.armed.swap(false, Ordering::SeqCst) {
                    let _ = self.entered.send(());
                    let _ = self.release.lock().recv();
                }
                self.inner.record_poll(session_id, polls, at_unix_ms, revision)
            }
            fn enqueue(&self, assignment: &StoredAssignment, at_front: bool) -> anyhow::Result<()> {
                self.inner.enqueue(assignment, at_front)
            }
            fn move_to_queue(
                &self,
                assignment_id: &str,
                queue: Queue,
                deliveries: u32,
                to_front: bool,
                revision: u64,
            ) -> anyhow::Result<()> {
                self.inner
                    .move_to_queue(assignment_id, queue, deliveries, to_front, revision)
            }
            fn complete_assignment(&self, assignment_id: &str, result: &StoredResult) -> anyhow::Result<()> {
                self.inner.complete_assignment(assignment_id, result)
            }
            fn cascade_session(&self, session_id: &str, results: &[StoredResult]) -> anyhow::Result<()> {
                self.inner.cascade_session(session_id, results)
            }
        }

        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");
        let (entered_tx, entered_rx) = sync_channel::<()>(1);
        let (release_tx, release_rx) = sync_channel::<()>(1);
        let store = Arc::new(GatedStore {
            inner: SqliteMailboxStore::open(&db).expect("test: open the mailbox store"),
            armed: std::sync::atomic::AtomicBool::new(true),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let sessions = Arc::new(ChatSessions::new());
        sessions.attach_store(Arc::clone(&store) as Arc<dyn MailboxStore>);

        let registration = sessions.register(LABEL, None).expect("test: registration");
        let receipt = sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                "inspect fixture",
                Disposition::Queue,
            )
            .expect("test: assignment accepted");

        let delayed = {
            let sessions = Arc::clone(&sessions);
            let session_id = registration.session_id.clone();
            let token = registration.token.clone();
            std::thread::spawn(move || sessions.pull(&session_id, &token, &[], Some(1)))
        };
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("test: the first pull must reach the store");

        let retry = sessions
            .pull(&registration.session_id, &registration.token, &[], Some(1))
            .expect("test: the retry pull");
        assert_eq!(
            retry.assignments.first().map(|entry| entry.assignment_id.clone()),
            Some(receipt.assignment_id.clone()),
            "the retry must reclaim and redeliver the same assignment"
        );
        let acked = sessions
            .pull(
                &registration.session_id,
                &registration.token,
                std::slice::from_ref(&receipt.assignment_id),
                Some(1),
            )
            .expect("test: the acknowledging pull");
        assert_eq!(acked.acked, 1);

        let _ = release_tx.send(());
        delayed
            .join()
            .expect("test: the delayed pulling thread")
            .expect("test: the delayed pull still answers");

        let listed = sessions.list();
        let live = listed.first().expect("test: the session is registered");
        assert_eq!(live.accepted, 1, "memory holds the acknowledgement");
        assert_eq!(live.polls, 3);

        let disk = store.hydrate().expect("test: read the mailbox file back");
        assert_eq!(
            disk.assignments.first().map(|entry| entry.queue),
            Some(Queue::Accepted),
            "a flush planned before the acknowledgement must not put the assignment back on disk as delivered"
        );
        assert_eq!(
            disk.sessions.first().map(|entry| entry.polls),
            Some(3),
            "the stored poll count may not run backwards either"
        );

        drop(sessions);
        drop(store);

        let after = mailbox_at(&db);
        let listed = after.list();
        let restored = listed.first().expect("test: the session survives the restart");
        assert_eq!(
            (restored.queued, restored.delivered, restored.accepted),
            (0, 0, 1),
            "a fresh process must not hand acknowledged work back out"
        );
    }

    /// Attaching the same store again — which the gateway supervisor does every
    /// time it restarts the gateway inside one daemon process — must not
    /// duplicate the result feed.
    ///
    /// MUTATION GUARD: replace `ResultLog::insert` with a plain `push_back` and
    /// the sequences read `[1, 1, 1]`, which is what the audit reproduced.
    #[tokio::test]
    async fn reattaching_the_store_never_duplicates_the_result_feed() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");
        let store: Arc<dyn MailboxStore> =
            Arc::new(SqliteMailboxStore::open(&db).expect("test: open the mailbox store"));
        let sessions = Arc::new(ChatSessions::new());
        sessions.attach_store(Arc::clone(&store));

        let registration = sessions.register(LABEL, None).expect("test: registration");
        let finish = |task: &str| {
            let receipt = sessions
                .assign(
                    &operator_policy(),
                    &AssignPrincipal::operator_plane(),
                    &registration.session_id,
                    task,
                    Disposition::Queue,
                )
                .expect("test: assignment accepted");
            sessions
                .pull(&registration.session_id, &registration.token, &[], Some(1))
                .expect("test: pull");
            sessions
                .report(
                    &registration.session_id,
                    &registration.token,
                    &receipt.assignment_id,
                    ResultStatus::Completed,
                    "done",
                )
                .expect("test: report")
        };
        let seqs = || {
            sessions
                .results_after(0, RESULT_LOG_CAPACITY)
                .results
                .iter()
                .map(|result| result.seq)
                .collect::<Vec<_>>()
        };

        let first = finish("count the crates");
        assert_eq!(seqs(), vec![first]);

        sessions.attach_store(Arc::clone(&store));
        assert_eq!(seqs(), vec![first], "a second attachment replays the same result once");
        sessions.attach_store(Arc::clone(&store));
        assert_eq!(seqs(), vec![first], "and a third changes nothing either");

        let second = finish("count them again");
        assert_eq!(seqs(), vec![first, second]);
        sessions.attach_store(Arc::clone(&store));
        assert_eq!(
            seqs(),
            vec![first, second],
            "replaying old and new results together must keep one ordered entry each"
        );

        let page = sessions.results_after(first, RESULT_LOG_CAPACITY);
        assert_eq!(
            page.results.iter().map(|result| result.seq).collect::<Vec<_>>(),
            vec![second],
            "a consumer cursor must still mean what it meant"
        );
        assert_eq!(
            page.oldest_seq, first,
            "nothing was trimmed, so nothing may be reported missed"
        );
    }

    /// The feed is a bounded, ordered, duplicate-free log whatever order
    /// entries arrive in — a replay may hand it sequences it already has, and
    /// sequences it trimmed long ago.
    #[test]
    fn the_result_log_merges_by_sequence_and_keeps_its_retention_floor() {
        fn entry(seq: u64) -> AssignmentResult {
            AssignmentResult {
                seq,
                assignment_id: format!("assignment-{seq}"),
                session_id: "session".to_string(),
                session_label: LABEL.to_string(),
                disposition: Disposition::Queue,
                status: ResultStatus::Completed,
                summary: "done".to_string(),
                completed_at_unix_ms: 0,
                origin: AssignPrincipal::operator_plane(),
            }
        }

        let mut log = ResultLog::new();
        let overflow = u64::try_from(RESULT_LOG_CAPACITY).unwrap_or(u64::MAX) + 2;
        for seq in 1..=overflow {
            log.insert(entry(seq));
        }
        assert_eq!(log.entries.len(), RESULT_LOG_CAPACITY);
        assert_eq!(log.oldest_seq, 3, "the two oldest entries were trimmed");

        log.insert(entry(overflow));
        assert_eq!(
            log.entries.len(),
            RESULT_LOG_CAPACITY,
            "a duplicate sequence is merged, not appended"
        );

        log.insert(entry(1));
        assert_eq!(
            log.oldest_seq, 3,
            "a sequence the feed already trimmed must not come back below its floor"
        );
        assert_eq!(log.entries.front().map(|first| first.seq), Some(3));

        log.insert(entry(overflow + 5));
        assert!(
            log.entries.iter().map(|held| held.seq).is_sorted(),
            "the feed stays ordered by sequence"
        );
        assert!(
            log.next_seq > overflow + 5,
            "minting must never reuse a sequence it replayed"
        );
    }

    /// Retiring an assignment and recording its result are one commit.
    ///
    /// The failure is injected into the real database rather than into a
    /// double: a trigger makes every result insert abort, which is exactly the
    /// boundary the audit crashed at. Either both halves land or neither does,
    /// so a failure has to leave the work outstanding on disk where a restart
    /// can find it.
    ///
    /// MUTATION GUARD: split `complete_assignment` back into a delete and an
    /// append and the stored assignment count reads 0.
    #[tokio::test]
    async fn a_failed_result_write_leaves_the_assignment_outstanding_on_disk() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");
        let sessions = mailbox_at(&db);

        let registration = sessions.register(LABEL, None).expect("test: registration");
        let receipt = sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                "inspect fixture",
                Disposition::Queue,
            )
            .expect("test: assignment accepted");
        sessions
            .pull(&registration.session_id, &registration.token, &[], Some(1))
            .expect("test: pull");

        let injector = rusqlite::Connection::open(&db).expect("test: open the mailbox file");
        injector
            .execute_batch(
                "CREATE TRIGGER refuse_results BEFORE INSERT ON chat_results
                 BEGIN SELECT RAISE(ABORT, 'injected result failure'); END;",
            )
            .expect("test: install the failure trigger");

        sessions
            .report(
                &registration.session_id,
                &registration.token,
                &receipt.assignment_id,
                ResultStatus::Completed,
                "fixture checked",
            )
            .expect("test: a failed write costs durability, never the answer");

        let disk = {
            let reader = SqliteMailboxStore::open(&db).expect("test: reopen the mailbox file");
            reader.hydrate().expect("test: read the mailbox file back")
        };
        assert!(
            disk.results.is_empty(),
            "the injected failure must keep the result off disk"
        );
        assert_eq!(
            disk.assignments.len(),
            1,
            "an assignment whose result could not be recorded must stay outstanding, not vanish with it"
        );
        assert_eq!(
            disk.assignments.first().map(|entry| entry.assignment_id.clone()),
            Some(receipt.assignment_id.clone())
        );

        injector
            .execute_batch("DROP TRIGGER refuse_results")
            .expect("test: remove the failure trigger");
        drop(injector);
        drop(sessions);

        let after = mailbox_at(&db);
        let pulled = after
            .pull(&registration.session_id, &registration.token, &[], None)
            .expect("test: pull after the restart");
        assert_eq!(
            pulled.assignments.first().map(|entry| entry.assignment_id.clone()),
            Some(receipt.assignment_id),
            "work whose completion never committed must come back, not be lost between two writes"
        );
    }

    /// A report whose response was lost is replayed by the chat session. The
    /// replay must find its own result rather than be told the assignment is
    /// unknown, and it must not mint a second completion sequence.
    ///
    /// MUTATION GUARD: remove the replay lookup from `report` and the second
    /// call refuses with `UnknownAssignment`.
    #[tokio::test]
    async fn replaying_a_report_returns_the_original_sequence() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");
        let sessions = mailbox_at(&db);

        let registration = sessions.register(LABEL, None).expect("test: registration");
        let receipt = sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                "inspect fixture",
                Disposition::Queue,
            )
            .expect("test: assignment accepted");
        sessions
            .pull(&registration.session_id, &registration.token, &[], Some(1))
            .expect("test: pull");

        let report_once = || {
            sessions.report(
                &registration.session_id,
                &registration.token,
                &receipt.assignment_id,
                ResultStatus::Completed,
                "fixture checked",
            )
        };
        let seq = report_once().expect("test: the first report");
        let replayed = report_once().expect("test: the replay must be answered, not refused");
        assert_eq!(replayed, seq, "a replay may not mint a second completion sequence");

        let recorded = sessions
            .results_after(0, RESULT_LOG_CAPACITY)
            .results
            .into_iter()
            .filter(|result| result.assignment_id == receipt.assignment_id)
            .count();
        assert_eq!(recorded, 1, "the feed holds one record for one assignment");

        let disk = {
            let reader = SqliteMailboxStore::open(&db).expect("test: reopen the mailbox file");
            reader.hydrate().expect("test: read the mailbox file back")
        };
        assert_eq!(disk.results.len(), 1, "and so does the file");
        assert!(disk.assignments.is_empty(), "the assignment is retired exactly once");
    }

    /// Deregistration is the same commit: the session cascade and every
    /// cancellation it owes go together, so a failure cannot leave an assigner
    /// with neither its work nor an answer.
    ///
    /// MUTATION GUARD: delete the session before recording the cancellations
    /// and the stored session count reads 0.
    #[tokio::test]
    async fn a_failed_deregistration_keeps_the_session_and_its_queue_on_disk() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");
        let sessions = mailbox_at(&db);

        let registration = sessions.register(LABEL, None).expect("test: registration");
        sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                "inspect fixture",
                Disposition::Queue,
            )
            .expect("test: assignment accepted");

        let injector = rusqlite::Connection::open(&db).expect("test: open the mailbox file");
        injector
            .execute_batch(
                "CREATE TRIGGER refuse_results BEFORE INSERT ON chat_results
                 BEGIN SELECT RAISE(ABORT, 'injected result failure'); END;",
            )
            .expect("test: install the failure trigger");

        assert_eq!(
            sessions.deregister(&registration.session_id),
            Some(1),
            "deregistration still answers while the disk is refusing"
        );

        let disk = {
            let reader = SqliteMailboxStore::open(&db).expect("test: reopen the mailbox file");
            reader.hydrate().expect("test: read the mailbox file back")
        };
        assert_eq!(
            disk.sessions.len(),
            1,
            "a cascade whose cancellation records could not be written must not commit half of itself"
        );
        assert_eq!(disk.assignments.len(), 1);
        assert!(disk.results.is_empty());
    }

    /// A pull that has to wait on a database another process holds spends
    /// **one** contention budget, not one per write it owes.
    ///
    /// MUTATION GUARD: drop the `BoundedBusyBudget` from `flush_pending` and
    /// each of the pull's writes waits out the full bound on its own, which is
    /// the eleven seconds the audit measured for a single assignment.
    #[tokio::test]
    async fn a_locked_mailbox_costs_one_contention_budget_for_a_whole_pull() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let db = dir.path().join("state").join("chat-mailbox.db");
        let sessions = mailbox_at(&db);

        let registration = sessions.register(LABEL, None).expect("test: registration");
        sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                "inspect fixture",
                Disposition::Queue,
            )
            .expect("test: assignment accepted");

        let foreign = rusqlite::Connection::open(&db).expect("test: open the mailbox file");
        foreign
            .execute_batch("BEGIN IMMEDIATE")
            .expect("test: hold the write lock as a second process would");
        let started = Instant::now();
        let pulled = sessions
            .pull(&registration.session_id, &registration.token, &[], Some(1))
            .expect("test: a locked database costs durability, never the work");
        let elapsed = started.elapsed();
        foreign.execute_batch("ROLLBACK").expect("test: release the write lock");

        assert_eq!(tasks_in_order(&pulled), vec!["inspect fixture".to_string()]);
        let budget = crate::runtime::sqlite_pool::BOUNDED_BUSY_WAIT;
        assert!(
            elapsed < budget.mul_f32(1.5),
            "one pull must spend one contention budget ({budget:?}), not one per write: took {elapsed:?}"
        );
    }
}
