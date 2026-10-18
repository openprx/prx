//! The System One HTTP transport (CDP 4.3 items 2 to 4; CDP-PLAN 1.5;
//! SP-CDP-A 5.8, 5.9; SP-CDP-C V2.01, V2.03, V2.05, V2.06): the compatible
//! backend over loopback or HTTPS, and the hosted TypeSafe
//! ([`super::typesafe`]) and OpenRouter ([`super::openrouter`]) adapters,
//! which differ from it only in what their modules document.
//!
//! - `POST {endpoint}{route}` with the System One wire codec. The endpoint is
//!   the backend's base URL. On the System One route (compatible, TypeSafe)
//!   a trailing `/v1` or `/v1/systemone` is not doubled (R-I1-phase2 R-3);
//!   OpenRouter's Decisions route keeps the override's path as written, so an
//!   override ending in `/v1` keeps it (R-I1-confirm). A hosted backend
//!   without an endpoint uses its service's base URL.
//! - The kind sets the locality (COORD-6): a hosted backend is remote, so
//!   `remote_enabled` gates it and it is never retried, even when its
//!   endpoint override is a loopback stub; the transport follows the URL (a
//!   loopback override is reached directly, from a loopback peer only).
//! - A hosted backend is pinned (`ModelVersionPolicy::ExactPinned`, D-043
//!   C-8) to `pinned_snapshot`, or to the configured model; the compatible
//!   backend is not.
//! - The provider request id (a header, or OpenRouter's body `id`) is kept
//!   only when it is at most 128 characters of `[A-Za-z0-9_-]` (DIFFS D-15).
//! - `authorization: Bearer ..` only when a credential was resolved; the
//!   plaintext is read only when the header is set, and `Debug` shows only
//!   whether one is present.
//! - Status classes: 400/422 invalid request, 401/403 authentication,
//!   404/405/501 unsupported, 408/504 timeout, 429 rate limited (with the
//!   wait the backend asks for), any other status unavailable; connection
//!   refused is unavailable, any other transport failure is transport.
//! - A body longer than `max_response_bytes` is an invalid response and the
//!   connection is dropped.
//! - A loopback response must come from a loopback peer.
//! - One retry, only on loopback, only for transport or unavailable, only
//!   when the remaining deadline covers the wait and a minimum attempt. A
//!   remote backend is never retried (a decision may be billed); the factory
//!   falls back instead.
//! - Errors carry the class only; the sanitized body goes to a debug log
//!   with the endpoint's user information redacted.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::backend::DecisionBackend;
use super::budget::{AttemptBinding, DecisionAttemptAccounting};
use super::error::{DecisionError, PolicyRule, RequestRule, ValidationRule};
use super::factory::BackendSpec;
use super::http::DecisionHttp;
use super::key::{DecisionKey, KeyDomain};
use super::loopback::check_endpoint;
use super::outbound::GuardedBody;
use super::protocol::{BackendIdentity, BackendKind, BackendLocality, DecisionRequest, DecisionResponse};
use super::validate::ModelVersionPolicy;
use super::wire::{ResponseEnvelope, SystemOneCodec};
use super::{openrouter, typesafe};
use crate::providers::catalog::ResolvedCredential;

/// The System One route under the endpoint.
const ROUTE: &str = "/v1/systemone";

/// The longest provider request id kept (DIFFS D-15).
const MAX_REQUEST_ID_CHARS: usize = 128;

/// The shortest retry worth starting.
const MIN_ATTEMPT: Duration = Duration::from_millis(20);

/// The request-id header a compatible endpoint may return.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// Where a failed attempt leaves the retry decision.
struct Failure {
    error: DecisionError,
    retry_after: Option<Duration>,
}

impl From<DecisionError> for Failure {
    fn from(error: DecisionError) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

/// The service a System One HTTP backend talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    Compatible,
    TypeSafe,
    OpenRouter,
}

impl Service {
    const fn of(kind: BackendKind) -> Option<Self> {
        match kind {
            BackendKind::CompatibleHttp => Some(Self::Compatible),
            BackendKind::TypeSafe => Some(Self::TypeSafe),
            BackendKind::OpenRouter => Some(Self::OpenRouter),
            BackendKind::Builtin => None,
            #[cfg(any(test, feature = "test-mock"))]
            BackendKind::Mock => None,
        }
    }

