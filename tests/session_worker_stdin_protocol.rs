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
use openprx::memory::MemoryEventRecording;
use openprx::session_worker::protocol::{WorkerControlFrame, WorkerManifest, WorkerResult};
use ring::hmac;
use sha2::{Digest, Sha256};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

/// Exercise the real worker executable with a sealed route and an isolated
/// loopback provider. The listener records headers from the OS child, while
/// HOME, config, state, and every credential remain inside this fixture.
#[test]
fn sealed_route_worker_sends_only_its_route_key_on_wire() {
    use openprx::providers::credentials::{CredentialRef, LegacySlot};
    use parking_lot::Mutex;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (_root, config_dir, workspace_dir, home) = worker_dirs();
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    listener.set_nonblocking(true).expect("nonblocking loopback");
    let destination = format!("custom:http://{}/v1", listener.local_addr().expect("loopback address"));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let nested_seen = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let server_seen = Arc::clone(&seen);
    let server_nested_seen = Arc::clone(&nested_seen);
    let server_stop = Arc::clone(&stop);
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
                    server_seen.lock().push(head.clone());
                    let nested_marker = [
                        "PRX-NEST-PARENT",
                        "PRX-NEST-ROUTE",
                        "PRX-NEST-AGENT-DIRECT",
                        "PRX-NEST-AGENT",
                        "PRX-NEST-GRANDCHILD",
                    ]
                    .into_iter()
                    .find(|marker| request_body.contains(marker));
                    if let Some(marker) = nested_marker {
                        server_nested_seen
                            .lock()
                            .push((marker.to_string(), head.clone(), request_body.clone()));
                    }
                    let nested_step = nested_marker.map(|marker| {
                        server_nested_seen
                            .lock()
                            .iter()
                            .filter(|(name, _, _)| name == marker)
                            .count()
                    });
                    let assistant = match (nested_marker, nested_step) {
                        (Some("PRX-NEST-PARENT"), Some(1)) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "tasks": [
                                {"task": "PRX-NEST-ROUTE"},
                                {"task": "PRX-NEST-AGENT", "agent": "prx_agent"},
                                {"task": "PRX-NEST-AGENT-DIRECT", "agent": "prx_agent", "model": "probe-model"}
                            ]
                        })),
                        (Some("PRX-NEST-PARENT" | "PRX-NEST-ROUTE"), Some(2)) => scripted_join_call(&request_body),
                        (Some("PRX-NEST-ROUTE"), Some(1)) => scripted_spawn_call(&serde_json::json!({
                            "action": "spawn_batch", "mode": "process", "tasks": ["PRX-NEST-GRANDCHILD"]
                        })),
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
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

    let mut config = openprx::config::Config {
        config_path: config_dir.join("config.toml"),
        workspace_dir: workspace_dir.clone(),
        default_provider: Some(destination.clone()),
        default_model: Some("hint:fast".to_string()),
        api_key: Some("PRX-WORKER-ROOT-KEY".to_string()),
        ..openprx::config::Config::default()
    };
    config.secrets.encrypt = false;
    config.sessions_spawn.process_memory_strategy = "isolated_private".to_string();
    config.model_routes.push(openprx::config::ModelRouteConfig {
        hint: "fast".to_string(),
        provider: destination.clone(),
        model: "probe-model".to_string(),
        api_key: Some("PRX-WORKER-ROUTE-KEY".to_string()),
        credential_ref: None,
    });
    std::fs::write(
        &config.config_path,
        toml::to_string_pretty(&config).expect("serialize isolated config"),
    )
    .expect("write isolated config");
    let mut manifest = manifest_for(&config_dir, &workspace_dir);
    manifest.provider_name = destination.clone();
    manifest.model = "probe-model".to_string();
    manifest.api_key = Some("PRX-WORKER-ROUTE-KEY".to_string());
    manifest.route_hint = Some("fast".to_string());
    manifest.route_source_provider = Some(destination);
    manifest.route_credential = Some(CredentialRef::LegacyInline(LegacySlot::ModelRoute("fast".to_string())));
    manifest.memory_strategy = Some("isolated_private".to_string());
    manifest.memory_db_path = workspace_dir.join("worker.db");
    manifest.worker_memory_db_path = Some(manifest.memory_db_path.clone());
    manifest.allowed_tools.clear();
    manifest.config_generation =
        openprx::session_worker::protocol::config_source_generation(&config_dir).expect("config generation");
    let expiry = far_future_expiry();
    let capability = worker_capability(&manifest, expiry);
    manifest.parent_capability = Some(capability.clone());
    let mut child = spawn_worker(&config_dir, &home, Some((&capability, expiry)));
    let mut stdin = take_stdin(&mut child);
    write_line(
        &mut stdin,
        &serde_json::to_string(&manifest).expect("sealed manifest JSON"),
    );
    drop(stdin);
    let run = finish(child, None);
    let result = assert_single_json_line(&run.stdout);
    assert!(run.status.success(), "worker exits cleanly: {:?}", run.status);
    assert!(result.success, "worker result: {:?}", result.error);
    let served_requests = seen.lock().len();
    assert!(served_requests > 0, "worker must call the route destination");

    manifest.run_id = "prx-tampered-route-key".to_string();
    manifest.api_key = Some("PRX-WORKER-ROOT-KEY".to_string());
    manifest.parent_capability = None;
    let bad_capability = worker_capability(&manifest, expiry);
    manifest.parent_capability = Some(bad_capability.clone());
    let mut bad_child = spawn_worker(&config_dir, &home, Some((&bad_capability, expiry)));
    let mut bad_stdin = take_stdin(&mut bad_child);
    write_line(
        &mut bad_stdin,
        &serde_json::to_string(&manifest).expect("tampered manifest JSON"),
    );
    drop(bad_stdin);
    let bad_run = finish(bad_child, None);
    let bad_result = assert_single_json_line(&bad_run.stdout);
    assert!(!bad_result.success, "wrong sealed key must be refused");
    assert_eq!(seen.lock().len(), served_requests);

    // A second worker inherits a named agent's profile on a same-provider
    // route with no route key. Its execution identity stays unset: the sealed
    // credential reference, not `agent_id`, is the source of authority.
    let profile = openprx::config::manage::credentials::CredentialReferences::for_config(&config)
        .expect("isolated credential store")
        .set(&manifest.provider_name, "prx-agent-route", "PRX-WORKER-AGENT-KEY")
        .expect("store isolated agent credential");
    config.model_routes[0].api_key = None;
    config.agents.insert(
        "prx_agent".to_string(),
        toml::from_str(&format!(
            "provider = \"{}\"\nmodel = \"hint:fast\"\ncredential_ref = \"{}\"\n",
            manifest.provider_name, profile.id
        ))
        .expect("agent config"),
    );
    std::fs::write(
        &config.config_path,
        toml::to_string_pretty(&config).expect("serialize agent config"),
    )
    .expect("write agent config");
    manifest.run_id = "prx-nested-agent-route".to_string();
    manifest.api_key = Some("PRX-WORKER-AGENT-KEY".to_string());
    manifest.route_credential = Some(CredentialRef::Profile { id: profile.id });
    manifest.agent_id = None;
    manifest.config_generation =
        openprx::session_worker::protocol::config_source_generation(&config_dir).expect("agent config generation");
    manifest.parent_capability = None;
    let agent_capability = worker_capability(&manifest, expiry);
    manifest.parent_capability = Some(agent_capability.clone());
    let mut agent_child = spawn_worker(&config_dir, &home, Some((&agent_capability, expiry)));
    let mut agent_stdin = take_stdin(&mut agent_child);
    write_line(
        &mut agent_stdin,
        &serde_json::to_string(&manifest).expect("agent manifest JSON"),
    );
    drop(agent_stdin);
    let agent_run = finish(agent_child, None);
    let agent_result = assert_single_json_line(&agent_run.stdout);
    let agent_requests = seen.lock().len();

    // The first manifest is sealed by the fixture. Every descendant is
    // launched by the worker's production SessionsSpawnTool in process mode.
    // The parent joins a batch containing an inherited route and an agent
    // override, so it cannot finish before both descendants report results.
    config.model_routes[0].api_key = Some("PRX-WORKER-ROUTE-KEY".to_string());
    config.model_routes.push(openprx::config::ModelRouteConfig {
        hint: "agent".to_string(),
        provider: manifest.provider_name.clone(),
        model: "agent-model".to_string(),
        api_key: None,
        credential_ref: None,
    });
    config.agents.get_mut("prx_agent").expect("agent fixture").model = "hint:agent".to_string();
    std::fs::write(
        &config.config_path,
        toml::to_string_pretty(&config).expect("serialize nested config"),
    )
    .expect("write nested config");
    manifest.run_id = "prx-nested-process-parent".to_string();
    manifest.task = "PRX-NEST-PARENT".to_string();
    manifest.model = "probe-model".to_string();
    manifest.api_key = Some("PRX-WORKER-ROUTE-KEY".to_string());
    manifest.route_hint = Some("fast".to_string());
    manifest.route_credential = Some(CredentialRef::LegacyInline(LegacySlot::ModelRoute("fast".to_string())));
    manifest.config_generation =
        openprx::session_worker::protocol::config_source_generation(&config_dir).expect("nested config generation");
    manifest.parent_capability = None;
    let nested_capability = worker_capability(&manifest, expiry);
    manifest.parent_capability = Some(nested_capability.clone());
    let mut nested_child = spawn_worker(&config_dir, &home, Some((&nested_capability, expiry)));
    let mut nested_stdin = take_stdin(&mut nested_child);
    write_line(
        &mut nested_stdin,
        &serde_json::to_string(&manifest).expect("nested parent manifest JSON"),
    );
    drop(nested_stdin);
    let nested_run = finish(nested_child, None);
    let nested_result = assert_single_json_line(&nested_run.stdout);
    let requests_before_missing_source = seen.lock().len();
    manifest.run_id = "prx-nested-missing-source".to_string();
    manifest.route_source_provider = None;
    manifest.parent_capability = None;
    let missing_source_capability = worker_capability(&manifest, expiry);
    manifest.parent_capability = Some(missing_source_capability.clone());
    let mut missing_source_child = spawn_worker(&config_dir, &home, Some((&missing_source_capability, expiry)));
    let mut missing_source_stdin = take_stdin(&mut missing_source_child);
    write_line(
        &mut missing_source_stdin,
        &serde_json::to_string(&manifest).expect("missing-source manifest JSON"),
    );
    drop(missing_source_stdin);
    let missing_source_run = finish(missing_source_child, None);
    let missing_source_result = assert_single_json_line(&missing_source_run.stdout);
    stop.store(true, Ordering::Relaxed);
    server.join().expect("loopback server joins");
    let requests = seen.lock();
    assert!(
        bad_run.status.success(),
        "refused worker exits cleanly: {:?}",
        bad_run.status
    );
    assert!(
        agent_run.status.success(),
        "agent worker exits cleanly: {:?}",
        agent_run.status
    );
    assert!(agent_result.success, "agent worker result: {:?}", agent_result.error);
    assert!(
        nested_run.status.success(),
        "nested parent exits cleanly: {:?}",
        nested_run.status
    );
    assert!(nested_result.success, "nested parent result: {:?}", nested_result.error);
    assert!(
        missing_source_run.status.success(),
        "refused missing-source worker exits cleanly"
    );
    assert!(!missing_source_result.success, "missing provenance must be refused");
    assert_eq!(
        requests.len(),
        requests_before_missing_source,
        "missing source caused outbound I/O"
    );
    assert!(requests.len() > served_requests, "agent worker must call the route");
    assert!(
        requests[..served_requests]
            .iter()
            .all(|head| head.contains("PRX-WORKER-ROUTE-KEY"))
    );
    assert!(requests.iter().all(|head| !head.contains("PRX-WORKER-ROOT-KEY")));
    assert!(
        requests[served_requests..agent_requests]
            .iter()
            .all(|head| head.contains("PRX-WORKER-AGENT-KEY"))
    );
    let nested = nested_seen.lock();
    assert!(nested.iter().any(|(name, _, _)| name == "PRX-NEST-PARENT"));
    assert!(nested.iter().any(|(name, _, _)| name == "PRX-NEST-ROUTE"));
    assert!(
        nested.iter().any(|(name, _, _)| name == "PRX-NEST-AGENT"),
        "nested request sequence: {:?}",
        nested.iter().map(|(name, _, _)| name).collect::<Vec<_>>()
    );
    assert!(nested.iter().any(|(name, _, _)| name == "PRX-NEST-AGENT-DIRECT"));
    assert!(
        nested.iter().any(|(name, _, _)| name == "PRX-NEST-GRANDCHILD"),
        "nested request sequence: {:?}",
        nested
            .iter()
            .map(|(name, _, body)| (name, &body[..body.len().min(250)]))
            .collect::<Vec<_>>()
    );
    for (name, head, body) in nested.iter() {
        let expected_key = if name.starts_with("PRX-NEST-AGENT") {
            "PRX-WORKER-AGENT-KEY"
        } else {
            "PRX-WORKER-ROUTE-KEY"
        };
        assert!(head.contains(expected_key), "{name} wrong credential");
        assert!(!head.contains("PRX-WORKER-ROOT-KEY"), "{name} got root credential");
        let expected_model = if name == "PRX-NEST-AGENT" {
            "agent-model"
        } else {
            "probe-model"
        };
        assert!(body.contains(expected_model), "{name} wrong model");
    }
    for marker in ["PRX-NEST-PARENT", "PRX-NEST-ROUTE"] {
        let requests = nested.iter().filter(|(name, _, _)| name == marker).collect::<Vec<_>>();
        assert!(requests.len() >= 3, "{marker} did not complete its batch join");
        let request: serde_json::Value = serde_json::from_str(&requests[2].2).expect("post-join request JSON");
        let joined = request["messages"]
            .as_array()
            .expect("post-join messages")
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")
            .and_then(|message| message["content"].as_str())
            .expect("join result content");
        let summary: serde_json::Value = serde_json::from_str(joined).expect("join summary JSON");
        let expected_total = if marker == "PRX-NEST-PARENT" { 3 } else { 1 };
        assert_eq!(summary["total"], expected_total, "{marker} join total");
        assert_eq!(summary["completed"].as_array().map(Vec::len), Some(expected_total));
        assert_eq!(summary["failed"].as_array().map(Vec::len), Some(0));
        assert_eq!(summary["killed"].as_array().map(Vec::len), Some(0));
        assert_eq!(summary["no_result"].as_array().map(Vec::len), Some(0));
        assert!(joined.contains("PRX-WORKER-OK"), "{marker} missing child output");
    }
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
    let start = request_body.find("batch-").expect("spawn batch id in parent request");
    let batch_id = &request_body[start..start + 42];
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
/// source instead, in the same style as `tests/architecture_boundaries.rs`: the
/// reader is a detached OS thread that the runtime neither owns nor joins.
#[test]
fn the_stdin_control_reader_stays_a_detached_os_thread() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("session_worker")
            .join("runner.rs"),
    )
    .expect("read src/session_worker/runner.rs");
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
