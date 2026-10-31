//! The decision HTTP client (CDP-PLAN 1.5; SP-CDP-A 5.8; AG-21).
//!
//! Every decision transport gets its `reqwest::Client` here, and nowhere
//! else under `src/decision/`:
//!
//! - it never follows a redirect;
//! - a remote backend is HTTPS only and goes through the runtime proxy under
//!   the service key `decision.<kind>`;
//! - a loopback backend ignores every proxy, including the `HTTP_PROXY`,
//!   `HTTPS_PROXY` and `ALL_PROXY` environment that `reqwest` honours by
//!   default, and resolves `localhost` to `127.0.0.1` itself, so no DNS
//!   lookup happens;
//! - the connect timeout is the smaller of the attempt timeout and one
//!   second; the bundled roots are used and no certificate check is ever
//!   disabled.
//!
//! Clients are cached per (locality, kind, timeout, localhost port).

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::Mutex;

use super::error::DecisionError;
use super::protocol::{BackendKind, BackendLocality};

/// The longest connect timeout.
const MAX_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// A cached client: its locality, kind, timeout, localhost port and the
/// runtime proxy generation it was built under.
type ClientKey = (BackendLocality, BackendKind, Duration, Option<u16>, u64);

static CLIENTS: OnceLock<Mutex<HashMap<ClientKey, reqwest::Client>>> = OnceLock::new();

/// The decision HTTP client factory.
#[derive(Debug, Clone, Copy)]
pub struct DecisionHttp;

impl DecisionHttp {
    /// The runtime-proxy service key of a remote backend of `kind`.
    #[must_use]
    pub fn proxy_service_key(kind: BackendKind) -> String {
        format!("decision.{}", kind.as_str())
    }

    /// The client for a backend of `kind` at `locality`, with `timeout` per
    /// attempt. `localhost_port` is the port of a loopback endpoint written
    /// as `localhost`, which is then resolved to `127.0.0.1` without DNS.
    ///
    /// # Errors
    ///
    /// [`DecisionError::Transport`] when the client cannot be built, and
    /// [`DecisionError::UnsupportedOperation`] for the builtin, which has no
    /// transport.
    pub fn client(
        locality: BackendLocality,
        kind: BackendKind,
        timeout: Duration,
        localhost_port: Option<u16>,
    ) -> Result<reqwest::Client, DecisionError> {
        Self::client_at(
            crate::config::runtime_proxy_generation(),
            locality,
            kind,
            timeout,
            localhost_port,
        )
    }

    /// [`Self::client`] under the runtime proxy generation `proxy`: a client
    /// built under an older generation is dropped and rebuilt, so a proxy
    /// reload reaches decision traffic (D-086 F-2).
    fn client_at(
        proxy: u64,
        locality: BackendLocality,
        kind: BackendKind,
        timeout: Duration,
        localhost_port: Option<u16>,
    ) -> Result<reqwest::Client, DecisionError> {
        let key = (locality, kind, timeout, localhost_port, proxy);
        let cache = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(client) = cache.lock().get(&key) {
            return Ok(client.clone());
        }
        let client = Self::build(locality, kind, timeout, localhost_port)?;
        let mut clients = cache.lock();
        clients.retain(|cached, _| cached.4 >= proxy);
        clients.insert(key, client.clone());
        Ok(client)
    }

    /// The loopback policy on top of `builder`: every proxy is dropped,
    /// including an explicit one and the environment's, plain HTTP is
    /// allowed, and `localhost` resolves to `127.0.0.1` without the resolver.
    fn loopback(builder: reqwest::ClientBuilder, localhost_port: Option<u16>) -> reqwest::ClientBuilder {
        let builder = builder.no_proxy().https_only(false);
        match localhost_port {
            Some(port) => builder.resolve("localhost", SocketAddr::from((Ipv4Addr::LOCALHOST, port))),
            None => builder,
        }
    }

