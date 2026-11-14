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
//! 2. With no mailbox binding, stdout stays exactly one `WorkerResult` JSON
//!    line. The opt-in mailbox fixture below exercises live Data/Ack lines
//!    followed by that same terminal result, with no non-protocol output.
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
//! route-credential test below also runs sealed workers under a real parent
//! and a loopback provider and checks the authorization headers on the wire.

use base64::Engine as _;
use openprx::memory::{Memory as _, MemoryEventRecording};
use openprx::session_worker::protocol::{WorkerControlFrame, WorkerManifest, WorkerResult};
use ring::hmac;
use sha2::{Digest, Sha256};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

// The real-parent harness is shared with `tests/ordinary_process_worker.rs`;
// this binary uses the wire-test subset of it.
#[allow(dead_code)]
#[path = "ordinary_process_worker/parent.rs"]
mod parent;
#[allow(dead_code)]
#[path = "ordinary_process_worker/provider.rs"]
mod provider;

const RECURSIVE_CHILD_TASK: &str = "Use sessions_spawn in process mode for PRX-NESTED-CHILD";
const FANOUT_CHILD_TASK: &str = "Use sessions_spawn in process mode for PRX-FANOUT-CHILD";
const WIRE_ROUTE_KEY: &str = "PRX-WORKER-ROUTE-KEY";
const WIRE_ROOT_KEY: &str = "PRX-WORKER-ROOT-KEY";
const WIRE_AGENT_KEY: &str = "PRX-WORKER-AGENT-KEY";
const WIRE_UNCHANGED: &str =
    "sealed route worker sends only its route key on wire: IPC identity or delivery state changed";

/// Markers in classification priority order.
const WIRE_MARKERS: [&str; 15] = [
    "PRX-ROUTE-PARENT",
    "PRX-AGENT-PARENT",
    "PRX-ROUTE-CHILD",
    "PRX-AGENT-CHILD",
    "PRX-OPTIN-PARENT",
    "PRX-OPTIN-CHILD",
    "PRX-NESTED-PARENT",
    "PRX-NESTED-CHILD",
    "PRX-NESTED-LEAF",
    "PRX-FANOUT-PARENT",
    "PRX-FANOUT-CHILD",
    "PRX-FANOUT-LEAF-A",
    "PRX-FANOUT-LEAF-B",
    "PRX-FANOUT-SECOND",
    "PRX-ROUTE-NONE",
];

const WIRE_PARENTS: [&str; 5] = [
    "PRX-ROUTE-PARENT",
    "PRX-AGENT-PARENT",
    "PRX-OPTIN-PARENT",
    "PRX-NESTED-PARENT",
    "PRX-FANOUT-PARENT",
];

fn wire_script(call: &provider::Call<'_>) -> provider::Reply {
    use provider::{scripted_join_call, scripted_spawn_call, text_reply};
    match (call.marker, call.index) {
        ("PRX-ROUTE-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
            "action": "spawn_batch", "mode": "process", "tasks": ["PRX-ROUTE-CHILD"]
        })),
        ("PRX-AGENT-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
            "action": "spawn_batch", "mode": "process", "tasks": [
                {"task": "PRX-AGENT-CHILD", "agent": "prx_agent"}
            ]
        })),
        ("PRX-OPTIN-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
            "action": "spawn_batch", "mode": "process", "allow_delegation": true,
            "tasks": ["PRX-OPTIN-CHILD"]
        })),
        ("PRX-NESTED-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
            "action": "spawn_batch", "mode": "process", "tasks": [
                {"task": RECURSIVE_CHILD_TASK, "allow_delegation": true}
            ]
        })),
        // An explicit `false` keeps the leaf non-delegating (an omitted flag
        // now asks for recursion by default).
        ("PRX-NESTED-CHILD", 1) => scripted_spawn_call(&serde_json::json!({
            "action": "spawn_batch", "mode": "process", "tasks": [
                {"task": "PRX-NESTED-LEAF", "allow_delegation": false}
            ]
        })),
        ("PRX-FANOUT-PARENT", 1) => scripted_spawn_call(&serde_json::json!({
            "action": "spawn_batch", "mode": "process", "tasks": [
                {"task": FANOUT_CHILD_TASK, "allow_delegation": true}
            ]
        })),
        ("PRX-FANOUT-CHILD", 1) => scripted_spawn_call(&serde_json::json!({
            "action": "spawn_batch", "mode": "process",
            "tasks": ["PRX-FANOUT-LEAF-A", "PRX-FANOUT-LEAF-B"]
        })),
        ("PRX-FANOUT-CHILD", 3) => scripted_spawn_call(&serde_json::json!({
            "action": "spawn_batch", "mode": "process", "tasks": ["PRX-FANOUT-SECOND"]
        })),
        (
            "PRX-ROUTE-PARENT" | "PRX-AGENT-PARENT" | "PRX-OPTIN-PARENT" | "PRX-NESTED-PARENT" | "PRX-NESTED-CHILD"
            | "PRX-FANOUT-PARENT",
            2,
        )
        | ("PRX-FANOUT-CHILD", 2 | 4) => scripted_join_call(call.body),
        _ => text_reply("PRX-WORKER-OK"),
    }
}

