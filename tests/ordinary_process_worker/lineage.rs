//! CF-R6: the sessions tools are direct on a chat root and in its whole
//! process lineage.
//!
//! A held `chat --plain` parent (no `always_include`) spawns a delegating
//! process child, which spawns a process grandchild, and a non-delegating
//! process sibling. The first
//! provider request of every turn carries its published `tools` array.

use super::parent::{ParentFixture, spawn_held_parent};
use super::provider::{
    OwnedProviderServer, ProviderRequest, ProviderSpec, WORKER_DEADLINE, scripted_join_call, scripted_spawn_call,
    text_reply,
};
use openprx::memory::Memory as _;
use std::io::Write as _;

const PARENT: &str = "PRX-LINEAGE-PARENT";
const KID: &str = "PRX-LINEAGE-KID";
const GRANDKID: &str = "PRX-LINEAGE-GRANDKID";
const LEAF: &str = "PRX-LINEAGE-LEAF";
const SESSIONS_TOOLS: [&str; 4] = ["sessions_spawn", "sessions_send", "sessions_list", "session_status"];

/// The tool names of the first request of `marker`.
fn first_request_tools(requests: &[ProviderRequest], marker: &str) -> Vec<String> {
    let request = requests
        .iter()
        .find(|request| request.marker == marker)
        .unwrap_or_else(|| panic!("no request for {marker}"));
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("test: request JSON");
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Poll until the parent's post-join reply request arrived (bounded).
fn wait_for_parent_reply(server: &OwnedProviderServer) -> bool {
    let deadline = std::time::Instant::now() + WORKER_DEADLINE;
    while std::time::Instant::now() < deadline {
        if server
            .requests()
            .iter()
            .filter(|request| request.marker == PARENT)
            .count()
            >= 3
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[test]
fn chat_process_child_and_grandchild_publish_sessions_tools() {
    let fixture = ParentFixture::new();
    let mut server = OwnedProviderServer::start(ProviderSpec {
        markers: vec![PARENT, GRANDKID, KID, LEAF],
        parents: vec![PARENT],
        manifest_root: fixture.worker_root.clone(),
        tasks: std::collections::BTreeMap::new(),
        hold: None,
        script: Box::new(|call| match (call.marker, call.index) {
            (PARENT, 1) => scripted_spawn_call(&serde_json::json!({
                "action": "spawn_batch", "mode": "process", "tasks": [
                    {"task": KID, "allow_delegation": true},
                    {"task": LEAF, "allow_delegation": false}
                ]
            })),
            (KID, 1) => scripted_spawn_call(&serde_json::json!({
                "action": "spawn_batch", "mode": "process", "tasks": [GRANDKID]
            })),
            (PARENT | KID, 2) => scripted_join_call(call.body),
            _ => text_reply("PRX-LINEAGE-DONE"),
        }),
    });
    let mut config = super::parent_config(&fixture, server.destination());
    config.tool_tiering.always_include.clear();
    super::save_parent_config(&config);

    let (parent, mut stdin) = spawn_held_parent(
        &fixture.config_dir,
        &fixture.home,
        // Each delegation level must strictly narrow its parent's set: naming
        // `file_read` gives the child {file_read, sessions_spawn}, so the
        // grandchild's {sessions_spawn} is still a strict subset.
        &format!("Use sessions_spawn with file_read for {PARENT}."),
    );
    let replied = wait_for_parent_reply(&server);
    stdin.write_all(b"/exit\n").expect("test: parent exit line");
    drop(stdin);
    let output = parent.wait();
    assert!(
        replied,
        "parent post-join reply exceeded WORKER_DEADLINE; stderr: {}",
        output.stderr
    );
    output.assert_success("lineage");
    server.stop();
    server.assert_no_failures();

    let requests = server.requests().clone();
    for marker in [PARENT, KID, GRANDKID] {
        let tools = first_request_tools(&requests, marker);
        for name in SESSIONS_TOOLS {
            assert!(
                tools.iter().any(|tool| tool == name),
                "{marker} first request lacks {name}: {tools:?}"
            );
        }
    }
    let leaf_tools = first_request_tools(&requests, LEAF);
    assert!(
        !leaf_tools.iter().any(|tool| tool == "sessions_spawn"),
        "a non-delegating child publishes no sessions_spawn: {leaf_tools:?}"
    );
    for marker in [KID, GRANDKID, LEAF] {
        let manifest = server.manifest(marker).expect("test: production worker manifest");
        let direct = manifest
            .manifest
            .direct_tools
            .unwrap_or_else(|| panic!("{marker} manifest carries no chat-lineage direct_tools"));
        for name in SESSIONS_TOOLS {
            assert!(
                direct.iter().any(|tool| tool == name),
                "{marker} direct_tools lacks {name}"
            );
        }
    }
}

/// Most parent-store events the key test reads back.
const MAX_STORE_EVENTS: usize = 1000;

/// A reader of the parent workspace scoped to `session_key`: it sees every
/// workspace-visible row plus that session's own rows.
fn store_principal(workspace: &std::path::Path, session_key: Option<&str>) -> openprx::memory::MemoryPrincipal {
    openprx::memory::MemoryPrincipal {
        workspace_id: workspace.to_string_lossy().into_owned(),
        session_key: session_key.map(str::to_owned),
        ..openprx::memory::MemoryPrincipal::default()
    }
}

/// `(subject run id, session key, payload)` of every stored `task.spawned`.
fn stored_task_spawns(workspace: &std::path::Path) -> Vec<(String, Option<String>, serde_json::Value)> {
    let memory = openprx::memory::SqliteMemory::new(workspace).expect("test: parent memory");
    let events = super::runtime()
        .block_on(memory.list_memory_events_recent(&store_principal(workspace, None), MAX_STORE_EVENTS))
        .expect("test: parent task events");
    assert!(
        events.len() < MAX_STORE_EVENTS,
        "test: the store exceeded the read bound"
    );
    events
        .into_iter()
        .filter(|event| event.event_type == "task.spawned")
        .map(|event| {
            let payload =
                serde_json::from_str(event.payload_json.as_deref().unwrap_or("{}")).expect("test: task payload JSON");
            (event.subject_id, event.session_key, payload)
        })
        .collect()
}

/// The session keys of the `sessions_spawn`-source message rows of `run`, as
/// a reader of `spawn:<run>` sees them.
fn stored_run_keys(workspace: &std::path::Path, run: &str) -> Vec<Option<String>> {
    let memory = openprx::memory::SqliteMemory::new(workspace).expect("test: parent memory");
    let own_key = format!("spawn:{run}");
    let events = super::runtime()
        .block_on(memory.list_message_events_recent(&store_principal(workspace, Some(&own_key)), MAX_STORE_EVENTS))
        .expect("test: parent message events");
    assert!(
        events.len() < MAX_STORE_EVENTS,
        "test: the store exceeded the read bound"
    );
    events
        .into_iter()
        .filter(|event| event.source == "sessions_spawn" && event.run_id.as_deref() == Some(run))
        .map(|event| event.session_key)
        .collect()
}

/// I1 and I4 through a real chat lineage: a process child whose input names a
/// tool outside its sealed manifest keeps `sessions_spawn` and spawns a
/// grandchild, and every spawned run records under its own `spawn:<id>` key
/// with its delegation classification on `task.spawned`.
#[test]
fn chat_process_child_naming_a_tool_spawns_a_grandchild_under_its_own_key() {
    const KID_TASK: &str = "PRX-LINEAGE-KID use sessions_spawn; read with file_read later";
    let fixture = ParentFixture::new();
    let mut server = OwnedProviderServer::start(ProviderSpec {
        markers: vec![PARENT, GRANDKID, KID],
        parents: vec![PARENT],
        manifest_root: fixture.worker_root.clone(),
        tasks: std::collections::BTreeMap::from([(KID, KID_TASK.to_owned())]),
        hold: None,
        script: Box::new(|call| match (call.marker, call.index) {
            (PARENT, 1) => scripted_spawn_call(&serde_json::json!({
                "action": "spawn_batch", "mode": "process", "tasks": [{"task": KID_TASK}]
            })),
            (KID, 1) => scripted_spawn_call(&serde_json::json!({
                "action": "spawn_batch", "mode": "process", "tasks": [GRANDKID]
            })),
            (PARENT | KID, 2) => scripted_join_call(call.body),
            _ => text_reply("PRX-LINEAGE-DONE"),
        }),
    });
    let mut config = super::parent_config(&fixture, server.destination());
    config.tool_tiering.always_include.clear();
    super::save_parent_config(&config);

    let (parent, mut stdin) = spawn_held_parent(
        &fixture.config_dir,
        &fixture.home,
        &format!("Use sessions_spawn for {PARENT}."),
    );
    let replied = wait_for_parent_reply(&server);
    stdin.write_all(b"/exit\n").expect("test: parent exit line");
    drop(stdin);
    let output = parent.wait();
    assert!(
        replied,
        "parent post-join reply exceeded WORKER_DEADLINE; stderr: {}",
        output.stderr
    );
    output.assert_success("lineage keys");
    server.stop();
    server.assert_no_failures();

    let requests = server.requests().clone();
    let kid_tools = first_request_tools(&requests, KID);
    assert!(
        kid_tools.iter().any(|tool| tool == "sessions_spawn"),
        "the naming child lists sessions_spawn: {kid_tools:?}"
    );
    assert!(
        requests.iter().any(|request| request.marker == GRANDKID),
        "the grandchild reached its model"
    );

    let spawns = stored_task_spawns(&fixture.parent_workspace);
    for (task, expected) in [(KID_TASK, Some("recursive")), (GRANDKID, None)] {
        let (run, key, payload) = spawns
            .iter()
            .find(|(_, _, payload)| payload["task"] == task)
            .unwrap_or_else(|| panic!("no task.spawned row for {task}: {spawns:?}"));
        let code = payload["child_delegation"].as_str();
        assert!(code.is_some_and(|code| !code.is_empty()), "{task}: {payload}");
        if let Some(expected) = expected {
            assert_eq!(code, Some(expected), "{task}: {payload}");
        }
        let own_key = format!("spawn:{run}");
        assert_eq!(key.as_deref(), Some(own_key.as_str()), "{task}: task.spawned key");
        let keys = stored_run_keys(&fixture.parent_workspace, run);
        assert!(
            !keys.is_empty() && keys.iter().all(|key| key.as_deref() == Some(own_key.as_str())),
            "{task}: every sessions_spawn row of the run uses {own_key}: {keys:?}"
        );
    }
}
