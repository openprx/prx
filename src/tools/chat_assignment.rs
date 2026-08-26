//! The return leg of "assign work to a `prx chat` session from a messaging
//! channel".
//!
//! [`crate::runtime::chat_sessions`] holds the mailbox and records *who* asked
//! for a piece of work, but it deliberately sends nothing: routing a finished
//! result back to a correspondent is an outbound action and belongs on the same
//! gated path every other outbound message uses. This module is that path.
//!
//! # Shape
//!
//! One accepted assignment spawns one return trip. The trip watches the
//! in-process result feed for its own `assignment_id` and, when the result
//! lands, relays it to the conversation the assignment came from.
//!
//! Why a trip per assignment rather than one daemon-wide consumer: the
//! conversation to reply into is a fact of the *assigning turn* (its trusted
//! `_zc_scope`), and the channel object to reply on is resolved from that same
//! turn. Capturing both at assignment time is what makes the reply immune to
//! whatever channel happens to be active minutes later, in exactly the way
//! `sessions_spawn`'s completion announcement captures its own recipient at
//! spawn time.
//!
//! # No wall clock
//!
//! [`RESULT_POLL_INTERVAL`] is a *sampling* interval, the same kind
//! `sessions_spawn`'s `JOIN_POLL_INTERVAL` is: there is no elapsed check, no
//! iteration cap and no `tokio::time::timeout` anywhere in [`run_return_trip`].
//! Shorten it and a result surfaces sooner; lengthen it and the only cost is
//! latency. A trip ends for one of three reasons, none of them a duration:
//! the result arrived, the result provably cannot arrive any more, or an
//! operator killed the trip's work-registry row.
//!
//! # Two identities, on purpose
//!
//! * **Who the reply is authorized as** comes from the *recorded* origin on the
//!   result ([`crate::runtime::chat_sessions::AssignmentResult::origin`]), the
//!   only place the correspondent survives in the clear, and the trip refuses to
//!   deliver a result whose recorded origin is not the one it was created for.
//! * **Where the reply goes** comes from the assigning turn's trusted scope and
//!   is fixed at assignment time. The model never supplies it, so `chat_assign`
//!   cannot be used to address a message to a recipient of the model's choosing
//!   — a narrower rule than the announcement path, which does accept an explicit
//!   `recipient` (and gates it).
//!
//! Either way the send passes [`crate::tools::sessions_spawn::announce_is_authorized`],
//! the same outbound gate the announcement and kill-notice paths funnel into, and
//! a refusal is recorded with the destination's `op_id` fingerprint rather than
//! its plaintext address.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::channels::traits::{Channel, SendMessage};
use crate::runtime::chat_sessions::{self, AssignPrincipal, AssignmentResult, ResultStatus};
use crate::runtime::registry::WorkId;
use crate::security::SecurityPolicy;
use crate::tools::sessions_spawn::{AnnounceOrigin, announce_is_authorized};

/// How often a return trip re-reads the result feed.
///
/// A sampling interval, not a deadline — see the module note. Matched to
/// `sessions_spawn`'s `JOIN_POLL_INTERVAL` so the two waits in this crate have
/// one cadence rather than two arbitrary ones.
pub(crate) const RESULT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How many feed entries one poll reads.
///
/// A read-batch size, not a cap on outstanding work: whatever does not fit is
/// still there on the next poll, because the cursor only advances over entries
/// this trip actually saw.
const RESULT_POLL_BATCH: usize = 64;

/// Which of the two things a relay carried, as it appears in the delivery log.
///
/// Both endings send, and only this field tells them apart: one says the chat
/// session answered, the other says the answer was lost before it could be read.
/// An operator reading the log needs that distinction, and the message body — the
/// only other place it is written down — deliberately never reaches the log.
const RELAY_RESULT: &str = "result";
/// The relay carried an eviction notice rather than an answer. See [`RELAY_RESULT`].
const RELAY_EVICTION: &str = "eviction";

/// Everything a return trip needs, captured at assignment time.
pub(crate) struct ReturnTrip {
    /// The assignment this trip is waiting for. Also the address an operator
    /// uses to kill the assignment itself; the trip's own row is separate.
    pub(crate) assignment_id: String,
    /// Redacted reference of the target chat session, identical to the one in
    /// the assignment's own registry row.
    pub(crate) session_ref: String,
    /// The origin this trip was created for. A result recorded against a
    /// different origin is not this trip's to deliver.
    pub(crate) origin: AssignPrincipal,
    /// Conversation to reply into, fixed at assignment time from the assigning
    /// turn's trusted scope. Never model-supplied.
    pub(crate) recipient: String,
    /// Channel object resolved from the assigning turn's channel name.
    pub(crate) channel: Arc<dyn Channel>,
    pub(crate) security: Arc<SecurityPolicy>,
}

