#![cfg(test)]

use super::*;

// ── chat-session assignment (`chat_sessions` / `chat_assign`) ────────────

/// A tool instance whose outbound registry knows `wacli`, under a policy the
/// caller supplies.
fn assignment_tool(security: Arc<SecurityPolicy>) -> (SessionsSpawnTool, Arc<Mutex<Vec<String>>>) {
    let (wacli, sent) = NamedRecordingChannel::new("wacli");
    let tool = make_tool_with_channels_and_security(
        wacli.clone(),
        Arc::new(EchoProvider { response: "ok".into() }),
        vec![wacli],
        security,
    );
    (tool, sent)
}

/// A policy that makes `wacli:alice` — the identity [`with_scope`] injects —
/// an owner of this daemon.
fn owner_security() -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy {
        assign_owners: vec!["wacli:alice".to_string()],
        ..SecurityPolicy::default()
    })
}

fn chat_assign_args(session_id: &str, disposition: Option<&str>) -> serde_json::Value {
    let mut args = json!({
        "action": "chat_assign",
        "session_id": session_id,
        "task": "summarise the TODOs",
    });
    if let (Some(object), Some(disposition)) = (args.as_object_mut(), disposition) {
        object.insert("disposition".to_string(), json!(disposition));
    }
    with_scope(args, "wacli", "group-42")
}

/// No trusted scope, no assignment. The refusal is the whole point: an
/// untrusted call must not degrade into an anonymous principal (which a
/// later rule could match) or into the operator plane (which is authorized
/// by construction).
///
/// MUTATION GUARD: make `assignment_principal` fall back to
/// `AssignPrincipal::operator_plane()` and this test goes red.
#[tokio::test]
async fn chat_assign_is_refused_when_the_call_carries_no_trusted_scope() {
    let registration = crate::runtime::chat_sessions::register("workstation", None).expect("registered");
    let (tool, _sent) = assignment_tool(owner_security());

    // Same arguments, minus the runtime-injected scope block — exactly what
    // a model that tried to assign on its own behalf would produce.
    let result = tool
        .execute(json!({
            "action": "chat_assign",
            "session_id": registration.session_id,
            "task": "summarise the TODOs",
        }))
        .await
        .expect("the tool answers rather than erroring out");
    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("no trusted caller scope"), "{error}");
    assert!(
        !error.contains(&registration.session_id),
        "the refusal must not confirm which sessions exist: {error}"
    );
    // And nothing reached the mailbox.
    let listed = crate::runtime::chat_sessions::list();
    let session = listed
        .iter()
        .find(|session| session.session_id == registration.session_id)
        .expect("session is registered");
    assert_eq!(session.queued, 0, "an unauthenticated call must queue nothing");
    crate::runtime::chat_sessions::deregister(&registration.session_id);
}

/// Assignment is default-deny, so a correspondent with no grant is refused —
/// and the refusal names neither the sender nor the session in the clear.
#[tokio::test]
async fn an_ungranted_correspondent_is_refused_and_the_refusal_names_nobody() {
    let registration = crate::runtime::chat_sessions::register("workstation", None).expect("registered");
    let (tool, _sent) = assignment_tool(test_security());

    let result = tool
        .execute(chat_assign_args(&registration.session_id, None))
        .await
        .expect("tool call");
    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(
        !error.contains("alice") && !error.contains(&registration.session_id),
        "the refusal must fingerprint both parties, not echo them: {error}"
    );
    assert!(
        error.contains(&crate::security::op_id::ref_for_channel_recipient(
            "chat",
            &registration.session_id
        )),
        "the refusal must use the same session fingerprint the registry row does: {error}"
    );
    crate::runtime::chat_sessions::deregister(&registration.session_id);
}

/// The owner path, and the three dispositions arriving intact.
///
/// The disposition is asserted from the *mailbox*, not from the tool's own
/// answer: what the tool printed proves nothing about what the chat session
/// will be told.
#[tokio::test]
async fn an_owner_may_assign_and_every_disposition_reaches_the_mailbox() {
    for disposition in ["queue", "steer", "interrupt"] {
        let registration = crate::runtime::chat_sessions::register("workstation", None).expect("registered");
        let (tool, _sent) = assignment_tool(owner_security());

        let result = tool
            .execute(chat_assign_args(&registration.session_id, Some(disposition)))
            .await
            .expect("tool call");
        assert!(result.success, "owner assignment refused: {:?}", result.error);
        let answer: serde_json::Value = serde_json::from_str(&result.output).expect("json answer");
        assert_eq!(answer["disposition"].as_str(), Some(disposition));

        let pulled = crate::runtime::chat_sessions::pull(&registration.session_id, &registration.token, &[], None)
            .expect("pull");
        assert_eq!(pulled.assignments.len(), 1, "one assignment per call");
        assert_eq!(pulled.assignments[0].task, "summarise the TODOs");
        assert_eq!(
            pulled.assignments[0].disposition.as_str(),
            disposition,
            "the disposition the caller chose must reach the session unchanged"
        );
        crate::runtime::chat_sessions::deregister(&registration.session_id);
    }
}