fn write_wire_config(config: &openprx::config::Config) {
    std::fs::write(
        &config.config_path,
        toml::to_string_pretty(config).expect("test: serialize worker config"),
    )
    .expect("test: write worker config");
}

/// Exercise the real worker executable with a sealed route and an isolated
/// loopback provider. The listener records headers from the OS child, while
/// HOME, config, state, and every credential remain inside this fixture.
#[test]
fn sealed_route_worker_sends_only_its_route_key_on_wire() {
    use openprx::providers::credentials::{CredentialRef, LegacySlot};
    use provider::{assert_join_summary, assert_parent_join, is_assessment_request};

    let fixture = parent::ParentFixture::new();
    let config_dir = fixture.config_dir.clone();
    let home = fixture.home.clone();
    let parent_workspace = fixture.parent_workspace.clone();
    let mut server = provider::OwnedProviderServer::start(provider::ProviderSpec {
        markers: WIRE_MARKERS.to_vec(),
        parents: WIRE_PARENTS.to_vec(),
        execution: WIRE_PARENTS
            .iter()
            .copied()
            .chain(["PRX-NESTED-CHILD", "PRX-FANOUT-CHILD"])
            .collect(),
        manifest_root: fixture.worker_root.clone(),
        tasks: [
            ("PRX-NESTED-CHILD", RECURSIVE_CHILD_TASK.to_owned()),
            ("PRX-FANOUT-CHILD", FANOUT_CHILD_TASK.to_owned()),
        ]
        .into_iter()
        .collect(),
        hold: None,
        script: Box::new(wire_script),
    });
    let destination = server.destination().to_owned();
    let mut config = openprx::config::Config {
        config_path: config_dir.join("config.toml"),
        workspace_dir: parent_workspace.clone(),
        default_provider: Some(destination.clone()),
        default_model: Some("hint:fast".to_string()),
        api_key: Some(WIRE_ROOT_KEY.to_string()),
        ..openprx::config::Config::default()
    };
    config.secrets.encrypt = false;
    config.sessions_spawn.process_memory_strategy = "isolated_private".to_string();
    config.sessions_spawn.worker_workspace_root = Some(fixture.worker_root.to_string_lossy().into_owned());
    config.sessions_spawn.cleanup_on_complete = false;
    config.tool_tiering.always_include.push("sessions_spawn".to_string());
    config.model_routes.push(openprx::config::ModelRouteConfig {
        settings: openprx::config::model_settings::ModelSettings::default(),
        hint: "fast".to_string(),
        provider: destination.clone(),
        model: provider::PROVIDER_MODEL.to_string(),
        api_key: Some(WIRE_ROUTE_KEY.to_string()),
        credential_ref: None,
    });
    provider::add_provider_fixture_model(&mut config, &destination, provider::PROVIDER_MODEL);
    write_wire_config(&config);

    // P1: route parent.
    let route_output = parent::run_parent(&config_dir, &home, &parent::spawn_request("PRX-ROUTE-PARENT"));
    route_output.assert_success("route");
    assert!(route_output.stdout.contains("PRX-WORKER-OK"), "{WIRE_UNCHANGED}");
    assert_parent_join(&server.requests(), "PRX-ROUTE-PARENT", 1);
    let route = server
        .manifest("PRX-ROUTE-CHILD")
        .expect("test: production route manifest");
    provider::assert_ordinary_manifest(&route);
    assert_sealed_child(&route.manifest, &server.requests(), "PRX-ROUTE-CHILD");
    parent::assert_completion_journal(&parent_workspace, &route.manifest);
    let served_requests = server.requests().len();
    assert!(
        server
            .requests()
            .iter()
            .filter(|request| request.marker == "PRX-ROUTE-CHILD")
            .all(|request| request.head.contains(WIRE_ROUTE_KEY)),
        "{WIRE_UNCHANGED}"
    );

    // P2: replay the genuine production-issued source with only the selected
    // credential changed. A fresh outer HMAC under `T` cannot make that
    // mutation match the authenticated model selection in the ordinary source.
    let transport_secret = fixture.transport_secret();
    let mut manifest = route.manifest.clone();
    assert_eq!(manifest.route_hint.as_deref(), Some("fast"), "{WIRE_UNCHANGED}");
    assert_eq!(
        manifest.route_credential,
        Some(CredentialRef::LegacyInline(LegacySlot::ModelRoute("fast".to_string()))),
        "{WIRE_UNCHANGED}"
    );
    manifest.api_key = Some(WIRE_ROOT_KEY.to_string());
    let expiry = far_future_expiry();
    let bad_run = run_forged_worker(&mut manifest, &home, &transport_secret, expiry);
    assert!(
        bad_run.status.success(),
        "refused worker exits cleanly: {:?}",
        bad_run.status
    );
    let bad_result = assert_single_json_line(&bad_run.stdout);
    assert!(!bad_result.success, "wrong sealed key must be refused");
    // The worker reports the full error chain (path context first); the
    // refusal reason must stay the innermost cause.
    let bad_result_error = bad_result.error.as_deref().unwrap_or_default();
    assert!(
        bad_result_error.ends_with("ordinary worker provider differs from authenticated selection"),
        "unexpected refusal: {bad_result_error}"
    );
    assert_eq!(server.requests().len(), served_requests, "{WIRE_UNCHANGED}");

    // P3: a second worker inherits a named agent's profile on a same-provider
    // route with no route key. The sealed credential reference, not
    // `agent_id`, is the source of authority.
    let profile = openprx::config::manage::credentials::CredentialReferences::for_config(&config)
        .expect("test: isolated credential store")
        .set(&destination, "prx-agent-route", WIRE_AGENT_KEY)
        .expect("test: store isolated agent credential");
    config.model_routes.push(openprx::config::ModelRouteConfig {
        settings: openprx::config::model_settings::ModelSettings::default(),
        hint: "agent".to_string(),
        provider: destination.clone(),
        model: "agent-model".to_string(),
        api_key: None,
        credential_ref: None,
    });
    provider::add_provider_fixture_model(&mut config, &destination, "agent-model");
    config.agents.insert(
        "prx_agent".to_string(),
        toml::from_str(&format!(
            "provider = \"{destination}\"\nmodel = \"hint:agent\"\ncredential_ref = \"{}\"\n",
            profile.id
        ))
        .expect("test: agent config"),
    );
    write_wire_config(&config);
    parent::run_parent(&config_dir, &home, &parent::spawn_request("PRX-AGENT-PARENT")).assert_success("agent");
    assert_parent_join(&server.requests(), "PRX-AGENT-PARENT", 1);
    let agent = server
        .manifest("PRX-AGENT-CHILD")
        .expect("test: production agent manifest");
    provider::assert_ordinary_manifest(&agent);
    assert_sealed_child(&agent.manifest, &server.requests(), "PRX-AGENT-CHILD");
    assert_eq!(
        agent.manifest.route_credential,
        Some(CredentialRef::Profile { id: profile.id }),
        "{WIRE_UNCHANGED}"
    );
    parent::assert_completion_journal(&parent_workspace, &agent.manifest);

    // P4: the same parent, with no config change, opts a child in to
    // recursive delegation; the child is spawned and completes.
    parent::run_parent(&config_dir, &home, &parent::spawn_request("PRX-OPTIN-PARENT")).assert_success("opt-in");
    {
        let requests = server.requests();
        assert_parent_join(&requests, "PRX-OPTIN-PARENT", 1);
        assert!(
            requests
                .iter()
                .all(|request| !request.body.contains("disabled by operator configuration")),
            "{WIRE_UNCHANGED}"
        );
    }
    let opt_in = server
        .manifest("PRX-OPTIN-CHILD")
        .expect("test: production opt-in manifest");
    provider::assert_ordinary_manifest(&opt_in);
    assert!(
        is_recursive(&opt_in.manifest),
        "an explicit opt-in derives a recursive child"
    );
    assert_sealed_child(&opt_in.manifest, &server.requests(), "PRX-OPTIN-CHILD");
    parent::assert_completion_journal(&parent_workspace, &opt_in.manifest);

    // P5: a real recursive worker spawns a real leaf.
    let nested_output = parent::run_parent(&config_dir, &home, &parent::spawn_request("PRX-NESTED-PARENT"));
    nested_output.assert_success("nested");
    assert!(
        nested_output.stdout.contains("PRX-WORKER-OK"),
        "nested parent stdout: {}",
        nested_output.stdout
    );
    {
        let requests = server.requests();
        assert_parent_join(&requests, "PRX-NESTED-PARENT", 1);
        assert_parent_join(&requests, "PRX-NESTED-CHILD", 1);
    }
    let nested = server
        .manifest("PRX-NESTED-CHILD")
        .expect("test: actual nested worker manifest");
    let leaf = server
        .manifest("PRX-NESTED-LEAF")
        .expect("test: actual grandchild manifest");
    provider::assert_ordinary_manifest(&nested);
    provider::assert_ordinary_manifest(&leaf);
    let nested_source = nested
        .manifest
        .ordinary_source
        .as_ref()
        .expect("test: nested ordinary source");
    let scope = &nested_source.capability_manifest;
    let parent_constraints = &nested_source.parent_constraints;
    assert!(
        is_recursive(&nested.manifest),
        "the recursive child keeps sessions_spawn"
    );
    assert!(
        scope.allowed.len() < parent_constraints.allowed.len(),
        "{WIRE_UNCHANGED}"
    );
    assert!(
        scope
            .allowed
            .iter()
            .all(|reference| parent_constraints.allowed.iter().any(|allowed| {
                allowed.id == reference.id
                    && allowed.fingerprint == reference.fingerprint
                    && allowed.generation == reference.generation
            })),
        "{WIRE_UNCHANGED}"
    );
    assert_eq!(
        Some(scope.remaining_depth),
        parent_constraints.remaining_depth.checked_sub(1),
        "{WIRE_UNCHANGED}"
    );
    assert!(
        scope.max_cost_cents <= parent_constraints.max_cost_cents,
        "{WIRE_UNCHANGED}"
    );
    assert!(scope.delegation.is_none(), "{WIRE_UNCHANGED}");
    assert_eq!(
        leaf.manifest.parent_run_id.as_deref(),
        Some(nested.manifest.run_id.as_str()),
        "{WIRE_UNCHANGED}"
    );
    assert_eq!(leaf.manifest.owner_id, nested.manifest.owner_id, "{WIRE_UNCHANGED}");
    let leaf_scope = &leaf
        .manifest
        .ordinary_source
        .as_ref()
        .expect("test: leaf ordinary source")
        .capability_manifest;
    assert_eq!(
        Some(leaf_scope.remaining_depth),
        scope.remaining_depth.checked_sub(1),
        "{WIRE_UNCHANGED}"
    );
    assert!(!is_recursive(&leaf.manifest), "the leaf carries no sessions_spawn ref");
    assert!(leaf_scope.max_cost_cents <= scope.max_cost_cents, "{WIRE_UNCHANGED}");
    {
        let requests = server.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.marker == "PRX-NESTED-LEAF" && !is_assessment_request(request))
                .count(),
            1,
            "{WIRE_UNCHANGED}"
        );
        for marker in ["PRX-NESTED-CHILD", "PRX-NESTED-LEAF"] {
            assert!(
                requests
                    .iter()
                    .filter(|request| request.marker == marker)
                    .all(|request| request.head.contains(WIRE_ROUTE_KEY)
                        && request.body.contains(provider::PROVIDER_MODEL)),
                "{WIRE_UNCHANGED}"
            );
        }
        assert_sealed_child(&nested.manifest, &requests, "PRX-NESTED-CHILD");
        assert_sealed_child(&leaf.manifest, &requests, "PRX-NESTED-LEAF");
    }
    parent::assert_completion_journal(&parent_workspace, &nested.manifest);
    parent::assert_completion_journal(&parent_workspace, &leaf.manifest);
    assert_ordinary_input_record(&nested.manifest);
    assert_ordinary_input_record(&leaf.manifest);

    // P6: the genuine leaf manifest re-signed under `T` with only its parent
    // run changed is refused before any provider request.
    let requests_before_wrong_parent = server.requests().len();
    let mut wrong_parent = leaf.manifest.clone();
    wrong_parent.parent_run_id = Some("prx_unrelated_parent".to_owned());
    let wrong_parent_run = run_forged_worker(&mut wrong_parent, &home, &transport_secret, expiry);
    let wrong_parent_result = assert_single_json_line(&wrong_parent_run.stdout);
    assert!(wrong_parent_run.status.success(), "{WIRE_UNCHANGED}");
    assert!(!wrong_parent_result.success, "{WIRE_UNCHANGED}");
    let wrong_parent_result_error = wrong_parent_result.error.as_deref().unwrap_or_default();
    assert!(
        wrong_parent_result_error.ends_with("ordinary worker parent provenance differs from verified scope"),
        "unexpected refusal: {wrong_parent_result_error}"
    );
    assert_eq!(
        server.requests().len(),
        requests_before_wrong_parent,
        "{WIRE_UNCHANGED}"
    );

    // P7: a fresh recursive worker fans out to two leaves in one batch, then
    // spawns a third in a later turn. Every carve comes from the child's own
    // funded balance.
    let fanout_config_dir = fixture.root().join("fanout-config");
    std::fs::create_dir_all(&fanout_config_dir).expect("test: isolated fanout config");
    let fanout_workspace = fanout_config_dir.join("workspace");
    let mut fanout_config = config.clone();
    fanout_config.config_path = fanout_config_dir.join("config.toml");
    fanout_config.workspace_dir = fanout_workspace.clone();
    fanout_config.cost.prices.insert(
        format!("{destination}/{}", provider::PROVIDER_MODEL),
        openprx::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    write_wire_config(&fanout_config);
    let fanout_output = parent::run_parent(&fanout_config_dir, &home, &parent::spawn_request("PRX-FANOUT-PARENT"));
    fanout_output.assert_success("fanout");
    assert!(fanout_output.stdout.contains("PRX-WORKER-OK"), "{WIRE_UNCHANGED}");
    {
        let requests = server.requests();
        assert_parent_join(&requests, "PRX-FANOUT-PARENT", 1);
        let child: Vec<_> = requests
            .iter()
            .filter(|request| request.marker == "PRX-FANOUT-CHILD" && !is_assessment_request(request))
            .collect();
        assert_eq!(
            child.len(),
            5,
            "both spawns, both joins and the final reply reach the model"
        );
        assert_join_summary(child[2], 2);
        assert_join_summary(child[4], 1);
        assert!(
            requests
                .iter()
                .all(|request| !request.body.contains("at most one child")
                    && !request.body.contains("already allocated")),
            "{WIRE_UNCHANGED}"
        );
    }
    let fanout = server
        .manifest("PRX-FANOUT-CHILD")
        .expect("test: real recursive worker");
    provider::assert_ordinary_manifest(&fanout);
    let allowance = fanout
        .manifest
        .ordinary_source
        .as_ref()
        .expect("test: fanout ordinary source")
        .capability_manifest
        .max_cost_cents;
    assert!(allowance > 0, "the fanout child must hold a funded allowance");
    let mut carved = 0_u32;
    for marker in ["PRX-FANOUT-LEAF-A", "PRX-FANOUT-LEAF-B", "PRX-FANOUT-SECOND"] {
        let leaf = server.manifest(marker).expect("test: real fanout leaf");
        provider::assert_ordinary_manifest(&leaf);
        assert_eq!(
            leaf.manifest.parent_run_id.as_deref(),
            Some(fanout.manifest.run_id.as_str()),
            "{WIRE_UNCHANGED}"
        );
        let cents = leaf
            .manifest
            .ordinary_source
            .as_ref()
            .expect("test: leaf ordinary source")
            .capability_manifest
            .max_cost_cents;
        // Leaves are carved in launch order; each fits the balance left.
        assert!(
            cents <= allowance - carved,
            "{marker} exceeds the child's balance at its carve"
        );
        carved += cents;
        parent::assert_completion_journal(&fanout_workspace, &leaf.manifest);
    }
    assert!(carved <= allowance, "fanout leaves exceed the child's funded allowance");
    parent::assert_completion_journal(&fanout_workspace, &fanout.manifest);
    write_wire_config(&config);
    let agent_requests = server.requests().len();

    // Keep the physical refusal control from the original contract: the
    // genuine agent source with its route provenance removed, re-signed under
    // `T`, may not reach the provider.
    let requests_before_missing_source = server.requests().len();
    let mut missing_source_manifest = agent.manifest.clone();
    missing_source_manifest.route_source_provider = None;
    let missing_source_run = run_forged_worker(&mut missing_source_manifest, &home, &transport_secret, expiry);
    let missing_source_result = assert_single_json_line(&missing_source_run.stdout);
    server.stop();
    server.assert_no_failures();
    let requests = server.requests();
    assert!(
        missing_source_run.status.success(),
        "refused missing-source worker exits cleanly"
    );
    assert!(!missing_source_result.success, "missing provenance must be refused");
    let missing_source_result_error = missing_source_result.error.as_deref().unwrap_or_default();
    assert!(
        missing_source_result_error.ends_with("ordinary worker provider differs from authenticated selection"),
        "unexpected refusal: {missing_source_result_error}"
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
            .all(|request| request.head.contains(WIRE_ROUTE_KEY) && request.body.contains(provider::PROVIDER_MODEL)),
        "route child request used the wrong key or model"
    );
    assert!(
        requests.iter().all(|request| !request.head.contains(WIRE_ROOT_KEY)),
        "{WIRE_UNCHANGED}"
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.marker == "PRX-AGENT-CHILD")
            .all(|request| request.head.contains(WIRE_AGENT_KEY) && request.body.contains("agent-model")),
        "named-agent child request used the wrong key or model"
    );
    assert!(agent_requests > served_requests, "{WIRE_UNCHANGED}");
}

