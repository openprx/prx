#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]
use super::*;
use serde_json::json;

fn id_only(ids: &[String]) -> Vec<DiscoveredModel> {
    ids.iter().map(|id| DiscoveredModel::id_only(id)).collect()
}
use std::io::{BufRead, Write};
use tempfile::TempDir;

const DISCOVERY_ROOT_SENTINEL: &str = "C0-DISCOVERY-ROOT-SENTINEL";

fn discovery_config(temp: &TempDir) -> Config {
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.secrets.encrypt = false;
    config.default_provider = Some("openai".to_string());
    config.api_key = Some(DISCOVERY_ROOT_SENTINEL.to_string());
    config.api_url = Some("http://127.0.0.1:9/v1".to_string());
    config
}

// ── model discovery off the async runtime ───────────────────

/// Serve exactly one HTTP exchange on loopback and then stop.
///
/// Mirrors the hand-rolled listener fixtures already used across the tree;
/// no mock-HTTP dependency is introduced.
fn spawn_canned_discovery_endpoint(status_line: &str, body: &str) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let response = format!(
        "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let handle = std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let Ok(peer) = stream.try_clone() else {
            return;
        };
        let mut reader = std::io::BufReader::new(peer);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });

    (addr, handle)
}

/// Hold every incoming connection open without replying until the test
/// raises `stop`, then drop them all.
///
/// The accept loop is non-blocking so the fixture terminates even when the
/// cancelled task never gets far enough to open a connection.
fn spawn_silent_discovery_endpoint(
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();

    let handle = std::thread::spawn(move || {
        let mut held = Vec::new();
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => held.push(stream),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        drop(held);
    });

    (addr, handle)
}

// ── USERINFO-2: onboarding and `prx models` name the provider without
//    URL userinfo ─────────────────────────────────────────────────────

const USERINFO_LABEL: &str = "custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1";
mod models;
mod persistence;
mod preferences;
mod providers;
mod workspace;
