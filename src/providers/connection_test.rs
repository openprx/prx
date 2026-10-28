//! Test a provider connection: locally by default, live only when asked
//! (PMM-D-7, D-018 Q7).
//!
//! [`test_connection`] always resolves the selection first, exactly as a
//! surface does before it builds a provider
//! ([`selection::check`]): the provider identity,
//! the model, the operation and its features, and the owner-bound
//! credential. That local check sends nothing.
//!
//! Only [`ProbeMode::Live`] goes on the network, and then with exactly one
//! request: a `GET` of the provider's model list. It is never a generation,
//! so it costs nothing and changes nothing upstream, and a redirect is
//! reported, never followed. It is sent only when the local check resolved,
//! with the credential provider construction would present (the one the
//! resolver released, else the provider's own store lookup), and its
//! answer is reduced to what a surface may show: whether the endpoint was
//! reachable, the HTTP status class, whether the model is listed, how many
//! models are, and the latency. An error is passed through
//! [`super::sanitize_api_error`] with URL userinfo
//! redacted and the credential itself masked, so an upstream that echoes the
//! key cannot make the report carry it.
//!
//! Consumers: `prx providers test` (PMM-E) and
//! `POST /api/providers/{id}/test` (PMM-F).

use super::catalog::{self, Requirement, UnknownPolicy, Verification};
use super::{ProviderId, ProviderNamespace, selection};
use crate::config::Config;
use serde::Serialize;
use std::time::{Duration, Instant};

/// How far a connection test goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMode {
    /// Resolve only; nothing is sent.
    Local,
    /// Resolve, then `GET` the provider's model list once.
    Live,
}

/// What the local check found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalCheck {
    /// Whether the selection resolved (a provider could be built).
    pub resolved: bool,
    /// Whether the evidence proves every requirement.
    pub verified: bool,
    /// Resolver and pricing diagnostics. Verified capabilities can still
    /// carry an unpriced-model warning. Never a credential.
    pub detail: String,
}

/// What the one live request found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveProbe {
    /// Whether an HTTP answer arrived.
    pub reachable: bool,
    /// `2xx`, `4xx`, ... when an answer arrived.
    pub http_status_class: Option<String>,
    /// Whether the model is in the list, when a list was read.
    pub model_listed: Option<bool>,
    /// How many models the list holds (0 when none was read).
    pub models_listed: usize,
    pub latency_ms: u64,
    /// Why the probe failed, sanitized; `None` on success.
    pub error: Option<String>,
}

/// The result of a connection test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionReport {
    /// The provider as given, URL userinfo redacted.
    pub provider: String,
    pub model: String,
    pub local: LocalCheck,
    /// `None` for [`ProbeMode::Local`], and when the local check refused the
    /// selection (then nothing is sent).
    pub live: Option<LiveProbe>,
}

/// The model-list request of one provider.
#[derive(Debug, PartialEq, Eq)]
struct ListRequest {
    url: String,
    headers: Vec<(&'static str, String)>,
    shape: ListShape,
}

/// How a provider's model list is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListShape {
    OpenAiCompatible,
    Gemini,
    Ollama,
}

const LIVE_TIMEOUT_SECS: u64 = 8;
const LIVE_CONNECT_TIMEOUT_SECS: u64 = 4;
/// The largest model list read (OpenRouter's is a few MiB).
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;
/// The largest error body read; the message is shortened after masking.
const MAX_ERROR_BYTES: usize = 16 * 1024;