/// How a return trip ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TripEnding {
    /// The result arrived and was handed to the delivery step.
    Delivered,
    /// The result arrived but its recorded origin is not the one this trip was
    /// created for, so it was not delivered.
    OriginMismatch,
    /// The assignment ended, but its result had already been evicted from the
    /// feed's report buffer before this trip could read it. The correspondent is
    /// told that much rather than left waiting on a result that will never come.
    ResultEvicted,
    /// An operator killed the trip's work-registry row. Nothing is sent: ending
    /// the relay was the explicit intent.
    Cancelled,
}

/// What one poll of the feed concluded.
enum PollVerdict {
    /// The result for this trip's assignment.
    Found(Box<AssignmentResult>),
    /// Nothing yet; keep waiting from the returned cursor.
    Waiting { cursor: u64 },
    /// The assignment's registry row is gone and the feed has nothing for it.
    ///
    /// Sound because of the order [`chat_sessions::report`] works in: the
    /// result is appended to the feed *before* the [`chat_sessions::Assignment`]
    /// (and with it the work-registry row it owns) is dropped. A row that has
    /// gone away while the feed still has nothing therefore cannot mean "not
    /// finished yet"; it means the entry was pushed out of the bounded buffer
    /// between two polls.
    Evicted,
}

/// Names the variant and, for a found result, its assignment id only.
///
/// Hand-written rather than derived because a derived form would print the
/// summary and the plaintext origin the result carries, and this type appears in
/// log and test-failure output.
impl std::fmt::Debug for PollVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Found(result) => write!(f, "Found({})", result.assignment_id),
            Self::Waiting { cursor } => write!(f, "Waiting({cursor})"),
            Self::Evicted => write!(f, "Evicted"),
        }
    }
}

/// Read the feed once and decide what this poll proves.
///
/// Split out of the loop so the decision — including the eviction inference,
/// which is the subtle one — can be asserted directly instead of only through a
/// spawned task.
fn poll_once(assignment_id: &str, cursor: u64) -> PollVerdict {
    let page = chat_sessions::results_after(cursor, RESULT_POLL_BATCH);
    // Read before the cursor moves: entries in `(cursor, oldest_seq)` existed and
    // are gone, so this poll cannot vouch for having seen everything.
    let missed_entries = page.oldest_seq > cursor.saturating_add(1);
    let mut cursor = cursor;
    for result in page.results {
        cursor = result.seq;
        if result.assignment_id == assignment_id {
            return PollVerdict::Found(Box::new(result));
        }
    }
    // The row check runs *after* the feed read, never before: reading the row
    // first would let a result that was appended in between look like an
    // eviction.
    let row_present = crate::runtime::registry::resolve_address(assignment_id).is_some();
    if result_is_unrecoverable(missed_entries, row_present) {
        return PollVerdict::Evicted;
    }
    PollVerdict::Waiting { cursor }
}

/// Whether "the feed has nothing for this assignment" means the result is gone
/// for good rather than merely not ready yet.
///
/// Both conditions are load-bearing and neither alone is sound:
///
/// * A **missing row** on its own is the ordinary state one poll before the
///   result is read — [`chat_sessions::report`] appends to the feed and *then*
///   drops the assignment — so concluding loss from it would abandon nearly
///   every assignment right at the finish line.
/// * **Missed entries** on their own say only that the buffer overflowed past
///   this cursor, which is silent about whether *this* assignment is among the
///   casualties; it may not even have started.
///
/// MUTATION GUARD: drop either operand and
/// `an_absent_result_is_only_called_lost_when_both_conditions_hold` goes red.
const fn result_is_unrecoverable(missed_entries: bool, row_present: bool) -> bool {
    missed_entries && !row_present
}