/// An unknown disposition is an error, never a silent downgrade to `queue`:
/// a caller who asked to interrupt and got a queue would have its intent
/// evaporate with nothing said.
#[tokio::test]
async fn an_unknown_disposition_is_rejected_rather_than_folded_into_the_default() {
    let registration = crate::runtime::chat_sessions::register("workstation", None).expect("registered");
    let (tool, _sent) = assignment_tool(owner_security());

    let error = tool
        .execute(chat_assign_args(&registration.session_id, Some("right-now")))
        .await
        .expect_err("an unknown disposition is a tool error");
    let typed = error
        .downcast_ref::<crate::tools::schema::ToolArgumentError>()
        .expect("typed rejection");
    assert!(typed.issues.iter().any(|issue| issue.path == "$.disposition"));

    let listed = crate::runtime::chat_sessions::list();
    let session = listed
        .iter()
        .find(|session| session.session_id == registration.session_id)
        .expect("registered");
    assert_eq!(session.queued, 0, "a rejected disposition must queue nothing");
    let valid = tool
        .execute(chat_assign_args(&registration.session_id, Some("queue")))
        .await
        .expect("valid assignment");
    assert!(valid.success, "{:?}", valid.error);
    let pulled =
        crate::runtime::chat_sessions::pull(&registration.session_id, &registration.token, &[], None).expect("pull");
    assert_eq!(pulled.assignments.len(), 1);
    assert_eq!(pulled.assignments[0].disposition.as_str(), "queue");
    crate::runtime::chat_sessions::deregister(&registration.session_id);
}

/// The listing is the caller's, not the daemon's.
///
/// `chat_sessions::assign` authorizes before it reports a session unknown so
/// that an unauthorized correspondent cannot enumerate sessions through it;
/// an unfiltered listing on the same principal's tool surface would hand
/// back exactly what that refusal withholds.
///
/// MUTATION GUARD: drop the `may_assign` filter in `execute_chat_sessions`
/// and this test goes red.
#[tokio::test]
async fn chat_sessions_lists_only_what_this_caller_may_assign_to() {
    let registration = crate::runtime::chat_sessions::register("workstation", None).expect("registered");

    let (ungranted, _) = assignment_tool(test_security());
    let result = ungranted
        .execute(with_scope(json!({"action": "chat_sessions"}), "wacli", "group-42"))
        .await
        .expect("tool call");
    assert!(result.success);
    let answer: serde_json::Value = serde_json::from_str(&result.output).expect("json");
    assert!(
        !result.output.contains(&registration.session_id),
        "an ungranted caller must not learn that a session exists: {}",
        result.output
    );
    assert_eq!(answer["sessions"].as_array().map(Vec::len), Some(0));

    let (owner, _) = assignment_tool(owner_security());
    let result = owner
        .execute(with_scope(json!({"action": "chat_sessions"}), "wacli", "group-42"))
        .await
        .expect("tool call");
    let answer: serde_json::Value = serde_json::from_str(&result.output).expect("json");
    let listed = answer["sessions"].as_array().cloned().unwrap_or_default();
    assert!(
        listed
            .iter()
            .any(|session| session["session_id"].as_str() == Some(registration.session_id.as_str())),
        "an owner sees the session it may assign to: {}",
        result.output
    );
    crate::runtime::chat_sessions::deregister(&registration.session_id);
}

/// A policy that lets `wacli:alice` assign anywhere *except* to sessions
/// carrying `denied_label`.
///
/// The wildcard allow is what makes an invented `session_id` reach
/// `UnknownSession` at all: a narrower whitelist would refuse it as
/// `NotAuthorized` first, and that is the path that must stay silent.
fn partial_assign_security(denied_label: &str) -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy {
        scope_rules: vec![crate::config::ScopeRule {
            channel: Some("wacli".to_string()),
            assign_allow: vec!["*".to_string()],
            assign_deny: vec![format!("{denied_label}:*")],
            ..crate::config::ScopeRule::default()
        }],
        ..SecurityPolicy::default()
    })
}