/// Test `provider` (default: the configured default provider) with `model`
/// (default: the configured default model) for chat.
pub async fn test_connection(
    config: &Config,
    provider: Option<&str>,
    model: Option<&str>,
    mode: ProbeMode,
) -> ConnectionReport {
    let provider = provider
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .unwrap_or_else(|| selection::default_provider(config));
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| selection::default_model(config));
    let shown = crate::security::redaction::redact_provider_text(provider);
    let resolved = selection::check(config, provider, model, &Requirement::chat(), UnknownPolicy::Invocation);
    let (local, resolved) = match resolved {
        Ok(resolved) => {
            let verified = matches!(resolved.verification(), Verification::Verified);
            let mut detail = resolved.warning().unwrap_or_default();
            if let Some(warning) = selection::pricing_warning(&resolved, &config.cost) {
                if !detail.is_empty() {
                    detail.push_str("; ");
                }
                detail.push_str(&warning);
            }
            (
                LocalCheck {
                    resolved: true,
                    verified,
                    detail,
                },
                Some(resolved),
            )
        }
        Err(refusal) => (
            LocalCheck {
                resolved: false,
                verified: false,
                detail: refusal.to_string(),
            },
            None,
        ),
    };
    let live = match (mode, resolved) {
        (ProbeMode::Live, Some(resolved)) => Some(live_probe(config, &resolved).await),
        _ => None,
    };
    ConnectionReport {
        provider: shown,
        model: model.to_string(),
        local,
        live,
    }
}

/// The one live request.
async fn live_probe(config: &Config, resolved: &catalog::ResolvedModel) -> LiveProbe {
    let failed = |error: String| LiveProbe {
        reachable: false,
        http_status_class: None,
        model_listed: None,
        models_listed: 0,
        latency_ms: 0,
        error: Some(error),
    };
    // The credential provider construction would present: the one the
    // resolver released (owner-bound), else the provider's own store lookup
    // (auth profile and provider-specific OAuth caches), exactly as the
    // factory resolves it (D-078 T-7). The lookup reads files and takes a
    // lock, so it runs off the runtime, and it never refreshes a token or
    // writes a credential file: a test has no side effect beyond its one
    // request (D-080 T-7b).
    let (label, released, options) = (
        resolved.provider_label().to_string(),
        resolved.credential().api_key().map(str::to_string),
        super::provider_runtime_options_from_config(config),
    );
    let lookup = crate::runtime::blocking::spawn_blocking(move || {
        super::probe_credential(&label, released.as_deref(), &options)
    })
    .await;
    let credential = match lookup {
        Ok(super::ProbeCredential::Ready(credential)) => credential,
        Ok(super::ProbeCredential::RefreshDue) => {
            return failed(
                "the provider's stored OAuth token has expired and the connection test does not refresh it (a \
                 test never rotates stored credentials); use the provider once so it refreshes the token, then \
                 test again"
                    .to_string(),
            );
        }
        Err(error) => {
            tracing::warn!(error = %error, "the credential lookup of the live probe failed");
            return failed(format!("the credential lookup failed: {error}"));
        }
    };
    let Some(request) = list_request(
        resolved.provider(),
        resolved.provider_label(),
        resolved.credential().api_url(),
        credential.as_deref(),
    ) else {
        return failed("no model-list endpoint is known for this provider; the local check is the only test".into());
    };
    let service = resolved
        .provider()
        .catalog_name()
        .map_or_else(|| "provider.compatible".to_string(), |name| format!("provider.{name}"));
    // A dedicated client that never follows a redirect: exactly one request
    // is sent, and no credential header can follow a redirect to another
    // host (reqwest keeps `x-api-key`/`x-goog-api-key` across one) (D-078
    // T-1). The runtime proxy settings still apply.
    let builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(LIVE_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(LIVE_CONNECT_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none());
    let client = match crate::config::apply_runtime_proxy_to_builder(builder, &service).build() {
        Ok(client) => client,
        Err(error) => return failed(scrub(&format!("{error:#}"), credential.as_deref())),
    };
    let mut builder = client.get(&request.url);
    for (name, value) in &request.headers {
        builder = builder.header(*name, value);
    }
    let started = Instant::now();
    let response = builder.send().await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return LiveProbe {
                latency_ms,
                ..failed(scrub(&error.to_string(), credential.as_deref()))
            };
        }
    };
    let status = response.status();
    let answered = |error: Option<String>| LiveProbe {
        reachable: true,
        http_status_class: Some(format!("{}xx", status.as_u16() / 100)),
        model_listed: None,
        models_listed: 0,
        latency_ms,
        error,
    };
    if status.is_redirection() {
        return answered(Some(format!(
            "HTTP {status}: the endpoint answered with a redirect, which the probe does not follow (the credential \
             would go to another address); configure the final URL"
        )));
    }
    let cap = if status.is_success() {
        MAX_LIST_BYTES
    } else {
        MAX_ERROR_BYTES
    };
    let body = match read_bounded(response, cap).await {
        Ok(body) => body,
        Err(error) => {
            let error = scrub(&error, credential.as_deref());
            tracing::warn!(error = %error, "the live probe could not read the answer");
            return answered(Some(error));
        }
    };
    if !status.is_success() {
        // The credential is masked in the whole body first; only then is the
        // message shortened, so no part of an echoed key survives a cut
        // (D-078 T-4).
        let text = String::from_utf8_lossy(&body.bytes);
        return answered(Some(scrub(&format!("HTTP {status}: {text}"), credential.as_deref())));
    }
    if body.truncated {
        return answered(Some(format!(
            "the model list is larger than {} MiB; it was not read",
            MAX_LIST_BYTES / (1024 * 1024)
        )));
    }
    let models = serde_json::from_slice::<serde_json::Value>(&body.bytes).map(|payload| match request.shape {
        ListShape::OpenAiCompatible => catalog::discovery::parse_openai_compatible_models(&payload),
        ListShape::Gemini => catalog::discovery::parse_gemini_models(&payload),
        ListShape::Ollama => catalog::discovery::parse_ollama_models(&payload),
    });
    models.map_or_else(
        |_| answered(Some("the model list is not JSON".to_string())),
        |models| {
            let model = resolved.model();
            let listed = models
                .iter()
                .any(|entry| entry.id == model || entry.id.strip_prefix("models/") == Some(model));
            LiveProbe {
                model_listed: Some(listed),
                models_listed: models.len(),
                ..answered(None)
            }
        },
    )
}

