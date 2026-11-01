#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use super::RamfluxChannel;
use crate::channels::rich::RichChannel;
use crate::config::RamfluxConfig;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

mod approval;
mod crypto;
mod erase;
mod inbound;
mod protocol;
mod socket;
mod surface_schema;
mod turn_queue;

fn tighten_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("test: dir mode");
}

fn config_at(path: &Path) -> RamfluxConfig {
    RamfluxConfig {
        socket_path: Some(path.to_path_buf()),
        ..RamfluxConfig::default()
    }
}

struct Host {
    channel: RamfluxChannel,
    path: PathBuf,
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
    write: tokio::net::unix::OwnedWriteHalf,
    _inbound: tokio::sync::mpsc::Receiver<crate::channels::rich::RichInbound>,
    _dir: tempfile::TempDir,
}

impl Host {
    async fn connect(mut config: RamfluxConfig) -> Self {
        let dir = tempfile::tempdir().expect("test: temp dir");
        tighten_dir(dir.path());
        let path = dir.path().join("ramflux.sock");
        config.socket_path = Some(path.clone());
        let channel = RamfluxChannel::new(config, dir.path()).expect("test: channel");
        channel.start().await.expect("test: start socket");
        let listening = channel.clone();
        let (tx, inbound) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            if let Err(error) = listening.listen_rich(tx).await {
                tracing::debug!(error = %error, "test listener ended");
            }
        });
        let stream = wait_connect(&path).await;
        let (read, write) = stream.into_split();
        Self {
            channel,
            path,
            read: BufReader::new(read),
            write,
            _inbound: inbound,
            _dir: dir,
        }
    }

    async fn send(&mut self, value: &Value) {
        let bytes = serde_json::to_vec(value).expect("test: encode");
        self.write.write_all(&bytes).await.expect("test: write");
        self.write.write_all(b"\n").await.expect("test: newline");
        self.write.flush().await.expect("test: flush");
    }

    async fn recv(&mut self) -> Value {
        let frame = read_frame(&mut self.read).await.expect("test: frame");
        serde_json::from_slice(&frame).expect("test: json")
    }

    async fn hello(&mut self, authorization: &str) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "bridge.hello",
            "params": {
                "protocol_version": "1",
                "agent_account_id": "agent-a",
                "capabilities": ["bridge"],
                "authorization": authorization
            }
        }))
        .await;
        self.recv().await
    }

    async fn ping(&mut self) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "bridge.ping",
            "params": {}
        }))
        .await;
        let ack = self.recv().await;
        assert_eq!(ack["result"]["ok"], true);
    }
}

async fn wait_connect(path: &Path) -> UnixStream {
    for _ in 0..50 {
        if let Ok(stream) = UnixStream::connect(path).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("test: socket was not created at {}", path.display());
}

async fn read_frame(read: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    read.read_until(b'\n', &mut buf).await?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    Ok(buf)
}