/// Whether the worker's sealed ordinary scope keeps `sessions_spawn`.
fn is_recursive(manifest: &WorkerManifest) -> bool {
    manifest
        .ordinary_source
        .as_ref()
        .expect("test: ordinary source")
        .capability_manifest
        .allowed
        .iter()
        .any(|reference| reference.id.ends_with(":sessions_spawn"))
}

/// A sealed child widens no explicit tool list, and its provider request
/// lists `sessions_spawn` exactly when its sealed scope keeps it: never for a
/// non-recursive child, always for a recursive one.
fn assert_sealed_child(manifest: &WorkerManifest, requests: &[provider::ProviderRequest], marker: &str) {
    assert!(
        manifest.allowed_tools.is_empty(),
        "sealed child widened its explicit tool allowlist"
    );
    let request = requests
        .iter()
        .find(|request| request.marker == marker && !provider::is_assessment_request(request))
        .expect("test: sealed child provider request");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("test: sealed child request JSON");
    let lists_spawn = body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool.pointer("/function/name").and_then(serde_json::Value::as_str) == Some("sessions_spawn"))
    });
    assert_eq!(
        lists_spawn,
        is_recursive(manifest),
        "{marker}: published sessions_spawn differs from its sealed scope"
    );
}

/// The `SessionsSpawn` input event the ordinary source names exists for the
/// child's owner, run and task, and carries no `delegation` record.
fn assert_ordinary_input_record(manifest: &WorkerManifest) {
    let source = manifest.ordinary_source.as_ref().expect("test: ordinary source");
    let input = source.input.as_ref().expect("test: recorded ordinary input");
    let workspace = std::path::Path::new(&source.parent_constraints.workspace);
    let memory = openprx::memory::SqliteMemory::new(workspace).expect("test: parent input memory");
    let event = parent::runtime()
        .block_on(memory.find_owned_message_event(
            &source.parent_constraints.workspace,
            parent::owner(manifest),
            &input.event_id,
        ))
        .expect("test: input event lookup")
        .expect("test: durable child input record");
    assert_eq!(event.owner_id, manifest.owner_id, "input record owner");
    assert_eq!(
        event.run_id.as_deref(),
        Some(manifest.run_id.as_str()),
        "input record run"
    );
    let payload: serde_json::Value =
        serde_json::from_str(event.raw_payload_json.as_deref().unwrap_or("{}")).expect("test: input payload JSON");
    assert!(
        payload["task"].as_str() == Some(manifest.task.as_str()) || event.content.contains(&manifest.task),
        "input record task: {payload}"
    );
    assert!(
        payload.get("delegation").is_none(),
        "input record carries a delegation key"
    );
}

