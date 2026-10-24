//! Drive approval and resume through the real chat loop, then submit later input.
use super::*;
use openprx::skills::lifecycle::{Bundle, OwnerScope, SkillLifecycleStore};
use std::os::unix::fs::PermissionsExt as _;

const LATER_REPLY: &str = "[PRX-RESUME-LATER-REPLY]";
const REFUSAL: &str = "Decision resume could not be admitted for this chat. No operation was run by this request.";

fn chat_owner(guard: &HarnessGuard) -> String {
    openprx::runtime::envelope::RuntimeEnvelope::chat_canonical(
        guard.workspace_dir.to_string_lossy(),
        "prx_resume_fixture",
        openprx::memory::MemoryVisibility::Workspace,
    )
    .resolved_owner_id()
}

fn import_fixture() -> (HarnessGuard, String, String) {
    import_fixture_at(openprx::security::AutonomyLevel::Full)
}

fn import_fixture_at(level: openprx::security::AutonomyLevel) -> (HarnessGuard, String, String) {
    let mut guard = new_harness_guard().expect("isolated resume fixture");
    // CONFIG_DIR takes precedence over WORKSPACE in the real CLI loader.
    guard.workspace_dir = guard.config_dir.join("workspace");
    let source = guard.workspace_dir.join("incoming/report-runner");
    std::fs::create_dir_all(source.join("scripts")).expect("bundle directory");
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: Report runner\ndescription: Runs the report\n---\n\nRun the report.\n",
    )
    .expect("skill instructions");
    std::fs::write(
        source.join("SKILL.toml"),
        "[skill]\nname = \"Report runner\"\ndescription = \"Runs the report\"\nversion = \"1.0.0\"\n\n\
         [[tools]]\nname = \"run\"\ndescription = \"run report\"\nkind = \"script\"\ncommand = \"scripts/run.sh\"\n",
    )
    .expect("skill manifest");
    let script = source.join("scripts/run.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf 'report complete\\n'\n").expect("skill script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("executable script");
    let revision = Bundle::from_dir(&source)
        .expect("reviewed bundle")
        .revision_id()
        .to_string();
    let arguments = serde_json::json!({
        "action": "lifecycle_import_executable", "source": source, "revision_id": revision,
    });
    let mut config = openprx::config::Config::default();
    config.autonomy.level = level;
    // Publish the lifecycle tool through operator configuration so the mock
    // provider can request it without a separate capability-discovery turn.
    config.tool_tiering.always_include.push("skills_manage".to_owned());
    config.skills_lifecycle.enabled = true;
    config.skills_lifecycle.mode = openprx::config::skills_lifecycle::SkillsLifecycleModeConfig::Auto;
    config.skills_lifecycle.owner_scope = Some(chat_owner(&guard));
    std::fs::write(
        guard.config_dir.join("config.toml"),
        toml::to_string_pretty(&config).expect("resume config"),
    )
    .expect("write resume config");
    (guard, arguments.to_string(), revision)
}

fn assert_import_generation(guard: &HarnessGuard, revision: &str, expected: usize) {
    let store = SkillLifecycleStore::open(
        &guard.workspace_dir,
        OwnerScope::new(&chat_owner(guard)).expect("owner"),
    )
    .expect("lifecycle store");
    let bindings = store.list_active_bindings().expect("active imports");
    assert_eq!(bindings.len(), expected);
    if expected != 0 {
        assert_eq!(bindings[0].revision_id.as_str(), revision);
        assert_eq!(bindings[0].generation, 1, "resume must execute the import only once");
    }
}

