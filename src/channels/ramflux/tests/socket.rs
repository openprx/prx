use super::{Host, config_at};
use crate::channels::rich::RichChannel;
use crate::channels::traits::Channel;
use crate::config::RamfluxConfig;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[tokio::test]
async fn refuses_symlink() {
    let dir = tempfile::tempdir().expect("test: temp");
    super::tighten_dir(dir.path());
    let path = dir.path().join("ramflux.sock");
    std::os::unix::fs::symlink("/tmp/not-a-socket", &path).expect("test: symlink");
    let channel = super::super::RamfluxChannel::new(config_at(&path), dir.path()).expect("test: channel");
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let error = channel
        .listen_rich(tx)
        .await
        .expect_err("test: symlink must be refused");
    assert!(error.to_string().contains("symlink"), "{error}");
}

#[tokio::test]
async fn refuses_loose_permissions() {
    let dir = tempfile::tempdir().expect("test: temp");
    super::tighten_dir(dir.path());
    let path = dir.path().join("ramflux.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).expect("test: bind");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("test: chmod");
    let channel = super::super::RamfluxChannel::new(config_at(&path), dir.path()).expect("test: channel");
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let error = channel.listen_rich(tx).await.expect_err("test: 0644 must be refused");
    assert!(error.to_string().contains("0600"), "{error}");
}

#[tokio::test]
async fn created_socket_is_0600_and_rejects_a_second_connection() {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    let meta = std::fs::symlink_metadata(&host.path).expect("test: metadata");
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let mut second = UnixStream::connect(&host.path).await.expect("test: second connect");
    second.write_all(b"\n").await.expect("test: poke");
    let mut reader = BufReader::new(second);
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(2), reader.read_until(b'\n', &mut buf))
        .await
        .expect("test: second connection timed out")
        .expect("test: read");
    assert!(read > 0, "test: expected a rejection frame");
    let value: serde_json::Value = serde_json::from_slice(&buf).expect("test: rejection json");
    assert!(value["error"]["message"].as_str().unwrap_or("").contains("second"));
    let ack = host.hello("supervised").await;
    assert_eq!(ack["result"]["ok"], true);
    assert_eq!(ack["result"]["effective_autonomy"], "supervised");
}

#[tokio::test]
async fn default_socket_listens_when_the_workspace_is_0755() {
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).expect("test: mode");
    let channel = super::super::RamfluxChannel::new(RamfluxConfig::default(), dir.path()).expect("test: channel");
    channel.start().await.expect("test: default socket in a 0755 workspace");
    let parent = channel.socket_path().parent().expect("test: parent");
    let mode = std::fs::metadata(parent).expect("test: meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(channel.socket_path().ends_with("ramflux/run/ramflux.sock"));
}

#[tokio::test]
async fn refuses_a_0755_parent() {
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).expect("test: mode");
    let path = dir.path().join("ramflux.sock");
    let channel = super::super::RamfluxChannel::new(config_at(&path), dir.path()).expect("test: channel");
    let error = channel.start().await.expect_err("test: 0755 parent");
    assert!(error.to_string().contains("0700"), "{error}");
}

#[tokio::test]
async fn refuses_a_mismatched_peer_uid() {
    let dir = tempfile::tempdir().expect("test: temp");
    super::tighten_dir(dir.path());
    let path = dir.path().join("ramflux.sock");
    let channel = super::super::RamfluxChannel::new(config_at(&path), dir.path()).expect("test: channel");
    channel.set_expected_peer(Some(1));
    channel.start().await.expect("test: start");
    let mut stream = UnixStream::connect(&path).await.expect("test: connect");
    stream.write_all(b"\n").await.expect("test: poke");
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), reader.read_until(b'\n', &mut buf))
        .await
        .expect("test: peer refusal timed out");
    let value: serde_json::Value = serde_json::from_slice(&buf).expect("test: json");
    assert!(
        value["error"]["message"].as_str().unwrap_or("").contains("peer"),
        "{value}"
    );
}

#[tokio::test]
async fn health_requires_a_recent_ping() {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    assert!(!host.channel.health_check().await);
    host.hello("full").await;
    assert!(!host.channel.health_check().await);
    host.ping().await;
    assert!(host.channel.health_check().await);
    let report = host.channel.doctor_report();
    assert_eq!(report.authorization, "follow_rf→full");
    assert!(report.permission.contains("0600"));
}
