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
    assert!(pulled.assignments.is_empty(), "expected an empty value");
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
            .is_empty(),
        "expected an empty value"
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
    assert!(third.assignments.is_empty(), "expected an empty value");

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
        fn record_poll(&self, _session_id: &str, _polls: u64, _at_unix_ms: u64, _revision: u64) -> anyhow::Result<()> {
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
        fn record_poll(&self, _session_id: &str, _polls: u64, _at_unix_ms: u64, _revision: u64) -> anyhow::Result<()> {
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
    let store: Arc<dyn MailboxStore> = Arc::new(SqliteMailboxStore::open(&db).expect("test: open the mailbox store"));
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
        assert_eq!(log.insert(entry(seq)), Admission::Added);
    }
    assert_eq!(log.entries.len(), RESULT_LOG_CAPACITY);
    assert_eq!(log.oldest_seq, 3, "the two oldest entries were trimmed");

    assert_eq!(log.insert(entry(overflow)), Admission::Duplicate);
    assert_eq!(
        log.entries.len(),
        RESULT_LOG_CAPACITY,
        "a duplicate sequence is merged, not appended"
    );

    assert_eq!(
        log.insert(entry(1)),
        Admission::BelowFloor,
        "a refusal below the floor is reported, so a live one can be logged"
    );
    assert_eq!(
        log.oldest_seq, 3,
        "a sequence the feed already trimmed must not come back below its floor"
    );
    assert_eq!(log.entries.front().map(|first| first.seq), Some(3));

    assert_eq!(log.insert(entry(overflow + 5)), Admission::Added);
    assert!(
        log.entries.iter().map(|held| held.seq).is_sorted(),
        "the feed stays ordered by sequence"
    );
    assert!(
        log.next_seq > overflow + 5,
        "minting must never reuse a sequence it replayed"
    );
}

