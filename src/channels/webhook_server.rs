//! Serving a channel's webhook endpoint for exactly as long as its listener
//! runs (D-072 M-2).
//!
//! The channel runtime stops a listener by dropping its `listen` future. The
//! HTTP server inside it must then stop taking requests for that listener:
//! release the port, and end the keep-alive connections it already accepted,
//! which the server serves from tasks of their own. Those tasks hold the old
//! listener's message sender, so a connection that outlived its listener
//! would keep delivering into a generation that is gone.

use tokio_util::sync::CancellationToken;

/// Serve `app` on `listener` until this future completes or is dropped.
///
/// Dropping the future starts a graceful shutdown: the listening socket is
/// closed, every accepted connection finishes the request it is serving and
/// closes instead of waiting for the next one.
///
/// # Errors
///
/// The error the HTTP server returns.
pub(crate) async fn serve_until_dropped(listener: tokio::net::TcpListener, app: axum::Router) -> std::io::Result<()> {
    let stop = CancellationToken::new();
    let _stop_on_drop = stop.clone().drop_guard();
    axum::serve(listener, app)
        .with_graceful_shutdown(stop.cancelled_owned())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Read from `stream` until `needle` has arrived, or fail on EOF.
    async fn read_until(stream: &mut tokio::net::TcpStream, needle: &str) -> String {
        let mut seen = Vec::new();
        let mut buf = [0_u8; 1024];
        loop {
            let n = stream.read(&mut buf).await.expect("test: read");
            assert!(n > 0, "test: the server closed before `{needle}`");
            seen.extend_from_slice(buf.get(..n).unwrap_or_default());
            let text = String::from_utf8_lossy(&seen).into_owned();
            if text.contains(needle) {
                return text;
            }
        }
    }

    /// D-072 M-2: when the listener that runs a webhook server is dropped (a
    /// channel-runtime restart), a keep-alive connection it accepted earlier
    /// is closed, and the port is free again.
    #[tokio::test]
    async fn dropping_the_server_closes_its_keep_alive_connections_and_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let address = listener.local_addr().expect("test: addr");
        let app = axum::Router::new().route("/hook", axum::routing::get(|| async { "delivered" }));
        let server = tokio::spawn(serve_until_dropped(listener, app));

        let mut connection = tokio::net::TcpStream::connect(address).await.expect("test: connect");
        connection
            .write_all(b"GET /hook HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("test: request");
        let response = read_until(&mut connection, "delivered").await;
        assert!(
            !response.to_ascii_lowercase().contains("connection: close"),
            "the connection is kept alive: {response}"
        );

        // What the channel runtime does on a restart: drop the listener.
        server.abort();
        assert!(server.await.is_err_and(|error| error.is_cancelled()));

        let mut buf = [0_u8; 64];
        let closed = tokio::time::timeout(Duration::from_mins(1), connection.read(&mut buf))
            .await
            .expect("the keep-alive connection is closed when its server is dropped");
        assert!(
            matches!(closed, Ok(0) | Err(_)),
            "the keep-alive connection ended: {closed:?}"
        );
        tokio::net::TcpListener::bind(address)
            .await
            .expect("the port is released when the server is dropped");
    }
}
