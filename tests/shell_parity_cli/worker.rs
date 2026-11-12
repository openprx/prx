#![cfg(test)]
//! Real parent CLI -> signed process worker -> native shell -> loopback provider.
use super::*;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

#[tokio::test]
async fn cli_workdir_reaches_real_worker_shell_and_unset_keeps_worker_workspace() {
    tokio::time::timeout(Duration::from_secs(120), async {
        for override_root in [true, false] {
            worker_pwd(override_root).await;
        }
    })
    .await
    .expect("test: worker fixture deadline");
}

async fn worker_pwd(override_root: bool) {
    let root = tempfile::tempdir().expect("test: fixture root");
    let config_dir = root.path().join("config");
    let workspace = config_dir.join("workspace");
    let workdir = root.path().join("working-root");
    let workers = root.path().join("workers");
    let marker = root.path().join("pwd.txt");
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
    let marker_for_server = marker.clone();
    let app = axum::Router::new().fallback(axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
        let parent_calls = parent_calls.clone();
        let child_calls = child_count.clone();
        let child_observed = observed.clone();
        let marker = marker_for_server.clone();
        async move {
            let messages = request.get("messages").and_then(Value::as_array).expect("test: request messages");
            let text = request.to_string();
            let child = messages.iter().rev().find(|message| message.get("role").and_then(Value::as_str) == Some("user")).and_then(|message| message.get("content").and_then(Value::as_str)).is_some_and(|input| input.contains("PRX-PWD-CHILD"));
            let tool_call = |name: &str, arguments: Value| json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call-{}", uuid::Uuid::new_v4()),"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]});
            let assistant = if text.contains("Review the proposed final reply against the CURRENT request") {
                json!({"role":"assistant","content":r#"{"decision":"accept","reason":"fixture evidence","constraints":[]}"#})
            } else if text.contains("Classify the execution requirement of current_request") {
                json!({"role":"assistant","content":r#"{"requirement":"execution_required"}"#})
            } else if child {
                if child_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    tool_call("shell", json!({"command":format!("pwd > '{}'", marker.display())}))
                } else {
                    child_observed.store(true, Ordering::SeqCst);
                    json!({"role":"assistant","content":"PRX-PWD-DONE"})
                }
            } else {
                match parent_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => tool_call("sessions_spawn", json!({"action":"spawn_batch","mode":"process","tasks":["PRX-PWD-CHILD"]})),
                    1 => {
                        let batch = messages.iter().rev().filter(|message| message.get("role").and_then(Value::as_str) == Some("tool")).filter_map(|message| message.get("content").and_then(Value::as_str)).filter_map(|content| serde_json::from_str::<Value>(content).ok()).find(|result| result.get("batch_id").is_some_and(Value::is_string)).unwrap_or_else(|| panic!("test: actual spawn batch; tool results: {:?}", messages.iter().filter(|message| message.get("role").and_then(Value::as_str) == Some("tool")).take(4).map(|message| message.get("content").and_then(Value::as_str).unwrap_or_default().chars().take(4096).collect::<String>()).collect::<Vec<_>>()));
                        tool_call("sessions_spawn", json!({"action":"join","batch_id":batch.get("batch_id").expect("test: batch identifier")}))
                    }
                    _ => json!({"role":"assistant","content":"PRX-PWD-DONE"}),
                }
            };
            let body = json!({"id":"prx-pwd-response","object":"chat.completion","model":"prx-pwd-model","choices":[{"index":0,"message":assistant,"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
            axum::Json(body)
        }
    }));
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
    config.causal_tree.enabled = false;
    config.secrets.encrypt = false;
    config.memory.auto_save = false;
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
    let args = if override_root {
        vec![
            "--workdir",
            workdir_str,
            "Use sessions_spawn process mode for PRX-PWD-PARENT",
        ]
    } else {
        vec!["Use sessions_spawn process mode for PRX-PWD-PARENT"]
    };
    let (status, output) = run(&config_dir, &args).await;
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
    let actual = std::fs::read_to_string(marker).expect("test: real shell pwd marker");
    if override_root {
        assert_eq!(actual.trim(), workdir_str, "test: signed CLI root reaches worker");
    } else {
        assert!(
            std::path::Path::new(actual.trim()).starts_with(&workers),
            "test: unset workdir retains worker workspace: {actual}"
        );
    }
}