/// Re-sign `manifest` under the parents' transport secret `T` and run the real
/// worker with the parents' environment. Returns the reaped run.
fn run_forged_worker(manifest: &mut WorkerManifest, home: &Path, secret: &[u8], expiry: u64) -> WorkerRun {
    manifest.parent_capability = None;
    let capability = worker_capability_under(manifest, expiry, secret);
    manifest.parent_capability = Some(capability.clone());
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(&manifest.config_dir)
        .arg("session-worker");
    parent::parent_environment(&mut command, home);
    command
        .env("OPENPRX_SESSION_WORKER_CAPABILITY", &capability)
        .env("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY", expiry.to_string())
        .stdin(Stdio::piped())
        .stderr(Stdio::null());
    let capture = tempfile::NamedTempFile::new_in(home).expect("test: private worker stdout capture");
    command.stdout(Stdio::from(
        capture.as_file().try_clone().expect("test: capture writer"),
    ));
    let mut child = OwnedWorker::new(
        command.spawn().expect("test: session-worker binary spawns"),
        Some(capture),
    );
    let mut stdin = take_stdin(&mut child);
    let line = serde_json::to_string(manifest).expect("test: forged manifest JSON");
    stdin
        .write_all(line.as_bytes())
        .expect("test: write forged manifest line");
    stdin.write_all(b"\n").expect("test: write forged manifest newline");
    stdin.flush().expect("test: flush forged manifest");
    drop(stdin);
    finish(child, None)
}

