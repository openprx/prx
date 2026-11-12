//! Execution-owned approval through real chat, SQLite and a terminal.
use super::*;
use openprx::skills::lifecycle::{Bundle, OwnerScope, SkillLifecycleStore};
use sha2::{Digest as _, Sha256};
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
    let mut guard = new_harness_guard().expect("test: isolated resume fixture");
    // CONFIG_DIR takes precedence over WORKSPACE in the real CLI loader.
    guard.workspace_dir = guard.config_dir.join("workspace");
    let source = guard.workspace_dir.join("incoming/report-runner");
    std::fs::create_dir_all(source.join("scripts")).expect("test: bundle directory");
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: Report runner\ndescription: Runs the report\n---\n\nRun the report.\n",
    )
    .expect("test: skill instructions");
    std::fs::write(
        source.join("SKILL.toml"),
        "[skill]\nname = \"Report runner\"\ndescription = \"Runs the report\"\nversion = \"1.0.0\"\n\n\
         [[tools]]\nname = \"run\"\ndescription = \"run report\"\nkind = \"script\"\ncommand = \"scripts/run.sh\"\n",
    )
    .expect("test: skill manifest");
    let script = source.join("scripts/run.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf 'report complete\\n'\n").expect("test: skill script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("test: executable script");
    let revision = Bundle::from_dir(&source)
        .expect("test: reviewed bundle")
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
        toml::to_string_pretty(&config).expect("test: resume config"),
    )
    .expect("test: write resume config");
    (guard, arguments.to_string(), revision)
}

fn assert_import_generation(guard: &HarnessGuard, revision: &str, expected: usize) {
    let store = SkillLifecycleStore::open(
        &guard.workspace_dir,
        OwnerScope::new(&chat_owner(guard)).expect("test: owner"),
    )
    .expect("test: lifecycle store");
    let bindings = store.list_active_bindings().expect("test: active imports");
    assert_eq!(bindings.len(), expected);
    if expected != 0 {
        assert_eq!(
            bindings.first().expect("test: one binding").revision_id.as_str(),
            revision
        );
        assert_eq!(
            bindings.first().expect("test: one binding").generation,
            1,
            "resume must execute the import only once"
        );
    }
}

