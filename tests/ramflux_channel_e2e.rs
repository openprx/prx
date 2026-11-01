//! Socket-level ramflux checks.
//!
//! The turn itself is driven by `process_channel_message` in
//! `channels::tests::ramflux_real_turn`, which is the production path against
//! this same socket. This file covers frames that never start a model turn.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use openprx::channels::RamfluxChannel;
use openprx::config::RamfluxConfig;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn connect(
    path: &std::path::Path,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(connected) = UnixStream::connect(path).await {
            stream = Some(connected);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (read, write) = stream.expect("test: socket").into_split();
    (BufReader::new(read), write)
}

#[tokio::test]
async fn malformed_null_id_and_version_mismatch_get_jsonrpc_errors() {
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("test: mode");
    let channel = RamfluxChannel::new(
        RamfluxConfig {
            socket_path: Some(dir.path().join("ramflux.sock")),
            ..RamfluxConfig::default()
        },
        dir.path(),
    )
    .expect("test: channel");
    channel.start().await.expect("test: start");
    let path = channel.socket_path().to_path_buf();
    {
        let (mut read, mut write) = connect(&path).await;
        write.write_all(b"{not json\n").await.expect("test: write");
        write.flush().await.expect("test: flush");
        let mut buf = Vec::new();
        read.read_until(b'\n', &mut buf).await.expect("test: read");
        let parsed: Value = serde_json::from_slice(&buf).expect("test: json");
        assert_eq!(parsed["id"], Value::Null);
        assert_eq!(parsed["error"]["code"], -32700);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let (mut read, mut write) = connect(&path).await;
        let null_id = json!({"jsonrpc":"2.0","id":null,"method":"bridge.ping","params":{}});
        let bytes = serde_json::to_vec(&null_id).expect("test: encode");
        write.write_all(&bytes).await.expect("test: write");
        write.write_all(b"\n").await.expect("test: nl");
        write.flush().await.expect("test: flush");
        let mut buf = Vec::new();
        read.read_until(b'\n', &mut buf).await.expect("test: read");
        let parsed: Value = serde_json::from_slice(&buf).expect("test: json");
        assert_eq!(parsed["error"]["code"], -32600);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let (mut read, mut write) = connect(&path).await;
        let bad_version = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "bridge.hello",
            "params": {
                "protocol_version": "9",
                "agent_account_id": "agent-a",
                "capabilities": ["bridge"],
                "authorization": "supervised"
            }
        });
        let bytes = serde_json::to_vec(&bad_version).expect("test: encode");
        write.write_all(&bytes).await.expect("test: write");
        write.write_all(b"\n").await.expect("test: nl");
        write.flush().await.expect("test: flush");
        let mut buf = Vec::new();
        read.read_until(b'\n', &mut buf).await.expect("test: read");
        let parsed: Value = serde_json::from_slice(&buf).expect("test: json");
        assert_eq!(parsed["error"]["code"], -32003);
    }
}

#[tokio::test]
async fn oversize_frame_and_unknown_method_are_rejected() {
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("test: mode");
    let channel = RamfluxChannel::new(
        RamfluxConfig {
            socket_path: Some(dir.path().join("ramflux.sock")),
            ..RamfluxConfig::default()
        },
        dir.path(),
    )
    .expect("test: channel");
    channel.start().await.expect("test: start");
    let path = channel.socket_path().to_path_buf();
    {
        let (mut read, mut write) = connect(&path).await;
        let writing = tokio::spawn(async move {
            let frame = vec![b'{'; 1024 * 1024 + 64];
            let _ = write.write_all(&frame).await;
        });
        let mut buf = Vec::new();
        let read_result = tokio::time::timeout(Duration::from_secs(5), read.read_until(b'\n', &mut buf)).await;
        writing.abort();
        let parsed: Value = serde_json::from_slice(&buf).expect("test: oversize json");
        assert!(read_result.is_ok(), "oversize frame produced no error");
        assert_eq!(parsed["id"], Value::Null);
        assert_eq!(parsed["error"]["code"], -32600);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let (mut read, mut write) = connect(&path).await;
        let hello = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "bridge.hello",
            "params": {
                "protocol_version": "1",
                "agent_account_id": "agent-a",
                "capabilities": ["bridge"],
                "authorization": "full"
            }
        });
        let bytes = serde_json::to_vec(&hello).expect("test: encode");
        write.write_all(&bytes).await.expect("test: write");
        write.write_all(b"\n").await.expect("test: nl");
        write.flush().await.expect("test: flush");
        let mut buf = Vec::new();
        read.read_until(b'\n', &mut buf).await.expect("test: hello");
        buf.clear();
        let unknown = json!({"jsonrpc":"2.0","id":2,"method":"no.such","params":{}});
        let bytes = serde_json::to_vec(&unknown).expect("test: encode");
        write.write_all(&bytes).await.expect("test: write");
        write.write_all(b"\n").await.expect("test: nl");
        write.flush().await.expect("test: flush");
        read.read_until(b'\n', &mut buf).await.expect("test: unknown");
        let parsed: Value = serde_json::from_slice(&buf).expect("test: json");
        assert_eq!(parsed["error"]["code"], -32601);
    }
}