/// Shared HMAC secret handed to the worker through `SESSION_WORKER_SECRET`, so
/// the test can mint a capability the worker will accept without reaching for
/// any state the developer's machine owns.
const TEST_SECRET: &str = "t6-stdin-protocol-secret-32bytes";

// The first check the worker performs *after* authentication has passed and
// the stdin control-frame reader has been started. Seeing this error is the
// proof that the reader thread exists in the run under test.

struct WorkerRun {
    status: std::process::ExitStatus,
    stdout: String,
}

/// Build a manifest that authenticates but cannot run: its config directory is
/// real (so the CLI/manifest cross-check passes). Without an authenticated
/// decision source, the run stops immediately after transport verification.
fn manifest_for(config_dir: &Path, workspace_dir: &Path) -> WorkerManifest {
    WorkerManifest {
        parent_capability: None,
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
        inherited_provenance: None,
        session_addr: None,
        compaction_config: None,
    }
}

/// Recompute the sealed capability the worker will verify:
/// `base64url(HMAC_SHA256(secret, run_id \0 expiry \0 sha256_hex(manifest)))`,
/// where the hashed manifest is the canonical JSON with `parent_capability`
/// blanked. Written out here rather than reused from the crate on purpose — the
/// test re-derives the wire contract independently of the code under test.
fn worker_capability(manifest: &WorkerManifest, expiry_unix: u64) -> String {
    worker_capability_under(manifest, expiry_unix, TEST_SECRET.as_bytes())
}

