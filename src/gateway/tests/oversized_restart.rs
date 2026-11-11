use super::*;
use std::io::{BufRead, Read, Write};
const MAX_CHILD_OUTPUT: u64 = 16_384;
const MAX_RESPONSE: usize = IDEMPOTENCY_MAX_REPLAY_BYTES + 16_384;
const TEST_DEADLINE: Duration = Duration::from_secs(90);
use std::process::{Child, Stdio};

struct DurableCountingProvider(std::path::PathBuf);

#[async_trait]
impl Provider for DurableCountingProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        let mut count = std::fs::OpenOptions::new().create(true).append(true).open(&self.0)?;
        count.write_all(b"call\n")?;
        count.sync_all()?;
        Ok(format!(
            "{} Bearer {}",
            "x".repeat(IDEMPOTENCY_MAX_REPLAY_BYTES + 1),
            Uuid::new_v4()
        ))
    }
}

#[tokio::test]
#[ignore = "child fixture invoked by oversized_terminal_survives_process_restart"]
async fn oversized_webhook_child() {
    let workspace =
        std::path::PathBuf::from(std::env::var_os("PRX_OVERSIZED_WORKSPACE").expect("test: child workspace"));
    let provider = Arc::new(DurableCountingProvider(workspace.join("provider.calls")));
    let mut state = webhook_test_state_at(provider, workspace);
    install_webhook_turn_runtime(&mut state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: fixture operation");
    let address = listener.local_addr().expect("test: fixture operation");
    let router = axum::Router::new()
        .route("/webhook", axum::routing::post(handle_webhook))
        .with_state(state);
    println!("PRX_OVERSIZED_READY {address}");
    std::io::stdout().flush().expect("test: fixture operation");
    tokio::time::timeout(TEST_DEADLINE, async {
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await
    })
    .await
    .expect("test: child process deadline")
    .expect("test: serve webhook");
}

struct WebhookChild {
    child: tokio::process::Child,
    reader: tokio::task::JoinHandle<()>,
}

impl WebhookChild {
    async fn stop(mut self) {
        self.child.kill().await.expect("test: kill and reap webhook child");
        self.reader.await.expect("test: join child output reader");
    }
}

async fn spawn_webhook_child(workspace: &std::path::Path) -> (WebhookChild, String) {
    let mut child = tokio::process::Command::new(std::env::current_exe().expect("test: current executable"))
        .args([
            "--exact",
            "gateway::tests::oversized_restart::oversized_webhook_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PRX_OVERSIZED_WORKSPACE", workspace)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("test: start webhook child");
    let stdout = child.stdout.take().expect("test: child stdout");
    let (send, receive) = tokio::sync::oneshot::channel();
    let reader = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout.take(MAX_CHILD_OUTPUT + 1)).lines();
        let mut consumed = 0_u64;
        let mut send = Some(send);
        while let Some(line) = lines.next_line().await.expect("test: child output") {
            consumed += line.len() as u64 + 1;
            assert!(consumed <= MAX_CHILD_OUTPUT, "test: child output exceeded its budget");
            if let Some(address) = line.strip_prefix("PRX_OVERSIZED_READY ")
                && let Some(send) = send.take()
            {
                let _ = send.send(address.to_owned());
            }
        }
    });
    let owner = WebhookChild { child, reader };
    let address = tokio::time::timeout(Duration::from_secs(30), receive)
        .await
        .expect("test: webhook startup deadline")
        .expect("test: webhook startup acknowledgement");
    (owner, format!("http://{address}/webhook"))
}

#[tokio::test]
async fn oversized_terminal_survives_process_restart() {
    tokio::time::timeout(TEST_DEADLINE, restart_scenario())
        .await
        .expect("test: restart scenario deadline");
}

fn field<'a>(value: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    value.get(name).expect("test: gateway response field missing")
}

async fn read_json(mut response: reqwest::Response) -> serde_json::Value {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.expect("test: read HTTP chunk") {
        assert!(
            body.len().saturating_add(chunk.len()) <= MAX_RESPONSE,
            "test: HTTP response exceeded fixture budget"
        );
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).expect("test: decode HTTP JSON")
}

async fn restart_scenario() {
    let workspace = tempfile::tempdir().expect("test: fixture operation");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("test: fixture operation");
    let request = |url: String| {
        client
            .post(url)
            .header("X-Idempotency-Key", "prx-oversized-restart")
            .json(&serde_json::json!({"message": "hello"}))
    };
    let (first_process, url) = spawn_webhook_child(workspace.path()).await;
    let first = request(url.clone()).send().await.expect("test: fixture operation");
    assert_eq!(
        first.status(),
        StatusCode::OK,
        "test: initial oversized turn must settle"
    );
    let first: serde_json::Value = read_json(first).await;
    assert_eq!(
        field(&first, "response")
            .as_str()
            .expect("test: fixture operation")
            .len(),
        IDEMPOTENCY_MAX_REPLAY_BYTES + 1 + 8 + 36,
        "test: initial response must retain the full reply"
    );
    let reply = field(&first, "response").as_str().expect("test: first response text");
    assert_ne!(
        crate::agent::terminal::redact_persisted_text(reply),
        reply,
        "test: fixture must exercise persistence redaction before digest-only storage"
    );
    let duplicate = request(url).send().await.expect("test: fixture operation");
    assert_eq!(
        duplicate.status(),
        StatusCode::CONFLICT,
        "test: in-process replay must be unavailable"
    );
    let duplicate: serde_json::Value = read_json(duplicate).await;
    assert_eq!(
        field(&duplicate, "status").as_str(),
        Some("replay_unavailable"),
        "test: original unavailable contract"
    );
    let expected_hash = webhook_result_hash(reply, "test-model");
    assert_eq!(
        field(&duplicate, "result_hash").as_str(),
        Some(expected_hash.as_str()),
        "test: result hash must preserve the original response before redaction"
    );
    first_process.stop().await;
    let (restarted, url) = spawn_webhook_child(workspace.path()).await;
    let replay = request(url).send().await.expect("test: fixture operation");
    assert_eq!(
        replay.status(),
        StatusCode::CONFLICT,
        "test: restarted process must refuse reexecution"
    );
    let replay: serde_json::Value = read_json(replay).await;
    assert_eq!(
        field(&replay, "status").as_str(),
        Some("replay_unavailable"),
        "test: durable unavailable contract"
    );
    assert_eq!(
        field(&replay, "result_hash"),
        field(&duplicate, "result_hash"),
        "test: restart changed result digest"
    );
    assert_eq!(
        field(&replay, "response_id"),
        field(&duplicate, "response_id"),
        "test: restart changed terminal identity"
    );
    assert_eq!(
        {
            let mut text = String::new();
            std::fs::File::open(workspace.path().join("provider.calls"))
                .expect("test: provider counter")
                .take(64)
                .read_to_string(&mut text)
                .expect("test: bounded counter read");
            text
        },
        "call\n",
        "test: restart must not call provider again"
    );
    restarted.stop().await;
}