/// A response body read up to a limit.
struct BoundedBody {
    bytes: Vec<u8>,
    /// More bytes followed the limit; they were not read.
    truncated: bool,
}

/// Read at most `cap` bytes of `response` (D-078 T-9), so an endpoint cannot
/// make the process buffer an unbounded body. A read error is returned for
/// the caller to log and report, never swallowed.
async fn read_bounded(mut response: reqwest::Response, cap: usize) -> Result<BoundedBody, String> {
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = cap.saturating_sub(bytes.len());
                if chunk.len() > room {
                    bytes.extend_from_slice(chunk.get(..room).unwrap_or_default());
                    return Ok(BoundedBody { bytes, truncated: true });
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => {
                return Ok(BoundedBody {
                    bytes,
                    truncated: false,
                });
            }
            Err(error) => return Err(format!("reading the answer failed: {error}")),
        }
    }
}

/// Where and how to list `provider`'s models, or `None` when no model-list
/// endpoint is known.
fn list_request(
    provider: &ProviderId,
    label: &str,
    api_url: Option<&str>,
    credential: Option<&str>,
) -> Option<ListRequest> {
    let bearer = |url: String| ListRequest {
        url,
        headers: credential
            .map(|key| vec![("authorization", format!("Bearer {key}"))])
            .unwrap_or_default(),
        shape: ListShape::OpenAiCompatible,
    };
    match provider.namespace() {
        ProviderNamespace::Custom => {
            let base = label.trim().strip_prefix("custom:")?;
            Some(bearer(super::endpoint_url(base, "/models")?))
        }
        ProviderNamespace::AnthropicCustom => {
            let base = label.trim().strip_prefix("anthropic-custom:")?;
            let base = reqwest::Url::parse(base.trim()).ok().map(|mut url| {
                let path = url.path().trim_end_matches('/').trim_end_matches("/v1").to_string();
                url.set_path(&path);
                url.to_string()
            })?;
            Some(ListRequest {
                url: super::endpoint_url(&base, "/v1/models")?,
                headers: anthropic_headers(credential),
                shape: ListShape::OpenAiCompatible,
            })
        }
        ProviderNamespace::Catalog => {
            let name = provider.catalog_name()?;
            // Where provider construction honours a configured endpoint,
            // the list request goes there and nowhere else: the key is only
            // ever presented to the host the provider itself addresses
            // (D-095 M-1).
            if let Some(base) = super::honoured_base_url(label, api_url) {
                if name == "ollama" {
                    let root = reqwest::Url::parse(base).ok().map(|mut url| {
                        let path = url.path().trim_end_matches('/').trim_end_matches("/v1").to_string();
                        url.set_path(&path);
                        url.to_string()
                    })?;
                    return Some(ListRequest {
                        url: super::endpoint_url(&root, "/api/tags")?,
                        headers: Vec::new(),
                        shape: ListShape::Ollama,
                    });
                }
                return crate::providers::model_discovery::resolve_live_models_endpoint(label, api_url).map(bearer);
            }
            match name {
                "openrouter" => Some(bearer("https://openrouter.ai/api/v1/models".to_string())),
                "anthropic" => Some(ListRequest {
                    url: "https://api.anthropic.com/v1/models".to_string(),
                    headers: anthropic_headers(credential),
                    shape: ListShape::OpenAiCompatible,
                }),
                "gemini" => Some(ListRequest {
                    url: "https://generativelanguage.googleapis.com/v1beta/models".to_string(),
                    headers: credential
                        .map(|key| vec![("x-goog-api-key", key.to_string())])
                        .unwrap_or_default(),
                    shape: ListShape::Gemini,
                }),
                "ollama" => Some(ListRequest {
                    url: "http://localhost:11434/api/tags".to_string(),
                    headers: Vec::new(),
                    shape: ListShape::Ollama,
                }),
                _ => crate::providers::model_discovery::resolve_live_models_endpoint(label, api_url).map(bearer),
            }
        }
        _ => None,
    }
}