    /// The adapter's name on the wire envelope.
    const fn name(self) -> &'static str {
        match self {
            Self::Compatible => CompatibleHttpBackend::NAME,
            Self::TypeSafe => typesafe::NAME,
            Self::OpenRouter => openrouter::NAME,
        }
    }

    /// The base URL a backend without an endpoint uses; the compatible
    /// backend always has one.
    const fn default_base(self) -> Option<&'static str> {
        match self {
            Self::Compatible => None,
            Self::TypeSafe => Some(typesafe::DEFAULT_BASE),
            Self::OpenRouter => Some(openrouter::DEFAULT_BASE),
        }
    }

    const fn route(self) -> &'static str {
        match self {
            Self::Compatible => ROUTE,
            Self::TypeSafe => typesafe::ROUTE,
            Self::OpenRouter => openrouter::ROUTE,
        }
    }

    /// The response header that carries the provider request id; OpenRouter
    /// puts it in the body.
    const fn request_id_header(self) -> Option<&'static str> {
        match self {
            Self::Compatible => Some(REQUEST_ID_HEADER),
            Self::TypeSafe => Some(typesafe::REQUEST_ID_HEADER),
            Self::OpenRouter => None,
        }
    }

    const fn hosted(self) -> bool {
        matches!(self, Self::TypeSafe | Self::OpenRouter)
    }
}

/// A System One backend over HTTP: the compatible backend, or a hosted
/// adapter.
pub struct CompatibleHttpBackend {
    priced_provider: Arc<str>,
    priced_model: Arc<str>,
    transport_binding: [u8; 32],
    identity: BackendIdentity,
    service: Service,
    /// Where the backend counts as running: what `remote_enabled`, retries
    /// and health probes follow.
    locality: BackendLocality,
    /// Where its URL points: what the client and the peer check follow.
    transport: BackendLocality,
    version_policy: ModelVersionPolicy,
    url: String,
    client: reqwest::Client,
    credential: ResolvedCredential,
    key: Arc<DecisionKey>,
    max_response_bytes: u32,
}

impl std::fmt::Debug for CompatibleHttpBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompatibleHttpBackend")
            .field("identity", &self.identity.kind.as_str())
            .field("service", &self.service.name())
            .field("locality", &self.locality.as_str())
            .field("transport", &self.transport.as_str())
            .field("url", &crate::providers::redact_endpoint_userinfo(&self.url))
            .field(
                "credential",
                &if self.credential.api_key().is_some() {
                    "present"
                } else {
                    "absent"
                },
            )
            .finish_non_exhaustive()
    }
}

/// The URL `service` is reached at: the configured endpoint, or a hosted
/// service's own base URL when none is configured.
fn base_url(service: Service, endpoint: &str) -> &str {
    match (endpoint.trim(), service.default_base()) {
        ("", Some(base)) => base,
        (endpoint, _) => endpoint,
    }
}

/// `endpoint` with `route`, not doubled. On the System One route the
/// endpoint is a base URL, and a trailing `/v1` or `/v1/systemone` is not
/// repeated (R-I1-phase2 R-3). Any other route (OpenRouter's) keeps the
/// endpoint's path as written: only the route itself is not doubled.
fn route_url(endpoint: &str, route: &str) -> String {
    let base = endpoint.trim().trim_end_matches('/');
    let base = if route == ROUTE {
        base.strip_suffix(ROUTE)
            .or_else(|| base.strip_suffix("/v1"))
            .unwrap_or(base)
    } else {
        base.strip_suffix(route).unwrap_or(base)
    };
    format!("{base}{route}")
}

/// `text` as a provider request id: kept only when it is 1 to 128
/// characters of `[A-Za-z0-9_-]` (DIFFS D-15); it is diagnostic only.
pub(super) fn provider_request_id(text: &str) -> Option<&str> {
    let valid = !text.is_empty()
        && text.len() <= MAX_REQUEST_ID_CHARS
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    valid.then_some(text)
}

/// The class of a transport failure.
fn transport_class(error: &reqwest::Error) -> DecisionError {
    if error.is_timeout() {
        return DecisionError::Timeout;
    }
    let refused = std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
        || error.to_string().contains("Connection refused");
    if error.is_connect() && refused {
        DecisionError::Unavailable
    } else {
        DecisionError::Transport
    }
}

