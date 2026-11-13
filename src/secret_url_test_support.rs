//! Bounded real-wire fixtures for URL confidentiality regressions.
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub(crate) const DEADLINE: Duration = Duration::from_secs(15);
pub(crate) const MARKER: &str = "PRX_URL_SECRET_MARKER";
const MAX_REQUEST: usize = 8192;
const MAX_REQUESTS: usize = 2;

/// Capture complete HTTP headers before dropping or returning a real status.
pub(crate) async fn dropping_server(
    requests: usize,
    status: Option<u16>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    assert!(
        requests <= MAX_REQUESTS,
        "test: fixture request count must remain bounded"
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind owned listener");
    let address = listener.local_addr().expect("test: listener address");
    let task = tokio::spawn(async move {
        tokio::time::timeout(DEADLINE, async move {
            for _ in 0..requests {
                let (mut socket, _) = listener.accept().await.expect("test: accept HTTP request");
                let mut bytes = [0u8; MAX_REQUEST];
                let mut used = 0;
                loop {
                    assert!(used < MAX_REQUEST, "test: request headers exceeded fixture limit");
                    let count = socket
                        .read(
                            bytes
                                .get_mut(used..)
                                .expect("test: read offset fits bounded request buffer"),
                        )
                        .await
                        .expect("test: read request bytes");
                    assert!(count > 0, "test: peer closed before complete request headers");
                    used += count;
                    if bytes
                        .get(..used)
                        .expect("test: received bytes fit bounded request buffer")
                        .windows(4)
                        .any(|window| window == b"\r\n\r\n")
                    {
                        break;
                    }
                }
                let wire = std::str::from_utf8(bytes.get(..used).expect("test: request text fits bounded buffer"))
                    .expect("test: HTTP request text");
                assert!(
                    wire.contains(MARKER),
                    "test: real request must carry the query credential"
                );
                if let Some(status) = status {
                    socket
                        .write_all(
                            format!("HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        )
                        .await
                        .expect("test: send status");
                }
            }
        })
        .await
        .expect("test: wire fixture deadline");
    });
    (address, task)
}