/// Wait for this trip's result and relay it.
///
/// The cursor starts at zero rather than at the feed's head: the feed keeps at
/// most `RESULT_LOG_CAPACITY` entries, walking them once costs nothing, and a
/// trip that started from the head would have to *ask* for the head — a value
/// that is already stale by the time it is read.
pub(crate) async fn run_return_trip(trip: ReturnTrip, cancel: CancellationToken) -> TripEnding {
    let mut cursor = 0u64;
    loop {
        match poll_once(&trip.assignment_id, cursor) {
            PollVerdict::Found(result) => {
                if result.origin != trip.origin {
                    // Not reachable through the mailbox as it stands — an
                    // assignment id is a fresh uuid — but the check is what makes
                    // "the reply is authorized as the origin the feed recorded"
                    // a property rather than a comment.
                    tracing::warn!(
                        session = %trip.session_ref,
                        recipient = %trip.recipient_ref(),
                        "chat assignment result withheld: its recorded origin is not the one that asked for it"
                    );
                    return TripEnding::OriginMismatch;
                }
                deliver(&trip, &result.origin, &render_result(&result), RELAY_RESULT).await;
                return TripEnding::Delivered;
            }
            PollVerdict::Evicted => {
                deliver(&trip, &trip.origin, &render_eviction(&trip), RELAY_EVICTION).await;
                return TripEnding::ResultEvicted;
            }
            PollVerdict::Waiting { cursor: next } => cursor = next,
        }
        tokio::select! {
            () = cancel.cancelled() => return TripEnding::Cancelled,
            () = tokio::time::sleep(RESULT_POLL_INTERVAL) => {}
        }
    }
}

impl ReturnTrip {
    /// The destination as it may appear in a log line.
    fn recipient_ref(&self) -> String {
        crate::security::op_id::ref_for_channel_recipient(self.channel.name(), &self.recipient)
    }
}

/// What the correspondent reads when their assignment finishes.
fn render_result(result: &AssignmentResult) -> String {
    let verdict = match result.status {
        ResultStatus::Completed => "finished",
        ResultStatus::Failed => "failed",
        ResultStatus::Rejected => "declined",
        ResultStatus::Cancelled => "was cancelled",
    };
    format!(
        "Chat session '{}' {} the task you assigned ({}):\n\n{}",
        result.session_label,
        verdict,
        result.assignment_id,
        result.summary.trim()
    )
}

/// What the correspondent reads when the result existed but is no longer
/// readable. Saying so is the honest ending; silence would leave them waiting on
/// a relay that has already given up.
///
/// The session is named by its `op_id` reference rather than its self-declared
/// label: this text goes out over a channel, and a label is whatever the chat
/// process claimed at registration.
fn render_eviction(trip: &ReturnTrip) -> String {
    format!(
        "The chat session you assigned {} to (session {}) finished it, but its result had already \
         been dropped from the daemon's report buffer before it could be relayed. Ask the session \
         directly.",
        trip.assignment_id, trip.session_ref
    )
}

/// Whether this principal may hand work to this session.
///
/// The listing consults it per session so a correspondent is shown only the
/// sessions it could actually assign to. Without that, `chat_sessions` would be
/// the enumeration oracle `chat_sessions::assign` deliberately refuses to be —
/// it authorizes before reporting a session unknown for exactly this reason.
pub(crate) fn may_assign(policy: &SecurityPolicy, principal: &AssignPrincipal, label: &str, session_id: &str) -> bool {
    match principal {
        // Holding the gateway token already means being the local operator; the
        // mailbox makes the same exemption in `assign`.
        AssignPrincipal::OperatorPlane => true,
        AssignPrincipal::Correspondent {
            channel,
            sender,
            chat_type,
        } => policy.is_assignment_allowed(sender, channel, chat_type, label, session_id),
    }
}

/// The live sessions `principal` may hand work to, rendered for a refusal that
/// would otherwise leave the caller guessing.
///
/// Exists because an unknown `session_id` is a refusal a model answers by
/// *inventing another one*: on the first real-machine run an agent made up
/// `prx-agent-chat-6914`, was refused, and only then asked for the real listing —
/// a whole extra round trip through the correspondent's conversation.
///
/// The filter is [`may_assign`], per session, and it is the same one
/// `execute_chat_sessions` applies. That is not a convenience: naming a session
/// this caller may not assign to would hand back exactly what
/// [`chat_sessions::assign`]'s authorize-before-report-unknown ordering exists to
/// withhold, and would do it on a *cheaper* path than the listing action. A hint
/// is a listing, and it is filtered like one.
///
/// [`None`] when nothing is assignable, so a caller with no grants is told
/// nothing beyond the bare refusal — not even that sessions exist.
pub(crate) fn assignable_sessions_hint(policy: &SecurityPolicy, principal: &AssignPrincipal) -> Option<String> {
    let assignable = chat_sessions::list()
        .into_iter()
        .filter(|session| may_assign(policy, principal, &session.label, &session.session_id))
        .map(|session| {
            format!(
                "'{}' (label '{}', ref {})",
                session.session_id, session.label, session.session_ref
            )
        })
        .collect::<Vec<_>>();
    (!assignable.is_empty()).then(|| assignable.join(", "))
}

