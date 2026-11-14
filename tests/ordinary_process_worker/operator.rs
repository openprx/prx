//! Operator children launch nested process children (PR #205 review r1 #1).
//!
//! The real `prx chat --plain` parent receives a local `/bg` operator command.
//! The operator child (task mode in the parent process, or process mode in a
//! real `prx session-worker`) calls `sessions_spawn` in process mode, so a
//! real leaf worker is launched under the child, completes, and is joined.
//! Owner rule: a child equals the main session; only depth, budget and
//! integrity bound recursion.

use super::parent::{ParentFixture, assert_completion_journal, spawn_held_parent};
use super::provider::{
    CapturedManifest, OwnedProviderServer, ProviderRequest, ProviderSpec, assert_join_summary,
    assert_ordinary_manifest, is_assessment_request, last_tool_summary, scripted_join_call, scripted_spawn_call,
    text_reply,
};
use std::io::Write as _;
use std::path::Path;

/// Longest wait for the operator child's join; it leaves the parent, whose
/// `PARENT_DEADLINE` runs from spawn, time to exit.
const JOIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
/// Polling interval while waiting for the operator child's join.
const JOIN_POLL: std::time::Duration = std::time::Duration::from_millis(20);

struct Case {
    mode: &'static str,
    child: &'static str,
    leaf: &'static str,
}

fn child_task(case: &Case) -> String {
    format!("Use sessions_spawn in process mode for {}", case.child)
}

/// The first request of `marker` whose last tool result is a join summary.
fn post_join(requests: &[ProviderRequest], marker: &str) -> Option<ProviderRequest> {
    requests
        .iter()
        .filter(|request| request.marker == marker && !is_assessment_request(request))
        .find(|request| last_tool_summary(request).is_some_and(|summary| summary.get("total").is_some()))
        .cloned()
}