/// Anthropic's headers, as its provider sends them: an OAuth setup token as
/// a bearer with the OAuth beta header, any other key as `x-api-key`.
fn anthropic_headers(credential: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![("anthropic-version", "2023-06-01".to_string())];
    match credential {
        Some(token) if crate::auth::discovery::is_claude_code_oauth_setup_token(token) => {
            headers.push(("authorization", format!("Bearer {token}")));
            headers.push(("anthropic-beta", "oauth-2025-04-20".to_string()));
        }
        Some(key) => headers.push(("x-api-key", key.to_string())),
        None => {}
    }
    headers
}

/// A message a surface may show: provider-error sanitizing, URL userinfo
/// redacted, and the credential itself masked wherever it appears.
fn scrub(message: &str, credential: Option<&str>) -> String {
    // Mask first: `sanitize_api_error` shortens the message, and a key cut in
    // half would no longer match (D-078 T-4).
    let masked = credential.filter(|key| !key.is_empty()).map_or_else(
        || message.to_string(),
        |key| message.replace(key, crate::config::secret_paths::REDACTION_MASK),
    );
    super::sanitize_api_error(&crate::security::redaction::redact_url_components(&masked))
}

/// Rendered for logs: never the credential.
impl std::fmt::Display for ConnectionReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider '{}' model '{}': ", self.provider, self.model)?;
        if !self.local.resolved {
            return write!(f, "refused locally: {}", self.local.detail);
        }
        f.write_str(if self.local.verified {
            "resolves"
        } else {
            "resolves (unverified)"
        })?;
        if !self.local.detail.is_empty() {
            write!(f, "; {}", self.local.detail)?;
        }
        if let Some(live) = &self.live {
            match &live.error {
                None => write!(
                    f,
                    "; live: {} models listed, model listed: {}, {} ms",
                    live.models_listed,
                    live.model_listed.unwrap_or(false),
                    live.latency_ms
                )?,
                Some(error) => write!(f, "; live probe failed: {error}")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};

    #[tokio::test]
    async fn unpriced_model_probe_explains_g6_without_sending() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for(dir.path(), "kimi-code");
        config.default_model = Some("kimi-for-coding".into());
        let report = test_connection(&config, None, None, ProbeMode::Local).await;
        assert!(report.local.resolved);
        assert!(report.live.is_none());
        let shown = report.to_string();
        assert!(
            shown.contains("G6 budget admission refuses unpriced model 'kimi-code/kimi-for-coding'"),
            "{shown}"
        );
        assert!(
            shown.contains(r#"[cost.prices."kimi-code/kimi-for-coding"]"#),
            "{shown}"
        );
        config.cost.prices.insert(
            "kimi-code/kimi-for-coding".into(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 2.0,
                cache_read: 0.5,
                cache_write: 1.0,
            },
        );
        let priced = test_connection(&config, None, None, ProbeMode::Local).await;
        assert!(!priced.to_string().contains("G6"));
    }

    /// A credential-shaped sentinel. Not a real secret.
    const KEY: &str = "pmmd7-probe-sentinel";

    fn config_for(dir: &std::path::Path, provider: &str) -> Config {
        Config {
            config_path: dir.join("config.toml"),
            workspace_dir: dir.join("workspace"),
            default_provider: Some(provider.to_string()),
            default_model: Some(PROBE_MODEL.to_string()),
            api_key: Some(KEY.to_string()),
            ..Config::default()
        }
    }

    /// PMM-D-7: the default test is local: it resolves and sends nothing.
    #[tokio::test]
    async fn a_local_test_sends_nothing() {
        let probe = CredentialProbe::start().await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = config_for(dir.path(), &probe.custom_provider());

        let report = test_connection(&config, None, None, ProbeMode::Local).await;

        assert!(report.local.resolved, "{report}");
        assert!(report.live.is_none());
        assert_eq!(probe.requests().len(), 0, "a local test must not send anything");
    }

    /// PMM-D-7: a live test sends exactly one GET, to the model list, with
    /// the owner-bound credential, and reports what it found.
    #[tokio::test]
    async fn a_live_test_sends_one_model_list_get() {
        let probe = CredentialProbe::start().await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = config_for(dir.path(), &probe.custom_provider());

        let report = test_connection(&config, None, None, ProbeMode::Live).await;

        let requests = probe.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let request = requests.first().expect("test: one request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v1/models");
        assert_eq!(request.authorization(), Some(format!("Bearer {KEY}").as_str()));
        let live = report.live.expect("test: a live result");
        assert!(live.reachable);
        assert_eq!(live.http_status_class.as_deref(), Some("2xx"));
        assert_eq!(live.model_listed, Some(true));
        assert_eq!(live.models_listed, 1);
        assert_eq!(live.error, None);
        let unlisted = test_connection(
            &config,
            Some(&probe.custom_provider()),
            Some("absent-model"),
            ProbeMode::Live,
        )
        .await;
        assert_eq!(unlisted.live.as_ref().and_then(|live| live.model_listed), Some(false));
        let rendered = serde_json::to_string(&unlisted).expect("test: json");
        assert!(!rendered.contains(KEY), "{rendered}");
    }

    /// RED before PMME-h (D-095 M-1): with `default_provider = "openai"`, a
    /// configured `api_url` (an operator's gateway, where provider
    /// construction sends the key) and a root key, the live test sent the
    /// key to `api.openai.com`. It now lists models at the configured
    /// endpoint, the one host construction addresses. Under the test guard a
    /// connection anywhere else kills the test.
    #[tokio::test]
    async fn a_live_test_of_a_catalog_provider_uses_its_configured_endpoint() {
        let probe = CredentialProbe::start().await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let mut config = config_for(dir.path(), "openai");
        config.default_model = Some("gpt-4o".to_string());
        config.api_url = Some(probe.base_url());

        let report = test_connection(&config, None, None, ProbeMode::Live).await;

        let requests = probe.requests();
        assert_eq!(requests.len(), 1, "{report} {requests:?}");
        let request = requests.first().expect("test: one request");
        assert_eq!(request.path, "/v1/models");
        assert_eq!(request.authorization(), Some(format!("Bearer {KEY}").as_str()));
        assert!(report.live.is_some_and(|live| live.reachable));
    }

    /// RED before PMME-h (D-095 M-1, L-4): the planned list request, without
    /// sending it. A catalog provider whose construction honours `api_url`
    /// is listed there; a `custom:` URL with a query keeps the query after
    /// the path (it was appended to the whole spelling).
    #[test]
    fn the_list_request_addresses_the_host_construction_uses() {
        let openai = ProviderId::of("openai");
        let request =
            list_request(&openai, "openai", Some("http://127.0.0.1:9/v1"), Some(KEY)).expect("test: a list request");
        assert_eq!(request.url, "http://127.0.0.1:9/v1/models");
        let vendor = list_request(&openai, "openai", None, Some(KEY)).expect("test: a list request");
        assert!(vendor.url.starts_with("https://api.openai.com/"), "{}", vendor.url);

        let custom = "custom:http://127.0.0.1:9/v1?sig=abc&flag";
        let request = list_request(&ProviderId::of(custom), custom, None, None).expect("test: a list request");
        assert_eq!(request.url, "http://127.0.0.1:9/v1/models?sig=abc&flag");
    }

    /// RED before PMME-h (D-095 N-1): `prx providers test` showed a
    /// provider URL's query values in its report.
    #[tokio::test]
    async fn a_connection_report_redacts_provider_url_query_values() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let provider = "custom:http://127.0.0.1:1/v1?tenant=sentinel-report-tenant";
        let config = config_for(dir.path(), provider);
        let report = test_connection(&config, None, None, ProbeMode::Local).await;
        let rendered = format!("{report} {}", serde_json::to_string(&report).expect("test: json"));
        assert!(!rendered.contains("sentinel-report-tenant"), "{rendered}");
    }

    /// PMM-D-7: an upstream error that echoes the credential is scrubbed.
    #[tokio::test]
    async fn an_echoed_credential_is_scrubbed_from_a_failed_probe() {
        let probe = CredentialProbe::start_failing(401, &format!("invalid api key {KEY}")).await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = config_for(dir.path(), &probe.custom_provider());

        let report = test_connection(&config, None, None, ProbeMode::Live).await;

        assert_eq!(probe.requests().len(), 1);
        let live = report.live.clone().expect("test: a live result");
        assert!(live.reachable);
        assert_eq!(live.http_status_class.as_deref(), Some("4xx"));
        let error = live.error.expect("test: an error");
        assert!(!error.contains(KEY), "{error}");
        let rendered = format!(
            "{report} {report:?} {}",
            serde_json::to_string(&report).expect("test: json")
        );
        assert!(!rendered.contains(KEY), "{rendered}");
    }

    /// PMM-D-7: a selection the resolver refuses is never probed, even
    /// when a live test was asked for.
    #[tokio::test]
    async fn a_refused_selection_is_never_probed() {
        let probe = CredentialProbe::start().await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let mut config = config_for(dir.path(), &probe.custom_provider());
        let declared: crate::config::schema::ProviderModelConfig = toml::from_str(&format!(
            "provider = \"{}\"\nmodel = \"{PROBE_MODEL}\"\noperations = [\"embedding\"]\n",
            probe.custom_provider()
        ))
        .expect("test: declaration");
        config.providers.models.push(declared);

        let report = test_connection(&config, None, None, ProbeMode::Live).await;

        assert!(!report.local.resolved, "{report}");
        assert!(report.live.is_none());
        assert_eq!(probe.requests().len(), 0);
    }

    /// A credential issued for another provider is never presented: the
    /// root key belongs to the configured default provider only.
    #[tokio::test]
    async fn another_providers_credential_is_never_presented() {
        let probe = CredentialProbe::start().await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        // The root key is the default provider's (openai), not the probe's.
        let config = config_for(dir.path(), "openai");

        let report = test_connection(&config, Some(&probe.custom_provider()), None, ProbeMode::Live).await;

        assert!(report.local.resolved, "{report}");
        let requests = probe.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            !probe.saw(KEY),
            "the default provider's key was sent to another endpoint"
        );
    }

    /// A loopback endpoint that answers every request with `response` and
    /// counts the requests and the credential headers it saw.
    struct RawEndpoint {
        addr: std::net::SocketAddr,
        heads: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for RawEndpoint {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn raw_endpoint(response: impl Fn() -> Vec<u8> + Send + Sync + 'static) -> RawEndpoint {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        let heads = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&heads);
        let response = std::sync::Arc::new(response);
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let sink = std::sync::Arc::clone(&sink);
                let response = std::sync::Arc::clone(&response);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(chunk.get(..read).unwrap_or_default()),
                        }
                    }
                    sink.lock().push(String::from_utf8_lossy(&buffer).to_ascii_lowercase());
                    let _ = socket.write_all(&response()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        RawEndpoint { addr, heads, server }
    }

    fn config_with_key(dir: &std::path::Path, provider: &str) -> Config {
        Config {
            default_model: Some("some-model".to_string()),
            ..config_for(dir, provider)
        }
    }

    /// RED before PMMDtail-f (D-078 T-1): the probe followed a redirect to
    /// another address, sending a second request that carried the
    /// `x-api-key` credential. Now exactly one request is sent and the
    /// redirect is reported.
    #[tokio::test]
    async fn a_redirect_is_reported_and_never_followed() {
        let target = raw_endpoint(|| {
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{\"data\":[]}".to_vec()
        })
        .await;
        let location = format!("http://{}/v1/models", target.addr);
        let origin = raw_endpoint(move || {
            format!("HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .into_bytes()
        })
        .await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        for provider in [
            format!("anthropic-custom:http://{}", origin.addr),
            format!("custom:http://{}/v1", origin.addr),
        ] {
            let config = config_with_key(dir.path(), &provider);

            let report = test_connection(&config, None, None, ProbeMode::Live).await;

            let live = report.live.expect("test: a live result");
            assert_eq!(live.http_status_class.as_deref(), Some("3xx"), "{provider}");
            assert!(live.error.is_some(), "{provider}");
        }
        assert_eq!(origin.heads.lock().len(), 2, "one request per provider");
        assert!(
            target.heads.lock().is_empty(),
            "the redirect was followed: {:?}",
            target.heads.lock()
        );
    }

    /// RED before PMMDtail-f (D-078 T-4): an echoed key that straddles the
    /// cut of a long error body left its prefix unmasked.
    #[tokio::test]
    async fn an_echoed_key_straddling_the_cut_is_masked_whole() {
        let key = "pmmd7-straddling-sentinel-key-0123456789";
        let padding = "x".repeat(140);
        let probe = CredentialProbe::start_failing(401, &format!("{padding}{key}")).await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = Config {
            api_key: Some(key.to_string()),
            ..config_for(dir.path(), &probe.custom_provider())
        };

        let report = test_connection(&config, None, None, ProbeMode::Live).await;

        let error = report.live.and_then(|live| live.error).expect("test: an error");
        let prefix = key.get(..8).unwrap_or(key);
        assert!(!error.contains(prefix), "a key prefix survived the cut: {error}");
    }

    /// RED before PMMDtail-f (D-078 T-9): the model list was read whole,
    /// however large. It is now bounded and the refusal says so.
    #[tokio::test]
    async fn an_oversized_model_list_is_not_read_whole() {
        // Larger than the list limit (16 MiB).
        let body_len = 17 * 1024 * 1024;
        let endpoint = raw_endpoint(move || {
            let mut response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {body_len}\r\nconnection: close\r\n\r\n"
            )
            .into_bytes();
            response.extend(std::iter::repeat_n(b' ', body_len));
            response
        })
        .await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = config_with_key(dir.path(), &format!("custom:http://{}/v1", endpoint.addr));

        let report = test_connection(&config, None, None, ProbeMode::Live).await;

        let error = report.live.and_then(|live| live.error).expect("test: an error");
        assert!(error.contains("larger than"), "{error}");
    }

    /// RED before PMMDtail-f (D-078 T-7): the probe looked the provider's own
    /// profile up by the raw label, not the way construction does (by
    /// provider identity), so a profile the runtime uses was not presented.
    #[tokio::test]
    async fn the_live_probe_presents_the_credential_construction_uses() {
        let probe = CredentialProbe::start().await;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let identity = crate::providers::ProviderId::of(&probe.custom_provider());
        crate::auth::profiles::AuthProfilesStore::new(dir.path(), false)
            .upsert_profile(
                crate::auth::profiles::AuthProfile::new_token(identity.as_str(), "default", KEY.to_string()),
                true,
            )
            .expect("test: the provider's own profile");
        // The same endpoint, spelled with a trailing slash: one identity.
        let spelled = format!("{}/", probe.custom_provider());
        let config = Config {
            api_key: None,
            ..config_for(dir.path(), &spelled)
        };

        let report = test_connection(&config, None, None, ProbeMode::Live).await;

        assert!(report.local.resolved, "{report}");
        assert_eq!(probe.requests().len(), 1);
        assert!(probe.saw(KEY), "the provider's own credential was not presented");
    }
}