fn receipts(guard: &HarnessGuard) -> Vec<serde_json::Value> {
    let db = rusqlite::Connection::open_with_flags(
        guard.workspace_dir.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("test: owned event store");
    let mut query = db
        .prepare("SELECT raw_payload_json FROM message_events WHERE event_type='tool.execution.finalized' LIMIT 2")
        .expect("test: tool receipts query");
    query
        .query_map([], |row| row.get::<_, String>(0))
        .expect("test: receipt rows")
        .map(|row| serde_json::from_str(&row.expect("test: receipt row")).expect("test: receipt JSON"))
        .collect()
}

fn executed_receipts(guard: &HarnessGuard) -> usize {
    receipts(guard)
        .iter()
        .filter(|row| row["outcome"]["status"] == "succeeded")
        .count()
}

fn assert_exact_execution(guard: &HarnessGuard, arguments: &str, succeeded: bool) {
    let rows = receipts(guard);
    assert_eq!(rows.len(), 1, "one proposal executes exactly once");
    let row = rows.first().expect("test: one execution receipt");
    let arguments: serde_json::Value = serde_json::from_str(arguments).expect("test: proposed arguments");
    let digest = hex::encode(Sha256::digest(arguments.to_string().as_bytes()));
    assert_eq!(
        row["reservation"]["input_sha256"], digest,
        "execution uses the exact proposed argument digest"
    );
    assert_eq!(row["outcome"]["descriptor"]["public_name"], "skills_manage");
    assert_eq!(row["outcome"]["status"], if succeeded { "succeeded" } else { "failed" });
    assert_eq!(row["outcome"]["result"]["success"], succeeded);
}

fn completed_decision(guard: &HarnessGuard) -> Option<String> {
    let db = rusqlite::Connection::open_with_flags(
        guard.workspace_dir.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let raw: String = db
        .query_row(
            "SELECT raw_payload_json FROM message_events WHERE event_type='turn.finalized' ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get("decision_id")?.as_str().map(str::to_owned)
}

// Poll a durable semantic event while servicing terminal DSR; no timing guess
// establishes completion. TrackedSession owns kill/reap on every failure path.
fn wait_for_completion(guard: &HarnessGuard, session: &mut TrackedSession) -> (String, String) {
    let deadline = std::time::Instant::now() + TURN_TIMEOUT;
    let mut output = String::new();
    let mut pending = String::new();
    let mut buffer = [0u8; 4096];
    loop {
        if let Some(id) = completed_decision(guard) {
            return (id, output);
        }
        match session.try_read(&mut buffer) {
            Ok(0) => panic!("test: chat exited before durable completion: {output}"),
            Ok(n) => {
                ack_all_dsr_with_pending(session, &buffer[..n], &mut pending);
                output.push_str(&String::from_utf8_lossy(&buffer[..n]));
                assert!(output.len() <= 2 * 1024 * 1024, "bounded PTY capture");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => panic!("test: completion PTY read: {error}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "durable completion timeout: {output}"
        );
    }
}

fn launch_call(guard: &HarnessGuard, arguments: &str, tui: bool, extra: &[(&str, &str)]) -> SessionGuard {
    let tool = format!("skills_manage:{arguments}");
    let mut env = vec![
        ("OPENPRX_MOCK_TOOL_CALL", tool.as_str()),
        ("OPENPRX_MOCK_RESPONSE", LATER_REPLY),
    ];
    env.extend_from_slice(extra);
    if tui {
        env.push(("PRX_TUI", "1"));
    }
    let mut process = spawn_chat_in(guard, if tui { &[] } else { &["--plain"] }, &env);
    let session = process.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session
        .send(format!(
            "Call the skills_manage tool with these arguments: {arguments}\r"
        ))
        .expect("test: propose actual import");
    process
}

fn later_input_and_exit(session: &mut TrackedSession) {
    session.send("/help\r").expect("test: help after tool turn");
    read_until_with_dsr(session, "Available commands:", TURN_TIMEOUT);
    session.send("hi\r").expect("test: later input");
    read_until_with_dsr(session, LATER_REPLY, TURN_TIMEOUT);
    session.send("/exit\r").expect("test: exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "chat exits after later input");
}

fn completed_publish(extra: &[(&str, &str)]) -> (HarnessGuard, SessionGuard, String, String) {
    let (guard, _, _) = import_fixture();
    let arguments =
        serde_json::json!({"action":"lifecycle_publish", "proposal_id":format!("prop-{}", "0".repeat(32))}).to_string();
    let mut process = launch_call(&guard, &arguments, false, extra);
    let (id, output) = wait_for_completion(&guard, process.session());
    assert!(!output.contains("Tool Approval"), "Full does not ask");
    assert_exact_execution(&guard, &arguments, false);
    (guard, process, id, arguments)
}

#[test]
#[serial(prx_chat_pty)]
fn chat_full_publish_failure_processes_later_input() {
    let (guard, mut process, _, args) = completed_publish(&[]);
    later_input_and_exit(process.session());
    assert_exact_execution(&guard, &args, false);
}

#[test]
#[serial(prx_chat_pty)]
fn chat_completed_operation_cannot_resume_in_same_chat() {
    let (guard, mut process, id, args) = completed_publish(&[]);
    process
        .session()
        .send(format!("/resume-decision {id}\r"))
        .expect("test: reject removed resume authority");
    read_until_with_dsr(process.session(), REFUSAL, TURN_TIMEOUT);
    later_input_and_exit(process.session());
    assert_exact_execution(&guard, &args, false);
}

fn wait_for_path(session: &mut TrackedSession, path: &std::path::Path, total: Duration) {
    let deadline = std::time::Instant::now() + total;
    let mut buffer = [0u8; 4096];
    let mut pending = String::new();
    while !path.exists() {
        match session.try_read(&mut buffer) {
            Ok(0) => panic!("test: EOF before semantic hold event"),
            Ok(n) => ack_all_dsr_with_pending(session, &buffer[..n], &mut pending),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => panic!("test: held turn read: {error}"),
        }
        assert!(std::time::Instant::now() < deadline, "held turn did not signal start");
    }
}

#[test]
#[serial(prx_chat_pty)]
fn chat_completed_operation_refusal_releases_queue() {
    let (guard, _, _) = import_fixture();
    let hold = guard.workspace_dir.join("prx_busy_resume_hold");
    std::fs::write(&hold, b"hold").expect("test: semantic hold file");
    let started = hold.with_extension("started");
    let spec = format!("prx_busy_resume_turn={}", hold.display());
    let args =
        serde_json::json!({"action":"lifecycle_publish", "proposal_id":format!("prop-{}", "0".repeat(32))}).to_string();
    let mut process = launch_call(&guard, &args, false, &[("OPENPRX_MOCK_HOLD_WHILE_FILE", &spec)]);
    let (id, _) = wait_for_completion(&guard, process.session());
    let session = process.session();
    session
        .send("hi prx_busy_resume_turn\r")
        .expect("test: start held turn");
    wait_for_path(session, &started, TURN_TIMEOUT);
    session.send("/queue\r").expect("test: inspect busy queue");
    read_until_with_dsr(session, "Main queue: 0 queued (0 priority), 1 running.", TURN_TIMEOUT);
    session
        .send(format!("/resume-decision {id}\r"))
        .expect("test: queue refused request");
    session.send("/queue\r").expect("test: inspect queued request");
    read_until_with_dsr(session, "Main queue: 1 queued (0 priority), 1 running.", TURN_TIMEOUT);
    std::fs::remove_file(&hold).expect("test: release held turn");
    read_until_with_dsr(session, REFUSAL, TURN_TIMEOUT);
    session.send("/queue\r").expect("test: inspect released queue");
    read_until_with_dsr(session, "Main queue: 0 queued (0 priority), 0 running.", TURN_TIMEOUT);
    later_input_and_exit(session);
    assert_exact_execution(&guard, &args, false);
}

fn full_import() -> (HarnessGuard, SessionGuard, String, String, String) {
    let (guard, args, revision) = import_fixture();
    let mut process = launch_call(&guard, &args, false, &[]);
    let (id, output) = wait_for_completion(&guard, process.session());
    assert!(!output.contains("Tool Approval"), "Full import has no forced Ask");
    assert_import_generation(&guard, &revision, 1);
    assert_exact_execution(&guard, &args, true);
    (guard, process, id, args, revision)
}

#[test]
#[serial(prx_chat_pty)]
fn chat_completed_import_cannot_resume_from_new_chat() {
    let (guard, mut original, id, args, revision) = full_import();
    original.session().send("/exit\r").expect("test: exit original chat");
    assert!(wait_for_exit(original.session(), EXIT_TIMEOUT));
    let mut fresh = spawn_chat_in(&guard, &["--plain"], &[("OPENPRX_MOCK_RESPONSE", LATER_REPLY)]);
    read_until_with_dsr(fresh.session(), "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(fresh.session(), Duration::from_millis(200));
    fresh
        .session()
        .send(format!("/resume-decision {id}\r"))
        .expect("test: reject completed cross-session replay");
    read_until_with_dsr(fresh.session(), REFUSAL, TURN_TIMEOUT);
    later_input_and_exit(fresh.session());
    assert_import_generation(&guard, &revision, 1);
    assert_exact_execution(&guard, &args, true);
}

#[test]
#[serial(prx_chat_pty)]
fn chat_full_import_refreshes_catalog_for_later_input() {
    let (guard, mut process, _, args, revision) = full_import();
    later_input_and_exit(process.session());
    assert_import_generation(&guard, &revision, 1);
    assert_exact_execution(&guard, &args, true);
}

fn supervised_import(approved: bool) {
    let (guard, args, revision) = import_fixture_at(openprx::security::AutonomyLevel::Supervised);
    let mut process = launch_call(&guard, &args, true, &[]);
    read_until_with_dsr(process.session(), "Tool Approval", TURN_TIMEOUT);
    assert_import_generation(&guard, &revision, 0);
    assert!(receipts(&guard).is_empty(), "review must precede execution");
    process
        .session()
        .send(if approved { "y" } else { "n" })
        .expect("test: actual interactive decision");
    let (id, _) = wait_for_completion(&guard, process.session());
    assert_import_generation(&guard, &revision, usize::from(approved));
    if approved {
        assert_exact_execution(&guard, &args, true);
    } else {
        assert_eq!(executed_receipts(&guard), 0, "denied call never executes");
    }
    process
        .session()
        .send(format!("/resume-decision {id}\r"))
        .expect("test: no replay authority");
    read_until_with_dsr(process.session(), REFUSAL, TURN_TIMEOUT);
    later_input_and_exit(process.session());
    assert_import_generation(&guard, &revision, usize::from(approved));
    assert_eq!(executed_receipts(&guard), usize::from(approved));
}

#[test]
#[serial(prx_chat_pty)]
fn chat_supervised_import_approval_executes_exact_call_once() {
    supervised_import(true);
}

#[test]
#[serial(prx_chat_pty)]
fn chat_supervised_import_denial_never_executes() {
    supervised_import(false);
}

// Provider failure remains visible and never executes an import.
#[test]
#[serial(prx_chat_pty)]
fn chat_turn_engine_error_is_visible_and_chat_keeps_running() {
    let (guard, arguments, revision) = import_fixture_at(openprx::security::AutonomyLevel::Supervised);
    let mut sg = spawn_chat_in(
        &guard,
        &["--plain"],
        &[
            ("OPENPRX_MOCK_RESPONSE", LATER_REPLY),
            ("OPENPRX_MOCK_ERROR", "PRX_COLLECTION_FIXTURE_FAILURE"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session
        .send(format!(
            "Call the skills_manage tool with these arguments: {arguments}\r"
        ))
        .expect("test: request import");
    read_until_with_dsr(session, "Error: ", TURN_TIMEOUT);
    session.send("/help\r").expect("test: help after a failed turn");
    read_until_with_dsr(session, "Available commands:", TURN_TIMEOUT);
    session.send("/exit\r").expect("test: exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "chat must exit after a failed turn"
    );
    assert_import_generation(&guard, &revision, 0);
    assert_eq!(executed_receipts(&guard), 0);
}