/// Put one relayed result on the channel, and record what happened either way.
///
/// # What the log may and may not say
///
/// Every outcome — withheld, failed, sent — is logged with the same identifying
/// fields (channel, recipient, session, assignment, relay kind), so the last hop
/// of an assignment is legible from the
/// server side alone instead of having to be inferred from the *absence* of a
/// warning. Two rules hold across all three lines:
///
/// * **The recipient is never in the clear.** It appears only as the
///   `op_id::ref_for_channel_recipient` fingerprint, the same function over the
///   same two inputs that the assignment's refusal and the relay's registry row
///   already use, so one correspondent reads identically everywhere. The channel
///   name stays plaintext: it is not sensitive and it is what makes the row
///   traceable.
/// * **The body is never logged.** `bytes` is the only thing said about it —
///   enough to tell a real answer from an empty one, and nothing more.
///
/// MUTATION GUARD: drop the [`announce_is_authorized`] branch and
/// `a_denied_recipient_is_withheld_and_never_sent` goes red — that test is the
/// only thing standing between this path and an ungated outbound send, because
/// the recipient here is never re-derived from anything the outbound ACL sees.
/// Drop the success `info!` and `a_relayed_result_is_logged_without_the_recipient_or_the_body`
/// goes red; write `trip.recipient` or `text` into any of the three lines and the
/// same test goes red.
async fn deliver(trip: &ReturnTrip, origin: &AssignPrincipal, text: &str, kind: &'static str) {
    let announce_origin = match origin {
        AssignPrincipal::Correspondent { sender, chat_type, .. } => AnnounceOrigin::from_parts(sender, chat_type),
        // No correspondent means nobody to reply to. The operator plane assigns
        // over HTTP and reads its results back from the feed endpoint.
        AssignPrincipal::OperatorPlane => return,
    };
    let channel_name = trip.channel.name();
    if !announce_is_authorized(&trip.security, Some(&announce_origin), channel_name, &trip.recipient) {
        // The refusal names the destination only as the fingerprint
        // `op_id::ref_for_channel_recipient` produces — the same function, over
        // the same two inputs, that the assignment's own refusal and registry
        // row already use, so one recipient reads identically everywhere.
        tracing::warn!(
            channel = %channel_name,
            recipient = %trip.recipient_ref(),
            session = %trip.session_ref,
            assignment = %trip.assignment_id,
            relay = kind,
            "Chat assignment result withheld: the configured scope rules do not permit this recipient"
        );
        return;
    }
    let message = SendMessage::new(text, &trip.recipient);
    let bytes = text.len();
    if let Err(error) = trip.channel.send(&message).await {
        tracing::error!(
            channel = %channel_name,
            recipient = %trip.recipient_ref(),
            session = %trip.session_ref,
            assignment = %trip.assignment_id,
            relay = kind,
            bytes,
            "Failed to relay chat assignment result: {error}"
        );
        return;
    }
    tracing::info!(
        channel = %channel_name,
        recipient = %trip.recipient_ref(),
        session = %trip.session_ref,
        assignment = %trip.assignment_id,
        relay = kind,
        bytes,
        "Relayed chat assignment result"
    );
}

