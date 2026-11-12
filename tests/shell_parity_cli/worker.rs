#![cfg(test)]
//! Real Chat workers and CLI shells share the selected working-root contract.
use super::*;
#[path = "decision.rs"]
mod decision;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

const MAX_PROVIDER_REQUEST_BYTES: usize = 1024 * 1024;

#[tokio::test]
async fn chat_config_workdir_reaches_real_worker_and_unset_keeps_worker_workspace() {
    tokio::time::timeout(Duration::from_secs(120), async {
        for override_root in [true, false] {
            Box::pin(root_pwd(true, override_root)).await;
        }
    })
    .await
    .expect("test: Chat worker fixture deadline");
}

#[tokio::test]
async fn cli_workdir_reaches_shell_and_unset_keeps_workspace() {
    tokio::time::timeout(Duration::from_secs(120), async {
        for override_root in [true, false] {
            Box::pin(root_pwd(false, override_root)).await;
        }
    })
    .await
    .expect("test: CLI shell fixture deadline");
}

async fn root_pwd(worker: bool, override_root: bool) {
    let root = tempfile::tempdir().expect("test: fixture root");
    let config_dir = root.path().join("config");
    let workspace = config_dir.join("workspace");
    let workdir = root.path().join("working-root");
    let workers = root.path().join("workers");

    for dir in [&config_dir, &workspace, &workdir, &workers] {
        std::fs::create_dir_all(dir).expect("test: fixture directory");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: provider bind");
    let address = listener.local_addr().expect("test: provider address");
    let parent_calls = Arc::new(AtomicUsize::new(0));
    let child_calls = Arc::new(AtomicUsize::new(0));
    let child_observed = Arc::new(AtomicBool::new(false));
    let observed = child_observed.clone();
    let child_count = child_calls.clone();
    let captured = Arc::new(parking_lot::Mutex::new(String::new()));
    let captured_for_server = captured.clone();
    let app = axum::Router::new().route("/v1/systemone", axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
        let candidates = body.pointer("/state/candidates").and_then(Value::as_array).expect("test: decision candidates");
        let delegating = candidates.iter().any(|candidate| candidate.get("action").and_then(Value::as_str) == Some("delegate"));
        let (action, target) = if delegating { ("delegate", "session") } else { ("use_tool", "shell") };
        axum::Json(decision::answers(&body, action, target).expect("test: real candidate predictions"))
    })).fallback(axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
        let parent_calls = parent_calls.clone();
        let child_calls = child_count.clone();
        let child_observed = observed.clone();
        let captured = captured_for_server.clone();
        async move {
            let messages = request.get("messages").and_then(Value::as_array).expect("test: request messages");
            let text = request.to_string();
            let child = messages.iter().rev().find(|message| message.get("role").and_then(Value::as_str) == Some("user")).and_then(|message| message.get("content").and_then(Value::as_str)).is_some_and(|input| input.contains("PRX-PWD-CHILD"));
            let tool_call = |name: &str, arguments: Value| json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call-{}", uuid::Uuid::new_v4()),"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]});
            let assistant = if text.contains("Review the proposed final reply against the CURRENT request") {
                json!({"role":"assistant","content":r#"{"decision":"accept","reason":"fixture evidence","constraints":[]}"#})
            } else if text.contains("Classify the execution requirement of current_request") {
                json!({"role":"assistant","content":r#"{"requirement":"execution_required"}"#})
            } else if child || !worker {
                if child_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    tool_call("shell", json!({"command":"pwd"}))
                } else {
                    const MAX_PWD_RESULT_BYTES: usize = 4096;
                    let result = messages.iter().rev().find(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
                        .and_then(|message| message.get("content")).and_then(Value::as_str).expect("test: real shell result on wire");
                    assert!(result.len() <= MAX_PWD_RESULT_BYTES, "test: shell pwd result exceeds fixture bound");
                    *captured.lock() = result.to_owned();
                    child_observed.store(true, Ordering::SeqCst);
                    json!({"role":"assistant","content":"PRX-PWD-DONE"})
                }
            } else {
                match parent_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => tool_call("sessions_spawn", json!({"action":"spawn_batch","mode":"process","tasks":["Run shell pwd for PRX-PWD-CHILD"]})),
                    1 => {
                        let batch = messages.iter().rev().filter(|message| message.get("role").and_then(Value::as_str) == Some("tool")).filter_map(|message| message.get("content").and_then(Value::as_str)).filter_map(|content| serde_json::from_str::<Value>(content).ok()).find(|result| result.get("batch_id").is_some_and(Value::is_string)).unwrap_or_else(|| panic!("test: actual spawn batch; tool results: {:?}", messages.iter().filter(|message| message.get("role").and_then(Value::as_str) == Some("tool")).take(4).map(|message| message.get("content").and_then(Value::as_str).unwrap_or_default().chars().take(4096).collect::<String>()).collect::<Vec<_>>()));
                        tool_call("sessions_spawn", json!({"action":"join","batch_id":batch.get("batch_id").expect("test: batch identifier")}))
                    }
                    _ => json!({"role":"assistant","content":"PRX-PWD-DONE"}),
                }
            };
            let body = json!({"id":"prx-pwd-response","object":"chat.completion","model":"prx-pwd-model","choices":[{"index":0,"message":assistant,"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
            if request.get("stream").and_then(Value::as_bool) == Some(true) {
                let (delta, reason) = assistant.get("tool_calls").and_then(Value::as_array).map_or_else(
                    || (json!({"content":assistant.get("content")}), "stop"),
                    |calls| {
                        let mut calls = calls.clone();
                        for (index, call) in calls.iter_mut().enumerate() {
                            call.as_object_mut().expect("test: tool-call object").insert("index".into(), json!(index));
                        }
                        (json!({"tool_calls":calls}), "tool_calls")
                    },
                );
                let chunk = json!({"id":"prx-pwd-stream","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                let finish = json!({"id":"prx-pwd-stream","choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n")).into_response()
            } else {
                axum::Json(body).into_response()
            }
        }
    }));
    let app = app.layer(axum::extract::DefaultBodyLimit::max(MAX_PROVIDER_REQUEST_BYTES));
    let stop = tokio_util::sync::CancellationToken::new();
    let stopping = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stopping.cancelled_owned())
            .await
    });
    let provider = format!("custom:http://{address}/v1");
    let mut config = openprx::config::Config {
        workspace_dir: workspace.clone(),
        config_path: config_dir.join("config.toml"),
        default_provider: Some(provider.clone()),
        default_model: Some("prx-pwd-model".into()),
        api_key: Some("prx-fixture-key".into()),
        ..openprx::config::Config::default()
    };
    config.causal_tree.enabled = worker;
    if worker {
        config.causal_tree.backend.kind =
            serde_json::from_value(json!("compatible_http")).expect("test: compatible decision backend");
        config.causal_tree.backend.endpoint = format!("http://{address}");
        config.causal_tree.backend.model = "prx-native-decision".into();
        config.causal_tree.backend.remote_enabled = false;
        config.causal_tree.rehearsal.timeout_ms = 450;
        let decision_provider = format!("custom:http://{address}");
        config.providers.models.push(
            serde_json::from_value(
                json!({"provider":decision_provider,"model":"prx-native-decision","operations":["decision_endpoint"]}),
            )
            .expect("test: decision model"),
        );
        config.cost.prices.insert(
            format!("{decision_provider}/prx-native-decision"),
            openprx::config::schema::ModelPricing {
                input: 0.1,
                output: 0.2,
                cache_write: 0.3,
                cache_read: 0.1,
            },
        );
    }
    config.secrets.encrypt = false;
    config.memory.auto_save = false;
    config.runtime.workdir = (worker && override_root).then(|| workdir.clone());
    config.sessions_spawn.process_memory_strategy = "isolated_private".into();
    config.sessions_spawn.worker_workspace_root = Some(workers.to_string_lossy().into_owned());
    config.sessions_spawn.cleanup_on_complete = false;
    config.tool_tiering.always_include = vec!["shell".into(), "sessions_spawn".into()];
    config.providers.models.push(
        serde_json::from_value(
            json!({"provider":provider,"model":"prx-pwd-model","operations":["chat"],"native_tools":true}),
        )
        .expect("test: model"),
    );
    config.cost.prices.insert(
        format!("{provider}/prx-pwd-model"),
        openprx::config::schema::ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    std::fs::write(
        &config.config_path,
        toml::to_string(&config).expect("test: config serialize"),
    )
    .expect("test: config write");
    let workdir_str = workdir.to_str().expect("test: workdir UTF-8");
    let (status, output) = if worker {
        run_program(
            &config_dir,
            &["chat", "--plain"],
            Some("Use sessions_spawn in process mode for PRX-PWD-PARENT; the worker must run shell pwd.\n/exit\n"),
        )
        .await
    } else if override_root {
        run(
            &config_dir,
            &["--workdir", workdir_str, "Run shell pwd for PRX-PWD-CLI"],
        )
        .await
    } else {
        run(&config_dir, &["Run shell pwd for PRX-PWD-CLI"]).await
    };
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("test: server deadline")
        .expect("test: server task")
        .expect("test: server shutdown");
    assert!(status.success(), "test: real parent/worker failed: {output}");
    assert!(
        child_observed.load(Ordering::SeqCst),
        "test: real worker completed shell call: {output}"
    );
    assert_eq!(
        child_calls.load(Ordering::SeqCst),
        2,
        "test: worker shell executes exactly once"
    );
    let wire_result = captured.lock().clone();
    let parsed = serde_json::from_str::<Value>(&wire_result).ok();
    let actual = parsed
        .as_ref()
        .and_then(|value| value.get("output"))
        .and_then(Value::as_str)
        .unwrap_or(&wire_result);
    assert!(
        actual.starts_with("Process exited with code 0\nProcess started: true\nOutput complete: true\n"),
        "test: real successful shell process required: {actual}"
    );
    let actual = actual
        .split_once("stdout:\n")
        .and_then(|(_, output)| output.rsplit_once("\nstderr:").map(|(stdout, _)| stdout))
        .expect("test: native process stdout on wire");
    if override_root {
        assert_eq!(actual.trim(), workdir_str, "test: selected root reaches native shell");
    } else if worker {
        const MAX_WORKER_DIRECTORIES: usize = 2;
        let directories: Vec<_> = std::fs::read_dir(&workers)
            .expect("test: worker directory entries")
            .map(|entry| entry.expect("test: worker directory entry").path())
            .filter(|path| path.is_dir())
            .take(MAX_WORKER_DIRECTORIES)
            .collect();
        assert_eq!(directories.len(), 1, "test: exactly one real worker workspace");
        let expected = directories.first().expect("test: worker workspace");
        assert_eq!(
            std::path::Path::new(actual.trim()),
            expected,
            "test: unset workdir retains the exact worker workspace"
        );
    } else {
        assert_eq!(
            actual.trim(),
            workspace.to_str().expect("test: workspace UTF-8"),
            "test: unset CLI workdir retains workspace"
        );
    }
}
