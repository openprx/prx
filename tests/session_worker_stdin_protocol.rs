#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::indexing_slicing
)]
//! Regression coverage for the **real** `prx session-worker` stdin protocol.
//!
//! Every other process-mode test in the repository substitutes `sh`/`sleep` for
//! the worker, so nothing exercised the binary's own entrypoint. These tests
//! drive `env!("CARGO_BIN_EXE_prx")` itself over real pipes and pin the four
//! properties the process-mode steering channel rests on:
//!
//! 1. The worker finishes even though the parent keeps the **stdin write end
//!    open** for the whole run. This is the single largest risk the steering
//!    design took on: the post-manifest control-frame reader must never be a
//!    `tokio::io::stdin` read on the blocking pool, because runtime drop joins
//!    that pool and the process would then wait for an EOF the parent
//!    deliberately never sends.
//! 2. Worker stdout stays **exactly one line of JSON** — the `WorkerResult` IPC
//!    contract asserted by `src/main.rs`'s "session-worker must stay
//!    stdout-clean for IPC JSON" comment.
//! 3. Authentication still gates the run: no parent capability in the
//!    environment means no run, regardless of what the manifest claims.
//! 4. A malformed manifest followed by control frames fails cleanly, still on
//!    exactly one stdout line.
//!
//! Natural worker exit while stdin remains open is the protocol assertion.
//! A fixture-only watchdog bounds regressions, and any forced cleanup fails;
//! it does not change the runtime's decision budgets. The negative control
//! uses a real EOF-waiting child and proves it remains alive until stdin closes.
//!
//! The baseline steering tests stop at config-generation validation. The
//! route-credential test below also runs sealed workers through a loopback
//! provider and checks the authorization headers observed on the wire.

use base64::Engine as _;
use openprx::memory::{Memory as _, MemoryEventRecording};
use openprx::session_worker::protocol::{WorkerControlFrame, WorkerManifest, WorkerResult};
use ring::hmac;
use sha2::{Digest, Sha256};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Output, Stdio};

const RECURSIVE_CHILD_TASK: &str = "Use sessions_spawn in process mode for PRX-NESTED-CHILD";
const FANOUT_CHILD_TASK: &str = "Use sessions_spawn in process mode for PRX-FANOUT-CHILD";