/// [`worker_capability`] under an explicit transport secret.
fn worker_capability_under(manifest: &WorkerManifest, expiry_unix: u64, secret: &[u8]) -> String {
    let mut value = serde_json::to_value(manifest).expect("test: manifest serializes");
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "parent_capability".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    let payload = serde_json::to_string(&value).expect("test: manifest reserializes");

    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    let manifest_hex = hex::encode(hasher.finalize());

    let mut signed = Vec::new();
    signed.extend_from_slice(manifest.run_id.as_bytes());
    signed.push(0);
    signed.extend_from_slice(expiry_unix.to_string().as_bytes());
    signed.push(0);
    signed.extend_from_slice(manifest_hex.as_bytes());

    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
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
    let capture = tempfile::NamedTempFile::new_in(home).expect("test: private worker stdout capture");
    command.stdout(Stdio::from(
        capture.as_file().try_clone().expect("test: capture writer"),
    ));
    OwnedWorker::new(
        command.spawn().expect("test: session-worker binary spawns"),
        Some(capture),
    )
}

/// Take the child's stdin so `Child::wait()` cannot close it behind our back —
/// `wait` drops `self.stdin` before waiting, which would silently turn every
/// "write end stays open" test into a "write end was closed" test.
const fn take_stdin(child: &mut OwnedWorker) -> ChildStdin {
    child.child.stdin.take().expect("test: worker stdin is piped")
}

