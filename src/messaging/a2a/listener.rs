//! Dedicated HTTP/1.1 listener with identity from the TLS connection only.
use super::{
    identity, peers,
    tls::{self, Trust},
};
use crate::messaging::{
    MessageAuth, PrxMessage, SessionAddr,
    mailbox::{MailboxAuthority, MailboxError},
    runtime::MessagingRuntime,
};
use anyhow::Result;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::{
    Request, Response, StatusCode,
    body::{Bytes, Incoming},
};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

type Body = Full<Bytes>;
pub struct Listener {
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn response(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::from(value.to_string())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}
fn refused(status: StatusCode, code: &str) -> Response<Body> {
    response(status, serde_json::json!({"error":code}))
}
fn target_authority(
    runtime: &MessagingRuntime,
    peer: &crate::config::A2aPeer,
    target: &SessionAddr,
    mut source: SessionAddr,
) -> Result<MailboxAuthority> {
    anyhow::ensure!(peers::allows(peer, target, &runtime.instance), "peer_not_allowed");
    runtime.resolve_scope(target)?;
    crate::messaging::mailbox::validate_address(&source)?;
    source.instance.clone_from(&peer.spiffe_id);
    source.owner = peer.owner.clone();
    Ok(MailboxAuthority::verified_peer(
        runtime.workspace.clone(),
        source,
        target.clone(),
        MessageAuth {
            issuer: peer.spiffe_id.clone(),
            subject: peer.spiffe_id.clone(),
            method: "mtls".into(),
        },
    )?)
}
async fn request(
    trust: Trust,
    _runtime: Arc<MessagingRuntime>,
    cert: rustls::pki_types::CertificateDer<'static>,
    rate: Arc<super::resources::RequestRate>,
    request: Request<Incoming>,
) -> Response<Body> {
    let route = if request.method() == hyper::Method::POST {
        match request.uri().path() {
            "/a2a/v1/pins" => Some(true),
            "/a2a/v1/messages" => Some(false),
            _ => None,
        }
    } else {
        None
    };
    let bytes = match Limited::new(request.into_body(), 352 * 1024).collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => return refused(StatusCode::PAYLOAD_TOO_LARGE, "message_too_large"),
    };
    crate::runtime::blocking::spawn_blocking(move || process(trust, cert, rate, route, bytes))
        .await
        .unwrap_or_else(|_| refused(StatusCode::SERVICE_UNAVAILABLE, "mailbox_unavailable"))
}
fn process(
    trust: Trust,
    cert: rustls::pki_types::CertificateDer<'static>,
    rate: Arc<super::resources::RequestRate>,
    route: Option<bool>,
    bytes: Bytes,
) -> Response<Body> {
    if trust.refresh().is_err() {
        return refused(StatusCode::UNAUTHORIZED, "peer_unauthenticated");
    }
    let Ok(effective) = trust.effective() else {
        return refused(StatusCode::UNAUTHORIZED, "peer_unauthenticated");
    };
    if !effective.a2a.messaging_enabled {
        return refused(StatusCode::UNAUTHORIZED, "peer_unauthenticated");
    }
    let Ok(runtime) = MessagingRuntime::for_config(&effective) else {
        return refused(StatusCode::SERVICE_UNAVAILABLE, "mailbox_unavailable");
    };
    let Ok(peer) = trust.resolve(&cert, rustls::pki_types::UnixTime::now()) else {
        return refused(StatusCode::UNAUTHORIZED, "peer_unauthenticated");
    };
    if !rate.admit(&peer.spiffe_id) {
        let mut response = refused(StatusCode::TOO_MANY_REQUESTS, "inbox_full");
        response
            .headers_mut()
            .insert(hyper::header::RETRY_AFTER, hyper::header::HeaderValue::from_static("1"));
        return response;
    }
    let Some(pins) = route else {
        return refused(StatusCode::NOT_FOUND, "not_found");
    };
    if pins {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PinRequest {
            to: SessionAddr,
        }
        let Ok(input) = serde_json::from_slice::<PinRequest>(&bytes) else {
            return refused(StatusCode::BAD_REQUEST, "field_invalid");
        };
        let source = SessionAddr::new(peer.spiffe_id.clone(), &peer.owner, "peer", None);
        let Ok(authority) = target_authority(&runtime, &peer, &input.to, source) else {
            return refused(StatusCode::FORBIDDEN, "peer_not_allowed");
        };
        let result = runtime
            .resolve_scope(&input.to)
            .and_then(|scope| runtime.store.recipient_pin(&authority, &scope, &input.to.session));
        return result.map_or_else(
            |_| refused(StatusCode::FORBIDDEN, "peer_not_allowed"),
            |pin| {
                response(
                    StatusCode::OK,
                    serde_json::json!({"pin":pin,"expires_at":chrono::Utc::now().timestamp()+300}),
                )
            },
        );
    }
    let mut message: PrxMessage = match serde_json::from_slice(&bytes) {
        Ok(message) => message,
        Err(_) => return refused(StatusCode::BAD_REQUEST, "field_invalid"),
    };
    if message.body.bytes.len() > crate::messaging::MAX_BODY_BYTES {
        return refused(StatusCode::PAYLOAD_TOO_LARGE, "message_too_large");
    }
    let Ok(authority) = target_authority(&runtime, &peer, &message.to, message.from.clone()) else {
        return refused(StatusCode::FORBIDDEN, "peer_not_allowed");
    };
    message.hop_count = 0;
    message.ancestry.clear();
    let target = message.to.clone();
    match runtime.store.accept(&authority, message) {
        Ok(receipt) => {
            runtime.wake(&target);
            response(
                StatusCode::ACCEPTED,
                serde_json::json!({"message_id":receipt.message_id,"state":receipt.state,"auto_start":receipt.chain.depth<8}),
            )
        }
        Err(MailboxError::BodyTooLarge) => refused(StatusCode::PAYLOAD_TOO_LARGE, "message_too_large"),
        Err(MailboxError::QueueFull | MailboxError::Quota(_) | MailboxError::RateLimited) => {
            let mut result = refused(StatusCode::TOO_MANY_REQUESTS, "inbox_full");
            result
                .headers_mut()
                .insert(hyper::header::RETRY_AFTER, hyper::header::HeaderValue::from_static("1"));
            result
        }
        Err(MailboxError::ChainDepth) => refused(StatusCode::UNPROCESSABLE_ENTITY, "chain_limit_reached"),
        Err(
            MailboxError::UnknownParent
            | MailboxError::InvalidMetadata
            | MailboxError::IdempotencyConflict
            | MailboxError::IdReuse
            | MailboxError::InvalidAddress
            | MailboxError::InvalidMime
            | MailboxError::StreamIdRequired
            | MailboxError::SeqConflict
            | MailboxError::NaturalKeyCollision,
        ) => refused(StatusCode::BAD_REQUEST, "field_invalid"),
        Err(_) => refused(StatusCode::FORBIDDEN, "peer_not_allowed"),
    }
}
pub async fn start(config: &crate::config::Config, shutdown: CancellationToken) -> Result<Option<Listener>> {
    if !config.a2a.messaging_enabled {
        return Ok(None);
    }
    let configured = config.clone();
    let (trust, runtime) = crate::runtime::blocking::spawn_blocking(move || {
        let trust = Trust::new(&configured);
        trust.refresh()?;
        let local = trust.identity()?;
        peers::validate_local(&configured.a2a, &local)?;
        let _ = tls::server(trust.clone())?;
        Ok::<_, anyhow::Error>((trust, MessagingRuntime::for_config(&configured)?))
    })
    .await??;
    let listener = tokio::net::TcpListener::bind(&config.a2a.bind).await?;
    let total = Arc::new(tokio::sync::Semaphore::new(128));
    let registry = Arc::new(super::connections::Connections::default());
    let rate = Arc::new(super::resources::RequestRate::default());
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        let mut refresh = tokio::time::interval(Duration::from_millis(250));
        loop {
            let accepted = tokio::select! {
                () = shutdown.cancelled() => break,
                _ = connections.join_next(), if !connections.is_empty() => continue,
                _=refresh.tick()=> {
                    let _ = trust.refresh_async().await;
                    let config=trust.current();let local=trust.identity();
                    registry.recheck(config.as_ref().ok(),local.as_ref().ok().map(|identity|identity.fingerprint.as_str()));
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let (socket, _) = match accepted {
                Ok(value) => value,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = total.clone().try_acquire_owned() else {
                continue;
            };
            let trust = trust.clone();
            let runtime = runtime.clone();
            let registry = registry.clone();
            let rate = rate.clone();
            let shutdown = shutdown.clone();
            connections.spawn(async move {
                let _permit = permit;
                let builder = trust.clone();
                let Ok(Ok((server, local))) = crate::runtime::blocking::spawn_blocking(move || {
                    builder.refresh()?;
                    Ok::<_, anyhow::Error>((tls::server(builder.clone())?, builder.identity()?))
                })
                .await
                else {
                    return;
                };
                let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
                let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(socket)).await else {
                    return;
                };
                let Some(cert) = tls
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|chain| chain.first())
                    .cloned()
                else {
                    return;
                };
                let Ok(peer) = trust.resolve(&cert, rustls::pki_types::UnixTime::now()) else {
                    return;
                };
                let fingerprint = identity::fingerprint(cert.as_ref());
                let Some(permit) = registry.register(peer, fingerprint, local.fingerprint.clone()) else {
                    return;
                };
                let cancellation = permit.cancellation();
                let _peer = permit;
                let service_trust = trust.clone();
                let service = hyper::service::service_fn(move |req| {
                    let trust = service_trust.clone();
                    let runtime = runtime.clone();
                    let cert = cert.clone();
                    let rate = rate.clone();
                    async move {
                        Ok::<_, Infallible>(
                            tokio::time::timeout(Duration::from_secs(30), request(trust, runtime, cert, rate, req))
                                .await
                                .unwrap_or_else(|_| refused(StatusCode::REQUEST_TIMEOUT, "request_timeout")),
                        )
                    }
                });
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .timer(hyper_util::rt::TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(10))
                    .keep_alive(true)
                    .max_buf_size(64 * 1024);
                let connection = builder.serve_connection(
                    hyper_util::rt::TokioIo::new(super::resources::IdleIo::new(tls)),
                    service,
                );
                tokio::pin!(connection);
                tokio::select! {
                    _=&mut connection=>{},
                    ()=shutdown.cancelled()=>{},
                    ()=cancellation.cancelled()=>{},
                }
            });
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    });
    Ok(Some(Listener { task }))
}