/// Exercise the real worker executable with a sealed route and an isolated
/// loopback provider. The listener records headers from the OS child, while
/// HOME, config, state, and every credential remain inside this fixture.
#[test]
fn sealed_route_worker_sends_only_its_route_key_on_wire() {
    use openprx::providers::credentials::{CredentialRef, LegacySlot};
    use parking_lot::Mutex;
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (_root, config_dir, workspace_dir, home) = worker_dirs();
    let parent_workspace = config_dir.join("workspace");
    let worker_workspace_root = workspace_dir.join("workers");
    std::fs::create_dir_all(&worker_workspace_root).expect("worker workspace root");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    listener.set_nonblocking(true).expect("nonblocking loopback");
    let destination = format!("custom:http://{}/v1", listener.local_addr().expect("loopback address"));
    let seen = Arc::new(Mutex::new(Vec::<ProviderRequest>::new()));
    let captured = Arc::new(Mutex::new(BTreeMap::<String, WorkerManifest>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let server_seen = Arc::clone(&seen);
    let server_captured = Arc::clone(&captured);
    let server_stop = Arc::clone(&stop);
    let server_workspace_root = worker_workspace_root.clone();
    let server = std::thread::spawn(move || {
        while !server_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                        .expect("loopback read timeout");
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let head_end = loop {
                        let Ok(read) = stream.read(&mut chunk) else { break None };
                        if read == 0 {
                            break None;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(offset) = buffer.windows(4).position(|part| part == b"\r\n\r\n") {
                            break Some(offset + 4);
                        }
                        if buffer.len() > 1_048_576 {
                            break None;
                        }
                    };
                    let Some(head_end) = head_end else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    while buffer.len().saturating_sub(head_end) < content_length {
                        let Ok(read) = stream.read(&mut chunk) else {
                            break;
                        };
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                    let request_body = String::from_utf8_lossy(&buffer[head_end..]).to_string();
                    let marker = [
                        "PRX-ROUTE-PARENT",
                        "PRX-AGENT-PARENT",
                        "PRX-ROUTE-CHILD",
                        "PRX-AGENT-CHILD",
                        "PRX-DENIED-PARENT",
                        "PRX-DENIED-CHILD",
                        "PRX-NESTED-PARENT",
                        "PRX-NESTED-CHILD",
                        "PRX-NESTED-LEAF",
                        "PRX-FANOUT-PARENT",
                        "PRX-FANOUT-CHILD",
                        "PRX-FANOUT-LEAF",
                        "PRX-FANOUT-DENIED-A",
                        "PRX-FANOUT-DENIED-B",
                        "PRX-FANOUT-SECOND",
                    ]
                    .into_iter()
                    .find(|candidate| request_body.contains(candidate))
                    .expect("fixture request marker");
                    if !matches!(
                        marker,
                        "PRX-ROUTE-PARENT"
                            | "PRX-AGENT-PARENT"
                            | "PRX-DENIED-PARENT"
                            | "PRX-NESTED-PARENT"
                            | "PRX-FANOUT-PARENT"
                    ) {
                        let mut manifests = server_captured.lock();
                        if !manifests.contains_key(marker) {
                            manifests.insert(
                                marker.to_string(),
                                capture_live_worker_manifest(
                                    &server_workspace_root,
                                    if marker == "PRX-NESTED-CHILD" {
                                        RECURSIVE_CHILD_TASK
                                    } else if marker == "PRX-FANOUT-CHILD" {
                                        FANOUT_CHILD_TASK
                                    } else {
                                        marker
                                    },
                                ),
                            );
                        }
                    }
                    let request_index = {
                        let mut requests = server_seen.lock();
                        requests.push(ProviderRequest {
                            marker: marker.to_string(),
                            head: head.clone(),
                            body: request_body.clone(),
                        });
                        requests.iter().filter(|request| request.marker == marker).count()
                    };
                    let assistant = match (marker, request_index) {
                        ("PRX-ROUTE-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "tasks": ["PRX-ROUTE-CHILD"]
                        })),
                        ("PRX-AGENT-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "tasks": [
                                {"task": "PRX-AGENT-CHILD", "agent": "prx_agent"}
                            ]
                        })),
                        ("PRX-DENIED-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "allow_delegation": true,
                            "tasks": ["PRX-DENIED-CHILD"]
                        })),
                        ("PRX-NESTED-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "tasks": [
                                {"task": RECURSIVE_CHILD_TASK, "allow_delegation": true}
                            ]
                        })),
                        ("PRX-NESTED-CHILD", 1) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "tasks": ["PRX-NESTED-LEAF"]
                        })),
                        ("PRX-FANOUT-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "tasks": [
                                {"task": FANOUT_CHILD_TASK, "allow_delegation": true}
                            ]
                        })),
                        ("PRX-FANOUT-CHILD", 1) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process",
                            "tasks": ["PRX-FANOUT-DENIED-A", "PRX-FANOUT-DENIED-B"]
                        })),
                        ("PRX-FANOUT-CHILD", 2) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "tasks": ["PRX-FANOUT-LEAF"]
                        })),
                        ("PRX-FANOUT-CHILD", 3) | ("PRX-FANOUT-PARENT", 2) => scripted_join_call(&request_body),
                        ("PRX-FANOUT-CHILD", 4) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "tasks": ["PRX-FANOUT-SECOND"]
                        })),
                        ("PRX-ROUTE-PARENT" | "PRX-AGENT-PARENT" | "PRX-NESTED-PARENT" | "PRX-NESTED-CHILD", 2) => {
                            scripted_join_call(&request_body)
                        }
                        _ => serde_json::json!({"role": "assistant", "content": "PRX-WORKER-OK"}),
                    };
                    let body = serde_json::json!({
                        "id": "chatcmpl-prx-worker",
                        "object": "chat.completion",
                        "created": 1,
                        "model": "probe-model",
                        "choices": [{"index": 0, "message": assistant, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                    .to_string();
                    let (content_type, body) = if request_body.contains("\"stream\":true") {
                        let (delta, finish_reason) = assistant.get("tool_calls").map_or_else(
                            || (serde_json::json!({"content": assistant["content"]}), "stop"),
                            |calls| {
                                let mut calls = calls.as_array().expect("tool calls array").clone();
                                for (index, call) in calls.iter_mut().enumerate() {
                                    call["index"] = serde_json::json!(index);
                                }
                                (serde_json::json!({"tool_calls": calls}), "tool_calls")
                            },
                        );
                        let chunk = serde_json::json!({
                            "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
                        });
                        let finish = serde_json::json!({
                            "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]
                        });
                        (
                            "text/event-stream",
                            format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
                        )
                    } else {
                        ("application/json", body)
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).expect("loopback reply");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("loopback accept: {error}"),
            }
        }
    });

    let server = OwnedProviderServer {
        stop: Arc::clone(&stop),
        thread: Some(server),
    };
    let mut config = openprx::config::Config {
        config_path: config_dir.join("config.toml"),
        workspace_dir,
        default_provider: Some(destination.clone()),
        default_model: Some("hint:fast".to_string()),
        api_key: Some("PRX-WORKER-ROOT-KEY".to_string()),
        ..openprx::config::Config::default()
    };
    config.secrets.encrypt = false;
    config.sessions_spawn.process_memory_strategy = "isolated_private".to_string();
    config.sessions_spawn.worker_workspace_root = Some(worker_workspace_root.to_string_lossy().into_owned());
    config.sessions_spawn.cleanup_on_complete = false;
    config.tool_tiering.always_include.push("sessions_spawn".to_string());
    config.model_routes.push(openprx::config::ModelRouteConfig {
        settings: openprx::config::model_settings::ModelSettings::default(),
        hint: "fast".to_string(),
        provider: destination.clone(),
        model: "probe-model".to_string(),
        api_key: Some("PRX-WORKER-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    add_provider_fixture_model(&mut config, &destination, "probe-model");
    write_worker_config(&config);

    let route_output = run_model_parent(&config_dir, &home, "PRX-ROUTE-PARENT");
    assert!(
        route_output.status.success(),
        "route parent stderr: {}",
        String::from_utf8_lossy(&route_output.stderr)
    );
    assert!(String::from_utf8_lossy(&route_output.stdout).contains("PRX-WORKER-OK"));
    assert_parent_join(&seen.lock(), "PRX-ROUTE-PARENT", 1);
    let mut manifest = captured
        .lock()
        .get("PRX-ROUTE-CHILD")
        .cloned()
        .expect("production route manifest");
    assert_authenticated_source(&manifest);
    assert_sealed_child(&manifest, &seen.lock(), "PRX-ROUTE-CHILD");
    assert_completion_journal(&parent_workspace, &manifest);
    let served_requests = seen.lock().len();
    assert!(
        seen.lock()
            .iter()
            .filter(|request| request.marker == "PRX-ROUTE-CHILD")
            .all(|request| request.head.contains("PRX-WORKER-ROUTE-KEY"))
    );

    // Replay the genuine production-issued source with only the selected
    // credential changed. A fresh outer HMAC cannot make that mutation match
    // the authenticated model selection carried by the inner source.
    assert_eq!(manifest.route_hint.as_deref(), Some("fast"));
    assert_eq!(
        manifest.route_credential,
        Some(CredentialRef::LegacyInline(LegacySlot::ModelRoute("fast".to_string())))
    );
    manifest.api_key = Some("PRX-WORKER-ROOT-KEY".to_string());
    manifest.parent_capability = None;
    let sealed_config_dir = manifest.config_dir.clone();
    let expiry = far_future_expiry();
    let bad_capability = worker_capability(&manifest, expiry);
    manifest.parent_capability = Some(bad_capability.clone());
    let mut bad_child = spawn_worker(&sealed_config_dir, &home, Some((&bad_capability, expiry)));
    let mut bad_stdin = take_stdin(&mut bad_child);
    write_production_manifest_line(
        &mut bad_stdin,
        &serde_json::to_string(&manifest).expect("tampered manifest JSON"),
    );
    drop(bad_stdin);
    let bad_run = finish(bad_child, None);
    let bad_result = assert_single_json_line(&bad_run.stdout);
    assert!(!bad_result.success, "wrong sealed key must be refused");
    assert_eq!(
        bad_result.error.as_deref(),
        Some("worker provider or credential provenance differs from issued selection")
    );
    assert_eq!(seen.lock().len(), served_requests);

    // A second worker inherits a named agent's profile on a same-provider
    // route with no route key. Its execution identity stays unset: the sealed
    // credential reference, not `agent_id`, is the source of authority.
    let profile = openprx::config::manage::credentials::CredentialReferences::for_config(&config)
        .expect("isolated credential store")
        .set(&destination, "prx-agent-route", "PRX-WORKER-AGENT-KEY")
        .expect("store isolated agent credential");
    config.model_routes.push(openprx::config::ModelRouteConfig {
        settings: openprx::config::model_settings::ModelSettings::default(),
        hint: "agent".to_string(),
        provider: destination.clone(),
        model: "agent-model".to_string(),
        api_key: None,
        credential_ref: None,
    });
    add_provider_fixture_model(&mut config, &destination, "agent-model");
    config.agents.insert(
        "prx_agent".to_string(),
        toml::from_str(&format!(
            "provider = \"{destination}\"\nmodel = \"hint:agent\"\ncredential_ref = \"{}\"\n",
            profile.id
        ))
        .expect("agent config"),
    );
    write_worker_config(&config);
    let agent_output = run_model_parent(&config_dir, &home, "PRX-AGENT-PARENT");
    assert!(
        agent_output.status.success(),
        "agent parent stderr: {}",
        String::from_utf8_lossy(&agent_output.stderr)
    );
    assert_parent_join(&seen.lock(), "PRX-AGENT-PARENT", 1);
    let agent_manifest = captured
        .lock()
        .get("PRX-AGENT-CHILD")
        .cloned()
        .expect("production agent manifest");
    assert_authenticated_source(&agent_manifest);
    assert_sealed_child(&agent_manifest, &seen.lock(), "PRX-AGENT-CHILD");
    assert_eq!(
        agent_manifest.route_credential,
        Some(CredentialRef::Profile { id: profile.id })
    );
    assert_completion_journal(&parent_workspace, &agent_manifest);

    // Default-off refusal uses the same real parent and spawn request. It
    // cannot be satisfied by a fixture that never reaches the spawn backend.
    let denied_output = run_model_parent(&config_dir, &home, "PRX-DENIED-PARENT");
    assert!(denied_output.status.success(), "denied parent exits cleanly");
    {
        let requests = seen.lock();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.marker == "PRX-DENIED-PARENT")
                .count(),
            2
        );
        assert!(requests.iter().any(|request| {
            request.marker == "PRX-DENIED-PARENT"
                && request
                    .body
                    .contains("recursive child delegation is disabled by operator configuration")
        }));
        assert!(requests.iter().all(|request| request.marker != "PRX-DENIED-CHILD"));
    }

    // Q88's positive phase now requires actual opt-in and real admitted workers.
    let mut recursive_config = config.clone();
    recursive_config.agents.clear();
    recursive_config.sessions_spawn.allow_recursive_delegation = true;
    assert_eq!(recursive_config.sessions_spawn.max_recursive_depth, 1);
    write_worker_config(&recursive_config);
    let nested_output = run_model_parent(&config_dir, &home, "PRX-NESTED-PARENT");
    assert!(
        nested_output.status.success(),
        "nested parent stderr: {}",
        String::from_utf8_lossy(&nested_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&nested_output.stdout).contains("PRX-WORKER-OK"),
        "nested parent stdout: {}",
        String::from_utf8_lossy(&nested_output.stdout)
    );
    {
        let requests = seen.lock();
        assert_parent_join(&requests, "PRX-NESTED-PARENT", 1);
        assert_parent_join(&requests, "PRX-NESTED-CHILD", 1);
    }
    let nested = captured
        .lock()
        .get("PRX-NESTED-CHILD")
        .cloned()
        .expect("actual nested worker manifest");
    let leaf = captured
        .lock()
        .get("PRX-NESTED-LEAF")
        .cloned()
        .expect("actual grandchild manifest");
    assert_authenticated_source(&nested);
    assert_authenticated_source(&leaf);
    let scope = nested
        .capability_manifest
        .as_ref()
        .expect("sealed recursive child scope");
    let parent = &nested
        .child_source
        .as_ref()
        .expect("admitted recursive source")
        .parent_constraints;
    assert_eq!(scope.allowed.len(), 1);
    assert!(
        scope
            .allowed
            .iter()
            .all(|reference| reference.id.ends_with(":sessions_spawn"))
    );
    assert!(scope.allowed.len() < parent.allowed.len());
    assert!(
        scope
            .allowed
            .iter()
            .all(|reference| parent.allowed.iter().any(|allowed| {
                allowed.id == reference.id
                    && allowed.fingerprint == reference.fingerprint
                    && allowed.generation == reference.generation
            }))
    );
    assert_eq!(scope.remaining_depth, 1);
    assert!(scope.max_cost_cents <= parent.max_cost_cents);
    let pins = scope
        .delegation
        .as_ref()
        .expect("explicit delegation recorded under seal");
    assert!(pins.requested);
    assert_eq!(
        openprx::providers::ProviderId::of(&pins.provider),
        openprx::providers::ProviderId::of(&destination)
    );
    assert_eq!(pins.model, "probe-model");
    assert_eq!(leaf.parent_run_id.as_deref(), Some(nested.run_id.as_str()));
    assert_eq!(leaf.owner_id, nested.owner_id);
    let leaf_scope = leaf.capability_manifest.as_ref().expect("sealed leaf scope");
    assert!(leaf_scope.allowed.is_empty());
    assert_eq!(leaf_scope.remaining_depth, 0);
    let leaf_pins = leaf_scope
        .delegation
        .as_ref()
        .expect("leaf retains inherited model pins");
    assert!(!leaf_pins.requested);
    assert_eq!(leaf_pins.provider, pins.provider);
    assert_eq!(leaf_pins.model, pins.model);
    assert!(leaf_scope.max_cost_cents <= scope.max_cost_cents);
    {
        let requests = seen.lock();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.marker == "PRX-NESTED-LEAF")
                .count(),
            1
        );
        assert!(
            requests
                .iter()
                .filter(|request| request.marker == "PRX-NESTED-CHILD")
                .all(|request| {
                    request.head.contains("PRX-WORKER-ROUTE-KEY") && request.body.contains("probe-model")
                })
        );
        assert!(
            requests
                .iter()
                .filter(|request| request.marker == "PRX-NESTED-LEAF")
                .all(|request| {
                    request.head.contains("PRX-WORKER-ROUTE-KEY") && request.body.contains("probe-model")
                })
        );
        assert_sealed_child(&leaf, &requests, "PRX-NESTED-LEAF");
    }
    assert_completion_journal(&parent_workspace, &nested);
    assert_completion_journal(&parent_workspace, &leaf);
    assert_delegation_input_record(&parent_workspace, &nested);
    assert_delegation_input_record(&parent_workspace, &leaf);
    let requests_before_wrong_parent = seen.lock().len();
    let mut wrong_parent = leaf.clone();
    wrong_parent.parent_run_id = Some("prx_unrelated_parent".to_owned());
    wrong_parent.parent_capability = None;
    let wrong_parent_capability = worker_capability(&wrong_parent, expiry);
    wrong_parent.parent_capability = Some(wrong_parent_capability.clone());
    let mut wrong_parent_child = spawn_worker(
        &wrong_parent.config_dir,
        &home,
        Some((&wrong_parent_capability, expiry)),
    );
    let mut wrong_parent_stdin = take_stdin(&mut wrong_parent_child);
    write_production_manifest_line(
        &mut wrong_parent_stdin,
        &serde_json::to_string(&wrong_parent).expect("wrong-parent manifest JSON"),
    );
    drop(wrong_parent_stdin);
    let wrong_parent_run = finish(wrong_parent_child, None);
    let wrong_parent_result = assert_single_json_line(&wrong_parent_run.stdout);
    assert!(wrong_parent_run.status.success());
    assert!(!wrong_parent_result.success);
    assert_eq!(
        wrong_parent_result.error.as_deref(),
        Some("session-worker parent decision differs from issued authority")
    );
    assert_eq!(seen.lock().len(), requests_before_wrong_parent);
    // Finding94: retain every original phase above, then exercise a new real
    // recursive worker's fanout and reissue attempts. A refused two-leaf batch
    // consumes no slot; one valid leaf can run, and another turn cannot mint
    // a second copy of that same subtree allowance.
    let fanout_config_dir = config_dir
        .parent()
        .expect("fixture config parent")
        .join("fanout-config");
    std::fs::create_dir_all(&fanout_config_dir).expect("isolated fanout config");
    let fanout_workspace = fanout_config_dir.join("workspace");
    let mut fanout_config = recursive_config.clone();
    fanout_config.config_path = fanout_config_dir.join("config.toml");
    fanout_config.workspace_dir = fanout_workspace.clone();
    fanout_config.cost.prices.insert(
        format!("{destination}/probe-model"),
        openprx::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    write_worker_config(&fanout_config);
    let fanout_output = run_model_parent(&fanout_config_dir, &home, "PRX-FANOUT-PARENT");
    assert!(
        fanout_output.status.success(),
        "fanout parent stderr: {}",
        String::from_utf8_lossy(&fanout_output.stderr)
    );
    assert!(String::from_utf8_lossy(&fanout_output.stdout).contains("PRX-WORKER-OK"));
    {
        let requests = seen.lock();
        assert_parent_join(&requests, "PRX-FANOUT-PARENT", 1);
        let child: Vec<_> = requests
            .iter()
            .filter(|request| request.marker == "PRX-FANOUT-CHILD")
            .collect();
        assert_eq!(
            child.len(),
            5,
            "all refusal, successful leaf and reissue phases must reach the model"
        );
        assert_join_summary(child[3], 1);
        assert!(child[1].body.contains("at most one child"));
        assert!(child[4].body.contains("already allocated its one child allowance"));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.marker == "PRX-FANOUT-LEAF")
                .count(),
            1
        );
        assert!(
            requests.iter().all(|request| !matches!(
                request.marker.as_str(),
                "PRX-FANOUT-DENIED-A" | "PRX-FANOUT-DENIED-B" | "PRX-FANOUT-SECOND"
            )),
            "refused fanout and reissue must not dispatch a leaf"
        );
    }
    let manifests = captured.lock();
    let fanout = manifests.get("PRX-FANOUT-CHILD").expect("real recursive worker");
    let only_leaf = manifests.get("PRX-FANOUT-LEAF").expect("one real leaf worker");
    assert_authenticated_source(fanout);
    assert_authenticated_source(only_leaf);
    let allowance = fanout
        .capability_manifest
        .as_ref()
        .expect("recursive allowance")
        .max_cost_cents;
    assert!(allowance > 0, "the fanout limit must protect a nonzero allowance");
    assert!(
        only_leaf
            .capability_manifest
            .as_ref()
            .expect("leaf allowance")
            .max_cost_cents
            <= allowance
    );
    assert_eq!(only_leaf.parent_run_id.as_deref(), Some(fanout.run_id.as_str()));
    assert_completion_journal(&fanout_workspace, fanout);
    assert_completion_journal(&fanout_workspace, only_leaf);
    drop(manifests);
    write_worker_config(&config);
    let agent_requests = seen.lock().len();

    // Keep the physical refusal controls from the original contract. They
    // replay a genuine production-issued source and change only the field the
    // refusal names; neither direct invocation may reach the provider.
    let requests_before_missing_source = seen.lock().len();
    let mut missing_source_manifest = agent_manifest;
    missing_source_manifest.route_source_provider = None;
    missing_source_manifest.parent_capability = None;
    let missing_source_capability = worker_capability(&missing_source_manifest, expiry);
    missing_source_manifest.parent_capability = Some(missing_source_capability.clone());
    let mut missing_source_child = spawn_worker(
        &missing_source_manifest.config_dir,
        &home,
        Some((&missing_source_capability, expiry)),
    );
    let mut missing_source_stdin = take_stdin(&mut missing_source_child);
    write_production_manifest_line(
        &mut missing_source_stdin,
        &serde_json::to_string(&missing_source_manifest).expect("missing-source manifest JSON"),
    );
    drop(missing_source_stdin);
    let missing_source_run = finish(missing_source_child, None);
    let missing_source_result = assert_single_json_line(&missing_source_run.stdout);
    stop.store(true, Ordering::Relaxed);
    server.finish();
    let requests = seen.lock();
    assert!(
        bad_run.status.success(),
        "refused worker exits cleanly: {:?}",
        bad_run.status
    );
    assert!(
        missing_source_run.status.success(),
        "refused missing-source worker exits cleanly"
    );
    assert!(!missing_source_result.success, "missing provenance must be refused");
    assert_eq!(
        missing_source_result.error.as_deref(),
        Some("worker provider or credential provenance differs from issued selection")
    );
    assert_eq!(
        requests.len(),
        requests_before_missing_source,
        "missing source caused outbound I/O"
    );
    assert!(requests.len() > served_requests, "agent worker must call the route");
    assert!(
        requests
            .iter()
            .filter(|request| request.marker == "PRX-ROUTE-CHILD")
            .all(|request| request.head.contains("PRX-WORKER-ROUTE-KEY"))
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.marker == "PRX-ROUTE-CHILD")
            .all(|request| request.body.contains("probe-model")),
        "route child request used the wrong model"
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.head.contains("PRX-WORKER-ROOT-KEY"))
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.marker == "PRX-AGENT-CHILD")
            .all(|request| request.head.contains("PRX-WORKER-AGENT-KEY"))
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.marker == "PRX-AGENT-CHILD")
            .all(|request| request.body.contains("agent-model")),
        "named-agent child request used the wrong model"
    );
    assert!(agent_requests > served_requests);
}

