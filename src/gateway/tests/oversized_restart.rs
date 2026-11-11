use super::*;
use std::io::{BufRead, Write};
use std::process::{Child, Stdio};

struct DurableCountingProvider(std::path::PathBuf);

#[async_trait]
impl Provider for DurableCountingProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        let mut count = std::fs::OpenOptions::new().create(true).append(true).open(&self.0)?;
        count.write_all(b"call\n")?;
        count.sync_all()?;
        Ok("x".repeat(IDEMPOTENCY_MAX_REPLAY_BYTES + 1))
    }
}

#[tokio::test]
#[ignore = "child fixture invoked by oversized_terminal_survives_process_restart"]
async fn oversized_webhook_child() {
    let workspace = std::path::PathBuf::from(std::env::var_os("PRX_OVERSIZED_WORKSPACE").expect("child workspace"));
    let provider = Arc::new(DurableCountingProvider(workspace.join("provider.calls")));
    let mut state = webhook_test_state_at(provider, workspace);
    install_webhook_turn_runtime(&mut state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route("/webhook", axum::routing::post(handle_webhook))
        .with_state(state);
    println!("PRX_OVERSIZED_READY {address}");
    std::io::stdout().flush().unwrap();
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

struct WebhookChild {
    child: Child,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WebhookChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        self.child.wait().expect("reap webhook child");
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join child output reader");
        }
    }
}

fn spawn_webhook_child(workspace: &std::path::Path) -> (WebhookChild, String) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "gateway::tests::oversized_restart::oversized_webhook_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PRX_OVERSIZED_WORKSPACE", workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start webhook child");
    let stdout = child.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.expect("child output");
            if let Some(address) = line.strip_prefix("PRX_OVERSIZED_READY ") {
                let _ = send.send(address.to_owned());
            }
        }
    });
    let owner = WebhookChild {
        child,
        reader: Some(reader),
    };
    let address = receive
        .recv_timeout(Duration::from_secs(30))
        .expect("webhook startup acknowledgement");
    (owner, format!("http://{address}/webhook"))
}

#[tokio::test]
async fn oversized_terminal_survives_process_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let request = |url: String| {
        client
            .post(url)
            .header("X-Idempotency-Key", "prx-oversized-restart")
            .json(&serde_json::json!({"message": "hello"}))
    };
    let (first_process, url) = spawn_webhook_child(workspace.path());
    let first = request(url.clone()).send().await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first: serde_json::Value = first.json().await.unwrap();
    assert_eq!(
        first["response"].as_str().unwrap().len(),
        IDEMPOTENCY_MAX_REPLAY_BYTES + 1
    );
    let duplicate = request(url).send().await.unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    let duplicate: serde_json::Value = duplicate.json().await.unwrap();
    assert_eq!(duplicate["status"], "replay_unavailable");
    assert_eq!(
        duplicate["result_hash"],
        webhook_result_hash(first["response"].as_str().unwrap(), "test-model")
    );
    drop(first_process);
    let (restarted, url) = spawn_webhook_child(workspace.path());
    let replay = request(url).send().await.unwrap();
    assert_eq!(replay.status(), StatusCode::CONFLICT);
    let replay: serde_json::Value = replay.json().await.unwrap();
    assert_eq!(replay["status"], "replay_unavailable");
    assert_eq!(replay["result_hash"], duplicate["result_hash"]);
    assert_eq!(replay["response_id"], duplicate["response_id"]);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("provider.calls")).unwrap(),
        "call\n"
    );
    drop(restarted);
}