fn write_line(stdin: &mut ChildStdin, line: &str) {
    // Fixed local cases send at most one manifest plus three <=256-byte
    // steer frames: <=3840 bytes in the initially empty macOS stdin pipe.
    assert!(line.len() < 3072, "fixture manifest line exceeds stdin precondition");
    stdin.write_all(line.as_bytes()).expect("test: write worker stdin line");
    stdin.write_all(b"\n").expect("test: write worker stdin newline");
    stdin.flush().expect("test: flush worker stdin");
}

fn steer_frame(message: &str) -> String {
    let frame = serde_json::to_string(&WorkerControlFrame::Steer {
        message: message.to_string(),
    })
    .expect("test: steer frame serializes");
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
            if let Some(status) = self.try_wait().expect("test: worker exit status") {
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
            let before = file.metadata().expect("test: capture metadata");
            assert!(
                before.is_file() && before.len() <= LIMIT,
                "worker stdout exceeded fixture limit"
            );
            file.seek(SeekFrom::Start(0)).expect("test: rewind capture");
            file.take(LIMIT + 1)
                .read_to_string(&mut stdout)
                .expect("test: complete UTF-8 worker stdout");
            let after = capture.as_file().metadata().expect("test: capture metadata after read");
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
        .expect("test: session-worker stdout line parses as a WorkerResult")
}

fn worker_dirs() -> (
    WorkerScratch,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let root = tempfile::tempdir().expect("test: temp dir");
    let config_dir = root.path().join("config");
    let workspace_dir = root.path().join("workspace");
    let home = root.path().join("home");
    for dir in [&config_dir, &workspace_dir, &home] {
        std::fs::create_dir_all(dir).expect("test: create worker dir");
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
        &serde_json::to_string(&manifest).expect("test: manifest serializes"),
    );
    write_line(&mut stdin, &steer_frame("steering must not be reachable"));

    let run = finish(child, Some(stdin));

    assert!(run.status.success(), "an unauthenticated run still reports on stdout");
    let result = assert_single_json_line(&run.stdout);
    assert!(
        !result.success,
        "worker refuses to run without the parent capability env: capacity enforcement or bounded delivery changed"
    );
    assert_eq!(
        result.error.as_deref(),
        Some("session-worker parent capability env is missing"),
        "authentication must fail before anything else in the run"
    );
}

/// Real `prx session-worker` over pipes, then FOUNDATION relaunch: same
/// [`openprx::messaging::SessionAddr`], new run.
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
    assert!(
        !result.success,
        "invalid manifest followed by a steer frame still writes exactly one line: IPC identity or delivery state changed"
    );
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
        .expect("test: stand-in EOF-waiting child spawns");
    let mut child = OwnedWorker::new(child, None);
    let mut stdin = take_stdin(&mut child);
    write_line(&mut stdin, &steer_frame("this child never stops reading"));

    assert!(
        child
            .try_wait()
            .expect("test: try_wait on the stand-in child")
            .is_none(),
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
    .expect("test: read src/session_worker/runner/steering.rs");
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

#[cfg(feature = "test-mock")]
async fn assert_mailbox_worker_frames(turns: usize) -> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};
    use tokio::io::AsyncReadExt as _;
    const MAX_MAILBOX_OUTPUT: u64 = 1024 * 1024;
    let (_root, config_dir, _unused_workspace, home) = worker_dirs();
    let workspace = config_dir.join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let mut config = openprx::config::Config {
        workspace_dir: workspace.clone(),
        config_path: config_dir.join("config.toml"),
        default_provider: Some("mock".into()),
        default_model: Some("prx-mailbox-model".into()),
        ..openprx::config::Config::default()
    };
    config.secrets.encrypt = false;
    config.autonomy.level = openprx::security::policy::AutonomyLevel::Full;
    config.sessions_spawn.cleanup_on_complete = false;
    config.sessions_spawn.worker_workspace_root = Some(workspace.join("workers").to_string_lossy().into_owned());
    config.tool_tiering.always_include.push("sessions_spawn".into());
    config.providers.models.push(serde_json::from_value(
        serde_json::json!({"provider":"mock","model":"prx-mailbox-model","operations":["chat"],"native_tools":true}),
    )?);
    config.cost.prices.insert(
        "mock/prx-mailbox-model".into(),
        openprx::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.save().await?;
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(&config_dir)
        .args(["__mailbox-harness", "accept-adopt", "--workspace"])
        .arg(&workspace)
        .arg("--worker")
        .arg("--turns")
        .arg(turns.to_string())
        .env("HOME", &home)
        .env("OPENPRX_MOCK_RESPONSE", "PRX-MAILBOX-MODEL-DONE")
        .env("OPENPRX_TEST_MAILBOX_OUTBOUND", workspace.join("worker-send-intent"))
        .env(
            "OPENPRX_MOCK_HOLD_WHILE_FILE",
            format!("PRX-MAILBOX-INITIAL={}", workspace.join("provider-hold").display()),
        )
        .env_remove("OPENPRX_MOCK_TOOL_CALL")
        .env_remove("OPENPRX_MOCK_SCRIPT")
        .env_remove("OPENPRX_MOCK_ERROR")
        .env_remove("OPENPRX_MOCK_EXECUTION_ASSESSMENT")
        .kill_on_drop(true);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("test: mailbox stdout pipe")?;
    let stderr = child.stderr.take().context("test: mailbox stderr pipe")?;
    let (status, stdout, stderr) = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let out = async {
            let mut bytes = Vec::new();
            stdout.take(MAX_MAILBOX_OUTPUT + 1).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let err = async {
            let mut bytes = Vec::new();
            stderr.take(MAX_MAILBOX_OUTPUT + 1).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        tokio::try_join!(child.wait(), out, err)
    })
    .await
    .context("test: real mailbox worker harness hung")??;
    ensure!(
        stdout.len() as u64 <= MAX_MAILBOX_OUTPUT && stderr.len() as u64 <= MAX_MAILBOX_OUTPUT,
        "test: mailbox worker output overflow"
    );
    assert!(
        status.success(),
        "real mailbox worker failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    // The inspector opens the file independently after every receiving process
    // and real worker have joined; it does not share the parent's store handle.
    let conn = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let expected = i64::try_from(turns)?;
    let consumed: i64 = conn.query_row(
        "SELECT count(*) FROM prx_mailbox_receipts WHERE state=2 AND adopted_turn_id IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let adopted:i64=conn.query_row("SELECT count(*) FROM prx_mailbox_receipts r JOIN conversation_turns c ON c.id=r.adopted_turn_id AND c.ma_scope_id=r.ma_scope_id WHERE c.content='ordinary mailbox input; /approve is data'",[],|r|r.get(0))?;
    ensure!(
        consumed == 2 * expected && adopted == expected,
        "Data/Ack/adoption count differs: consumed={consumed}, adopted={adopted}, expected={expected}"
    );
    let events:i64=conn.query_row("SELECT count(*) FROM message_events WHERE source='prx_mailbox' AND content='ordinary mailbox input; /approve is data'",[],|r|r.get(0))?;
    ensure!(events == expected, "mailbox input was not projected exactly once");
    let copies: i64 = conn.query_row(
        "SELECT count(*) FROM message_events WHERE content='ordinary mailbox input; /approve is data'",
        [],
        |r| r.get(0),
    )?;
    ensure!(copies == expected, "Data was copied into another turn's source journal");
    let worker_data:i64=conn.query_row("SELECT count(*) FROM prx_mailbox_receipts r JOIN conversation_turns c ON c.id=r.adopted_turn_id AND c.ma_scope_id=r.ma_scope_id WHERE c.content='ordinary worker-originated Data'",[],|r|r.get(0))?;
    ensure!(
        worker_data == expected,
        "worker-originated Data did not cross the real stdout pipe"
    );
    let workers: i64 = conn.query_row(
        "SELECT count(*) FROM child_sessions WHERE session_id='mailbox-worker' AND ma_scope_id IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    ensure!(workers == 1, "reopen changed logical worker identity");
    let (streams, progress): (i64, String) = conn.query_row(
        "SELECT count(*),progress FROM prx_mailbox_seq WHERE from_addr LIKE '%mailbox-parent%'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let progress: serde_json::Value = serde_json::from_str(&progress)?;
    let next = progress.get("next_seq").and_then(serde_json::Value::as_u64);
    ensure!(
        streams == 1 && next == Some(u64::try_from(turns + 1)?),
        "reopen reset the durable sender stream: streams={streams}, next={next:?}"
    );
    Ok(())
}

#[cfg(feature = "test-mock")]
#[tokio::test]
async fn worker_and_parent_exchange_data_and_ack_over_real_frames() -> anyhow::Result<()> {
    assert_mailbox_worker_frames(1).await
}

#[cfg(feature = "test-mock")]
#[tokio::test]
async fn send_before_and_after_process_reopen_uses_same_session_addr() -> anyhow::Result<()> {
    assert_mailbox_worker_frames(2).await
}