fn approve_and_resume_import(guard: &HarnessGuard, arguments: &str, revision: &str) -> (SessionGuard, String) {
    let tool_call = format!("skills_manage:{arguments}");
    let mut sg = spawn_chat_in(
        guard,
        &["--plain"],
        &[
            ("OPENPRX_MOCK_TOOL_CALL", &tool_call),
            ("OPENPRX_MOCK_RESPONSE", LATER_REPLY),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session
        .send(format!(
            "Call the skills_manage tool with these arguments: {arguments}\r"
        ))
        .expect("request import");
    let pending = read_until_with_dsr(session, "The operation has not run.", TURN_TIMEOUT);
    let captures = regex::Regex::new(r"Approval is required for decision ([0-9a-f-]{36})")
        .expect("decision regex")
        .captures(&pending)
        .unwrap_or_else(|| panic!("missing pending approval: {pending}"));
    let decision_id = captures[1].to_owned();
    assert_import_generation(guard, revision, 0);
    session
        .send(format!("/approve --decision {decision_id}\r"))
        .expect("approve");
    read_until_with_dsr(session, "explicit resume is required.", TURN_TIMEOUT);
    assert_import_generation(guard, revision, 0);
    session
        .send(format!("/resume-decision {decision_id}\r"))
        .expect("resume");
    read_until_with_dsr(session, "\"replaced\": null", TURN_TIMEOUT);
    assert_import_generation(guard, revision, 1);
    (sg, decision_id)
}

fn later_input_and_exit(session: &mut TrackedSession) {
    session.send("/help\r").expect("help after resume");
    read_until_with_dsr(session, "Available commands:", TURN_TIMEOUT);
    session.send("hi\r").expect("later message");
    read_until_with_dsr(session, LATER_REPLY, TURN_TIMEOUT);
    session.send("/exit\r").expect("exit after resume");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "chat must exit after later input");
}

// A real grant-bound call whose tool failure leaves the pinned catalog intact.
// This covers the same foreground completion path while allowing the following
// model turn; successful catalog-changing imports are covered separately below.
fn approve_and_resume_publish_failure() -> (HarnessGuard, SessionGuard, String) {
    approve_and_resume_publish_failure_with_delay("")
}

fn approve_and_resume_publish_failure_with_delay(delay: &str) -> (HarnessGuard, SessionGuard, String) {
    let (guard, _, _) = import_fixture();
    let arguments =
        serde_json::json!({"action": "lifecycle_publish", "proposal_id": format!("prop-{}", "0".repeat(32))});
    let tool_call = format!("skills_manage:{arguments}");
    let mut sg = spawn_chat_in(
        &guard,
        &["--plain"],
        &[
            ("OPENPRX_MOCK_TOOL_CALL", &tool_call),
            ("OPENPRX_MOCK_RESPONSE", LATER_REPLY),
            ("OPENPRX_MOCK_DELAY_MS_BY_PROMPT", delay),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session
        .send(format!(
            "Call the skills_manage tool with these arguments: {arguments}\r"
        ))
        .expect("request publish");
    let pending = read_until_with_dsr(session, "The operation has not run.", TURN_TIMEOUT);
    let captures = regex::Regex::new(r"Approval is required for decision ([0-9a-f-]{36})")
        .expect("decision regex")
        .captures(&pending)
        .unwrap_or_else(|| panic!("missing pending approval: {pending}"));
    let decision_id = captures[1].to_owned();
    assert_publish_attempts(&guard, 0);
    session
        .send(format!("/approve --decision {decision_id}\r"))
        .expect("approve publish");
    read_until_with_dsr(session, "explicit resume is required.", TURN_TIMEOUT);
    assert_publish_attempts(&guard, 0);
    session
        .send(format!("/resume-decision {decision_id}\r"))
        .expect("resume publish");
    read_until_with_dsr(session, "Approved operation could not be completed.", TURN_TIMEOUT);
    assert_publish_attempts(&guard, 1);
    (guard, sg, decision_id)
}

fn assert_publish_attempts(guard: &HarnessGuard, expected: usize) {
    let db = rusqlite::Connection::open_with_flags(
        guard.workspace_dir.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read owned fixture events");
    let mut query = db
        .prepare("SELECT raw_payload_json FROM message_events WHERE event_type = 'tool.execution.finalized'")
        .expect("query tool execution receipts");
    let receipts = query
        .query_map([], |row| row.get::<_, String>(0))
        .expect("receipt rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("read receipts");
    assert_eq!(
        receipts.len(),
        expected,
        "approval must not execute; a consumed resume must not execute again"
    );
    for receipt in receipts {
        let receipt: serde_json::Value = serde_json::from_str(&receipt).expect("execution receipt");
        assert_eq!(receipt["outcome"]["descriptor"]["public_name"], "skills_manage");
        assert_eq!(
            receipt["outcome"]["status"], "failed",
            "resume must reach tool execution"
        );
        assert_eq!(receipt["outcome"]["result"]["success"], false);
        assert!(
            receipt["outcome"]["result"]["error"]
                .as_str()
                .expect("tool error")
                .contains("proposal")
        );
    }
    let resolved: i64 = db
        .query_row(
            "SELECT count(*) FROM message_events WHERE event_type = 'approval.decision.resolved'",
            [],
            |row| row.get(0),
        )
        .expect("consumed grants");
    assert_eq!(
        resolved,
        i64::try_from(expected).expect("receipt count"),
        "one-use grant consumption must match the execution"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn chat_resume_decision_processes_later_commands_and_messages() {
    let (guard, mut sg, _decision_id) = approve_and_resume_publish_failure();
    later_input_and_exit(sg.session());
    assert_publish_attempts(&guard, 1);
}

#[test]
#[serial(prx_chat_pty)]
fn chat_resume_consumed_decision_in_same_chat_refuses_and_processes_later_input() {
    let (guard, mut sg, decision_id) = approve_and_resume_publish_failure();
    let session = sg.session();
    session
        .send(format!("/resume-decision {decision_id}\r"))
        .expect("resume consumed decision");
    read_until_with_dsr(session, REFUSAL, TURN_TIMEOUT);
    later_input_and_exit(session);
    assert_publish_attempts(&guard, 1);
}

#[test]
#[serial(prx_chat_pty)]
fn chat_queued_consumed_resume_releases_queue_and_processes_later_input() {
    let (guard, mut sg, decision_id) = approve_and_resume_publish_failure_with_delay("prx_busy_resume_turn=5000");
    let session = sg.session();
    session.send("hi prx_busy_resume_turn\r").expect("start delayed turn");
    session.send("/queue\r").expect("inspect running turn");
    read_until_with_dsr(session, "Main queue: 0 queued (0 priority), 1 running.", TURN_TIMEOUT);
    session
        .send(format!("/resume-decision {decision_id}\r"))
        .expect("queue consumed resume behind running turn");
    session.send("/queue\r").expect("inspect queued resume");
    // Prove the resume actually waited behind a running turn, rather than
    // relying on the fixture delay alone to establish the interleaving.
    read_until_with_dsr(session, "Main queue: 1 queued (0 priority), 1 running.", TURN_TIMEOUT);
    read_until_with_dsr(session, REFUSAL, TURN_TIMEOUT);
    session.send("/queue\r").expect("inspect released queue");
    read_until_with_dsr(session, "Main queue: 0 queued (0 priority), 0 running.", TURN_TIMEOUT);
    later_input_and_exit(session);
    assert_publish_attempts(&guard, 1);
}

#[test]
#[serial(prx_chat_pty)]
fn chat_resume_consumed_decision_from_new_chat_refuses_and_processes_later_input() {
    let (guard, arguments, revision) = import_fixture();
    let (mut original, decision_id) = approve_and_resume_import(&guard, &arguments, &revision);
    original.session().send("/exit\r").expect("exit original chat");
    assert!(
        wait_for_exit(original.session(), EXIT_TIMEOUT),
        "original chat must exit"
    );
    let mut fresh = spawn_chat_in(&guard, &["--plain"], &[("OPENPRX_MOCK_RESPONSE", LATER_REPLY)]);
    let session = fresh.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session
        .send(format!("/resume-decision {decision_id}\r"))
        .expect("resume consumed decision");
    read_until_with_dsr(session, REFUSAL, TURN_TIMEOUT);
    later_input_and_exit(session);
    assert_import_generation(&guard, &revision, 1);
}

#[test]
#[serial(prx_chat_pty)]
fn chat_import_resume_refreshes_catalog_for_later_input_in_the_same_chat() {
    let (guard, arguments, revision) = import_fixture();
    let (mut sg, decision_id) = approve_and_resume_import(&guard, &arguments, &revision);
    let session = sg.session();
    session.send("/help\r").expect("help after import resume");
    read_until_with_dsr(session, "Available commands:", TURN_TIMEOUT);
    session
        .send(format!("/approval --decision {decision_id}\r"))
        .expect("inspect after import resume");
    // The consumed decision stays consumed. A later source turn gets a new
    // catalog publication without restarting this chat or reusing the grant.
    read_until_with_dsr(
        session,
        "No inspectable pending decision is available for this chat.",
        TURN_TIMEOUT,
    );
    session.send("hi\r").expect("message after catalog-changing resume");
    read_until_with_dsr(session, LATER_REPLY, TURN_TIMEOUT);
    session.send("/exit\r").expect("exit after import resume");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "chat must exit after import resume"
    );
    assert_import_generation(&guard, &revision, 1);
}

/// Finalized, succeeded `skills_manage` execution receipts: the approved import.
fn executed_receipts(guard: &HarnessGuard) -> usize {
    let db = rusqlite::Connection::open_with_flags(
        guard.workspace_dir.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read owned fixture events");
    let mut query = db
        .prepare("SELECT raw_payload_json FROM message_events WHERE event_type = 'tool.execution.finalized'")
        .expect("query tool execution receipts");
    let receipts = query
        .query_map([], |row| row.get::<_, String>(0))
        .expect("receipt rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("read receipts");
    receipts
        .iter()
        .map(|receipt| serde_json::from_str::<serde_json::Value>(receipt).expect("execution receipt"))
        .filter(|receipt| {
            receipt["outcome"]["descriptor"]["public_name"] == "skills_manage"
                && receipt["outcome"]["status"] == "succeeded"
        })
        .count()
}

// Finding 115.1: at supervised autonomy a named, grant-bound skills_manage call
// is collected by the plain chat loop (the dispatcher stream path), recorded as
// a pending approval, approved, and resumed into exactly one execution.
#[test]
#[serial(prx_chat_pty)]
fn chat_supervised_named_import_collects_approves_and_resumes_once() {
    let (guard, arguments, revision) = import_fixture_at(openprx::security::AutonomyLevel::Supervised);
    let (mut sg, decision_id) = approve_and_resume_import(&guard, &arguments, &revision);
    assert_eq!(executed_receipts(&guard), 1, "one approved call is one execution");
    let session = sg.session();
    session
        .send(format!("/resume-decision {decision_id}\r"))
        .expect("resume the consumed decision");
    read_until_with_dsr(session, REFUSAL, TURN_TIMEOUT);
    assert_eq!(executed_receipts(&guard), 1, "a consumed decision does not run again");
    session.send("/help\r").expect("help after resume");
    read_until_with_dsr(session, "Available commands:", TURN_TIMEOUT);
    session.send("/exit\r").expect("exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "chat must exit after resume");
    assert_import_generation(&guard, &revision, 1);
}

// Finding 115.2: a turn-engine error on the dispatcher path is shown in chat
// and the loop keeps accepting input. Here the provider answers a collection
// request with text instead of the one tool call it asked for.
#[test]
#[serial(prx_chat_pty)]
fn chat_turn_engine_error_is_visible_and_chat_keeps_running() {
    let (guard, arguments, revision) = import_fixture_at(openprx::security::AutonomyLevel::Supervised);
    let mut sg = spawn_chat_in(&guard, &["--plain"], &[("OPENPRX_MOCK_RESPONSE", LATER_REPLY)]);
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session
        .send(format!(
            "Call the skills_manage tool with these arguments: {arguments}\r"
        ))
        .expect("request import");
    read_until_with_dsr(session, "Error: ", TURN_TIMEOUT);
    session.send("/help\r").expect("help after a failed turn");
    read_until_with_dsr(session, "Available commands:", TURN_TIMEOUT);
    session.send("/exit\r").expect("exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "chat must exit after a failed turn"
    );
    assert_import_generation(&guard, &revision, 0);
    assert_eq!(executed_receipts(&guard), 0);
}