    fn build(
        locality: BackendLocality,
        kind: BackendKind,
        timeout: Duration,
        localhost_port: Option<u16>,
    ) -> Result<reqwest::Client, DecisionError> {
        let builder = reqwest::Client::builder()
            .user_agent(crate::PRX_USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout.min(MAX_CONNECT_TIMEOUT));
        let builder = match locality {
            BackendLocality::Builtin => return Err(DecisionError::UnsupportedOperation),
            BackendLocality::Loopback => Self::loopback(builder, localhost_port),
            BackendLocality::Remote => {
                crate::config::apply_runtime_proxy_to_builder(builder.https_only(true), &Self::proxy_service_key(kind))
            }
        };
        builder.build().map_err(|error| {
            tracing::warn!(
                kind = kind.as_str(),
                locality = locality.as_str(),
                "decision HTTP client: {error}"
            );
            DecisionError::Transport
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn the_builtin_has_no_transport_and_clients_are_cached() {
        assert_eq!(
            DecisionHttp::client(
                BackendLocality::Builtin,
                BackendKind::Builtin,
                Duration::from_millis(250),
                None
            )
            .err(),
            Some(DecisionError::UnsupportedOperation)
        );
        let key = (
            BackendLocality::Loopback,
            BackendKind::CompatibleHttp,
            Duration::from_millis(251),
            Some(8900),
            u64::MAX - 1,
        );
        DecisionHttp::client_at(key.4, key.0, key.1, key.2, key.3).expect("test: client");
        assert!(CLIENTS.get().expect("test: cache").lock().contains_key(&key));
        assert_eq!(
            DecisionHttp::proxy_service_key(BackendKind::CompatibleHttp),
            "decision.compatible_http"
        );
    }

    /// D-086 F-2: a client built under an older runtime proxy generation is
    /// dropped and rebuilt (`config::schema` tests that every proxy change
    /// moves the generation).
    #[test]
    #[serial_test::serial]
    fn a_proxy_change_rebuilds_the_decision_clients() {
        let timeout = Duration::from_millis(257);
        let key = |proxy| {
            (
                BackendLocality::Remote,
                BackendKind::CompatibleHttp,
                timeout,
                None,
                proxy,
            )
        };
        let old = u64::MAX - 3;
        DecisionHttp::client_at(old, key(old).0, key(old).1, timeout, None).expect("test: client");
        assert!(CLIENTS.get().expect("test: cache").lock().contains_key(&key(old)));
        DecisionHttp::client_at(old + 1, key(old).0, key(old).1, timeout, None).expect("test: client");
        let cache = CLIENTS.get().expect("test: cache").lock();
        assert!(cache.contains_key(&key(old + 1)), "rebuilt under the new generation");
        assert!(!cache.contains_key(&key(old)), "the old client is dropped");
    }

    /// V2.05 transport row: a remote client never speaks plain HTTP; the
    /// request is refused before any connection.
    #[tokio::test]
    async fn a_remote_client_refuses_plain_http_before_connecting() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("test: bind");
        let port = listener.local_addr().expect("test: addr").port();
        let client = DecisionHttp::client(
            BackendLocality::Remote,
            BackendKind::CompatibleHttp,
            Duration::from_millis(300),
            None,
        )
        .expect("test: client");
        let result = client
            .post(format!("http://127.0.0.1:{port}/v1/systemone"))
            .send()
            .await;
        assert!(result.is_err(), "plain http is refused");
        let accepted = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
        assert!(accepted.is_err(), "no connection was made");
    }

    /// A one-shot HTTP stub: answers the first request with 204 and reports
    /// whether a connection arrived before `wait` ran out.
    async fn stub(listener: tokio::net::TcpListener, wait: Duration) -> bool {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Ok(Ok((mut stream, _))) = tokio::time::timeout(wait, listener.accept()).await else {
            return false;
        };
        let mut buffer = [0_u8; 1024];
        let _ = stream.read(&mut buffer).await;
        let _ = stream
            .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
        true
    }

    async fn bind() -> (tokio::net::TcpListener, u16) {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("test: bind");
        let port = listener.local_addr().expect("test: addr").port();
        (listener, port)
    }

    /// A loopback client drops every proxy, even one set on the builder
    /// before the policy (the environment proxy takes the same path).
    #[tokio::test]
    async fn a_loopback_client_ignores_every_proxy() {
        let (proxy, proxy_port) = bind().await;
        let (backend, backend_port) = bind().await;
        let proxied = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}")).expect("test: proxy"))
            .timeout(Duration::from_millis(500));
        let client = DecisionHttp::loopback(proxied, None).build().expect("test: client");
        let proxy_hit = tokio::spawn(stub(proxy, Duration::from_millis(300)));
        let backend_hit = tokio::spawn(stub(backend, Duration::from_millis(300)));
        let sent = client.get(format!("http://127.0.0.1:{backend_port}/")).send().await;
        assert!(sent.is_ok(), "the backend answered directly: {sent:?}");
        assert!(backend_hit.await.expect("test: backend stub"));
        assert!(!proxy_hit.await.expect("test: proxy stub"), "the proxy saw nothing");
    }

    /// A resolver that answers nothing: any DNS lookup fails.
    struct NoDns;

    impl reqwest::dns::Resolve for NoDns {
        fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
            Box::pin(async { Err::<reqwest::dns::Addrs, _>("test: no DNS".into()) })
        }
    }

    /// A loopback `localhost` endpoint reaches `127.0.0.1` without a DNS
    /// lookup.
    #[tokio::test]
    async fn a_loopback_localhost_endpoint_needs_no_dns() {
        let (backend, port) = bind().await;
        let no_dns = reqwest::Client::builder()
            .dns_resolver(std::sync::Arc::new(NoDns))
            .timeout(Duration::from_millis(500));
        let client = DecisionHttp::loopback(no_dns, Some(port))
            .build()
            .expect("test: client");
        let hit = tokio::spawn(stub(backend, Duration::from_millis(300)));
        let sent = client.get(format!("http://localhost:{port}/")).send().await;
        assert!(sent.is_ok(), "localhost resolved without DNS: {sent:?}");
        assert!(hit.await.expect("test: stub"));
    }
}
