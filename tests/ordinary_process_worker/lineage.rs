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