struct ProviderRequest {
    marker: String,
    head: String,
    body: String,
}

struct OwnedProviderServer {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OwnedProviderServer {
    fn finish(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("loopback server joins");
        }
    }
}

impl Drop for OwnedProviderServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Preserve an original fixture panic while confirming server exit.
            let _ = thread.join();
        }
    }
}

fn assert_delegation_input_record(workspace: &Path, manifest: &WorkerManifest) {
    let source = manifest.child_source.as_ref().expect("admitted child source");
    let scope = manifest.capability_manifest.as_ref().expect("sealed child scope");
    let pins = scope.delegation.as_ref().expect("sealed model pins");
    let memory = openprx::memory::SqliteMemory::new(workspace).expect("parent admission memory");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let event = runtime
        .block_on(memory.find_owned_message_event(
            &source.parent_constraints.workspace,
            manifest.owner_id.as_deref().expect("child owner"),
            &source.input_event_id,
        ))
        .unwrap()
        .expect("durable child admission record");
    let payload: serde_json::Value = serde_json::from_str(event.raw_payload_json.as_deref().unwrap()).unwrap();
    let delegation = &payload["delegation"];
    assert_eq!(delegation["requested"], pins.requested);
    assert_eq!(delegation["remaining_depth"], scope.remaining_depth);
    assert_eq!(delegation["max_cost_cents"], scope.max_cost_cents);
    assert_eq!(delegation["model"], pins.model);
    assert_eq!(
        delegation["allowed_tools"].as_array().map(Vec::len),
        Some(scope.allowed.len())
    );
}