/// The wait a refusal asks for: `retry-after-ms` first, then
/// `retry-after` in seconds (D-042), capped at the breaker's longest wait
/// (D-079 L-7).
fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let exact = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let cap = u64::try_from(super::breaker::MAX_REQUESTED_WAIT.as_millis()).unwrap_or(u64::MAX);
    exact
        .or_else(|| crate::providers::traits::retry_after_ms_from_headers(headers))
        .map(|wait| wait.min(cap))
}

/// Whether a first failure of class `error` is tried once more: only a
/// loopback backend's transport or unavailability failure, and only when
/// `remaining` leaves room for the `wait` and an attempt (D-042).
fn retries(locality: BackendLocality, error: &DecisionError, wait: Duration, remaining: Duration) -> bool {
    locality == BackendLocality::Loopback
        && matches!(error, DecisionError::Transport | DecisionError::Unavailable)
        && remaining >= wait + MIN_ATTEMPT
}

/// The class of a non-success status.
pub(super) const fn status_class(status: u16, retry_after_ms: Option<u64>) -> DecisionError {
    match status {
        400 | 422 => DecisionError::InvalidRequest(RequestRule::RejectedByBackend),
        401 | 403 => DecisionError::Authentication,
        404 | 405 | 501 => DecisionError::UnsupportedOperation,
        408 | 504 => DecisionError::Timeout,
        429 => DecisionError::RateLimited { retry_after_ms },
        _ => DecisionError::Unavailable,
    }
}

impl CompatibleHttpBackend {
    /// The adapter's name on the wire envelope.
    pub const NAME: &'static str = "compatible_http";

    /// Build the backend `spec` describes. Building performs no I/O.
    ///
    /// # Errors
    ///
    /// [`DecisionError::UnsupportedOperation`] for a kind that is not
    /// served over System One HTTP, [`DecisionError::InvalidRequest`] when
    /// the endpoint does not pass the endpoint check or the spec's locality
    /// is not the kind's, and the client errors of [`DecisionHttp::client`].
    pub fn new(spec: &BackendSpec<'_>) -> Result<Self, DecisionError> {
        let refused = DecisionError::InvalidRequest(RequestRule::EndpointRefused);
        let service = Service::of(spec.identity.kind).ok_or(DecisionError::UnsupportedOperation)?;
        let endpoint = base_url(service, spec.endpoint);
        let transport = match check_endpoint(endpoint).map_err(|_| refused.clone())? {
            super::loopback::EndpointLocality::Loopback => BackendLocality::Loopback,
            super::loopback::EndpointLocality::Remote => BackendLocality::Remote,
        };
        // The kind sets the locality (COORD-6): a hosted backend is remote
        // whatever its URL; the compatible backend is where its URL is.
        let locality = if service.hosted() {
            BackendLocality::Remote
        } else {
            transport
        };
        if locality != spec.locality {
            return Err(refused);
        }
        let client = DecisionHttp::client(
            transport,
            spec.identity.kind,
            spec.timeout,
            super::loopback::localhost_port(endpoint),
        )?;
        let transport_binding = spec.key.hmac(
            KeyDomain::AttemptAccounting,
            &[
                endpoint.as_bytes(),
                spec.resolved.provider_label().as_bytes(),
                spec.expected_model.as_bytes(),
                spec.resolved.credential().api_key().unwrap_or("").as_bytes(),
                spec.resolved.credential().api_url().unwrap_or("").as_bytes(),
            ],
        );
        Ok(Self {
            priced_provider: Arc::from(spec.resolved.provider_label()),
            priced_model: Arc::from(spec.resolved.model()),
            transport_binding,
            identity: spec.identity.clone(),
            service,
            locality,
            transport,
            version_policy: if service.hosted() {
                ModelVersionPolicy::ExactPinned(Arc::clone(&spec.expected_model))
            } else {
                // A compatible endpoint names its model its own way.
                ModelVersionPolicy::Unpinned
            },
            url: route_url(endpoint, service.route()),
            client,
            credential: spec.resolved.credential().clone(),
            key: Arc::clone(&spec.key),
            max_response_bytes: spec.max_response_bytes,
        })
    }