/// Every result is on the feed before a later sequence can be minted.
///
/// Two reports finishing at once used to publish in whatever order their
/// store writes returned. When a later sequence landed first, the feed's
/// floor jumped past the earlier one, the earlier result was refused as
/// "already trimmed" although its report had succeeded, and its assignment
/// row retired anyway: the `chat_assignment` return trip for it ended
/// `ResultEvicted` instead of `Delivered` and the correspondent was told
/// the answer was lost. A cursor that had read the later entry skipped the
/// earlier one for good.
///
/// The store write is where the interleaving happened, so this store
/// records, from inside each write that carries a result, whether another
/// thread could mint a sequence at that moment. It covers all three
/// places that mint: a report, an operator kill, and a deregistration.
///
/// What this test pins at run time is that the turn is held *during* the
/// write. That publishing happens before the turn ends is pinned at
/// compile time: a [`Minted`] borrows the [`PublicationTurn`] that minted
/// it and only [`PublicationTurn::publish`] consumes it, so carrying a
/// result out of its turn and publishing it afterwards does not build.
///
/// MUTATION GUARD: write outside the turn on any of the three paths and
/// this test goes red; publish after the turn is dropped and the crate
/// does not compile (E0505).
#[tokio::test]
async fn a_result_is_on_the_feed_before_a_later_sequence_can_be_minted() {
    use std::sync::{OnceLock, Weak};

    #[derive(Default)]
    struct ObservingStore {
        sessions: OnceLock<Weak<ChatSessions>>,
        minting_open: Mutex<Vec<(&'static str, bool)>>,
    }

    impl ObservingStore {
        fn observe(&self, path: &'static str) {
            let open = self
                .sessions
                .get()
                .and_then(Weak::upgrade)
                .is_some_and(|sessions| sessions.publication.try_lock().is_some());
            self.minting_open.lock().push((path, open));
        }
    }

    impl MailboxStore for ObservingStore {
        fn hydrate(&self) -> anyhow::Result<StoredState> {
            Ok(StoredState::default())
        }
        fn insert_session(&self, _session: &StoredSession) -> anyhow::Result<()> {
            Ok(())
        }
        fn record_poll(&self, _: &str, _: u64, _: u64, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn enqueue(&self, _assignment: &StoredAssignment, _at_front: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn move_to_queue(&self, _: &str, _: Queue, _: u32, _: bool, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn complete_assignment(&self, _assignment_id: &str, _result: &StoredResult) -> anyhow::Result<()> {
            self.observe("complete_assignment");
            Ok(())
        }
        fn cascade_session(&self, _session_id: &str, results: &[StoredResult]) -> anyhow::Result<()> {
            if !results.is_empty() {
                self.observe("cascade_session");
            }
            Ok(())
        }
    }

    let store = Arc::new(ObservingStore::default());
    let sessions = Arc::new(ChatSessions::new());
    store
        .sessions
        .set(Arc::downgrade(&sessions))
        .expect("test: the store is observed once");
    sessions.attach_store(Arc::clone(&store) as Arc<dyn MailboxStore>);

    let registration = sessions.register(LABEL, None).expect("test: registration");
    let assign = |task: &str| {
        sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                task,
                Disposition::Queue,
            )
            .expect("test: assignment accepted")
    };
    let reported = assign("report on this one");
    let killed = assign("this one is killed");
    let _cascaded = assign("this one goes with the session");
    sessions
        .pull(&registration.session_id, &registration.token, &[], Some(3))
        .expect("test: delivery");

    sessions
        .report(
            &registration.session_id,
            &registration.token,
            &reported.assignment_id,
            ResultStatus::Completed,
            "done",
        )
        .expect("test: report accepted");
    let session = sessions
        .find(&registration.session_id)
        .expect("test: the session is registered");
    sessions.discard(&session, &killed.assignment_id);
    drop(session);
    sessions.deregister(&registration.session_id);

    let observed = store.minting_open.lock().clone();
    assert_eq!(
        observed,
        vec![
            ("complete_assignment", false),
            ("complete_assignment", false),
            ("cascade_session", false),
        ],
        "while a result is being written, no later sequence may be minted (true = another thread could have minted)"
    );
    assert_eq!(
        sessions
            .results_after(0, RESULT_LOG_CAPACITY)
            .results
            .iter()
            .map(|result| result.seq)
            .collect::<Vec<_>>(),
        vec![1, 2, 3],
        "all three results are on the feed, in sequence order"
    );
}

/// A store write that sits out its whole contention budget, the way one
/// does while a foreign process holds the mailbox file lock.
struct StuckStore;

impl MailboxStore for StuckStore {
    fn hydrate(&self) -> anyhow::Result<StoredState> {
        Ok(StoredState::default())
    }
    fn insert_session(&self, _session: &StoredSession) -> anyhow::Result<()> {
        Ok(())
    }
    fn record_poll(&self, _: &str, _: u64, _: u64, _: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn enqueue(&self, _assignment: &StoredAssignment, _at_front: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn move_to_queue(&self, _: &str, _: Queue, _: u32, _: bool, _: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn complete_assignment(&self, _assignment_id: &str, _result: &StoredResult) -> anyhow::Result<()> {
        std::thread::sleep(crate::runtime::sqlite_pool::bounded_busy_remaining());
        anyhow::bail!("database is locked")
    }
    fn cascade_session(&self, _session_id: &str, _results: &[StoredResult]) -> anyhow::Result<()> {
        std::thread::sleep(crate::runtime::sqlite_pool::bounded_busy_remaining());
        anyhow::bail!("database is locked")
    }
}

/// Reports queued behind a stuck database give up together, within about
/// one contention budget, instead of one budget each.
///
/// The publication lock serializes result writers. With the budget opened
/// only inside the store write, each queued report started a fresh
/// [`crate::runtime::sqlite_pool::BOUNDED_BUSY_WAIT`] once it got the
/// lock, so the K-th report answered after K budgets. The budget is now
/// opened before the lock is taken and bounds the wait for it too: the
/// first report spends it on the database, the rest lose durability for
/// their result, as the store's own lock timeout would make them, and
/// still publish in order.
///
/// Four reports: about four budgets before, about one after. The bound
/// asserted is two, so only the serial pile-up can cross it.
///
/// MUTATION GUARD: open the budget after taking `publication`, or wait
/// for `publication` without a bound, and this test goes red.
#[test]
fn reports_queued_behind_a_stuck_database_give_up_within_one_budget() {
    use crate::runtime::sqlite_pool::BOUNDED_BUSY_WAIT;
    const REPORTS: usize = 4;

    let sessions = Arc::new(ChatSessions::new());
    sessions.attach_store(Arc::new(StuckStore));
    let registration = sessions.register(LABEL, None).expect("test: registration");
    let receipts = (0..REPORTS)
        .map(|index| {
            sessions
                .assign(
                    &operator_policy(),
                    &AssignPrincipal::operator_plane(),
                    &registration.session_id,
                    &format!("task {index}"),
                    Disposition::Queue,
                )
                .expect("test: assignment accepted")
        })
        .collect::<Vec<_>>();
    sessions
        .pull(&registration.session_id, &registration.token, &[], Some(REPORTS))
        .expect("test: delivery");

    let started = Instant::now();
    std::thread::scope(|scope| {
        for receipt in &receipts {
            let sessions = &sessions;
            let registration = &registration;
            scope.spawn(move || {
                sessions
                    .report(
                        &registration.session_id,
                        &registration.token,
                        &receipt.assignment_id,
                        ResultStatus::Completed,
                        "done",
                    )
                    .expect("test: a report is accepted even when it cannot be made durable")
            });
        }
    });
    let elapsed = started.elapsed();

    assert!(
        elapsed < BOUNDED_BUSY_WAIT * 2,
        "{REPORTS} reports behind a stuck database took {elapsed:?}; the budget is {BOUNDED_BUSY_WAIT:?}"
    );
    assert_eq!(
        sessions
            .results_after(0, RESULT_LOG_CAPACITY)
            .results
            .iter()
            .map(|result| result.seq)
            .collect::<Vec<_>>(),
        (1..=u64::try_from(REPORTS).unwrap_or(u64::MAX)).collect::<Vec<_>>(),
        "every result still reaches the feed, in sequence order"
    );
}

/// Queue `reports` behind a publication turn that this thread holds past
/// every report's contention budget, then let them go.
///
/// Each report signals just before it asks for the turn; the holder waits
/// for every signal and then out-waits the whole budget, so every report
/// deterministically takes the turn with its budget already spent — the
/// path `PublicationTurn::take` calls not durable. Returns each report's
/// answer together with the `tracing` output of the thread that made it.
fn report_behind_an_expired_turn(
    sessions: &ChatSessions,
    registration: &Registration,
    assignment_ids: &[String],
) -> Vec<(Result<u64, ChatSessionError>, String)> {
    use crate::runtime::sqlite_pool::log_capture::capturing_logs;

    let turn = sessions.publication.lock();
    let (ready, queued) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let mut reporters = Vec::with_capacity(assignment_ids.len());
        for assignment_id in assignment_ids {
            let ready = ready.clone();
            reporters.push(scope.spawn(move || {
                capturing_logs(|| {
                    ready.send(()).expect("test: the holder is listening");
                    sessions.report(
                        &registration.session_id,
                        &registration.token,
                        assignment_id,
                        ResultStatus::Completed,
                        "done",
                    )
                })
            }));
        }
        for _ in assignment_ids {
            queued.recv().expect("test: every reporter starts");
        }
        std::thread::sleep(BOUNDED_BUSY_WAIT + Duration::from_millis(500));
        drop(turn);
        reporters
            .into_iter()
            .map(|reporter| reporter.join().expect("test: a reporter thread finishes"))
            .collect()
    })
}

/// R-FLAKEf R1: reports that wait the whole budget out for the turn still
/// ask the store, so each one the database refuses is counted.
///
/// The pool report's `writes_failed` and `durable` are the operator's only
/// view of results a restart will not find. A turn that skipped the store
/// after its budget ran out left both untouched: K results lost behind a
/// foreign lock read as none, and `durable` stayed true.
///
/// MUTATION GUARD: skip the store in `PublicationTurn::write` when the turn
/// is not durable and `writes_failed` stays at zero here.
#[test]
fn reports_that_spent_their_budget_on_the_turn_are_counted_as_failed_writes() {
    use crate::runtime::registry::PoolStatsSource;
    const REPORTS: usize = 3;

    let dir = tempfile::tempdir().expect("test: tempdir");
    let db = dir.path().join("state").join("chat-mailbox.db");
    let store = Arc::new(SqliteMailboxStore::open(&db).expect("test: open the mailbox store"));
    let sessions = Arc::new(ChatSessions::new());
    sessions.attach_store(Arc::clone(&store) as Arc<dyn MailboxStore>);
    let registration = sessions.register(LABEL, None).expect("test: registration");
    let assignment_ids = (0..REPORTS)
        .map(|index| {
            sessions
                .assign(
                    &operator_policy(),
                    &AssignPrincipal::operator_plane(),
                    &registration.session_id,
                    &format!("task {index}"),
                    Disposition::Queue,
                )
                .expect("test: assignment accepted")
                .assignment_id
        })
        .collect::<Vec<_>>();
    sessions
        .pull(&registration.session_id, &registration.token, &[], Some(REPORTS))
        .expect("test: delivery");
    let failed = |metrics: &serde_json::Value| metrics.get("writes_failed").and_then(serde_json::Value::as_u64);
    assert_eq!(failed(&store.pool_metrics()), Some(0), "setup writes all landed");

    let foreign = rusqlite::Connection::open(&db).expect("test: open the mailbox file");
    foreign
        .execute_batch("BEGIN IMMEDIATE")
        .expect("test: hold the write lock as a second process would");
    let answers = report_behind_an_expired_turn(&sessions, &registration, &assignment_ids);
    foreign.execute_batch("ROLLBACK").expect("test: release the write lock");

    for (answer, _) in &answers {
        assert!(
            answer.is_ok(),
            "a report is accepted even when it cannot be made durable: {answer:?}"
        );
    }
    let metrics = store.pool_metrics();
    assert_eq!(
        failed(&metrics),
        Some(u64::try_from(REPORTS).unwrap_or(u64::MAX)),
        "every result the locked database refused is a counted failure: {metrics}"
    );
    assert_eq!(
        metrics.get("durable").and_then(serde_json::Value::as_bool),
        Some(false),
        "results a restart will not find must not read as durable: {metrics}"
    );
}

/// R-FLAKEf R1 and R4: a report whose budget ran out on the turn still
/// lands once the database is free, and only a report that owes the store
/// a write warns that it may not be durable.
///
/// Skipping the store threw away a write that would have succeeded, and the
/// warning fired in `take`, before the caller knew it had anything to
/// write — so a replayed report, which writes nothing, warned too.
///
/// MUTATION GUARD: skip the store when the turn is not durable and the
/// fresh result is missing from the file; warn in `take` again and the
/// replay's log carries the warning.
#[test]
fn a_report_that_spent_its_budget_on_the_turn_lands_once_the_store_is_free() {
    let dir = tempfile::tempdir().expect("test: tempdir");
    let db = dir.path().join("state").join("chat-mailbox.db");
    let sessions = mailbox_at(&db);
    let registration = sessions.register(LABEL, None).expect("test: registration");
    let assignment_ids = ["already reported", "fresh"].map(|task| {
        sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                task,
                Disposition::Queue,
            )
            .expect("test: assignment accepted")
            .assignment_id
    });
    sessions
        .pull(&registration.session_id, &registration.token, &[], Some(2))
        .expect("test: delivery");
    let [reported_id, _] = &assignment_ids;
    let replayed_seq = sessions
        .report(
            &registration.session_id,
            &registration.token,
            reported_id,
            ResultStatus::Completed,
            "done",
        )
        .expect("test: first report");

    let [(replay, replay_logs), (fresh, fresh_logs)]: [_; 2] =
        report_behind_an_expired_turn(&sessions, &registration, &assignment_ids)
            .try_into()
            .expect("test: one answer per report");
    assert_eq!(
        replay.as_ref().ok(),
        Some(&replayed_seq),
        "a replay is answered with the original sequence"
    );
    assert!(
        !replay_logs.contains("durable"),
        "a replay owes the store nothing and must not warn about durability: {replay_logs}"
    );
    assert!(fresh.is_ok(), "the fresh report is accepted: {fresh:?}");
    assert!(
        fresh_logs.contains("may not be durable"),
        "a write made with no budget left says so: {fresh_logs}"
    );

    let disk = SqliteMailboxStore::open(&db)
        .expect("test: reopen the mailbox file")
        .hydrate()
        .expect("test: read the mailbox file back");
    let mut persisted = disk
        .results
        .iter()
        .map(|result| result.assignment_id.clone())
        .collect::<Vec<_>>();
    persisted.sort();
    let mut expected = assignment_ids;
    expected.sort();
    assert_eq!(
        persisted, expected,
        "the late write reached the free database instead of being skipped"
    );
}

/// An operator kill that has to wait for the publication lock waits on the
/// blocking pool, never on an async worker.
///
/// The kill watcher is an async task. Recording the cancellation takes the
/// publication lock and writes to the store, both blocking; run inline, a
/// kill queued behind a slow result write parked the worker thread, and on
/// a current-thread runtime nothing else could run until it was let go.
///
/// The holder lets go after three seconds at the latest, so the unfixed
/// code fails instead of hanging; a 50 ms sleep on the same runtime has to
/// finish in under one.
///
/// MUTATION GUARD: call `discard` directly from the watcher task and this
/// test goes red.
#[tokio::test]
async fn a_kill_waiting_on_publication_does_not_hold_an_async_worker() {
    use std::sync::mpsc::sync_channel;

    let sessions = Arc::new(ChatSessions::new());
    let registration = sessions.register(LABEL, None).expect("test: registration");
    let receipt = sessions
        .assign(
            &operator_policy(),
            &AssignPrincipal::operator_plane(),
            &registration.session_id,
            "kill me while a result is being written",
            Disposition::Queue,
        )
        .expect("test: assignment accepted");

    let (held_tx, held_rx) = sync_channel::<()>(1);
    let (release_tx, release_rx) = sync_channel::<()>(1);
    let holder = {
        let sessions = Arc::clone(&sessions);
        std::thread::spawn(move || {
            let _held = sessions.publication.lock();
            let _ = held_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
        })
    };
    held_rx.recv().expect("test: the holder takes the lock");

    let kill = tokio::spawn(crate::runtime::registry::kill(receipt.work_id, true));
    let started = Instant::now();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let slept = started.elapsed();
    let _ = release_tx.send(());
    holder.join().expect("test: the holder thread");
    let outcomes = kill.await.expect("test: the kill task");

    assert!(
        slept < Duration::from_secs(1),
        "a 50 ms sleep took {slept:?}: the kill parked the runtime's worker"
    );
    assert_eq!(
        outcomes.first().map(|result| result.outcome),
        Some(KillOutcome::Killed),
        "the kill still completes once the lock is free"
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

/// F-01 (R03): a report of work an operator killed is **not** a replay of
/// this session's own report.
///
/// The kill records a `Cancelled` result under the same
/// `(assignment_id, session_id)` pair the replay path matches on, so the
/// late report used to be answered `Ok(seq)` — the operator's kill handed
/// back as the session's own receipt, with the session's real status and
/// summary dropped on the floor. The session is now told what actually
/// happened, and the feed still carries exactly one record for the
/// assignment.
///
/// The second half of the test keeps R03's guarantee honest: a genuine
/// replay of a report the session did make still answers with the original
/// sequence and mints nothing new.
///
/// MUTATION GUARD: answer the `Cancelled` arm of `report`'s
/// `recorded_result` match with `Ok(seq)` and the refusal assertions go
/// red; refuse every recorded result instead and the replay assertion does.
#[tokio::test]
async fn a_report_of_killed_work_is_refused_instead_of_replaying_the_cancellation() {
    let sessions = Arc::new(ChatSessions::new());
    let registration = sessions.register(LABEL, None).expect("test: registration");
    let assign = |task: &str| {
        sessions
            .assign(
                &operator_policy(),
                &AssignPrincipal::operator_plane(),
                &registration.session_id,
                task,
                Disposition::Queue,
            )
            .expect("test: assignment accepted")
    };
    let killed = assign("inspect fixture");
    let reported = assign("inspect the other fixture");

    // Both are delivered and acknowledged: the session holds them and owes
    // a report on each.
    sessions
        .pull(&registration.session_id, &registration.token, &[], Some(2))
        .expect("test: delivery");
    let acked = sessions
        .pull(
            &registration.session_id,
            &registration.token,
            &[killed.assignment_id.clone(), reported.assignment_id.clone()],
            Some(2),
        )
        .expect("test: acknowledgement");
    assert_eq!(acked.acked, 2);

    // An operator kills the first one out from under the session.
    let session = sessions
        .find(&registration.session_id)
        .expect("test: the session is registered");
    sessions.discard(&session, &killed.assignment_id);

    let error = sessions
        .report(
            &registration.session_id,
            &registration.token,
            &killed.assignment_id,
            ResultStatus::Completed,
            "fixture checked",
        )
        .expect_err("a report of work an operator killed must not be answered as a replay");
    let ChatSessionError::AssignmentCancelled { summary } = &error else {
        panic!("test: expected a cancellation refusal, got {error:?}");
    };
    assert!(
        summary.contains("killed by an operator"),
        "the refusal must carry why the work ended: {summary}"
    );
    assert_eq!(error.code(), "assignment_cancelled");

    // The genuine replay path is untouched: a report the session really
    // made answers with its original sequence, however often it is retried.
    let first = sessions
        .report(
            &registration.session_id,
            &registration.token,
            &reported.assignment_id,
            ResultStatus::Completed,
            "the other fixture is fine",
        )
        .expect("test: the session's own report");
    let replay = sessions
        .report(
            &registration.session_id,
            &registration.token,
            &reported.assignment_id,
            ResultStatus::Completed,
            "the other fixture is fine",
        )
        .expect("test: a replay of the session's own report");
    assert_eq!(replay, first, "a genuine replay answers with the original sequence");

    // One record per assignment: the refusal minted no second completion,
    // and the replay minted no duplicate.
    let feed = sessions.results_after(0, RESULT_LOG_CAPACITY).results;
    let killed_records = feed
        .iter()
        .filter(|result| result.assignment_id == killed.assignment_id)
        .map(|result| (result.status, result.summary.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        killed_records.len(),
        1,
        "the refused report must not add a second record: {killed_records:?}"
    );
    assert_eq!(
        killed_records.first().map(|(status, _)| *status),
        Some(ResultStatus::Cancelled),
        "the record that stands is the operator's cancellation"
    );
    assert_eq!(
        feed.iter()
            .filter(|result| result.assignment_id == reported.assignment_id)
            .count(),
        1,
        "a replayed report must not duplicate the record either"
    );
}

/// F-05 (R01): a pull counts itself and reads the clock **inside** the lock
/// that mints the revision ordering its write.
///
/// Sampled before the lock, two overlapping pulls can pair the lower poll
/// count with the higher revision — the pull that incremented first takes
/// the lock second — and the store's revision guard then keeps the lower
/// count, rolling the persisted liveness backwards.
///
/// Two halves, because a race needs both a deterministic reproduction and
/// the invariant it violates. The first holds the state lock and proves the
/// counter cannot move without it. The second runs overlapping pulls and
/// pins the invariant itself: ordered by revision, the poll counts are
/// exactly `1..=n` and the clock never runs backwards.
///
/// MUTATION GUARD: move the `polls.fetch_add` and the `unix_ms()` sample
/// back above `session.state.lock()` and the first half goes red at once.
#[tokio::test]
async fn a_pull_counts_itself_under_the_lock_that_orders_its_write() {
    /// Records what each pull asked the store to persist about liveness.
    struct RecordingStore {
        polls: Mutex<Vec<(u64, u64, u64)>>,
    }

    impl MailboxStore for RecordingStore {
        fn hydrate(&self) -> anyhow::Result<StoredState> {
            Ok(StoredState::default())
        }
        fn insert_session(&self, _session: &StoredSession) -> anyhow::Result<()> {
            Ok(())
        }
        fn record_poll(&self, _session_id: &str, polls: u64, at_unix_ms: u64, revision: u64) -> anyhow::Result<()> {
            self.polls.lock().push((revision, polls, at_unix_ms));
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

    let store = Arc::new(RecordingStore {
        polls: Mutex::new(Vec::new()),
    });
    let sessions = Arc::new(ChatSessions::new());
    sessions.attach_store(Arc::clone(&store) as Arc<dyn MailboxStore>);
    let registration = sessions.register(LABEL, None).expect("test: registration");
    let session = sessions
        .find(&registration.session_id)
        .expect("test: the session is registered");

    // Half one, deterministic: while the state lock is held here, no pull
    // may count itself, because the count belongs to the same critical
    // section as the revision that orders it.
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let blocked = {
        let guard = session.state.lock();
        let blocked = {
            let sessions = Arc::clone(&sessions);
            let started = Arc::clone(&started);
            let session_id = registration.session_id.clone();
            let token = registration.token.clone();
            std::thread::spawn(move || {
                started.store(true, Ordering::SeqCst);
                sessions.pull(&session_id, &token, &[], Some(1))
            })
        };
        while !started.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        // Long enough that a pull sampling its counter before the lock has
        // finished doing so many times over.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            session.polls.load(Ordering::Relaxed),
            0,
            "a pull may not count itself before it holds the lock that mints its revision"
        );
        drop(guard);
        blocked
    };
    blocked
        .join()
        .expect("test: the blocked pulling thread")
        .expect("test: the blocked pull still answers");

    // Half two: the invariant itself, under real overlap.
    let threads = (0..4)
        .map(|_| {
            let sessions = Arc::clone(&sessions);
            let session_id = registration.session_id.clone();
            let token = registration.token.clone();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    sessions
                        .pull(&session_id, &token, &[], Some(1))
                        .expect("test: concurrent pull");
                }
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().expect("test: a concurrent pulling thread");
    }

    let mut recorded = store.polls.lock().clone();
    recorded.sort_by_key(|(revision, _, _)| *revision);
    assert_eq!(recorded.len(), 201, "every pull records its liveness exactly once");
    let mut previous_at_unix_ms = 0_u64;
    for (index, (revision, polls, at_unix_ms)) in recorded.iter().enumerate() {
        let expected = index as u64 + 1;
        assert_eq!(
            *polls, expected,
            "revision {revision} carries poll count {polls} where revision order requires {expected}: \
                 a higher revision must never carry a lower count"
        );
        assert!(
            *at_unix_ms >= previous_at_unix_ms,
            "revision {revision} carries a clock reading older than the revision before it"
        );
        previous_at_unix_ms = *at_unix_ms;
    }
}
/// Mutation: log the raw chain in write_through_failed.
#[test]
fn mailbox_failure_logs_hide_nested_credentials() {
    let error = anyhow::anyhow!("token=opaque7credential").context("failed");
    let (_, logs) = crate::runtime::sqlite_pool::log_capture::capturing_logs(|| {
        write_through_failed("report", &error);
    });
    assert!(logs.contains("failed: token=[REDACTED]"), "{logs}");
    assert!(!logs.contains("opaque7credential"), "{logs}");
}