/// Wait (bounded by `JOIN_DEADLINE`) until the operator child joined its leaf.
fn wait_for_child_join(server: &OwnedProviderServer, marker: &str) -> Option<ProviderRequest> {
    let deadline = std::time::Instant::now() + JOIN_DEADLINE;
    loop {
        let found = post_join(&server.requests(), marker);
        if found.is_some() {
            return found;
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(JOIN_POLL);
    }
}

/// The marker, index and last tool result of every captured request, for a
/// failure message.
fn request_trail(server: &OwnedProviderServer) -> Vec<String> {
    server
        .requests()
        .iter()
        .map(|request| {
            format!(
                "{} assessment={} tool={:?}",
                request.marker,
                is_assessment_request(request),
                super::last_tool_text(request)
            )
        })
        .collect()
}

/// Every parent workspace event, for a failure message.
fn event_trail(workspace: &Path) -> Vec<String> {
    let memory = openprx::memory::SqliteMemory::new(workspace).expect("test: parent memory");
    let principal = openprx::memory::MemoryPrincipal {
        workspace_id: workspace.to_string_lossy().into_owned(),
        ..openprx::memory::MemoryPrincipal::default()
    };
    super::runtime()
        .block_on(openprx::memory::Memory::list_message_events_recent(
            &memory, &principal, 200,
        ))
        .unwrap_or_default()
        .into_iter()
        .map(|event| format!("{}: {} {:?}", event.event_type, event.content, event.raw_payload_json))
        .collect()
}

/// The leaf's durable ordinary input record names its spawning run.
fn input_record_parent_run(leaf: &CapturedManifest) -> Option<String> {
    let source = leaf
        .manifest
        .ordinary_source
        .as_ref()
        .expect("test: leaf ordinary source");
    let input = source.input.as_ref().expect("test: recorded leaf input");
    let memory = openprx::memory::SqliteMemory::new(Path::new(&source.parent_constraints.workspace))
        .expect("test: leaf input memory");
    super::runtime()
        .block_on(openprx::memory::Memory::find_owned_message_event(
            &memory,
            &source.parent_constraints.workspace,
            super::owner(&leaf.manifest),
            &input.event_id,
        ))
        .expect("test: leaf input lookup")
        .expect("test: durable leaf input record")
        .parent_run_id
}

fn run_case(case: &Case) {
    let fixture = ParentFixture::new();
    let process = case.mode == "process";
    let (child, leaf) = (case.child, case.leaf);
    // A task-mode operator child runs inside the parent process (no worker
    // manifest); a process-mode one is a worker whose manifest is captured.
    let parents = if process { Vec::new() } else { vec![child] };
    let mut server = OwnedProviderServer::start(ProviderSpec {
        markers: vec![child, leaf],
        parents,
        execution: vec![child],
        manifest_root: fixture.worker_root.clone(),
        tasks: std::iter::once((child, child_task(case))).collect(),
        hold: None,
        script: Box::new(move |call| match (call.marker, call.index) {
            (marker, 1) if marker == child => scripted_spawn_call(&serde_json::json!({
                "action": "spawn_batch", "mode": "process", "tasks": [
                    {"task": leaf, "allow_delegation": false}
                ]
            })),
            (marker, 2) if marker == child => scripted_join_call(call.body),
            (marker, _) if marker == child => text_reply("PRX-OPERATOR-CHILD-DONE"),
            _ => text_reply("PRX-WORKER-OK"),
        }),
    });
    let mut config = super::parent_config(&fixture, server.destination());
    case.mode.clone_into(&mut config.sessions_spawn.default_mode);
    super::save_parent_config(&config);

    let (parent, mut stdin) =
        spawn_held_parent(&fixture.config_dir, &fixture.home, &format!("/bg {}", child_task(case)));
    let joined = wait_for_child_join(&server, child);
    stdin.write_all(b"/exit\n").expect("test: parent exit line");
    drop(stdin);
    let output = parent.wait();
    let Some(joined) = joined else {
        panic!(
            "operator child join exceeded JOIN_DEADLINE; requests: {:#?}; events: {:#?}; stdout: {}; stderr: {}",
            request_trail(&server),
            event_trail(&fixture.parent_workspace),
            output.stdout,
            output.stderr
        );
    };
    output.assert_success(case.mode);
    server.stop();
    server.assert_no_failures();

    // The operator child launched exactly one real leaf worker, which
    // completed and was joined.
    assert_join_summary(&joined, 1);
    let leaf_manifest = server.manifest(leaf).expect("test: production leaf manifest");
    assert_ordinary_manifest(&leaf_manifest);
    assert_completion_journal(&fixture.parent_workspace, &leaf_manifest.manifest);

    // One identity: the manifest and the durable input record both name the
    // operator child's logical task run, never one of its segment runs.
    let parent_run = leaf_manifest
        .manifest
        .parent_run_id
        .clone()
        .expect("test: leaf parent run");
    assert!(
        !parent_run.starts_with("operator-segment:") && !parent_run.starts_with("operator-worker-segment:"),
        "a nested child names the logical operator task, not a segment: {parent_run}"
    );
    assert_eq!(
        input_record_parent_run(&leaf_manifest).as_deref(),
        Some(parent_run.as_str())
    );
    if process {
        let operator = server.manifest(child).expect("test: operator worker manifest");
        assert!(
            operator.manifest.operator_source.is_some(),
            "the /bg child is an operator worker"
        );
        assert_eq!(parent_run, operator.manifest.run_id);
    }
}

/// An operator task child (`/bg`, task mode) launches a nested process child.
#[test]
fn operator_task_child_launches_a_nested_process_child() {
    run_case(&Case {
        mode: "task",
        child: "PRX-OPTASK-CHILD",
        leaf: "PRX-OPTASK-LEAF",
    });
}

/// An operator process child (`/bg`, process mode) launches a nested process
/// child from its own worker.
#[test]
fn operator_process_child_launches_a_nested_process_child() {
    run_case(&Case {
        mode: "process",
        child: "PRX-OPPROC-CHILD",
        leaf: "PRX-OPPROC-LEAF",
    });
}