    async fn attempt(
        &self,
        codec: &SystemOneCodec<'_>,
        body: &GuardedBody,
        deadline: Instant,
        accounting: Option<&dyn DecisionAttemptAccounting>,
        request_id: &str,
        remaining_attempts: u8,
    ) -> Result<DecisionResponse, Failure> {
        let started = Instant::now();
        let mut builder = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, concat!("prx/", env!("CARGO_PKG_VERSION")))
            .body(body.bytes().to_vec());
        if let Some(credential) = self.credential.api_key() {
            builder = builder.bearer_auth(credential);
        }
        let owner = accounting.ok_or(DecisionError::BudgetExceeded)?;
        let receipt = owner
            .begin(AttemptBinding {
                backend: &self.identity,
                provider: &self.priced_provider,
                model: &self.priced_model,
                transport_binding: &self.transport_binding,
                guarded_body: body.bytes(),
                request_id,
                deadline,
                remaining_attempts,
            })
            .await?;
        let owner_controls_deadline = receipt.deadline() <= deadline;
        let deadline_error = || {
            if owner_controls_deadline {
                DecisionError::BudgetExceeded
            } else {
                DecisionError::Timeout
            }
        };
        let deadline = deadline.min(receipt.deadline());
        if Instant::now() >= deadline {
            return Err(DecisionError::BudgetExceeded.into());
        }
        #[cfg(test)]
        let mut material_send =
            crate::causal_tree::decision_engine::original_observation::material::send(request_id, body.bytes());
        let sent = tokio::time::timeout_at(deadline.into(), builder.send())
            .await
            .map_err(|_| Failure::from(deadline_error()))?;
        #[cfg(test)]
        material_send.returned();
        let mut response = sent.map_err(|error| Failure::from(transport_class(&error)))?;
        if self.transport == BackendLocality::Loopback
            && !response.remote_addr().is_some_and(|peer| peer.ip().is_loopback())
        {
            return Err(DecisionError::PolicyRejection(PolicyRule::PeerNotLoopback).into());
        }
        let status = response.status();
        if !status.is_success() {
            // The body of a refusal is never read: it is unbounded, and a
            // backend may echo the request state in it (D-086 F-1, F-7).
            // Dropping the response closes the connection.
            let retry_after_ms = retry_after_ms(response.headers());
            tracing::debug!(
                endpoint = %crate::providers::redact_endpoint_userinfo(&self.url),
                status = status.as_u16(),
                body_bytes = ?response.content_length(),
                "decision backend refused the request"
            );
            let error = match self.service {
                Service::OpenRouter => openrouter::status_class(status.as_u16(), retry_after_ms),
                Service::Compatible | Service::TypeSafe => status_class(status.as_u16(), retry_after_ms),
            };
            return Err(Failure {
                error,
                retry_after: retry_after_ms.map(Duration::from_millis),
            });
        }
        let request_id = self
            .service
            .request_id_header()
            .and_then(|header| response.headers().get(header))
            .and_then(|value| value.to_str().ok())
            .and_then(provider_request_id)
            .map(str::to_string);
        let limit = usize::try_from(self.max_response_bytes).unwrap_or(usize::MAX);
        // A declared length over the limit is refused before any byte is
        // read, and no chunk is kept past the limit (D-086 F-8).
        if response
            .content_length()
            .is_some_and(|declared| usize::try_from(declared).map_or(true, |declared| declared > limit))
        {
            return Err(DecisionError::InvalidResponse(ValidationRule::Oversize).into());
        }
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::time::timeout_at(deadline.into(), response.chunk())
                .await
                .map_err(|_| Failure::from(deadline_error()))?
                .map_err(|error| Failure::from(transport_class(&error)))?;
            let Some(chunk) = chunk else { break };
            if bytes.len().saturating_add(chunk.len()) > limit {
                // Dropping the response closes the connection.
                return Err(DecisionError::InvalidResponse(ValidationRule::Oversize).into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let response = match self.service {
            Service::OpenRouter => openrouter::decode_response(codec, &bytes, elapsed_ms),
            Service::Compatible | Service::TypeSafe => codec.decode_response(
                &bytes,
                ResponseEnvelope {
                    backend: self.service.name(),
                    provider_request_id: request_id.as_deref(),
                    elapsed_ms,
                },
            ),
        }
        .map_err(Failure::from)?;
        // Accounting survives factory semantic/model validation failure.
        tokio::time::timeout_at(deadline.into(), receipt.settle(response.usage))
            .await
            .map_err(|_| Failure::from(DecisionError::BudgetExceeded))??;
        Ok(response)
    }
}

#[async_trait]
impl DecisionBackend for CompatibleHttpBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }

    fn locality(&self) -> BackendLocality {
        self.locality
    }

    fn version_policy(&self) -> ModelVersionPolicy {
        self.version_policy.clone()
    }

    fn bounds_transport_independently(&self) -> bool {
        true
    }

    async fn evaluate(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        self.evaluate_with_accounting(request, body, deadline, None).await
    }

    async fn evaluate_with_accounting(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        accounting: Option<&dyn DecisionAttemptAccounting>,
    ) -> Result<DecisionResponse, DecisionError> {
        // The codec decodes the answer; the body sent is the guarded one.
        let codec = SystemOneCodec::new(request, &self.key)?;
        let mut retried = false;
        loop {
            match self
                .attempt(
                    &codec,
                    body,
                    deadline,
                    accounting,
                    request.request_id(),
                    if !retried && self.locality == BackendLocality::Loopback {
                        2
                    } else {
                        1
                    },
                )
                .await
            {
                Ok(response) => return Ok(response),
                Err(failure) => {
                    let wait = failure.retry_after.unwrap_or(Duration::ZERO);
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if retried || !retries(self.locality, &failure.error, wait, remaining) {
                        return Err(failure.error);
                    }
                    retried = true;
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    async fn health(
        &self,
        probe: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
    ) -> Result<(), DecisionError> {
        self.health_with_accounting(probe, body, deadline, None).await
    }

    async fn health_with_accounting(
        &self,
        probe: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        accounting: Option<&dyn DecisionAttemptAccounting>,
    ) -> Result<(), DecisionError> {
        if self.locality != BackendLocality::Loopback {
            // A remote backend reports configuration readiness only: no I/O.
            return Ok(());
        }
        self.evaluate_with_accounting(probe, body, deadline, accounting)
            .await
            .map(|_| ())
    }
}

/// The production transports: the compatible backend and the hosted
/// TypeSafe and OpenRouter adapters.
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpDecisionTransports;

impl super::factory::DecisionTransports for HttpDecisionTransports {
    fn build(&self, spec: &BackendSpec<'_>) -> Result<Box<dyn DecisionBackend>, DecisionError> {
        Ok(Box::new(CompatibleHttpBackend::new(spec)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-042: only a loopback backend's transport or unavailability failure
    /// is retried, and only with room left; a remote failure never is.
    #[test]
    fn only_a_loopback_transport_failure_is_retried() {
        let room = Duration::from_secs(1);
        let wait = Duration::ZERO;
        for error in [DecisionError::Transport, DecisionError::Unavailable] {
            assert!(retries(BackendLocality::Loopback, &error, wait, room));
            assert!(
                !retries(BackendLocality::Remote, &error, wait, room),
                "remote: {error:?}"
            );
            assert!(!retries(BackendLocality::Loopback, &error, wait, MIN_ATTEMPT / 2));
            assert!(!retries(BackendLocality::Loopback, &error, room, room));
        }
        for error in [
            DecisionError::Timeout,
            DecisionError::Authentication,
            DecisionError::RateLimited { retry_after_ms: None },
        ] {
            assert!(!retries(BackendLocality::Loopback, &error, wait, room), "{error:?}");
        }
    }

    #[test]
    fn the_route_is_appended_once() {
        assert_eq!(
            route_url("http://127.0.0.1:8900", ROUTE),
            "http://127.0.0.1:8900/v1/systemone"
        );
        assert_eq!(
            route_url("http://127.0.0.1:8900/", ROUTE),
            "http://127.0.0.1:8900/v1/systemone"
        );
        assert_eq!(
            route_url(" http://127.0.0.1:8900/v1/systemone/ ", ROUTE),
            "http://127.0.0.1:8900/v1/systemone"
        );
        assert_eq!(
            route_url("https://d.example.com/api", ROUTE),
            "https://d.example.com/api/v1/systemone"
        );
        assert_eq!(
            route_url(openrouter::DEFAULT_BASE, openrouter::ROUTE),
            "https://openrouter.ai/api/alpha/decisions"
        );
        assert_eq!(
            route_url(typesafe::DEFAULT_BASE, typesafe::ROUTE),
            "https://api.typesafe.ai/v1/systemone"
        );
        // A base URL written with `/v1` is not doubled (R-I1-phase2 R-3).
        assert_eq!(
            route_url("http://127.0.0.1:8900/v1", ROUTE),
            "http://127.0.0.1:8900/v1/systemone"
        );
        assert_eq!(
            route_url("https://d.example.com/api/v1/", ROUTE),
            "https://d.example.com/api/v1/systemone"
        );
        assert_eq!(
            route_url("https://d.example.com/v1x", ROUTE),
            "https://d.example.com/v1x/v1/systemone"
        );
        // R-I1-confirm: the strip is the System One route's only. An
        // OpenRouter override ending in `/v1` keeps it; its own route is still
        // not doubled.
        assert_eq!(
            route_url("https://gateway.example.com/v1", openrouter::ROUTE),
            "https://gateway.example.com/v1/api/alpha/decisions"
        );
        assert_eq!(
            route_url("https://gateway.example.com/v1/api/alpha/decisions/", openrouter::ROUTE),
            "https://gateway.example.com/v1/api/alpha/decisions"
        );
    }

    #[test]
    fn the_retry_wait_prefers_milliseconds_and_is_capped() {
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut map = reqwest::header::HeaderMap::new();
            for (name, value) in pairs {
                map.insert(
                    *name,
                    reqwest::header::HeaderValue::from_str(value).expect("test: header"),
                );
            }
            map
        };
        assert_eq!(retry_after_ms(&headers(&[("retry-after", "30")])), Some(30_000));
        assert_eq!(
            retry_after_ms(&headers(&[("retry-after-ms", "1500"), ("retry-after", "30")])),
            Some(1500)
        );
        assert_eq!(
            retry_after_ms(&headers(&[("retry-after-ms", "99999999999")])),
            Some(3_600_000)
        );
        assert_eq!(retry_after_ms(&headers(&[])), None);
    }

    #[test]
    fn statuses_map_to_classes() {
        let class = |code: u16| status_class(code, Some(30_000));
        assert_eq!(
            class(400),
            DecisionError::InvalidRequest(RequestRule::RejectedByBackend)
        );
        assert_eq!(
            class(422),
            DecisionError::InvalidRequest(RequestRule::RejectedByBackend)
        );
        assert_eq!(class(401), DecisionError::Authentication);
        assert_eq!(class(403), DecisionError::Authentication);
        for code in [404, 405, 501] {
            assert_eq!(class(code), DecisionError::UnsupportedOperation, "{code}");
        }
        assert_eq!(class(408), DecisionError::Timeout);
        assert_eq!(class(504), DecisionError::Timeout);
        assert_eq!(
            class(429),
            DecisionError::RateLimited {
                retry_after_ms: Some(30_000)
            }
        );
        for code in [500, 502, 503, 302] {
            assert_eq!(class(code), DecisionError::Unavailable, "{code}");
        }
    }

    /// A hosted service without an endpoint uses its own base URL; an
    /// override replaces it (COORD-6); the compatible backend has none.
    #[test]
    fn a_hosted_service_defaults_to_its_base_url() {
        assert_eq!(base_url(Service::TypeSafe, " "), typesafe::DEFAULT_BASE);
        assert_eq!(base_url(Service::OpenRouter, ""), openrouter::DEFAULT_BASE);
        assert_eq!(
            base_url(Service::OpenRouter, " http://127.0.0.1:9/or "),
            "http://127.0.0.1:9/or"
        );
        assert_eq!(base_url(Service::Compatible, ""), "");
    }

    /// DIFFS D-15: a provider request id is kept only as a bounded
    /// diagnostic token.
    #[test]
    fn provider_request_ids_are_bounded_tokens() {
        for kept in [
            "req_01a0d2227d55785bb4906ebbc6f684a9",
            "gen-dec-1789738314-X5e5eKGQdvR9rblyX250",
        ] {
            assert_eq!(provider_request_id(kept), Some(kept));
        }
        for refused in ["", "a b", "req\n1", "ünïcode", "<script>", &"x".repeat(129)] {
            assert_eq!(provider_request_id(refused), None, "{refused:?}");
        }
    }
}