/// Start the return trip for an accepted assignment and hand back the address of
/// its work-registry row.
///
/// The row exists because the relay can park with no upper bound: it waits on a
/// chat session that has no deadline, and then on a channel send that talks to a
/// platform over a socket this process does not control. Once nothing expires by
/// itself, being seen is the precondition for being ended.
///
/// Two decisions copied verbatim from the assignment row this trip shadows:
///
/// * **Its own run id, never the assignment's.** `registry::resolve_address`
///   answers a run id with that run's lineage root, so two rows sharing one run
///   id would make `prx tasks kill <assignment_id>` ambiguous — an operator
///   aiming at the assignment could end up ending the relay instead, or the
///   reverse.
/// * **No parent.** The trip outlives the turn that requested it by design (that
///   turn answers "queued" and ends), so hanging it under `current_work_id()`
///   would put it in a cascade that kills it immediately.
///
/// MUTATION GUARD: drop the registration and
/// `a_return_trip_is_listed_and_killable` goes red; write the recipient or the
/// session id into the row name in the clear and
/// `a_return_trip_row_fingerprints_both_ends` goes red.
pub(crate) fn spawn_return_trip(trip: ReturnTrip) -> WorkId {
    let cancel = CancellationToken::new();
    let name = format!(
        "chat_result {} {} → {} {}",
        chat_sessions::CHAT_SESSION_CHANNEL,
        trip.session_ref,
        trip.channel.name(),
        trip.recipient_ref(),
    );
    let run_id = uuid::Uuid::new_v4().to_string();
    let work = crate::runtime::registry::register_sub_agent(&name, &run_id, None, None, Some(cancel.clone()));
    let work_id = work.id();
    tokio::spawn(async move {
        // Held for the whole trip: dropping the guard is what retires the row,
        // so it must not be dropped before the relay is actually over.
        let _work = work;
        run_return_trip(trip, cancel).await
    });
    work_id
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::new_ret_no_self)]
    use super::*;
    use crate::channels::traits::ChannelMessage;
    use crate::config::ScopeRule;
    use crate::runtime::chat_sessions::Disposition;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::io::Write;
    use tracing_subscriber::fmt::writer::MakeWriter;

    const SENDER: &str = "+15550009999";
    const CHANNEL: &str = "wacli";

    /// Collects formatted `tracing` output so a test can assert that a line was
    /// actually emitted — and, just as importantly, what it does *not* contain.
    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock()).into_owned()
        }
    }

    impl Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturedLogs {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    struct RecordingChannel {
        sent: Arc<Mutex<Vec<SendMessage>>>,
    }

    impl RecordingChannel {
        fn new() -> (Arc<dyn Channel>, Arc<Mutex<Vec<SendMessage>>>) {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let channel: Arc<dyn Channel> = Arc::new(Self {
                sent: Arc::clone(&sent),
            });
            (channel, sent)
        }
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            CHANNEL
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().push(message.clone());
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn correspondent() -> AssignPrincipal {
        AssignPrincipal::Correspondent {
            channel: CHANNEL.to_string(),
            sender: SENDER.to_string(),
            chat_type: "direct".to_string(),
        }
    }

    /// A policy that lets this correspondent assign anywhere, with the outbound
    /// rules the test wants layered on top.
    fn policy(send_allow: &[&str], send_deny: &[&str]) -> Arc<SecurityPolicy> {
        let rule = ScopeRule {
            channel: Some(CHANNEL.to_string()),
            assign_allow: vec!["*:*".to_string()],
            send_allow: send_allow.iter().map(|entry| (*entry).to_string()).collect(),
            send_deny: send_deny.iter().map(|entry| (*entry).to_string()).collect(),
            ..ScopeRule::default()
        };
        Arc::new(SecurityPolicy {
            scope_rules: vec![rule],
            ..SecurityPolicy::default()
        })
    }

    fn trip(
        assignment_id: &str,
        origin: AssignPrincipal,
        channel: Arc<dyn Channel>,
        security: Arc<SecurityPolicy>,
    ) -> ReturnTrip {
        ReturnTrip {
            assignment_id: assignment_id.to_string(),
            session_ref: "session-ref".to_string(),
            origin,
            recipient: SENDER.to_string(),
            channel,
            security,
        }
    }

    /// Register a session, assign one task to it, and pull it the way a live
    /// chat would — `report` only accepts an assignment the session has actually
    /// been handed.
    fn assign_one(policy: &SecurityPolicy, principal: &AssignPrincipal) -> (String, String, String) {
        let registration = chat_sessions::register("workstation", None).expect("registration");
        let receipt = chat_sessions::assign(
            policy,
            principal,
            &registration.session_id,
            "do the thing",
            Disposition::Queue,
        )
        .expect("assignment accepted");
        chat_sessions::pull(&registration.session_id, &registration.token, &[], None).expect("pull");
        (registration.session_id, registration.token, receipt.assignment_id)
    }

    #[tokio::test]
    async fn a_finished_assignment_is_relayed_to_the_conversation_that_asked_for_it() {
        let security = policy(&[], &[]);
        let (channel, sent) = RecordingChannel::new();
        let (session_id, token, assignment_id) = assign_one(&security, &correspondent());

        let ending = tokio::spawn({
            let trip = trip(
                &assignment_id,
                correspondent(),
                Arc::clone(&channel),
                Arc::clone(&security),
            );
            let cancel = CancellationToken::new();
            async move { run_return_trip(trip, cancel).await }
        });

        chat_sessions::report(
            &session_id,
            &token,
            &assignment_id,
            ResultStatus::Completed,
            "37 TODOs, mostly in the parser",
        )
        .expect("result accepted");

        assert_eq!(ending.await.expect("trip joined"), TripEnding::Delivered);
        let sent = sent.lock();
        assert_eq!(sent.len(), 1, "exactly one relay per assignment");
        assert_eq!(
            sent[0].recipient, SENDER,
            "the reply goes to the conversation that asked"
        );
        assert!(
            sent[0].content.contains("37 TODOs, mostly in the parser"),
            "the summary must reach the correspondent: {}",
            sent[0].content
        );
        assert!(
            sent[0].content.contains("workstation"),
            "the reply must say which session answered: {}",
            sent[0].content
        );
        chat_sessions::deregister(&session_id);
    }

    #[tokio::test]
    async fn a_denied_recipient_is_withheld_and_never_sent() {
        let security = policy(&[], &[&format!("{CHANNEL}:{SENDER}")]);
        let (channel, sent) = RecordingChannel::new();
        let (session_id, token, assignment_id) = assign_one(&security, &correspondent());

        let ending = tokio::spawn({
            let trip = trip(
                &assignment_id,
                correspondent(),
                Arc::clone(&channel),
                Arc::clone(&security),
            );
            let cancel = CancellationToken::new();
            async move { run_return_trip(trip, cancel).await }
        });

        chat_sessions::report(&session_id, &token, &assignment_id, ResultStatus::Completed, "done").expect("reported");

        assert_eq!(ending.await.expect("trip joined"), TripEnding::Delivered);
        assert!(
            sent.lock().is_empty(),
            "an outbound ACL that denies this recipient must stop the relay"
        );
        chat_sessions::deregister(&session_id);
    }

    #[tokio::test]
    async fn a_result_recorded_for_another_origin_is_not_relayed() {
        let security = policy(&[], &[]);
        let (channel, sent) = RecordingChannel::new();
        let (session_id, token, assignment_id) = assign_one(&security, &correspondent());

        let stranger = AssignPrincipal::Correspondent {
            channel: CHANNEL.to_string(),
            sender: "+15550001234".to_string(),
            chat_type: "direct".to_string(),
        };
        let ending = tokio::spawn({
            let trip = trip(&assignment_id, stranger, Arc::clone(&channel), Arc::clone(&security));
            let cancel = CancellationToken::new();
            async move { run_return_trip(trip, cancel).await }
        });

        chat_sessions::report(&session_id, &token, &assignment_id, ResultStatus::Completed, "done").expect("reported");

        assert_eq!(ending.await.expect("trip joined"), TripEnding::OriginMismatch);
        assert!(sent.lock().is_empty(), "a result belongs only to the origin that asked");
        chat_sessions::deregister(&session_id);
    }

    #[tokio::test]
    async fn a_killed_return_trip_stops_without_sending() {
        let security = policy(&[], &[]);
        let (channel, sent) = RecordingChannel::new();
        let (session_id, _token, assignment_id) = assign_one(&security, &correspondent());
        let cancel = CancellationToken::new();

        let ending = tokio::spawn({
            let trip = trip(
                &assignment_id,
                correspondent(),
                Arc::clone(&channel),
                Arc::clone(&security),
            );
            let cancel = cancel.clone();
            async move { run_return_trip(trip, cancel).await }
        });
        cancel.cancel();

        assert_eq!(ending.await.expect("trip joined"), TripEnding::Cancelled);
        assert!(sent.lock().is_empty(), "a killed relay says nothing");
        chat_sessions::deregister(&session_id);
    }

    #[tokio::test]
    async fn a_return_trip_is_listed_and_killable() {
        let security = policy(&[], &[]);
        let (channel, _sent) = RecordingChannel::new();
        let (session_id, _token, assignment_id) = assign_one(&security, &correspondent());

        let work_id = spawn_return_trip(trip(
            &assignment_id,
            correspondent(),
            Arc::clone(&channel),
            Arc::clone(&security),
        ));

        let row = crate::runtime::registry::snapshot(work_id).expect("the relay must be listed while it waits");
        assert_eq!(
            row.parent, None,
            "a relay outlives its requesting turn, so it has no parent"
        );
        assert_ne!(
            row.run_id.as_deref(),
            Some(assignment_id.as_str()),
            "the relay must not borrow the assignment's kill address"
        );
        let killed = crate::runtime::registry::kill(work_id, true).await;
        assert_eq!(
            killed.first().map(|result| result.outcome),
            Some(crate::runtime::registry::KillOutcome::Killed),
            "the relay must really stop, not merely be asked to"
        );
        chat_sessions::deregister(&session_id);
    }

    #[tokio::test]
    async fn a_return_trip_row_fingerprints_both_ends() {
        let security = policy(&[], &[]);
        let (channel, _sent) = RecordingChannel::new();
        let (session_id, _token, assignment_id) = assign_one(&security, &correspondent());

        let work_id = spawn_return_trip(trip(
            &assignment_id,
            correspondent(),
            Arc::clone(&channel),
            Arc::clone(&security),
        ));
        let row = crate::runtime::registry::snapshot(work_id).expect("row");
        assert!(
            !row.name.contains(SENDER),
            "the relay row must not carry the plaintext recipient: {}",
            row.name
        );
        assert!(
            row.name.contains(CHANNEL),
            "the channel stays readable so the row can say where the reply is going: {}",
            row.name
        );
        assert!(
            row.name
                .contains(&crate::security::op_id::ref_for_channel_recipient(CHANNEL, SENDER)),
            "the recipient must appear as the shared op_id fingerprint: {}",
            row.name
        );
        crate::runtime::registry::kill(work_id, true).await;
        chat_sessions::deregister(&session_id);
    }

    #[tokio::test]
    async fn an_operator_plane_result_is_never_relayed_to_a_channel() {
        let security = policy(&[], &[]);
        let (channel, sent) = RecordingChannel::new();
        let trip = trip(
            "assignment-from-the-operator",
            AssignPrincipal::operator_plane(),
            Arc::clone(&channel),
            security,
        );
        deliver(&trip, &AssignPrincipal::operator_plane(), "anything", RELAY_RESULT).await;
        assert!(
            sent.lock().is_empty(),
            "the operator plane is not a correspondent and has no conversation to answer in"
        );
    }

    #[tokio::test]
    async fn an_outstanding_assignment_keeps_the_trip_waiting_until_its_result_lands() {
        let security = policy(&[], &[]);
        let (session_id, token, assignment_id) = assign_one(&security, &correspondent());

        let PollVerdict::Waiting { cursor } = poll_once(&assignment_id, 0) else {
            panic!("an outstanding assignment is neither found nor lost");
        };

        chat_sessions::report(
            &session_id,
            &token,
            &assignment_id,
            ResultStatus::Failed,
            "ran out of disk",
        )
        .expect("reported");

        match poll_once(&assignment_id, cursor) {
            PollVerdict::Found(result) => {
                assert_eq!(result.status, ResultStatus::Failed);
                assert_eq!(result.summary, "ran out of disk");
            }
            other => panic!("the cursor must still see the result that arrives after it: {other:?}"),
        }
        chat_sessions::deregister(&session_id);
    }

    /// The last hop of a chat assignment has to be legible from the server side
    /// on its own. Before this line the only evidence a relay had succeeded was
    /// the *absence* of a warning, which is not evidence at all — it looks
    /// identical to a relay that never ran.
    ///
    /// The line is held to the same two rules as the refusals beside it: the
    /// destination appears only as its `op_id` fingerprint, and the message body
    /// does not appear at all. `bytes` is the whole of what is said about the
    /// body, and it is asserted against what the channel actually carried so the
    /// number cannot quietly become a placeholder.
    ///
    /// MUTATION GUARD: drop the success `info!` in `deliver`, or log
    /// `trip.recipient` / `text` in place of the fingerprint and the length, and
    /// this test goes red.
    #[tokio::test]
    async fn a_relayed_result_is_logged_without_the_recipient_or_the_body() {
        const BODY: &str = "canary-summary-9f31c2: rebuilt the index";
        let security = policy(&[], &[]);
        let (channel, sent) = RecordingChannel::new();
        let (session_id, token, assignment_id) = assign_one(&security, &correspondent());

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .finish();
        // Thread-local: a `#[tokio::test]` drives its tasks on this very thread,
        // so the spawned trip logs into this writer and no other test's output
        // can land in it.
        let guard = tracing::subscriber::set_default(subscriber);

        let ending = tokio::spawn({
            let trip = trip(
                &assignment_id,
                correspondent(),
                Arc::clone(&channel),
                Arc::clone(&security),
            );
            let cancel = CancellationToken::new();
            async move { run_return_trip(trip, cancel).await }
        });
        chat_sessions::report(&session_id, &token, &assignment_id, ResultStatus::Completed, BODY)
            .expect("result accepted");
        assert_eq!(ending.await.expect("trip joined"), TripEnding::Delivered);
        drop(guard);

        let text = logs.text();
        let relayed = sent.lock().first().map(|message| message.content.clone());
        let relayed = relayed.expect("test: the relay must have sent something to log about");

        assert!(text.contains("Relayed chat assignment result"), "test: {text}");
        assert!(
            text.contains(&format!("assignment={assignment_id}")),
            "test: the line must name the assignment it closes: {text}"
        );
        assert!(
            text.contains(&format!("channel={CHANNEL}")),
            "test: the channel stays readable — it is what makes the hop traceable: {text}"
        );
        assert!(
            text.contains(&format!(
                "recipient={}",
                crate::security::op_id::ref_for_channel_recipient(CHANNEL, SENDER)
            )),
            "test: the destination must appear as the shared op_id fingerprint: {text}"
        );
        assert!(
            text.contains("session=session-ref"),
            "test: the session reference ties this line to the assign line: {text}"
        );
        assert!(
            text.contains("relay=\"result\""),
            "test: an answer and an eviction notice must not read alike: {text}"
        );
        assert!(
            text.contains(&format!("bytes={}", relayed.len())),
            "test: `bytes` must describe what was actually sent: {text}"
        );
        assert!(
            !text.contains(SENDER),
            "test: the plaintext recipient must never reach the log: {text}"
        );
        assert!(
            !text.contains(BODY) && !text.contains("rebuilt the index"),
            "test: the relayed body must never reach the log: {text}"
        );
        chat_sessions::deregister(&session_id);
    }

    /// The hint is omitted, not emptied, when the caller may assign nowhere.
    ///
    /// "No session with that id" and "you may assign to none of them" have to
    /// read identically, or the enriched refusal still confirms that sessions
    /// exist — precisely what [`chat_sessions::assign`] authorizes before
    /// reporting unknown in order to avoid. Asserted against a default-deny
    /// policy rather than a crafted denylist so that sessions registered by other
    /// tests in this process cannot make it pass or fail by accident.
    ///
    /// MUTATION GUARD: return `Some` for an empty listing — `Some(String::new())`
    /// or an unconditional `Some(assignable.join(", "))` — and this test goes red.
    #[tokio::test]
    async fn an_unassignable_principal_gets_no_session_hint_at_all() {
        let denied = Arc::new(SecurityPolicy::default());
        let registration = chat_sessions::register("workstation", None).expect("registration");

        let hint = assignable_sessions_hint(&denied, &correspondent());

        assert_eq!(
            hint, None,
            "a principal that may assign nowhere must not learn that any session exists"
        );
        chat_sessions::deregister(&registration.session_id);
    }

    /// The other half of the same rule: a principal that may assign somewhere is
    /// told where, by an id it can actually use, so an unknown-session refusal
    /// does not cost a round trip.
    ///
    /// MUTATION GUARD: drop either half of the rendered entry — the `session_id`
    /// a caller has to pass back, or the `session_ref` that ties it to
    /// `prx tasks list` — and this test goes red. That the hint is *reached* at
    /// all is guarded separately, by
    /// `sessions_spawn::tests::an_unknown_session_id_is_answered_with_the_sessions_this_caller_may_assign_to`.
    #[tokio::test]
    async fn an_assignable_principal_is_given_the_ids_it_may_use() {
        let security = policy(&[], &[]);
        let registration = chat_sessions::register("workstation", None).expect("registration");

        let hint = assignable_sessions_hint(&security, &correspondent()).unwrap_or_default();

        assert!(
            hint.contains(&registration.session_id),
            "the hint must carry the id the caller would pass back: {hint}"
        );
        assert!(
            hint.contains(&registration.session_ref),
            "and the reference that ties it to the work-registry rows: {hint}"
        );
        chat_sessions::deregister(&registration.session_id);
    }

    /// The eviction inference, asserted on its own rather than by overflowing
    /// the shared feed — 257 results pushed through a global buffer would evict
    /// entries other tests in this binary are waiting on.
    #[test]
    fn an_absent_result_is_only_called_lost_when_both_conditions_hold() {
        assert!(
            result_is_unrecoverable(true, false),
            "entries were dropped and the assignment is over: its result is gone"
        );
        assert!(
            !result_is_unrecoverable(true, true),
            "a live row means the assignment simply has not finished"
        );
        assert!(
            !result_is_unrecoverable(false, false),
            "a retired row with nothing dropped is the ordinary poll before the result is read"
        );
        assert!(!result_is_unrecoverable(false, false));
    }
}
