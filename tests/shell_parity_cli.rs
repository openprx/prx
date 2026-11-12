#![cfg(test)]
//! CLI rejection must precede any provider request.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::AsyncReadExt;

const CLI_OUTPUT_LIMIT: u64 = 64 * 1024;

async fn run(config: &std::path::Path, args: &[&str]) -> (std::process::ExitStatus, String) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(config)
        .arg("agent")
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", config.parent().expect("test: home"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("test: CLI spawn");
    let stdout = child.stdout.take().expect("test: stdout");
    let stderr = child.stderr.take().expect("test: stderr");
    let (status, out, err) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(
            child.wait(),
            async {
                let mut bytes = Vec::new();
                stdout
                    .take(CLI_OUTPUT_LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .expect("test: bounded stdout");
                assert!(bytes.len() as u64 <= CLI_OUTPUT_LIMIT, "test: CLI output exceeded cap");
                bytes
            },
            async {
                let mut bytes = Vec::new();
                stderr
                    .take(CLI_OUTPUT_LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .expect("test: bounded stderr");
                assert!(bytes.len() as u64 <= CLI_OUTPUT_LIMIT, "test: CLI output exceeded cap");
                bytes
            }
        )
    })
    .await
    .expect("test: CLI deadline");
    let mut text = String::from_utf8_lossy(&out).into_owned();
    text.push_str(&String::from_utf8_lossy(&err));
    (status.expect("test: CLI exit"), text)
}

#[tokio::test]
async fn invalid_workdir_exits_before_provider_request() {
    let root = tempfile::tempdir().expect("test: root");
    let config = root.path().join("config");
    std::fs::create_dir(&config).expect("test: config directory");
    let (status, help) = run(&config, &["--help"]).await;
    assert!(
        status.success() && help.contains("--workdir"),
        "test: actual agent help exposes workdir: {help}"
    );
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: fake provider bind");
    let address = listener.local_addr().expect("test: provider address");
    let stop = tokio_util::sync::CancellationToken::new();
    let stopping = stop.clone();
    let server = tokio::spawn(async move {
        let app = axum::Router::new().fallback(move || {
            requests.fetch_add(1, Ordering::SeqCst);
            async { "unexpected request" }
        });
        axum::serve(listener, app)
            .with_graceful_shutdown(stopping.cancelled_owned())
            .await
            .expect("test: provider server");
    });
    let config_text = format!(
        "default_provider = \"custom:http://{address}\"\ndefault_model = \"prx-test\"\n[runtime]\nworkdir = {:?}\n",
        root.path().to_string_lossy()
    );
    std::fs::write(config.join("config.toml"), &config_text).expect("test: config");
    for path in ["relative", root.path().join("missing").to_str().expect("test: path")] {
        let (status, output) = run(&config, &["--workdir", path, "hello"]).await;
        assert_eq!(status.code(), Some(2), "test: clap rejection: {output}");
    }
    let invalid = config_text.replace(
        &format!("workdir = {:?}", root.path().to_string_lossy()),
        "workdir = \"relative\"",
    );
    std::fs::write(config.join("config.toml"), invalid).expect("test: invalid runtime.workdir");
    let (status, output) = run(&config, &["hello"]).await;
    assert_eq!(status.code(), Some(1), "test: lifecycle rejection: {output}");
    assert!(output.contains("workdir"), "test: lifecycle diagnostic: {output}");
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("test: server stop deadline")
        .expect("test: server join");
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "test: invalid paths never reach provider"
    );
}

#[path = "shell_parity_cli/worker.rs"]
mod worker;