fn capture_live_worker_manifest(worker_workspace_root: &Path, task: &str) -> WorkerManifest {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(runs) = std::fs::read_dir(worker_workspace_root) {
            for run in runs.flatten() {
                let Ok(file_type) = run.file_type() else { continue };
                if !file_type.is_dir() {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(run.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if !name.to_string_lossy().starts_with(".prx-worker-") {
                        continue;
                    }
                    let path = entry.path().join("manifest.json");
                    let Ok(bytes) = std::fs::read(path) else { continue };
                    let Ok(manifest) = serde_json::from_slice::<WorkerManifest>(&bytes) else {
                        continue;
                    };
                    if manifest.task == task {
                        return manifest;
                    }
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "live production manifest for {task} was not found"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn add_provider_fixture_model(config: &mut openprx::config::Config, provider: &str, model: &str) {
    config.cost.prices.insert(
        format!("{provider}/{model}"),
        openprx::config::schema::ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    if !config
        .providers
        .models
        .iter()
        .any(|entry| entry.provider == provider && entry.model.as_deref() == Some(model))
    {
        config
            .providers
            .models
            .push(openprx::config::schema::ProviderModelConfig {
                provider: provider.to_string(),
                model: Some(model.to_string()),
                operations: Some(vec![openprx::config::schema::ModelOperationSetting::Chat]),
                native_tools: Some(true),
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: None,
                tool_call_dialect: None,
                context_window: None,
            });
    }
}

fn write_worker_config(config: &openprx::config::Config) {
    std::fs::write(
        &config.config_path,
        toml::to_string_pretty(config).expect("serialize worker config"),
    )
    .expect("write worker config");
}

fn run_model_parent(config_dir: &Path, home: &Path, message: &str) -> Output {
    let xdg_data = home.join("xdg-data");
    let xdg_cache = home.join("xdg-cache");
    let xdg_state = home.join("xdg-state");
    let xdg_config = home.join("xdg-config");
    for path in [&xdg_data, &xdg_cache, &xdg_state, &xdg_config] {
        std::fs::create_dir_all(path).expect("isolated parent directory");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(config_dir)
        .arg("chat")
        .arg("--plain")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_DATA_HOME", xdg_data)
        .env("XDG_CACHE_HOME", xdg_cache)
        .env("XDG_STATE_HOME", xdg_state)
        .env("XDG_CONFIG_HOME", xdg_config)
        .env("PRX_TUI", "0")
        .env("NO_COLOR", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("RUST_LOG", "off")
        .env("SESSION_WORKER_SECRET", TEST_SECRET)
        .env("OPENPRX_SESSION_WORKER_SECRET_PATH", home.join("session_worker.secret"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("model parent binary");
    child
        .stdin
        .take()
        .expect("model parent stdin")
        .write_all(format!("Use sessions_spawn in process mode for {message}.\n/exit\n").as_bytes())
        .expect("model parent script");
    child.wait_with_output().expect("model parent output")
}

fn assert_authenticated_source(manifest: &WorkerManifest) {
    assert_eq!(
        usize::from(manifest.child_source.is_some())
            + usize::from(manifest.ordinary_source.is_some())
            + usize::from(manifest.operator_source.is_some()),
        1,
        "production parent must issue exactly one authenticated source"
    );
}

fn assert_sealed_child(manifest: &WorkerManifest, requests: &[ProviderRequest], marker: &str) {
    assert!(
        manifest.allowed_tools.is_empty(),
        "sealed child widened its explicit tool allowlist"
    );
    let request = requests
        .iter()
        .find(|request| request.marker == marker)
        .expect("sealed child provider request");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("sealed child request JSON");
    assert!(
        body["tools"].as_array().is_none_or(|tools| tools
            .iter()
            .all(|tool| tool.pointer("/function/name").and_then(serde_json::Value::as_str) != Some("sessions_spawn"))),
        "sealed child published sessions_spawn"
    );
}

fn assert_completion_journal(workspace: &Path, manifest: &WorkerManifest) {
    let owner = manifest.owner_id.as_deref().expect("worker owner");
    let (workspace_id, key, event_type, evidence) = match (
        &manifest.child_source,
        &manifest.ordinary_source,
        &manifest.operator_source,
    ) {
        (Some(source), None, None) => (
            source.parent_constraints.workspace.as_str(),
            format!("worker:{}:completion", manifest.run_id),
            "worker.completion.confirmed",
            "granted",
        ),
        (None, Some(source), None) => (
            source.parent_constraints.workspace.as_str(),
            format!("worker:{}:ordinary-completion", manifest.run_id),
            "worker.ordinary.completion.confirmed",
            "ordinary",
        ),
        (None, None, Some(source)) => (
            source.publication.workspace.as_str(),
            format!("worker:{}:operator-completion", manifest.run_id),
            "worker.operator.completion.confirmed",
            "operator",
        ),
        _ => panic!("completion journal requires exactly one authenticated source"),
    };
    assert_eq!(Path::new(workspace_id), workspace, "journal workspace lineage");
    let memory = openprx::memory::SqliteMemory::new(workspace).expect("open parent completion memory");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("completion journal runtime");
    let event = runtime
        .block_on(memory.find_owned_message_event_by_idempotency_key(workspace_id, owner, &key))
        .expect("completion journal lookup")
        .expect("completion journal");
    assert_eq!(event.event_type, event_type);
    assert_eq!(event.run_id.as_deref(), Some(manifest.run_id.as_str()));
    let result: WorkerResult = serde_json::from_str(event.raw_payload_json.as_deref().expect("completion payload"))
        .expect("completion result");
    assert!(result.success, "{evidence} completion result: {:?}", result.error);
    let evidence_run_id = result
        .completion_evidence
        .as_ref()
        .map(|value| value.run_id.as_str())
        .or_else(|| result.ordinary_completion.as_ref().map(|value| value.run_id.as_str()))
        .or_else(|| result.operator_completion.as_ref().map(|value| value.run_id.as_str()));
    assert_eq!(evidence_run_id, Some(manifest.run_id.as_str()));
}

fn assert_parent_join(requests: &[ProviderRequest], marker: &str, expected_total: usize) {
    let post_join = requests
        .iter()
        .filter(|request| request.marker == marker)
        .nth(2)
        .expect("parent post-join request");
    assert_join_summary(post_join, expected_total);
}

fn assert_join_summary(post_join: &ProviderRequest, expected_total: usize) {
    let request: serde_json::Value = serde_json::from_str(&post_join.body).expect("parent post-join JSON");
    let summary = request["messages"]
        .as_array()
        .expect("parent messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .and_then(|content| serde_json::from_str::<serde_json::Value>(content).ok())
        .expect("parent join summary");
    assert_eq!(summary["total"], expected_total);
    assert_eq!(
        summary["completed"].as_array().map(Vec::len),
        Some(expected_total),
        "{summary}"
    );
    assert_eq!(summary["failed"], serde_json::json!([]));
    assert_eq!(summary["killed"], serde_json::json!([]));
    assert_eq!(summary["no_result"], serde_json::json!([]));
}

fn scripted_spawn_call(arguments: &serde_json::Value) -> serde_json::Value {
    static NEXT_CALL_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    let call_id = format!(
        "call-prx-spawn-{}",
        NEXT_CALL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    serde_json::json!({
        "role": "assistant", "content": null,
        "tool_calls": [{
            "id": call_id, "type": "function",
            "function": {"name": "sessions_spawn", "arguments": arguments.to_string()}
        }]
    })
}

fn scripted_join_call(request_body: &str) -> serde_json::Value {
    let request: serde_json::Value = serde_json::from_str(request_body).expect("provider request JSON");
    let batch = request["messages"]
        .as_array()
        .expect("provider request messages")
        .iter()
        .rev()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .filter_map(|content| serde_json::from_str::<serde_json::Value>(content).ok())
        .find(|result| result["batch_id"].is_string())
        .unwrap_or_else(|| {
            let results: Vec<_> = request["messages"]
                .as_array()
                .expect("provider messages")
                .iter()
                .rev()
                .filter(|message| message["role"] == "tool")
                .take(3)
                .map(|message| message["content"].to_string().chars().take(2000).collect::<String>())
                .collect();
            panic!("spawn batch tool result in parent request; actual tool results={results:?}")
        });
    let batch_id = batch["batch_id"].as_str().expect("actual production batch id");
    scripted_spawn_call(&serde_json::json!({"action": "join", "batch_id": batch_id}))
}

/// Shared HMAC secret handed to the worker through `SESSION_WORKER_SECRET`, so
/// the test can mint a capability the worker will accept without reaching for
/// any state the developer's machine owns.
const TEST_SECRET: &str = "t6-session-worker-stdin-protocol-secret";

// The first check the worker performs *after* authentication has passed and
// the stdin control-frame reader has been started. Seeing this error is the
// proof that the reader thread exists in the run under test.

struct WorkerRun {
    status: std::process::ExitStatus,
    stdout: String,
}

/// Build a manifest that authenticates but cannot run: its config directory is
/// real (so the CLI/manifest cross-check passes) but holds no `config.toml`, so
/// the run stops at `config_source_generation`.
fn manifest_for(config_dir: &Path, workspace_dir: &Path) -> WorkerManifest {
    WorkerManifest {
        parent_capability: None,
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "t6-run".to_string(),
        task: "noop".to_string(),
        provider_name: "mock".to_string(),
        model: "mock".to_string(),
        api_key: None,
        route_hint: None,
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.7,
        temperature_override: None,
        config_dir: config_dir.to_path_buf(),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: None,
        runtime_config_source_revision: None,
        workspace_dir: workspace_dir.to_path_buf(),
        memory_db_path: workspace_dir.join("brain.db"),
        memory_workspace_id: Some(workspace_dir.to_string_lossy().to_string()),
        memory_strategy: Some("shared_fabric".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(workspace_dir.join("brain.db")),
        worker_memory_db_path: Some(workspace_dir.join("worker.db")),
        agent_id: None,
        persona_id: None,
        memory_event_recording: MemoryEventRecording::default(),
        allowed_tools: vec!["file_read".to_string()],
        channel_excluded_tools: Vec::new(),
        system_prompt: None,
        identity_dir: None,
        scope_sender: None,
        scope_channel: None,
        scope_chat_type: None,
        scope_chat_id: None,
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
        spawn_depth: 0,
        session_scope_key: "sessions_spawn:global".to_string(),
        parent_run_id: None,
        compaction_config: None,
    }
}

/// Recompute the sealed capability the worker will verify:
/// `base64url(HMAC_SHA256(secret, run_id \0 expiry \0 sha256_hex(manifest)))`,
/// where the hashed manifest is the canonical JSON with `parent_capability`
/// blanked. Written out here rather than reused from the crate on purpose — the
/// test re-derives the wire contract independently of the code under test.
fn worker_capability(manifest: &WorkerManifest, expiry_unix: u64) -> String {
    let mut value = serde_json::to_value(manifest).expect("manifest serializes");
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "parent_capability".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    let payload = serde_json::to_string(&value).expect("manifest reserializes");

    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    let manifest_hex = hex::encode(hasher.finalize());

    let mut signed = Vec::new();
    signed.extend_from_slice(manifest.run_id.as_bytes());
    signed.push(0);
    signed.extend_from_slice(expiry_unix.to_string().as_bytes());
    signed.push(0);
    signed.extend_from_slice(manifest_hex.as_bytes());

    let key = hmac::Key::new(hmac::HMAC_SHA256, TEST_SECRET.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hmac::sign(&key, &signed).as_ref())
}

/// An absolute expiry far enough ahead that no plausible machine clock skew or
/// build delay can make it stale, and no wall-clock wait is implied by it.
fn far_future_expiry() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
        + 86_400
}

/// Spawn the real `prx session-worker`, with the capability environment either
/// supplied or explicitly absent.
fn spawn_worker(config_dir: &Path, home: &Path, capability_env: Option<(&str, u64)>) -> OwnedWorker {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(config_dir)
        .arg("session-worker")
        // Keep the run off the developer's real state directory: nothing here
        // may read or mint `~/.openprx/keys/session_worker.secret`.
        .env("HOME", home)
        .env("SESSION_WORKER_SECRET", TEST_SECRET)
        .env("OPENPRX_SESSION_WORKER_SECRET_PATH", home.join("session_worker.secret"))
        .env_remove("RUST_LOG")
        .env_remove("OPENPRX_SESSION_WORKER_CAPABILITY")
        .env_remove("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some((capability, expiry)) = capability_env {
        command
            .env("OPENPRX_SESSION_WORKER_CAPABILITY", capability)
            .env("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY", expiry.to_string());
    }
    let capture = tempfile::NamedTempFile::new_in(home).expect("private worker stdout capture");
    command.stdout(Stdio::from(capture.as_file().try_clone().expect("capture writer")));
    OwnedWorker::new(command.spawn().expect("session-worker binary spawns"), Some(capture))
}

/// Take the child's stdin so `Child::wait()` cannot close it behind our back —
/// `wait` drops `self.stdin` before waiting, which would silently turn every
/// "write end stays open" test into a "write end was closed" test.
const fn take_stdin(child: &mut OwnedWorker) -> ChildStdin {
    child.child.stdin.take().expect("worker stdin is piped")
}

fn write_line(stdin: &mut ChildStdin, line: &str) {
    // Fixed local cases send at most one manifest plus three <=256-byte
    // steer frames: <=3840 bytes in the initially empty macOS stdin pipe.
    assert!(line.len() < 3072, "fixture manifest line exceeds stdin precondition");
    stdin.write_all(line.as_bytes()).expect("write worker stdin line");
    stdin.write_all(b"\n").expect("write worker stdin newline");
    stdin.flush().expect("flush worker stdin");
}

fn write_production_manifest_line(stdin: &mut ChildStdin, line: &str) {
    stdin
        .write_all(line.as_bytes())
        .expect("write production manifest line");
    stdin.write_all(b"\n").expect("write production manifest newline");
    stdin.flush().expect("flush production manifest");
}

fn steer_frame(message: &str) -> String {
    let frame = serde_json::to_string(&WorkerControlFrame::Steer {
        message: message.to_string(),
    })
    .expect("steer frame serializes");
    assert!(frame.len() < 256, "fixture steer line exceeds stdin precondition");
    frame
}

/// Confirm child reap, then read bounded private stdout capture. `held_stdin`
/// stays open until natural completion; watchdog/cleanup failure cannot pass.
fn finish(child: OwnedWorker, held_stdin: Option<ChildStdin>) -> WorkerRun {
    let result = child.finish();
    drop(held_stdin);
    result
}

// Test watchdog only: the runtime's decision/cleanup budgets are unchanged.
const WORKER_WATCHDOG: std::time::Duration = std::time::Duration::from_mins(3);

struct OwnedWorker {
    child: Child,
    reaped: bool,
    capture: Option<tempfile::NamedTempFile>,
}

impl OwnedWorker {
    const fn new(child: Child, capture: Option<tempfile::NamedTempFile>) -> Self {
        Self {
            child,
            reaped: false,
            capture,
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped |= status.is_some();
        Ok(status)
    }

    fn finish(mut self) -> WorkerRun {
        use std::io::{Seek as _, SeekFrom};
        let deadline = std::time::Instant::now() + WORKER_WATCHDOG;
        let status = loop {
            if let Some(status) = self.try_wait().expect("worker exit status") {
                break status;
            }
            assert!(std::time::Instant::now() < deadline, "worker fixture watchdog expired");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        // Only inspect output after the direct child has actually been reaped.
        // This does not authorize the separately blocked nested-worker case.
        let mut stdout = String::new();
        if let Some(capture) = &mut self.capture {
            const LIMIT: u64 = 1024 * 1024;
            let file = capture.as_file_mut();
            let before = file.metadata().expect("capture metadata");
            assert!(
                before.is_file() && before.len() <= LIMIT,
                "worker stdout exceeded fixture limit"
            );
            file.seek(SeekFrom::Start(0)).expect("rewind capture");
            file.take(LIMIT + 1)
                .read_to_string(&mut stdout)
                .expect("complete UTF-8 worker stdout");
            let after = capture.as_file().metadata().expect("capture metadata after read");
            assert!(stdout.len() as u64 <= LIMIT, "worker stdout exceeded fixture limit");
            assert_eq!(before.len(), after.len(), "capture changed after direct-child reap");
            assert_eq!(after.len(), stdout.len() as u64, "capture was not read completely");
        }
        WorkerRun { status, stdout }
    }
}

impl Drop for OwnedWorker {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // Child ownership prevents a reused PID being signalled before reap.
        let _ = self.child.kill();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match self.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
                Err(_) => break,
            }
        }
        if let Some(capture) = self.capture.take() {
            std::mem::forget(capture);
        }
        let _ = writeln!(
            std::io::stderr(),
            "worker cleanup UNKNOWN; retained scratch; direct child PID {}",
            self.child.id()
        );
        assert!(
            std::thread::panicking(),
            "worker cleanup did not confirm direct-child reap"
        );
    }
}

// Preserve fixture state on every failed unwind, including unknown child reap.
struct WorkerScratch(Option<tempfile::TempDir>);
impl Drop for WorkerScratch {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(root) = self.0.take()
        {
            let _ = writeln!(
                std::io::stderr(),
                "worker failure scratch retained: {}",
                root.keep().display()
            );
        }
    }
}

/// Assert the one hard IPC invariant — stdout is exactly one newline-terminated
/// JSON line — and return the parsed result.
fn assert_single_json_line(stdout: &str) -> WorkerResult {
    assert!(
        stdout.ends_with('\n'),
        "session-worker stdout must be newline-terminated, got {stdout:?}"
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        1,
        "session-worker stdout must be exactly one JSON line, got {stdout:?}"
    );
    serde_json::from_str::<WorkerResult>(lines.first().copied().unwrap_or_default())
        .expect("session-worker stdout line parses as a WorkerResult")
}

fn worker_dirs() -> (
    WorkerScratch,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let root = tempfile::tempdir().expect("temp dir");
    let config_dir = root.path().join("config");
    let workspace_dir = root.path().join("workspace");
    let home = root.path().join("home");
    for dir in [&config_dir, &workspace_dir, &home] {
        std::fs::create_dir_all(dir).expect("create worker dir");
    }
    (WorkerScratch(Some(root)), config_dir, workspace_dir, home)
}

// The property the whole process-mode steering design hangs on: the parent
// holds the stdin write end open for the entire run (that is how later steer
// frames stay possible), and the worker must still terminate on its own.
//
// The run authenticates, so the post-manifest control-frame reader really is
// started — `POST_AUTH_FAILURE` in the result is what proves it — and three
// steer frames really are on the pipe behind the manifest. Nothing closes the
// pipe; the assertion is simply that `wait()` returns.
// Relocated intact to private LIB worker_stdin_protocol tests.

// The contrast case for the test above: closing the write end immediately after
// the manifest must reach the same outcome. Together the two pin that worker
// termination is independent of when — or whether — the parent closes stdin.
// Relocated intact to private LIB worker_stdin_protocol tests.

/// Steering is strictly downstream of authentication: a manifest carrying a
/// perfectly well-formed capability is still refused when the environment does
/// not corroborate it, and the refusal itself respects the one-line contract.
#[test]
fn worker_refuses_to_run_without_the_parent_capability_env() {
    let (_root, config_dir, workspace_dir, home) = worker_dirs();
    let mut manifest = manifest_for(&config_dir, &workspace_dir);
    let expiry = far_future_expiry();
    manifest.parent_capability = Some(worker_capability(&manifest, expiry));

    let mut child = spawn_worker(&config_dir, &home, None);
    let mut stdin = take_stdin(&mut child);
    write_line(
        &mut stdin,
        &serde_json::to_string(&manifest).expect("manifest serializes"),
    );
    write_line(&mut stdin, &steer_frame("steering must not be reachable"));

    let run = finish(child, Some(stdin));

    assert!(run.status.success(), "an unauthenticated run still reports on stdout");
    let result = assert_single_json_line(&run.stdout);
    assert!(!result.success);
    assert_eq!(
        result.error.as_deref(),
        Some("session-worker parent capability env is missing"),
        "authentication must fail before anything else in the run"
    );
}

/// A manifest that is not JSON at all, with a control frame queued behind it:
/// the worker must reject it, stay on one stdout line, and not be held open by
/// the frame still sitting unread in the pipe.
#[test]
fn invalid_manifest_followed_by_a_steer_frame_still_writes_exactly_one_line() {
    let (_root, config_dir, _workspace_dir, home) = worker_dirs();

    let mut child = spawn_worker(&config_dir, &home, None);
    let mut stdin = take_stdin(&mut child);
    write_line(&mut stdin, "not json{");
    write_line(&mut stdin, &steer_frame("frame behind a broken manifest"));

    let run = finish(child, Some(stdin));

    assert!(run.status.success(), "a malformed manifest is reported, not crashed on");
    let result = assert_single_json_line(&run.stdout);
    assert!(!result.success);
    assert!(
        result
            .error
            .unwrap_or_default()
            .contains("Invalid worker manifest JSON"),
        "the malformed manifest must be named in the result"
    );
}

/// Negative control for the discriminator every test above relies on.
///
/// Run the same harness — take the stdin handle, write, hold the write end —
/// against a child that genuinely blocks until stdin EOF. It is still running,
/// which is exactly the state a `tokio::io::stdin`-based control-frame reader
/// would have left the worker in; only closing the write end lets it finish.
/// Without this, "`wait()` returned" would be an assertion with nothing to
/// distinguish it from.
///
/// `try_wait` here is not a race: a child blocked on a pipe whose only write end
/// this process holds open cannot have exited, so the observation is stable and
/// needs no sleeping.
#[cfg(unix)]
#[test]
fn holding_the_stdin_write_end_keeps_an_eof_waiting_child_alive() {
    let child = Command::new("sh")
        .arg("-c")
        .arg("exec cat > /dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("stand-in EOF-waiting child spawns");
    let mut child = OwnedWorker::new(child, None);
    let mut stdin = take_stdin(&mut child);
    write_line(&mut stdin, &steer_frame("this child never stops reading"));

    assert!(
        child.try_wait().expect("try_wait on the stand-in child").is_none(),
        "a child waiting for stdin EOF must still be running while the parent holds the write end"
    );

    drop(stdin);
    let status = finish(child, None).status;
    assert!(status.success(), "the stand-in child exits once stdin reaches EOF");
}

/// Structural guard for the half of the risk the behavioural tests provably
/// cannot reach.
///
/// Moving the control-frame reader onto `tokio::io::stdin` or the blocking pool
/// does not make the worker hang — `RUNTIME_SHUTDOWN_TIMEOUT` in `src/main.rs`
/// caps runtime drop at two seconds — it makes every process-mode sub-agent pay
/// a silent two-second exit tax. No assertion on `Child::wait()` can see that
/// difference, and the only assertion that could is a wall-clock deadline, which
/// this repository does not allow anywhere. So the invariant is pinned at the
/// source instead, in the same style as `tests/architecture_boundaries/`: the
/// reader is a detached OS thread that the runtime neither owns nor joins.
#[test]
fn the_stdin_control_reader_stays_a_detached_os_thread() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("session_worker")
            .join("runner")
            .join("steering.rs"),
    )
    .expect("read src/session_worker/runner/steering.rs");
    let body = function_body(&source, "spawn_stdin_control_reader");

    assert!(
        body.contains("std::thread::spawn"),
        "the stdin control reader must run on a detached OS thread"
    );
    for forbidden in [
        "tokio::io::stdin",
        "spawn_blocking",
        "tokio::task::spawn",
        "tokio::spawn",
        ".join()",
    ] {
        assert!(
            !body.contains(forbidden),
            "spawn_stdin_control_reader reintroduced runtime-owned stdin reading via {forbidden}: \
             the worker would then pay the runtime-drop wait on every exit"
        );
    }
}

/// Return the source text of `fn <name>`, from its signature to the closing
/// brace at the same indentation.
fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}");
    let start = source.find(&marker).unwrap_or_else(|| panic!("{name} must exist"));
    let rest = source.get(start..).unwrap_or_default();
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("{name} must have a closing brace"));
    rest.get(..end).unwrap_or_default()
}