/// An invented `session_id` costs a round trip unless the refusal says what
/// the real ones are — and the real-machine run showed a model will invent
/// one (`prx-agent-chat-6914`) rather than ask.
///
/// The listing attached to the refusal is the caller's, not the daemon's: a
/// session this caller may not assign to must not appear, or the refusal
/// becomes a cheaper enumeration oracle than the `chat_sessions` action it
/// borrows its filter from.
///
/// Labels are test-unique because the session registry is process-wide and
/// this binary runs its tests concurrently.
///
/// MUTATION GUARD: drop the `may_assign` filter in
/// `chat_assignment::assignable_sessions_hint` and this test goes red on the
/// off-limits session; drop the hint entirely and it goes red on the granted
/// one.
#[tokio::test]
async fn an_unknown_session_id_is_answered_with_the_sessions_this_caller_may_assign_to() {
    const GRANTED: &str = "c6-granted-workstation";
    const OFF_LIMITS: &str = "c6-off-limits-vault";
    let granted = crate::runtime::chat_sessions::register(GRANTED, None).expect("registered");
    let off_limits = crate::runtime::chat_sessions::register(OFF_LIMITS, None).expect("registered");
    let (tool, _sent) = assignment_tool(partial_assign_security(OFF_LIMITS));

    let result = tool
        .execute(chat_assign_args("prx-agent-chat-6914", None))
        .await
        .expect("tool call");
    assert!(!result.success, "an invented session id is still refused");
    let error = result.error.unwrap_or_default();
    assert!(
        error.contains("no chat session with that id is registered"),
        "the refusal itself is unchanged: {error}"
    );
    assert!(
        error.contains(&granted.session_id) && error.contains(GRANTED),
        "a session this caller may assign to belongs in the refusal: {error}"
    );
    assert!(
        !error.contains(&off_limits.session_id) && !error.contains(OFF_LIMITS),
        "a session this caller may not assign to must not leak through the refusal: {error}"
    );
    crate::runtime::chat_sessions::deregister(&granted.session_id);
    crate::runtime::chat_sessions::deregister(&off_limits.session_id);
}

/// Where the answer goes is the assigning turn's business, never the
/// model's.
///
/// `spawn` accepts an explicit `recipient` (and gates it); `chat_assign`
/// does not accept one at all, because there is no reason for one
/// correspondent to start work whose result lands in someone else's
/// conversation. The relay's registry row is where that decision is
/// observable: it fingerprints the destination it will actually use.
///
/// MUTATION GUARD: read the recipient from `args` in `execute_chat_assign`
/// and this test goes red.
#[tokio::test]
async fn the_model_cannot_redirect_a_chat_assignment_result_to_a_recipient_of_its_own() {
    let registration = crate::runtime::chat_sessions::register("workstation", None).expect("registered");
    let (tool, _sent) = assignment_tool(owner_security());

    let mut args = chat_assign_args(&registration.session_id, None);
    if let Some(object) = args.as_object_mut() {
        object.insert("recipient".to_string(), json!("+15550009999"));
    }
    let result = tool.execute(args).await.expect("tool call");
    assert!(result.success, "{:?}", result.error);
    let answer: serde_json::Value = serde_json::from_str(&result.output).expect("json");
    let relay =
        crate::runtime::registry::WorkId::parse(answer["relay_work_id"].as_str().expect("the relay is addressable"))
            .expect("a parseable work id");
    let row = crate::runtime::registry::snapshot(relay).expect("the relay is listed");

    assert!(
        row.name
            .contains(&crate::security::op_id::ref_for_channel_recipient("wacli", "group-42")),
        "the relay must answer in the conversation the assignment came from: {}",
        row.name
    );
    assert!(
        !row.name.contains(&crate::security::op_id::ref_for_channel_recipient(
            "wacli",
            "+15550009999"
        )),
        "a model-supplied recipient must not become the relay's destination: {}",
        row.name
    );
    crate::runtime::registry::kill(relay, true).await;
    crate::runtime::chat_sessions::deregister(&registration.session_id);
}

/// Listing is an identity-bearing action too, so it fails closed on the same
/// condition `chat_assign` does.
#[tokio::test]
async fn chat_sessions_is_refused_without_a_trusted_scope() {
    let (tool, _sent) = assignment_tool(owner_security());
    let result = tool
        .execute(json!({"action": "chat_sessions"}))
        .await
        .expect("tool call");
    assert!(!result.success);
    assert!(
        result.error.unwrap_or_default().contains("no trusted caller scope"),
        "listing must fail closed on the same condition assignment does"
    );
}

/// Both actions have to be reachable and documented, or the model can never
/// use them — and both live on this tool rather than on new ones.
#[test]
fn the_schema_advertises_the_chat_assignment_actions_on_this_tool() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let schema = tool.execution_schema();
    let actions: Vec<&str> = schema["properties"]["action"]["enum"]
        .as_array()
        .expect("enumerated actions")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(actions.contains(&"chat_sessions"), "{actions:?}");
    assert!(actions.contains(&"chat_assign"), "{actions:?}");
    let dispositions: Vec<&str> = schema["properties"]["disposition"]["enum"]
        .as_array()
        .expect("enumerated dispositions")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert_eq!(dispositions, vec!["queue", "steer", "interrupt"]);
    assert_eq!(schema["properties"]["session_id"]["type"].as_str(), Some("string"));
    assert!(tool.description().contains("chat_assign"), "{}", tool.description());
}
