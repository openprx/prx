#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::indexing_slicing
)]
//! Ordinary process workers under a real parent (G5B-R5).
//!
//! Each test runs the real `prx chat --plain` binary (`CARGO_BIN_EXE_prx`)
//! against a loopback scripted provider. The parent spawns real
//! `prx session-worker` children through the production process launcher;
//! parents and workers share one process-local `HOME` and transport secret
//! file, so production key acquisition runs unchanged. Every wait is bounded
//! (`parent::PARENT_DEADLINE`, `provider::WORKER_DEADLINE`,
//! `provider::HOLD_WAIT`) and every guard cleans up on failure.

#[path = "ordinary_process_worker/parent.rs"]
mod parent;
#[path = "ordinary_process_worker/provider.rs"]
mod provider;
#[path = "ordinary_process_worker/scan.rs"]
mod scan;

use openprx::memory::Memory as _;
use openprx::session_worker::protocol::WorkerManifest;
use parent::{
    ParentFixture, assert_completion_journal, completion_event, owner, run_parent, runtime, spawn_parent, spawn_request,
};
use provider::{
    OwnedProviderServer, PROVIDER_MODEL, ProviderSpec, Reply, WORKER_DEADLINE, add_provider_fixture_model,
    assert_ordinary_manifest, assert_parent_join, scripted_join_call, scripted_spawn_call, text_reply,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// An AWS-key-shaped sentinel that production redaction recognizes.
const SENTINEL: &str = "AKIAIOSFODNN7EXAMPLE";
const ROOT_KEY: &str = "PRX-WORKER-ROOT-KEY";

/// Write the parent config: the loopback provider as the default chat model,
/// `sessions_spawn` always published, worker workspaces kept for inspection.
fn write_parent_config(fixture: &ParentFixture, destination: &str) {
    let mut config = openprx::config::Config {
        config_path: fixture.config_dir.join("config.toml"),
        workspace_dir: fixture.parent_workspace.clone(),
        default_provider: Some(destination.to_owned()),
        default_model: Some(PROVIDER_MODEL.to_owned()),
        api_key: Some(ROOT_KEY.to_owned()),
        ..openprx::config::Config::default()
    };
    config.secrets.encrypt = false;
    config.sessions_spawn.worker_workspace_root = Some(fixture.worker_root.to_string_lossy().into_owned());
    config.sessions_spawn.cleanup_on_complete = false;
    config.tool_tiering.always_include.push("sessions_spawn".to_owned());
    add_provider_fixture_model(&mut config, destination, PROVIDER_MODEL);
    std::fs::write(
        &config.config_path,
        toml::to_string_pretty(&config).expect("test: serialize parent config"),
    )
    .expect("test: write parent config");
}

/// A parent turn that spawns `tasks` as one process batch, joins it, and ends.
fn spawn_and_join(
    parent_marker: &'static str,
    tasks: serde_json::Value,
) -> impl FnMut(&str, usize, &str) -> Option<Reply> {
    move |marker, index, body| {
        if marker != parent_marker {
            return None;
        }
        Some(match index {
            1 => scripted_spawn_call(&serde_json::json!({
                "action": "spawn_batch", "mode": "process", "tasks": tasks.clone()
            })),
            2 => scripted_join_call(body),
            _ => text_reply("PRX-WORKER-OK"),
        })
    }
}

fn start_server(
    fixture: &ParentFixture,
    markers: Vec<&'static str>,
    parents: Vec<&'static str>,
    hold: Option<&'static str>,
    mut script: impl FnMut(&str, usize, &str) -> Reply + Send + 'static,
) -> OwnedProviderServer {
    let server = OwnedProviderServer::start(ProviderSpec {
        markers,
        execution: parents.clone(),
        parents,
        manifest_root: fixture.worker_root.clone(),
        tasks: BTreeMap::new(),
        hold,
        script: Box::new(move |call| script(call.marker, call.index, call.body)),
    });
    write_parent_config(fixture, server.destination());
    server
}

fn workspace_event_types(workspace: &Path) -> Vec<String> {
    let memory = openprx::memory::SqliteMemory::new(workspace).expect("test: parent memory");
    let principal = openprx::memory::MemoryPrincipal {
        workspace_id: workspace.to_string_lossy().into_owned(),
        ..openprx::memory::MemoryPrincipal::default()
    };
    runtime()
        .block_on(memory.list_message_events_recent(&principal, 500))
        .expect("test: workspace events")
        .into_iter()
        .map(|event| event.event_type)
        .collect()
}

fn assert_worker_terminal_events(workspace: &Path) {
    let events = workspace_event_types(workspace);
    for expected in ["turn.finalized", "worker.result.created"] {
        assert!(
            events.iter().any(|event| event == expected),
            "parent workspace memory lacks {expected}: {events:?}"
        );
    }
}

/// The pinned provider bodies: the original `CredentialProbe` reply and error
/// shapes with the sentinel messages.
fn secret_reply_body() -> String {
    format!(
        r#"{{"id":"chatcmpl-probe","object":"chat.completion","created":1,"model":"{PROVIDER_MODEL}","choices":[{{"index":0,"message":{{"role":"assistant","content":"Here is the key {SENTINEL} you asked for"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}}}"#
    )
}

fn secret_error_body() -> String {
    format!(r#"{{"error":{{"message":"upstream rejected key {SENTINEL}","type":"probe_error"}}}}"#)
}

fn run_secret_probe(status: u16, body: String) -> (ParentFixture, OwnedProviderServer, parent::ParentOutput) {
    const PARENT: &str = "PRX-SECRET-PARENT";
    const CHILD: &str = "PRX-SECRET-CHILD";
    let fixture = ParentFixture::new();
    let mut parent_script = spawn_and_join(PARENT, serde_json::json!([CHILD]));
    let server = start_server(
        &fixture,
        vec![PARENT, CHILD],
        vec![PARENT],
        None,
        move |marker, index, request| {
            parent_script(marker, index, request).unwrap_or_else(|| Reply::Raw {
                status,
                body: body.clone(),
            })
        },
    );
    let output = run_parent(&fixture.config_dir, &fixture.home, &spawn_request(PARENT));
    output.assert_success("secret probe");
    server.assert_no_failures();
    (fixture, server, output)
}

/// A process child whose provider reply carries a secret completes; the secret
/// reaches the parent's join, and nothing under the fixture keeps it.
#[test]
fn session_worker_persists_no_secret_from_a_reply() {
    let (fixture, server, _output) = run_secret_probe(200, secret_reply_body());
    {
        let requests = server.requests();
        let post_join = provider::post_join_request(&requests, "PRX-SECRET-PARENT");
        assert!(
            post_join.body.contains(SENTINEL),
            "the child result must carry the sentinel to the parent's join request"
        );
        assert!(
            requests.iter().any(|request| request.marker == "PRX-SECRET-CHILD"),
            "the child request must reach the provider"
        );
    }
    scan::assert_nothing_under_persists_secret(fixture.root(), SENTINEL);
    scan::assert_no_decision_records(fixture.root());
    assert_worker_terminal_events(&fixture.parent_workspace);
}

/// A process child whose provider fails with an error carrying a secret fails;
/// its error reaches the parent's join, and nothing under the fixture keeps
/// the secret.
#[test]
fn session_worker_persists_no_secret_from_a_provider_error() {
    let (fixture, server, _output) = run_secret_probe(400, secret_error_body());
    {
        let requests = server.requests();
        let post_join = provider::post_join_request(&requests, "PRX-SECRET-PARENT");
        let summary = provider::last_tool_summary(post_join).expect("test: join summary");
        let failed = summary["failed"].to_string();
        assert!(
            summary["failed"].as_array().is_some_and(|failed| failed.len() == 1),
            "the child must fail: {summary}"
        );
        assert!(
            failed.contains(SENTINEL) || failed.contains("upstream rejected key [REDACTED]"),
            "the provider error must reach the parent: {failed}"
        );
    }
    scan::assert_nothing_under_persists_secret(fixture.root(), SENTINEL);
    scan::assert_no_decision_records(fixture.root());
    assert_worker_terminal_events(&fixture.parent_workspace);
}

/// P2-R18 (process): one process child issues from the ordinary source and
/// completes through the ordinary completion journal.
#[test]
fn process_child_issues_without_decision_records() {
    const PARENT: &str = "PRX-PROCESS-PARENT";
    const CHILD: &str = "PRX-PROCESS-CHILD";
    let fixture = ParentFixture::new();
    let mut parent_script = spawn_and_join(PARENT, serde_json::json!([CHILD]));
    let server = start_server(
        &fixture,
        vec![PARENT, CHILD],
        vec![PARENT],
        None,
        move |marker, index, body| parent_script(marker, index, body).unwrap_or_else(|| text_reply("PRX-PROCESS-DONE")),
    );
    let output = run_parent(&fixture.config_dir, &fixture.home, &spawn_request(PARENT));
    output.assert_success("process");
    assert!(
        output.stdout.contains("PRX-WORKER-OK"),
        "parent stdout: {}",
        output.stdout
    );
    server.assert_no_failures();
    assert_parent_join(&server.requests(), PARENT, 1);
    assert!(
        server
            .requests()
            .iter()
            .filter(|request| request.marker == CHILD)
            .all(|request| request.head.contains(ROOT_KEY)),
        "the process child must call the configured provider with its sealed key"
    );
    let child = server.manifest(CHILD).expect("test: production child manifest");
    assert_ordinary_manifest(&child);
    assert_completion_journal(&fixture.parent_workspace, &child.manifest);
    scan::assert_no_decision_records(fixture.root());
}

/// P2-R18 (batch): two process members issue under a v2 batch journal and
/// both join.
#[test]
fn batch_children_issue_without_decision_records() {
    const PARENT: &str = "PRX-BATCH-PARENT";
    const FIRST: &str = "PRX-BATCH-CHILD-A";
    const SECOND: &str = "PRX-BATCH-CHILD-B";
    let fixture = ParentFixture::new();
    let mut parent_script = spawn_and_join(PARENT, serde_json::json!([FIRST, SECOND]));
    let server = start_server(
        &fixture,
        vec![PARENT, FIRST, SECOND],
        vec![PARENT],
        None,
        move |marker, index, body| parent_script(marker, index, body).unwrap_or_else(|| text_reply("PRX-BATCH-DONE")),
    );
    run_parent(&fixture.config_dir, &fixture.home, &spawn_request(PARENT)).assert_success("batch");
    server.assert_no_failures();
    let batch_id = {
        let requests = server.requests();
        assert_parent_join(&requests, PARENT, 2);
        let post_join = provider::post_join_request(&requests, PARENT);
        provider::batch_id_in(&post_join.body).expect("test: production batch id")
    };
    for marker in [FIRST, SECOND] {
        let child = server.manifest(marker).expect("test: production member manifest");
        assert_ordinary_manifest(&child);
        assert_completion_journal(&fixture.parent_workspace, &child.manifest);
    }
    let memory = openprx::memory::SqliteMemory::new(&fixture.parent_workspace).expect("test: parent memory");
    let workspace_id = fixture.parent_workspace.to_string_lossy().into_owned();
    let runtime = runtime();
    for phase in ["plan", "dispatch"] {
        let event = runtime
            .block_on(memory.find_message_event_by_idempotency_key(&workspace_id, &format!("batch:{batch_id}:{phase}")))
            .expect("test: batch journal lookup")
            .unwrap_or_else(|| panic!("batch journal {phase} record missing"));
        assert_eq!(event.event_type, format!("worker.batch.{phase}"));
        let payload: serde_json::Value =
            serde_json::from_str(event.raw_payload_json.as_deref().expect("test: batch payload"))
                .expect("test: batch payload JSON");
        assert_eq!(payload["fields"]["version"], 2, "batch journal is v2");
        assert_eq!(
            payload["fields"]["members"].as_array().map(Vec::len),
            Some(2),
            "batch journal lists both members"
        );
    }
    scan::assert_no_decision_records(fixture.root());
}

/// The recovery launch record's worker PID, polled until the process is gone
/// (restart recovery only recovers an absent process).
fn wait_for_worker_exit(workspace: &Path, manifest: &WorkerManifest) {
    let memory = openprx::memory::SqliteMemory::new(workspace).expect("test: parent memory");
    let launch = runtime()
        .block_on(memory.find_owned_message_event_by_idempotency_key(
            &workspace.to_string_lossy(),
            owner(manifest),
            &format!("worker:{}:ordinary-launch", manifest.run_id),
        ))
        .expect("test: launch record lookup")
        .expect("test: ordinary launch record");
    let payload: serde_json::Value =
        serde_json::from_str(launch.raw_payload_json.as_deref().expect("test: launch payload"))
            .expect("test: launch JSON");
    let pid = payload["fields"]["worker_pid"]
        .as_i64()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
        .expect("test: positive worker PID");
    let deadline = std::time::Instant::now() + WORKER_DEADLINE;
    while rustix::process::test_kill_process(pid) != Err(rustix::io::Errno::SRCH) {
        assert!(std::time::Instant::now() < deadline, "worker exceeded WORKER_DEADLINE");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn wait_for_completion(workspace: &Path, manifest: &WorkerManifest) {
    let deadline = std::time::Instant::now() + WORKER_DEADLINE;
    while completion_event(workspace, manifest).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "worker completion journal exceeded WORKER_DEADLINE"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn last_tool_text(request: &provider::ProviderRequest) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(&request.body).ok()?;
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .map(str::to_owned)
}

/// P2-R18 (parent restart): the parent dies while its process child runs; the
/// worker finishes; a restarted parent on the same `HOME` recovers the run
/// from its signed launch and completion journal and reports it completed.
#[test]
fn restarted_parent_recovers_and_verifies_ordinary_launch() {
    const PARENT: &str = "PRX-RESTART-PARENT";
    const CHILD: &str = "PRX-RESTART-CHILD";
    const HISTORY: &str = "PRX-RESTART-HISTORY";
    const CHILD_OUTPUT: &str = "PRX-RESTART-DONE";
    let fixture = ParentFixture::new();
    let run_slot = Arc::new(parking_lot::Mutex::new(None::<String>));
    let script_run = Arc::clone(&run_slot);
    let mut parent_script = spawn_and_join(PARENT, serde_json::json!([CHILD]));
    let mut server = start_server(
        &fixture,
        // The history request is classified first: a restarted parent may
        // quote its earlier turn.
        vec![HISTORY, PARENT, CHILD],
        vec![PARENT, HISTORY],
        Some(CHILD),
        move |marker, index, body| {
            if let Some(reply) = parent_script(marker, index, body) {
                return reply;
            }
            match (marker, index) {
                (HISTORY, 1) => script_run.lock().clone().map_or_else(
                    || text_reply("PRX-FIXTURE-NO-RUN"),
                    |run| scripted_spawn_call(&serde_json::json!({"action": "history", "run_id": run})),
                ),
                (HISTORY, _) => text_reply("PRX-HISTORY-OK"),
                _ => text_reply(CHILD_OUTPUT),
            }
        },
    );
    let mut first = spawn_parent(&fixture.config_dir, &fixture.home, &spawn_request(PARENT));
    assert!(
        server.wait_for_hold(WORKER_DEADLINE),
        "the held child request never reached the provider"
    );
    let child = server.manifest(CHILD).expect("test: production child manifest");
    assert_ordinary_manifest(&child);
    *run_slot.lock() = Some(child.manifest.run_id.clone());
    first.kill();
    drop(first);
    server.release_hold();
    wait_for_completion(&fixture.parent_workspace, &child.manifest);
    wait_for_worker_exit(&fixture.parent_workspace, &child.manifest);

    let restarted = run_parent(
        &fixture.config_dir,
        &fixture.home,
        &format!("Use sessions_spawn history for {HISTORY}."),
    );
    restarted.assert_success("restarted");
    // Both parents and the worker read the one transport secret file.
    assert_eq!(fixture.transport_secret().len(), 32);
    {
        let requests = server.requests();
        let history = requests
            .iter()
            .filter(|request| request.marker == HISTORY && !provider::is_assessment_request(request))
            .find_map(last_tool_text)
            .expect("test: restarted parent history tool result");
        assert!(
            history.contains(&format!(
                "Conversation history for sub-agent `{}` (1 entries",
                child.manifest.run_id
            )) && history.contains(CHILD_OUTPUT),
            "the restarted parent must report the recovered run completed with its output: {history}"
        );
    }
    server.stop();
    server.assert_no_failures();
    assert_completion_journal(&fixture.parent_workspace, &child.manifest);
    scan::assert_no_decision_records(fixture.root());
}
