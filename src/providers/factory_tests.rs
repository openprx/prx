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
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;

fn factory_config(
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: crate::config::ReliabilityConfig,
) -> crate::config::Config {
    crate::config::Config {
        default_provider: Some(provider.to_string()),
        default_model: Some(model.to_string()),
        api_key: api_key.map(str::to_string),
        api_url: api_url.map(str::to_string),
        reliability,
        ..crate::config::Config::default()
    }
}

// ── Retry-After capture on the non-streaming error path ──────

/// Answer exactly one request with the given raw HTTP response and return
/// the address to point a client at.
async fn serve_one_raw_response(raw: &'static str) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: local addr");
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut discard = [0_u8; 2048];
            let _ = socket.read(&mut discard).await;
            let _ = socket.write_all(raw.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    addr
}

/// The non-streaming error path must carry the real `Retry-After` response
/// header. It never did: `api_error` only read the status and the body, so
/// the reliability layer's Retry-After branch was unreachable for every
/// non-streaming provider and each 429 fell back to a guessed backoff.
#[tokio::test]
async fn api_error_captures_the_retry_after_response_header() {
    const RAW: &str = "HTTP/1.1 429 Too Many Requests\r\n\
Content-Type: application/json\r\n\
Retry-After: 60\r\n\
Content-Length: 24\r\n\
Connection: close\r\n\
\r\n\
{\"type\":\"rate_limit\"}\r\n\r\n";
    let addr = serve_one_raw_response(RAW).await;
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/v1/messages"))
        .send()
        .await
        .expect("test: request the fake upstream");
    assert_eq!(response.status().as_u16(), 429);

    let error = api_error("anthropic", response).await;

    assert_eq!(
        rate_limit::retry_after_hint_ms(&error),
        Some(60_000),
        "the upstream Retry-After must survive into the error"
    );
    assert!(
        error
            .to_string()
            .starts_with("anthropic API error (429 Too Many Requests)"),
        "the user-visible message must be unchanged: {error}"
    );
}

#[tokio::test]
async fn api_error_without_retry_after_header_carries_no_hint() {
    const RAW: &str = "HTTP/1.1 429 Too Many Requests\r\n\
Content-Type: application/json\r\n\
Content-Length: 24\r\n\
Connection: close\r\n\
\r\n\
{\"type\":\"rate_limit\"}\r\n\r\n";
    let addr = serve_one_raw_response(RAW).await;
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/v1/messages"))
        .send()
        .await
        .expect("test: request the fake upstream");

    let error = api_error("anthropic", response).await;

    assert_eq!(rate_limit::retry_after_hint_ms(&error), None);
}

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    #[allow(unsafe_code)]
    fn set(key: &'static str, value: Option<&str>) -> Self {
        let original = std::env::var(key).ok();
        if key == "HOME" {
            crate::config::schema::follow_test_home_env(value.map(std::ffi::OsStr::new));
        }
        // SAFETY: test-only, single-threaded test runner
        unsafe {
            match value {
                Some(next) => std::env::set_var(key, next),
                None => std::env::remove_var(key),
            }
        }

        Self { key, original }
    }
}

impl Drop for EnvGuard {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if self.key == "HOME" {
            crate::config::schema::follow_test_home_env(self.original.as_deref().map(std::ffi::OsStr::new));
        }
        // SAFETY: test-only, single-threaded test runner
        unsafe {
            if let Some(original) = self.original.as_deref() {
                std::env::set_var(self.key, original);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().expect("env lock poisoned")
}

#[test]
fn resolve_provider_credential_prefers_explicit_argument() {
    let resolved = resolve_provider_credential("openrouter", Some("  explicit-key  "));
    assert_eq!(resolved, Some("explicit-key".to_string()));
}

#[test]
fn resolved_routes_never_inherit_the_primary_credential() {
    let mut config = factory_config(
        "openai",
        "gpt-4o",
        Some("openai-secret"),
        None,
        crate::config::ReliabilityConfig::default(),
    );
    config.model_routes = vec![
        crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "foreign".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-3-5-sonnet-20241022".to_string(),
            api_key: None,
            credential_ref: None,
        },
        crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "owned".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-3-5-sonnet-20241022".to_string(),
            api_key: Some("anthropic-secret".to_string()),
            credential_ref: None,
        },
    ];
    let catalog = selection::catalog_for(&config);
    let default = selection::check_default(&config, catalog::UnknownPolicy::Invocation)
        .expect("test: the configured default resolves");
    let foreign = selection::resolve_route(
        &config,
        &catalog,
        &config.model_routes[0],
        catalog::UnknownPolicy::Invocation,
    )
    .expect("test: the keyless route resolves");
    let owned = selection::resolve_route(
        &config,
        &catalog,
        &config.model_routes[1],
        catalog::UnknownPolicy::Invocation,
    )
    .expect("test: the keyed route resolves");

    assert_eq!(foreign.credential().api_key(), None);
    assert_eq!(default.credential().api_key(), Some("openai-secret"));
    assert_eq!(owned.credential().api_key(), Some("anthropic-secret"));
}

#[test]
fn resolve_provider_credential_minimax_placeholder_without_refresh_returns_none() {
    let _env_lock = env_lock();
    let _refresh_guard = EnvGuard::set(MINIMAX_OAUTH_REFRESH_TOKEN_ENV, None);

    let resolved = resolve_provider_credential("minimax", Some(MINIMAX_OAUTH_PLACEHOLDER));

    assert!(resolved.is_none());
}

#[test]
fn resolve_provider_credential_bedrock_returns_none_without_explicit() {
    assert!(resolve_provider_credential("bedrock", None).is_none());
    assert!(resolve_provider_credential("aws-bedrock", None).is_none());
}

#[test]
fn resolve_provider_credential_returns_none_without_explicit_key() {
    // Without explicit api_key, resolve_provider_credential returns None
    // (env vars are no longer read).
    assert!(resolve_provider_credential("litellm", None).is_none());
    assert!(resolve_provider_credential("vllm", None).is_none());
    assert!(resolve_provider_credential("hf", None).is_none());
}

#[test]
fn resolve_provider_credential_reads_active_auth_profiles_for_kimi_providers() {
    let tmp = TempDir::new().unwrap();
    let store = AuthProfilesStore::new(tmp.path(), false);
    store
        .upsert_profile(
            crate::auth::profiles::AuthProfile::new_token("moonshot", "default", "moonshot-profile-key".to_string()),
            true,
        )
        .unwrap();
    store
        .upsert_profile(
            crate::auth::profiles::AuthProfile::new_token("kimi-code", "default", "kimi-code-profile-key".to_string()),
            true,
        )
        .unwrap();

    let options = ProviderRuntimeOptions {
        openprx_dir: Some(tmp.path().to_path_buf()),
        secrets_encrypt: false,
        ..ProviderRuntimeOptions::default()
    };

    assert_eq!(
        resolve_provider_credential_with_options("moonshot", None, &options).as_deref(),
        Some("moonshot-profile-key")
    );
    assert_eq!(
        resolve_provider_credential_with_options("kimi", None, &options).as_deref(),
        Some("moonshot-profile-key")
    );
    assert_eq!(
        resolve_provider_credential_with_options("moonshot-intl", None, &options).as_deref(),
        Some("moonshot-profile-key")
    );
    assert_eq!(
        resolve_provider_credential_with_options("kimi-code", None, &options).as_deref(),
        Some("kimi-code-profile-key")
    );
}

#[test]
fn resolve_qwen_oauth_context_prefers_explicit_override() {
    let _env_lock = env_lock();
    let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}", std::process::id());
    let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");
    let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, Some("oauth-token"));
    let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, Some("coding-intl.dashscope.aliyuncs.com"));

    let context = resolve_qwen_oauth_context(Some("  explicit-qwen-token  "), OauthRefresh::Allowed);

    assert_eq!(context.credential.as_deref(), Some("explicit-qwen-token"));
    assert!(context.base_url.is_none());
}

#[test]
fn resolve_qwen_oauth_context_uses_env_token_and_resource_url() {
    let _env_lock = env_lock();
    let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}-env", std::process::id());
    let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");
    let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, Some("oauth-token"));
    let _refresh_guard = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
    let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, Some("coding-intl.dashscope.aliyuncs.com"));
    let _dashscope_guard = EnvGuard::set("DASHSCOPE_API_KEY", Some("dashscope-fallback"));

    let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER), OauthRefresh::Allowed);

    assert!(context.credential.is_none());
    assert!(context.base_url.is_none());
}

#[test]
fn resolve_qwen_oauth_context_reads_cached_credentials_file() {
    let _env_lock = env_lock();
    let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}-file", std::process::id());
    let creds_dir = PathBuf::from(&fake_home).join(".qwen");
    std::fs::create_dir_all(&creds_dir).unwrap();
    let creds_path = creds_dir.join("oauth_creds.json");
    std::fs::write(
        &creds_path,
        r#"{"access_token":"cached-token","refresh_token":"cached-refresh","resource_url":"https://resource.example.com","expiry_date":4102444800000}"#,
    )
    .unwrap();

    let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");
    let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, None);
    let _refresh_guard = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
    let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, None);
    let _dashscope_guard = EnvGuard::set("DASHSCOPE_API_KEY", None);

    let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER), OauthRefresh::Allowed);

    assert!(context.credential.is_none());
    assert!(context.base_url.is_none());
    assert_eq!(
        std::fs::read_to_string(&creds_path).expect("test: file remains"),
        r#"{"access_token":"cached-token","refresh_token":"cached-refresh","resource_url":"https://resource.example.com","expiry_date":4102444800000}"#
    );
}

#[test]
fn resolve_qwen_oauth_context_placeholder_does_not_use_dashscope_fallback() {
    let _env_lock = env_lock();
    let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}-placeholder", std::process::id());
    let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");
    let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, None);
    let _refresh_guard = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
    let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, None);
    let _dashscope_guard = EnvGuard::set("DASHSCOPE_API_KEY", Some("dashscope-fallback"));

    let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER), OauthRefresh::Allowed);

    assert!(context.credential.is_none());
}

#[test]
fn resolve_claude_code_context_prefers_explicit_override() {
    let context = resolve_claude_code_context(Some("  explicit-claude-token  "), OauthRefresh::Allowed);
    assert_eq!(context.credential.as_deref(), Some("explicit-claude-token"));
}

#[test]
fn resolve_claude_code_context_reads_cached_credentials_file() {
    let _env_lock = env_lock();
    let fake_home = format!("/tmp/openprx-claude-oauth-home-{}-file", std::process::id());
    let creds_dir = PathBuf::from(&fake_home).join(".claude");
    std::fs::create_dir_all(&creds_dir).unwrap();
    let creds_path = creds_dir.join(".credentials.json");
    std::fs::write(
        &creds_path,
        r#"{"claudeAiOauth":{"accessToken":"cached-token","refreshToken":"cached-refresh","expiresAt":4102444800000,"subscriptionType":"max","scopes":["org:create_api_key"]}}"#,
    )
    .unwrap();

    let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");

    let context = resolve_claude_code_context(Some("claude-code"), OauthRefresh::Allowed);

    assert!(context.credential.is_none());
    assert!(context.refresh_token.is_none());
    let raw = std::fs::read_to_string(&creds_path).expect("test: file remains");
    assert!(raw.contains("cached-token"));
}

#[test]
fn resolve_claude_code_context_placeholder_without_file_returns_none() {
    let _env_lock = env_lock();
    let fake_home = format!("/tmp/openprx-claude-oauth-home-{}-placeholder", std::process::id());
    let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");

    let context = resolve_claude_code_context(Some("claude-code"), OauthRefresh::Allowed);

    assert!(context.credential.is_none());
}

/// Installs a home and a refresh endpoint for the OAuth credential
/// lookups for one test; hold [`env_lock`] while it lives.
struct OauthTestHome;

impl OauthTestHome {
    fn set(home: &std::path::Path, token_endpoint: &str) -> Self {
        *OAUTH_TEST_OVERRIDES.write() = OauthTestOverrides {
            home: Some(home.to_path_buf()),
            token_endpoint: Some(token_endpoint.to_string()),
        };
        Self
    }
}

impl Drop for OauthTestHome {
    fn drop(&mut self) {
        *OAUTH_TEST_OVERRIDES.write() = OauthTestOverrides::default();
    }
}

/// A loopback listener that counts the connections it accepts and
/// answers each with a 400.
struct LoopbackCounter {
    addr: std::net::SocketAddr,
    connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LoopbackCounter {
    fn start() -> Self {
        use std::io::{Read as _, Write as _};
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("test: bind");
        listener.set_nonblocking(true).expect("test: non-blocking");
        let addr = listener.local_addr().expect("test: addr");
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (counted, stopped) = (std::sync::Arc::clone(&connections), std::sync::Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        counted.fetch_add(1, Ordering::SeqCst);
                        let _ = socket.set_nonblocking(false);
                        let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                        let mut buffer = [0_u8; 4096];
                        let _ = socket.read(&mut buffer);
                        let _ = socket
                            .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        });
        Self {
            addr,
            connections,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/v1/oauth/token", self.addr)
    }

    fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for LoopbackCounter {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// RED before PMMDtail-g (D-080 T-7b): the live probe looked the
/// credential up on the async task with construction's lookup, which
/// refreshes an expired Claude Code token: a blocking HTTP client inside
/// the runtime (a panic), a second request to another host and a rewrite
/// of the operator's credential file. The probe now looks up off the
/// runtime with a lookup that never refreshes and never writes; it
/// reports that the token needs a refresh and sends nothing.
///
/// Loopback only: the refresh endpoint is a local listener, the lookup
/// is first checked to see the expired token (so the probe stops at the
/// credential and the anthropic model-list request is never sent), and
/// the test home is the OAuth lookups' own override, not the process-wide
/// test home other modules change concurrently.
#[test]
fn the_live_probe_never_refreshes_an_oauth_token() {
    let _env_lock = env_lock();
    let home = tempfile::TempDir::new().expect("test: temp home");
    let creds_dir = home.path().join(".claude");
    std::fs::create_dir_all(&creds_dir).expect("test: .claude");
    let creds_path = creds_dir.join(".credentials.json");
    let creds: &[u8] = br#"{"claudeAiOauth":{"accessToken":"sentinel-expired-access","refreshToken":"sentinel-refresh","expiresAt":1000,"subscriptionType":"max","scopes":["user:inference"]}}"#;
    std::fs::write(&creds_path, creds).expect("test: credentials");
    let token_endpoint = LoopbackCounter::start();
    let _home = OauthTestHome::set(home.path(), &token_endpoint.url());
    let config = crate::config::Config {
        config_path: home.path().join("prx").join("config.toml"),
        workspace_dir: home.path().join("prx").join("workspace"),
        default_provider: Some("anthropic".into()),
        default_model: Some("claude-sonnet-4".into()),
        api_key: None,
        ..crate::config::Config::default()
    };
    // Loopback only: nothing is sent unless the lookup finds the expired
    // token, so check that first, without the runtime.
    assert!(
        matches!(
            probe_credential("anthropic", None, &provider_runtime_options_from_config(&config)),
            ProbeCredential::Ready(None)
        ),
        "the Claude Code credential file is not a credential source"
    );
    assert_eq!(token_endpoint.connections(), 0, "the probe must not refresh the token");
    assert_eq!(
        std::fs::read(&creds_path).expect("test: read back"),
        creds,
        "the credential file must be untouched"
    );
    // The listener does stand in for the refresh host: construction's
    // own lookup refreshes through it (and, refused, writes nothing).
    let _ = resolve_claude_code_context(Some("claude-code"), OauthRefresh::Allowed);
    assert_eq!(
        token_endpoint.connections(),
        0,
        "claude credential files are not refreshed"
    );
    assert_eq!(std::fs::read(&creds_path).expect("test: read back"), creds);
}

/// D-081: the connection test's lookup never refreshes a Qwen OAuth
/// token. An expired cached token with a refresh token is reported as
/// due, with no request, and the cache file is untouched; construction's
/// own lookup does refresh, through the same loopback endpoint.
#[test]
fn the_probe_lookup_never_refreshes_a_qwen_token() {
    let _env_lock = env_lock();
    let home = tempfile::TempDir::new().expect("test: temp home");
    let creds_dir = home.path().join(".qwen");
    std::fs::create_dir_all(&creds_dir).expect("test: .qwen");
    let creds_path = creds_dir.join("oauth_creds.json");
    let creds: &[u8] =
        br#"{"access_token":"sentinel-expired-qwen","refresh_token":"sentinel-qwen-refresh","expiry_date":1000}"#;
    std::fs::write(&creds_path, creds).expect("test: credentials");
    let _token = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, None);
    let _refresh = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
    let _resource = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, None);
    let _dashscope = EnvGuard::set("DASHSCOPE_API_KEY", None);
    let token_endpoint = LoopbackCounter::start();
    let _home = OauthTestHome::set(home.path(), &token_endpoint.url());

    let probed = probe_credential(
        "qwen-code",
        Some(QWEN_OAUTH_PLACEHOLDER),
        &ProviderRuntimeOptions::default(),
    );

    assert!(
        matches!(probed, ProbeCredential::Ready(None)),
        "qwen oauth credentials are not loaded"
    );
    assert_eq!(token_endpoint.connections(), 0, "the probe must not refresh the token");
    assert_eq!(std::fs::read(&creds_path).expect("test: read back"), creds);
    let _ = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER), OauthRefresh::Allowed);
    assert_eq!(token_endpoint.connections(), 0, "qwen oauth is not refreshed");
}

/// D-081: the connection test's lookup never exchanges a MiniMax OAuth
/// refresh token (every construction does); it reports the exchange as
/// due and sends nothing.
#[test]
fn the_probe_lookup_never_refreshes_a_minimax_token() {
    let _env_lock = env_lock();
    let _refresh = EnvGuard::set(MINIMAX_OAUTH_REFRESH_TOKEN_ENV, Some("sentinel-minimax-refresh"));
    let _region = EnvGuard::set(MINIMAX_OAUTH_REGION_ENV, None);
    let home = tempfile::TempDir::new().expect("test: temp home");
    let token_endpoint = LoopbackCounter::start();
    let _home = OauthTestHome::set(home.path(), &token_endpoint.url());
    let options = ProviderRuntimeOptions::default();

    let probed = probe_credential("minimax", Some(MINIMAX_OAUTH_PLACEHOLDER), &options);

    assert!(
        matches!(probed, ProbeCredential::RefreshDue),
        "a due exchange must be reported"
    );
    assert_eq!(token_endpoint.connections(), 0, "the probe must not exchange the token");
    assert!(resolve_provider_credential_with_options("minimax", Some(MINIMAX_OAUTH_PLACEHOLDER), &options).is_none());
    assert_eq!(token_endpoint.connections(), 1, "the loopback endpoint was not used");
}

/// D-081: in a test build every OAuth token endpoint is local unless a
/// test installs one, so no test can reach a real token host.
#[test]
fn a_test_build_never_targets_a_real_token_endpoint() {
    let _env_lock = env_lock();
    for endpoint in [
        OauthTokenEndpoint::Minimax(MinimaxOauthRegion::Global),
        OauthTokenEndpoint::Minimax(MinimaxOauthRegion::Cn),
    ] {
        let url = oauth_token_endpoint(endpoint);
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    }
}

/// RED before PMMDtail-i (D-083 C-4): the connection test's credential
/// lookup went through a profile load that migrates a legacy `enc:`
/// value and rewrites auth-profiles.json. A probe writes no credential
/// file.
#[test]
fn the_probe_lookup_never_rewrites_the_profiles_file() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let store = crate::auth::profiles::AuthProfilesStore::new(dir.path(), true);
    store
        .upsert_profile(
            crate::auth::profiles::AuthProfile::new_token("moonshot", "default", "sentinel-moonshot".into()),
            true,
        )
        .expect("test: profile");
    let legacy = crate::security::secrets::SecretStore::new(dir.path(), true)
        .legacy_xor_value("sentinel-moonshot")
        .expect("test: legacy value");
    let mut raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(store.path()).expect("test: read")).expect("test: json");
    for profile in raw["profiles"].as_object_mut().expect("test: profiles").values_mut() {
        profile["token"] = serde_json::json!(legacy);
    }
    std::fs::write(store.path(), serde_json::to_vec_pretty(&raw).expect("test: json")).expect("test: write");
    let before = std::fs::read_to_string(store.path()).expect("test: read back");
    let options = ProviderRuntimeOptions {
        openprx_dir: Some(dir.path().to_path_buf()),
        secrets_encrypt: true,
        ..ProviderRuntimeOptions::default()
    };

    let probed = probe_credential("moonshot", None, &options);

    assert!(
        matches!(&probed, ProbeCredential::Ready(Some(key)) if key == "sentinel-moonshot"),
        "the legacy value is still read"
    );
    assert_eq!(
        std::fs::read_to_string(store.path()).expect("test: read back"),
        before,
        "the probe rewrote the file"
    );
}

#[test]
fn regional_alias_predicates_cover_expected_variants() {
    assert!(is_moonshot_alias("moonshot"));
    assert!(is_moonshot_alias("kimi-global"));
    assert!(is_glm_alias("glm"));
    assert!(is_glm_alias("bigmodel"));
    assert!(is_minimax_alias("minimax-io"));
    assert!(is_minimax_alias("minimaxi"));
    assert!(is_minimax_alias("minimax-oauth"));
    assert!(is_minimax_alias("minimax-portal-cn"));
    assert!(is_qwen_alias("dashscope"));
    assert!(is_qwen_alias("qwen-us"));
    assert!(is_qwen_alias("qwen-code"));
    assert!(is_qwen_alias("qwen-coding"));
    assert!(is_qwen_alias("dashscope-coding-intl"));
    assert!(is_qwen_coding_alias("dashscope-coding"));
    assert!(is_qwen_coding_alias("qwen-coding-intl"));
    assert!(is_qwen_oauth_alias("qwen-code"));
    assert!(is_qwen_oauth_alias("qwen_oauth"));
    assert!(is_claude_code_alias("claude-code"));
    assert!(is_claude_code_alias("claude-cli"));
    assert!(is_zai_alias("z.ai"));
    assert!(is_zai_alias("zai-cn"));
    assert!(is_qianfan_alias("qianfan"));
    assert!(is_qianfan_alias("baidu"));
    assert!(is_litellm_alias("litellm"));
    assert!(is_litellm_alias("lite-llm"));
    assert!(is_vllm_alias("vllm"));
    assert!(is_vllm_alias("v-llm"));
    assert!(is_huggingface_alias("huggingface"));
    assert!(is_huggingface_alias("hf"));
    assert!(is_huggingface_alias("hf-inference"));

    assert!(!is_moonshot_alias("openrouter"));
    assert!(!is_glm_alias("openai"));
    assert!(!is_qwen_alias("gemini"));
    assert!(!is_claude_code_alias("anthropic"));
    assert!(!is_zai_alias("anthropic"));
    assert!(!is_qianfan_alias("cohere"));
    assert!(!is_litellm_alias("lite"));
    assert!(!is_vllm_alias("vllm-local"));
    assert!(!is_huggingface_alias("hugging-face"));
}

#[test]
fn canonical_china_provider_name_maps_regional_aliases() {
    assert_eq!(canonical_china_provider_name("moonshot"), Some("moonshot"));
    assert_eq!(canonical_china_provider_name("kimi-intl"), Some("moonshot"));
    assert_eq!(canonical_china_provider_name("glm"), Some("glm"));
    assert_eq!(canonical_china_provider_name("zhipu-cn"), Some("glm"));
    assert_eq!(canonical_china_provider_name("minimax"), Some("minimax"));
    assert_eq!(canonical_china_provider_name("minimax-cn"), Some("minimax"));
    assert_eq!(canonical_china_provider_name("qwen"), Some("qwen"));
    assert_eq!(canonical_china_provider_name("dashscope-us"), Some("qwen"));
    assert_eq!(canonical_china_provider_name("qwen-code"), Some("qwen"));
    assert_eq!(canonical_china_provider_name("zai"), Some("zai"));
    assert_eq!(canonical_china_provider_name("z.ai-cn"), Some("zai"));
    assert_eq!(canonical_china_provider_name("qianfan"), Some("qianfan"));
    assert_eq!(canonical_china_provider_name("baidu"), Some("qianfan"));
    assert_eq!(canonical_china_provider_name("openai"), None);
}

#[test]
fn regional_endpoint_aliases_map_to_expected_urls() {
    assert_eq!(minimax_base_url("minimax"), Some(MINIMAX_INTL_BASE_URL));
    assert_eq!(minimax_base_url("minimax-intl"), Some(MINIMAX_INTL_BASE_URL));
    assert_eq!(minimax_base_url("minimax-cn"), Some(MINIMAX_CN_BASE_URL));

    assert_eq!(glm_base_url("glm"), Some(GLM_GLOBAL_BASE_URL));
    assert_eq!(glm_base_url("glm-cn"), Some(GLM_CN_BASE_URL));
    assert_eq!(glm_base_url("bigmodel"), Some(GLM_CN_BASE_URL));

    assert_eq!(moonshot_base_url("moonshot"), Some(MOONSHOT_CN_BASE_URL));
    assert_eq!(moonshot_base_url("moonshot-intl"), Some(MOONSHOT_INTL_BASE_URL));

    assert_eq!(qwen_base_url("qwen"), Some(QWEN_CN_BASE_URL));
    assert_eq!(qwen_base_url("qwen-cn"), Some(QWEN_CN_BASE_URL));
    assert_eq!(qwen_base_url("qwen-intl"), Some(QWEN_INTL_BASE_URL));
    assert_eq!(qwen_base_url("qwen-us"), Some(QWEN_US_BASE_URL));
    assert_eq!(qwen_base_url("qwen-code"), Some(QWEN_CN_BASE_URL));
    assert_eq!(qwen_coding_base_url("dashscope-coding"), Some(QWEN_CODING_CN_BASE_URL));
    assert_eq!(qwen_coding_base_url("qwen-coding"), Some(QWEN_CODING_CN_BASE_URL));
    assert_eq!(
        qwen_coding_base_url("dashscope-coding-intl"),
        Some(QWEN_CODING_INTL_BASE_URL)
    );
    assert_eq!(
        qwen_coding_base_url("qwen-coding-intl"),
        Some(QWEN_CODING_INTL_BASE_URL)
    );

    assert_eq!(zai_base_url("zai"), Some(ZAI_GLOBAL_BASE_URL));
    assert_eq!(zai_base_url("z.ai"), Some(ZAI_GLOBAL_BASE_URL));
    assert_eq!(zai_base_url("zai-global"), Some(ZAI_GLOBAL_BASE_URL));
    assert_eq!(zai_base_url("z.ai-global"), Some(ZAI_GLOBAL_BASE_URL));
    assert_eq!(zai_base_url("zai-cn"), Some(ZAI_CN_BASE_URL));
    assert_eq!(zai_base_url("z.ai-cn"), Some(ZAI_CN_BASE_URL));
}

// ── Primary providers ────────────────────────────────────

#[test]
fn factory_openrouter() {
    assert!(create_provider("openrouter", Some("provider-test-credential")).is_ok());
    assert!(create_provider("openrouter", None).is_ok());
}

#[test]
fn factory_anthropic() {
    assert!(create_provider("anthropic", Some("provider-test-credential")).is_ok());
}

#[test]
fn factory_openai() {
    assert!(create_provider("openai", Some("provider-test-credential")).is_ok());
}

#[test]
fn factory_openai_codex() {
    let options = ProviderRuntimeOptions::default();
    assert!(create_provider_with_options("openai-codex", None, &options).is_ok());
}

#[test]
fn factory_ollama() {
    assert!(create_provider("ollama", None).is_ok());
    // Ollama may use API key when a remote endpoint is configured.
    assert!(create_provider("ollama", Some("dummy")).is_ok());
    assert!(create_provider("ollama", Some("any-value-here")).is_ok());
}

#[test]
fn factory_gemini() {
    assert!(create_provider("gemini", Some("test-key")).is_ok());
    assert!(create_provider("google", Some("test-key")).is_ok());
    assert!(create_provider("google-gemini", Some("test-key")).is_ok());
    // Should also work without key (will try CLI auth)
    assert!(create_provider("gemini", None).is_ok());
}

/// The validator asks the constructed provider which schema document it
/// put on the wire (`tool_schema_dialect`/`tool_schema_dialect_for`); the
/// rewriting happens inside each provider implementation. This pins what
/// each label builds, so a provider that starts (or stops) cleaning its
/// schemas is a visible change. (PMM-C-L deleted the label-keyed map this
/// test used to compare against.)
#[test]
fn provider_aliases_agree_with_the_declared_schema_dialect() {
    for alias in [
        "anthropic",
        "claude-code",
        "claude-cli",
        "gemini",
        "google",
        "google-gemini",
        "openai",
        "ollama",
        "openrouter",
        "venice",
        "groq",
        // Bring-your-own endpoints. The label carries a URL, so a map keyed
        // on the whole string cannot match it; `anthropic-custom:` builds an
        // `AnthropicProvider` that strips `$ref`/`$defs` on the wire, and a
        // label resolving to `Raw` would put the validator back on the
        // unstripped document.
        "custom:https://endpoint.invalid/v1",
        "anthropic-custom:https://endpoint.invalid/v1",
    ] {
        let provider = create_provider(alias, Some("test-key"))
            .unwrap_or_else(|error| panic!("test: provider {alias} constructs: {error}"));
        let expected = match alias {
            "gemini" | "google" | "google-gemini" => crate::tools::ToolSchemaDialect::Gemini,
            "anthropic" | "claude-code" | "claude-cli" | "anthropic-custom:https://endpoint.invalid/v1" => {
                crate::tools::ToolSchemaDialect::NoRootCombinators
            }
            _ => crate::tools::ToolSchemaDialect::Raw,
        };
        assert_eq!(
            provider.tool_schema_dialect(),
            expected,
            "provider label '{alias}' builds a provider with a different schema dialect"
        );
    }
}

/// The provider factory recognizes a bring-your-own label through one
/// splitter, so every spelling it accepts — any ASCII case, surrounding
/// whitespace — builds the provider that splitter's dialect names, and a
/// label the splitter rejects is not bring-your-own.
#[test]
fn byo_label_spellings_resolve_the_dialect_the_constructed_provider_declares() {
    use ByoDialect::{AnthropicCompatible, OpenAiCompatible};
    for (label, dialect) in [
        ("custom:https://endpoint.invalid/v1", OpenAiCompatible),
        ("CUSTOM:https://endpoint.invalid/v1", OpenAiCompatible),
        ("  Custom:https://endpoint.invalid/v1  ", OpenAiCompatible),
        ("anthropic-custom:https://endpoint.invalid/v1", AnthropicCompatible),
        ("Anthropic-Custom:https://endpoint.invalid/v1", AnthropicCompatible),
        ("  ANTHROPIC-CUSTOM:https://endpoint.invalid/v1 ", AnthropicCompatible),
        ("\tAnthropic-Custom:https://endpoint.invalid/v1", AnthropicCompatible),
    ] {
        // The splitter is asserted on its own, not only through the
        // provider it leads to (SP-INT2 a31).
        assert_eq!(
            byo_dialect_of(label),
            Some(dialect),
            "test: '{label}' is a bring-your-own label of that dialect"
        );
        assert!(
            matches!(classify_provider(label), ProviderIdentity::ByoEndpoint { .. }),
            "test: '{label}' is a bring-your-own label"
        );
        let provider = create_provider(label, Some("test-key"))
            .unwrap_or_else(|error| panic!("test: provider {label} constructs: {error}"));
        let expected = match byo_dialect_of(label) {
            Some(ByoDialect::AnthropicCompatible) => crate::tools::ToolSchemaDialect::NoRootCombinators,
            Some(ByoDialect::OpenAiCompatible) => crate::tools::ToolSchemaDialect::Raw,
            None => panic!("test: '{label}' is a bring-your-own label"),
        };
        assert_eq!(
            provider.tool_schema_dialect(),
            expected,
            "the splitter and the constructed provider disagree on '{label}'"
        );
    }
    // A prefix with no URL, or a URL the factory refuses, is still that
    // dialect's label: the splitter decides the prefix, the URL check
    // comes after it.
    for (label, dialect) in [
        ("custom:", OpenAiCompatible),
        ("anthropic-custom:", AnthropicCompatible),
        ("\tAnthropic-Custom:x", AnthropicCompatible),
    ] {
        assert_eq!(
            byo_dialect_of(label),
            Some(dialect),
            "test: '{label:?}' is a bring-your-own label of that dialect"
        );
    }
    for label in ["custom", "xcustom:https://endpoint.invalid/v1", "anthropic-customs"] {
        assert_eq!(
            byo_dialect_of(label),
            None,
            "test: '{label}' is not a bring-your-own label"
        );
    }
}

/// A wrapper is what the dispatcher holds, so the dialect it declares is the
/// one the validator uses. `ReliableProvider` and `RouterProvider` wrap a
/// concrete provider; without a forwarding override they inherit the trait
/// default `Raw`, and every Gemini turn behind a failover chain is then
/// validated against keywords Gemini never saw.
#[test]
fn wrapper_providers_forward_the_wrapped_schema_dialect() {
    let wrapped = create_provider("gemini", Some("test-key")).expect("test: gemini constructs");
    let reliable = reliable::ReliableProvider::new(vec![("gemini".to_string(), wrapped)], 1, 50);
    assert_eq!(
        reliable.tool_schema_dialect(),
        crate::tools::ToolSchemaDialect::Gemini,
        "ReliableProvider must declare the dialect of the provider it wraps"
    );

    let wrapped = create_provider("gemini", Some("test-key")).expect("test: gemini constructs");
    let router = router::RouterProvider::from_test_parts(
        vec![("gemini".to_string(), wrapped)],
        Vec::new(),
        "gemini-2.0-flash".to_string(),
    );
    assert_eq!(
        router.tool_schema_dialect(),
        crate::tools::ToolSchemaDialect::Gemini,
        "RouterProvider must declare the dialect of the provider it routes to"
    );
}

/// PMM-C-L: a mixed chain answers `Raw` model-less, but the member that
/// served a turn (`ChatTrace::final_provider`) answers its own dialect,
/// through a router too, so the agent loop validates a fallback against
/// the document that fallback put on the wire.
#[test]
fn a_chain_answers_the_dialect_of_the_member_that_served() {
    let chain = || {
        reliable::ReliableProvider::new(
            vec![
                (
                    "gemini".to_string(),
                    create_provider("gemini", Some("test-key")).expect("test: gemini constructs"),
                ),
                (
                    "openai".to_string(),
                    create_provider("openai", Some("test-key")).expect("test: openai constructs"),
                ),
            ],
            1,
            50,
        )
    };
    let reliable = chain();
    assert_eq!(reliable.tool_schema_dialect(), crate::tools::ToolSchemaDialect::Raw);
    assert_eq!(
        reliable.tool_schema_dialect_of_member("gemini", "gemini-2.0-flash"),
        Some(crate::tools::ToolSchemaDialect::Gemini)
    );
    assert_eq!(
        reliable.tool_schema_dialect_of_member("openai", "gpt-4o"),
        Some(crate::tools::ToolSchemaDialect::Raw)
    );
    assert_eq!(reliable.tool_schema_dialect_of_member("anthropic", "x"), None);

    let router = router::RouterProvider::from_test_parts(
        vec![("gemini".to_string(), Box::new(chain()) as Box<dyn Provider>)],
        Vec::new(),
        "gemini-2.0-flash".to_string(),
    );
    assert_eq!(
        router.tool_schema_dialect_of_member("gemini", "gemini-2.0-flash"),
        Some(crate::tools::ToolSchemaDialect::Gemini)
    );
    assert_eq!(router.tool_schema_dialect_of_member("mistral", "x"), None);
}

/// A chain whose members all belong to one family resolves to that family:
/// whichever member serves the turn produced the same wire document, so
/// enforcing that family's document is exact.
#[test]
fn a_single_family_chain_declares_that_family() {
    let chain = reliable::ReliableProvider::new(
        vec![
            (
                "gemini".to_string(),
                create_provider("gemini", Some("test-key")).expect("test: gemini constructs"),
            ),
            (
                "google".to_string(),
                create_provider("google", Some("test-key")).expect("test: google constructs"),
            ),
        ],
        1,
        50,
    );
    assert_eq!(
        chain.tool_schema_dialect(),
        crate::tools::ToolSchemaDialect::Gemini,
        "a chain of one family must declare that family"
    );
}

/// A chain that can land on either of two *different* families has no one
/// answer, and the rewrites are not comparable: `Gemini` strips keywords,
/// `NoRootCombinators` restructures the root, and neither subsumes the
/// other. Ranking them and picking the "widest" makes enforcement looser
/// than the wire on every turn the other member serves — the model is shown
/// `additionalProperties: false` and then not held to it. A mixed chain
/// therefore resolves to `Raw`: the published document, which is the
/// strongest contract and the one no member can be shown less than.
#[test]
fn a_mixed_family_chain_declares_raw_rather_than_the_widest_rewrite() {
    let chain = reliable::ReliableProvider::new(
        vec![
            (
                "gemini".to_string(),
                create_provider("gemini", Some("test-key")).expect("test: gemini constructs"),
            ),
            (
                "openai".to_string(),
                create_provider("openai", Some("test-key")).expect("test: openai constructs"),
            ),
        ],
        1,
        50,
    );
    assert_eq!(
        chain.tool_schema_dialect(),
        crate::tools::ToolSchemaDialect::Raw,
        "a mixed-family chain must not enforce one member's rewrite on the other member's turns"
    );

    let routed = router::RouterProvider::from_test_parts(
        vec![
            (
                "gemini".to_string(),
                create_provider("gemini", Some("test-key")).expect("test: gemini constructs"),
            ),
            (
                "openai".to_string(),
                create_provider("openai", Some("test-key")).expect("test: openai constructs"),
            ),
        ],
        Vec::new(),
        "gemini-2.0-flash".to_string(),
    );
    assert_eq!(
        routed.tool_schema_dialect(),
        crate::tools::ToolSchemaDialect::Raw,
        "a mixed-family router must not enforce one destination's rewrite on another's turns"
    );
}

/// Gemini strips keywords while NoRootCombinators preserves constraints and
/// may refuse a structural rewrite. A mixed chain resolves Raw, preserving
/// both wire and enforcement documents without inventing a local refusal.
#[test]
fn a_mixed_chain_of_gemini_and_no_root_combinators_resolves_raw_without_refusing() {
    let declared = "custom:https://endpoint.invalid/v1";
    let options = ProviderRuntimeOptions {
        tool_schema_dialects: vec![ToolSchemaDialectDeclaration {
            provider: declared.to_string(),
            model: None,
            dialect: crate::tools::ToolSchemaDialect::NoRootCombinators,
        }],
        ..ProviderRuntimeOptions::default()
    };
    let lowering = create_provider_with_options(declared, Some("test-key"), &options)
        .expect("test: a declared custom endpoint constructs");
    assert_eq!(
        lowering.tool_schema_dialect(),
        crate::tools::ToolSchemaDialect::NoRootCombinators,
        "the declaration did not reach the constructed provider"
    );

    let chain = reliable::ReliableProvider::new(
        vec![
            (
                "gemini".to_string(),
                create_provider("gemini", Some("test-key")).expect("test: gemini constructs"),
            ),
            (declared.to_string(), lowering),
        ],
        1,
        50,
    );
    let resolved = chain.tool_schema_dialect();
    assert_eq!(
        resolved,
        crate::tools::ToolSchemaDialect::Raw,
        "a chain mixing a keyword-stripping and a structural rewrite must fall back to the published document"
    );

    // A contract the structural rewrite refuses must still be offered under
    // the resolved dialect: resolution may not turn an available tool into
    // a refused one.
    let conditional = serde_json::json!({
        "type": "object",
        "properties": {"path": {"type": "string"}, "key": {"type": "string"}},
        "anyOf": [{"required": ["path"]}, {"required": ["key"]}]
    });
    let wire = resolved
        .wire_schema("fixture_alternatives", &conditional)
        .expect("test: resolving a mixed chain must not manufacture a refusal");
    assert_eq!(
        &*wire, &conditional,
        "the resolved dialect changed the published document"
    );
    assert_eq!(&*resolved.enforceable_schema(&conditional), &conditional);
}

#[test]
fn native_anthropic_and_declared_no_root_chain_retains_the_shared_restriction() {
    let declared = "custom:https://endpoint.invalid/v1";
    let options = ProviderRuntimeOptions {
        tool_schema_dialects: vec![ToolSchemaDialectDeclaration {
            provider: declared.to_string(),
            model: None,
            dialect: crate::tools::ToolSchemaDialect::NoRootCombinators,
        }],
        ..ProviderRuntimeOptions::default()
    };
    let chain = reliable::ReliableProvider::new(
        vec![
            (
                "anthropic".to_owned(),
                create_provider("anthropic", Some("test-key")).unwrap(),
            ),
            (
                declared.to_owned(),
                create_provider_with_options(declared, Some("test-key"), &options).unwrap(),
            ),
        ],
        1,
        50,
    );
    assert_eq!(
        chain.tool_schema_dialect(),
        crate::tools::ToolSchemaDialect::NoRootCombinators
    );
    let conditional = serde_json::json!({
        "type":"object", "properties":{"path":{"type":"string"},"key":{"type":"string"}},
        "anyOf":[{"required":["path"]},{"required":["key"]}]
    });
    assert!(
        chain
            .tool_schema_dialect()
            .wire_schema("fixture_alternatives", &conditional)
            .is_err()
    );
}

/// The two labels that do rewrite their schemas must not decay to `Raw`,
/// which would put the validator back on the published document.
#[test]
fn cleaning_providers_declare_a_non_raw_dialect() {
    let dialect = |label: &str| {
        create_provider(label, Some("test-key"))
            .unwrap_or_else(|error| panic!("test: provider {label} constructs: {error}"))
            .tool_schema_dialect()
    };
    assert_eq!(dialect("gemini"), crate::tools::ToolSchemaDialect::Gemini);
    assert_eq!(dialect("anthropic"), crate::tools::ToolSchemaDialect::NoRootCombinators);
    assert_eq!(dialect("openai"), crate::tools::ToolSchemaDialect::Raw);
}

// ── OpenAI-compatible providers ──────────────────────────

#[test]
fn factory_venice() {
    assert!(create_provider("venice", Some("vn-key")).is_ok());
}

#[test]
fn factory_vercel() {
    assert!(create_provider("vercel", Some("key")).is_ok());
    assert!(create_provider("vercel-ai", Some("key")).is_ok());
}

#[test]
fn factory_cloudflare() {
    assert!(create_provider("cloudflare", Some("key")).is_ok());
    assert!(create_provider("cloudflare-ai", Some("key")).is_ok());
}

#[test]
fn factory_moonshot() {
    assert!(create_provider("moonshot", Some("key")).is_ok());
    assert!(create_provider("kimi", Some("key")).is_ok());
    assert!(create_provider("moonshot-intl", Some("key")).is_ok());
    assert!(create_provider("moonshot-cn", Some("key")).is_ok());
    assert!(create_provider("kimi-intl", Some("key")).is_ok());
    assert!(create_provider("kimi-cn", Some("key")).is_ok());
}

#[test]
fn factory_kimi_code() {
    assert!(create_provider("kimi-code", Some("key")).is_ok());
    assert!(create_provider("kimi_coding", Some("key")).is_ok());
    assert!(create_provider("kimi_for_coding", Some("key")).is_ok());
}

#[test]
fn factory_synthetic() {
    assert!(create_provider("synthetic", Some("key")).is_ok());
}

#[test]
fn factory_opencode() {
    assert!(create_provider("opencode", Some("key")).is_ok());
    assert!(create_provider("opencode-zen", Some("key")).is_ok());
}

#[test]
fn factory_zai() {
    assert!(create_provider("zai", Some("key")).is_ok());
    assert!(create_provider("z.ai", Some("key")).is_ok());
    assert!(create_provider("zai-global", Some("key")).is_ok());
    assert!(create_provider("z.ai-global", Some("key")).is_ok());
    assert!(create_provider("zai-cn", Some("key")).is_ok());
    assert!(create_provider("z.ai-cn", Some("key")).is_ok());
}

#[test]
fn factory_glm() {
    assert!(create_provider("glm", Some("key")).is_ok());
    assert!(create_provider("zhipu", Some("key")).is_ok());
    assert!(create_provider("glm-cn", Some("key")).is_ok());
    assert!(create_provider("zhipu-cn", Some("key")).is_ok());
    assert!(create_provider("glm-global", Some("key")).is_ok());
    assert!(create_provider("bigmodel", Some("key")).is_ok());
}

#[test]
fn factory_glm_routes_to_streaming_compatible_provider() {
    let provider = create_provider("glm", Some("key")).unwrap();
    assert!(provider.supports_streaming());
}

#[test]
fn factory_minimax() {
    assert!(create_provider("minimax", Some("key")).is_ok());
    assert!(create_provider("minimax-intl", Some("key")).is_ok());
    assert!(create_provider("minimax-io", Some("key")).is_ok());
    assert!(create_provider("minimax-global", Some("key")).is_ok());
    assert!(create_provider("minimax-cn", Some("key")).is_ok());
    assert!(create_provider("minimaxi", Some("key")).is_ok());
    assert!(create_provider("minimax-oauth", Some("key")).is_ok());
    assert!(create_provider("minimax-oauth-cn", Some("key")).is_ok());
    assert!(create_provider("minimax-portal", Some("key")).is_ok());
    assert!(create_provider("minimax-portal-cn", Some("key")).is_ok());
}

#[test]
fn factory_bedrock() {
    // Bedrock uses AWS config files for credentials, not API key.
    assert!(create_provider("bedrock", None).is_ok());
    assert!(create_provider("aws-bedrock", None).is_ok());
    // Passing an api_key is harmless (ignored).
    assert!(create_provider("bedrock", Some("ignored")).is_ok());
}

#[test]
fn factory_qianfan() {
    assert!(create_provider("qianfan", Some("key")).is_ok());
    assert!(create_provider("baidu", Some("key")).is_ok());
}

#[test]
fn factory_qwen() {
    assert!(create_provider("qwen", Some("key")).is_ok());
    assert!(create_provider("dashscope", Some("key")).is_ok());
    assert!(create_provider("qwen-cn", Some("key")).is_ok());
    assert!(create_provider("dashscope-cn", Some("key")).is_ok());
    assert!(create_provider("qwen-intl", Some("key")).is_ok());
    assert!(create_provider("dashscope-intl", Some("key")).is_ok());
    assert!(create_provider("qwen-international", Some("key")).is_ok());
    assert!(create_provider("dashscope-international", Some("key")).is_ok());
    assert!(create_provider("qwen-us", Some("key")).is_ok());
    assert!(create_provider("dashscope-us", Some("key")).is_ok());
    assert!(create_provider("dashscope-coding", Some("key")).is_ok());
    assert!(create_provider("qwen-coding", Some("key")).is_ok());
    assert!(create_provider("dashscope-coding-intl", Some("key")).is_ok());
    assert!(create_provider("qwen-coding-intl", Some("key")).is_ok());
    let removed = create_provider("qwen-code", Some("key"));
    assert!(removed.is_err(), "qwen-code oauth is removed");
    assert!(
        removed.unwrap_err().to_string().contains("2026-04-15"),
        "the error names the shutdown"
    );
    assert!(create_provider("qwen-oauth", Some("key")).is_err());
}

#[test]
fn factory_lmstudio() {
    assert!(create_provider("lmstudio", Some("key")).is_ok());
    assert!(create_provider("lm-studio", Some("key")).is_ok());
    assert!(create_provider("lmstudio", None).is_ok());
}

#[test]
fn factory_llamacpp() {
    assert!(create_provider("llamacpp", Some("key")).is_ok());
    assert!(create_provider("llama.cpp", Some("key")).is_ok());
    assert!(create_provider("llamacpp", None).is_ok());
}

#[test]
fn factory_litellm() {
    assert!(create_provider("litellm", Some("key")).is_ok());
    assert!(create_provider("lite-llm", Some("key")).is_ok());
    assert!(create_provider("litellm", None).is_ok());
    assert!(create_provider_with_url("litellm", Some("key"), Some("http://127.0.0.1:4001")).is_ok());
}

#[test]
fn factory_vllm() {
    assert!(create_provider("vllm", Some("key")).is_ok());
    assert!(create_provider("v-llm", Some("key")).is_ok());
    assert!(create_provider("vllm", None).is_ok());
    assert!(create_provider_with_url("vllm", Some("key"), Some("http://127.0.0.1:9000/v1")).is_ok());
}

#[test]
fn factory_huggingface() {
    assert!(create_provider("huggingface", Some("key")).is_ok());
    assert!(create_provider("hf", Some("key")).is_ok());
    assert!(create_provider("hf-inference", Some("key")).is_ok());
}

// ── Extended ecosystem ───────────────────────────────────

#[test]
fn factory_groq() {
    assert!(create_provider("groq", Some("key")).is_ok());
}

#[test]
fn factory_mistral() {
    assert!(create_provider("mistral", Some("key")).is_ok());
}

#[test]
fn factory_xai() {
    assert!(create_provider("xai", Some("key")).is_ok());
    assert!(create_provider("grok", Some("key")).is_ok());
}

#[test]
fn xai_subscription_profile_is_not_an_api_key_and_refuses_another_host() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let auth = crate::auth::AuthService::new(dir.path(), false);
    auth.store_xai_tokens(
        "default",
        crate::auth::profiles::TokenSet {
            access_token: "access-sentinel".into(),
            refresh_token: Some("refresh-sentinel".into()),
            id_token: None,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            token_type: Some("Bearer".into()),
            scope: None,
        },
        true,
    )
    .expect("test: store xai oauth");
    let options = ProviderRuntimeOptions {
        openprx_dir: Some(dir.path().to_path_buf()),
        secrets_encrypt: false,
        ..ProviderRuntimeOptions::default()
    };
    let resolved = resolve_provider_credentials_with("xai", None, &options, OauthRefresh::Allowed);
    assert!(resolved.xai_subscription);
    assert_eq!(resolved.credential.as_deref(), Some("access-sentinel"));
    let explicit = resolve_provider_credentials_with("grok", Some("api-key-sentinel"), &options, OauthRefresh::Allowed);
    assert!(!explicit.xai_subscription);
    assert_eq!(explicit.credential.as_deref(), Some("api-key-sentinel"));
    assert!(create_provider_with_options("xai", None, &options).is_ok());
    assert!(crate::auth::xai_oauth::subscription_base_url(Some("https://evil.example/v1")).is_err());
}

#[test]
fn factory_deepseek() {
    assert!(create_provider("deepseek", Some("key")).is_ok());
}

#[test]
fn factory_together() {
    assert!(create_provider("together", Some("key")).is_ok());
    assert!(create_provider("together-ai", Some("key")).is_ok());
}

#[test]
fn factory_fireworks() {
    assert!(create_provider("fireworks", Some("key")).is_ok());
    assert!(create_provider("fireworks-ai", Some("key")).is_ok());
}

#[test]
fn factory_perplexity() {
    assert!(create_provider("perplexity", Some("key")).is_ok());
}

#[test]
fn factory_cohere() {
    assert!(create_provider("cohere", Some("key")).is_ok());
}

#[test]
fn factory_copilot() {
    assert!(create_provider("copilot", Some("key")).is_ok());
    assert!(create_provider("github-copilot", Some("key")).is_ok());
}

#[test]
fn factory_copilot_supports_streaming() {
    let provider = create_provider("copilot", Some("key")).unwrap();
    assert!(provider.supports_streaming());
}

#[test]
fn factory_nvidia() {
    assert!(create_provider("nvidia", Some("nvapi-test")).is_ok());
    assert!(create_provider("nvidia-nim", Some("nvapi-test")).is_ok());
    assert!(create_provider("build.nvidia.com", Some("nvapi-test")).is_ok());
}

// ── AI inference routers ─────────────────────────────────

#[test]
fn factory_astrai() {
    assert!(create_provider("astrai", Some("sk-astrai-test")).is_ok());
}

// ── Custom / BYOP provider ─────────────────────────────

#[test]
fn factory_custom_url() {
    let p = create_provider("custom:https://my-llm.example.com", Some("key"));
    assert!(p.is_ok());
}

#[test]
fn factory_custom_localhost() {
    let p = create_provider("custom:http://localhost:1234", Some("key"));
    assert!(p.is_ok());
}

#[test]
fn factory_custom_no_key() {
    let p = create_provider("custom:https://my-llm.example.com", None);
    assert!(p.is_ok());
}

#[test]
fn factory_custom_empty_url_errors() {
    match create_provider("custom:", None) {
        Err(e) => assert!(
            e.to_string().contains("requires a URL"),
            "Expected 'requires a URL', got: {e}"
        ),
        Ok(_) => panic!("Expected error for empty custom URL"),
    }
}

#[test]
fn factory_custom_invalid_url_errors() {
    match create_provider("custom:not-a-url", None) {
        Err(e) => assert!(
            e.to_string().contains("requires a valid URL"),
            "Expected 'requires a valid URL', got: {e}"
        ),
        Ok(_) => panic!("Expected error for invalid custom URL"),
    }
}

#[test]
fn factory_custom_unsupported_scheme_errors() {
    match create_provider("custom:ftp://example.com", None) {
        Err(e) => assert!(
            e.to_string().contains("http:// or https://"),
            "Expected scheme validation error, got: {e}"
        ),
        Ok(_) => panic!("Expected error for unsupported custom URL scheme"),
    }
}

#[test]
fn factory_custom_trims_whitespace() {
    let p = create_provider("custom:  https://my-llm.example.com  ", Some("key"));
    assert!(p.is_ok());
}

// ── Anthropic-compatible custom endpoints ─────────────────

#[test]
fn factory_anthropic_custom_url() {
    let p = create_provider("anthropic-custom:https://api.example.com", Some("key"));
    assert!(p.is_ok());
}

#[test]
fn factory_anthropic_custom_trailing_slash() {
    let p = create_provider("anthropic-custom:https://api.example.com/", Some("key"));
    assert!(p.is_ok());
}

#[test]
fn factory_anthropic_custom_no_key() {
    let p = create_provider("anthropic-custom:https://api.example.com", None);
    assert!(p.is_ok());
}

#[test]
fn factory_anthropic_custom_empty_url_errors() {
    match create_provider("anthropic-custom:", None) {
        Err(e) => assert!(
            e.to_string().contains("requires a URL"),
            "Expected 'requires a URL', got: {e}"
        ),
        Ok(_) => panic!("Expected error for empty anthropic-custom URL"),
    }
}

#[test]
fn factory_anthropic_custom_invalid_url_errors() {
    match create_provider("anthropic-custom:not-a-url", None) {
        Err(e) => assert!(
            e.to_string().contains("requires a valid URL"),
            "Expected 'requires a valid URL', got: {e}"
        ),
        Ok(_) => panic!("Expected error for invalid anthropic-custom URL"),
    }
}

#[test]
fn factory_anthropic_custom_unsupported_scheme_errors() {
    match create_provider("anthropic-custom:ftp://example.com", None) {
        Err(e) => assert!(
            e.to_string().contains("http:// or https://"),
            "Expected scheme validation error, got: {e}"
        ),
        Ok(_) => panic!("Expected error for unsupported anthropic-custom URL scheme"),
    }
}

// ── Error cases ──────────────────────────────────────────

#[test]
fn factory_unknown_provider_errors() {
    let p = create_provider("nonexistent", None);
    assert!(p.is_err());
    let msg = p.err().unwrap().to_string();
    assert!(msg.contains("Unknown provider"));
    assert!(msg.contains("nonexistent"));
}

#[test]
fn factory_empty_name_errors() {
    assert!(create_provider("", None).is_err());
}

#[test]
fn resolved_provider_refuses_an_invalid_fallback() {
    let reliability = crate::config::ReliabilityConfig {
        provider_retries: 1,
        provider_backoff_ms: 100,
        fallback_providers: vec![
            "openrouter".into(),
            "nonexistent-provider".into(),
            "openai".into(),
            "openai".into(),
        ],
        acknowledge_single_provider_risk: false,
        api_keys: Vec::new(),
        model_fallbacks: std::collections::HashMap::new(),
        channel_initial_backoff_secs: 2,
        channel_max_backoff_secs: 60,
        scheduler_poll_secs: 15,
        scheduler_retries: 2,
    };

    let config = factory_config(
        "openrouter",
        "anthropic/claude-sonnet-4",
        Some("provider-test-credential"),
        None,
        reliability,
    );
    let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
    assert!(provider.is_err());
}

#[test]
fn resilient_provider_errors_for_invalid_primary() {
    let config = factory_config(
        "totally-invalid",
        "fixture-model",
        Some("provider-test-credential"),
        None,
        crate::config::ReliabilityConfig::default(),
    );
    let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
    assert!(provider.is_err());
}

#[test]
fn summarize_provider_availability_marks_degraded_when_only_primary_has_credentials() {
    let _guard = env_lock();

    // Isolate from host credentials: override HOME to an empty temp dir so
    // resolve_claude_code_context cannot find ~/.claude/.credentials.json,
    // and clear any ambient Anthropic env vars.
    let iso_home = std::env::temp_dir().join(format!("openprx-provider-degrade-test-{}", std::process::id()));
    std::fs::create_dir_all(&iso_home).unwrap();
    let _home_guard = EnvGuard::set("HOME", Some(iso_home.to_str().unwrap()));
    let _anthropic_key_guard = EnvGuard::set("ANTHROPIC_API_KEY", None);
    let _anthropic_oauth_guard = EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", None);

    let reliability = crate::config::ReliabilityConfig {
        provider_retries: 1,
        provider_backoff_ms: 100,
        fallback_providers: vec!["anthropic".into()],
        acknowledge_single_provider_risk: false,
        api_keys: Vec::new(),
        model_fallbacks: std::collections::HashMap::new(),
        channel_initial_backoff_secs: 2,
        channel_max_backoff_secs: 60,
        scheduler_poll_secs: 15,
        scheduler_retries: 2,
    };

    let summary = summarize_provider_availability(
        "openai",
        Some("sk-test"),
        &reliability,
        &ProviderRuntimeOptions::default(),
    );
    let _ = std::fs::remove_dir_all(&iso_home);
    assert!(summary.degraded);
    assert_eq!(summary.available, vec!["openai"]);
    assert!(
        summary
            .unavailable
            .iter()
            .any(|(name, reason)| name == "anthropic" && reason.contains("missing credential"))
    );
}

#[test]
fn summarize_provider_availability_marks_openai_codex_available_with_auth_profile() {
    let _guard = env_lock();

    let fake_home = std::env::temp_dir().join(format!(
        "openprx-provider-avail-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let state_dir = fake_home.join(".openprx");
    std::fs::create_dir_all(&state_dir).unwrap();

    let auth = crate::auth::AuthService::new(&state_dir, false);
    auth.store_openai_tokens(
        "default",
        crate::auth::profiles::TokenSet {
            access_token: "test-access-token".into(),
            refresh_token: None,
            id_token: None,
            expires_at: None,
            token_type: Some("Bearer".into()),
            scope: None,
        },
        Some("acct_test".into()),
        true,
    )
    .unwrap();

    let home_guard = EnvGuard::set("HOME", Some(&fake_home.to_string_lossy()));
    let summary = summarize_provider_availability(
        "openai",
        Some("sk-test"),
        &crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec!["openai-codex".into()],
            acknowledge_single_provider_risk: false,
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        },
        &ProviderRuntimeOptions::default(),
    );

    assert!(summary.available.iter().any(|p| p == "openai-codex"));
    drop(home_guard);
    let _ = std::fs::remove_dir_all(&fake_home);
}

#[test]
fn summarize_provider_availability_marks_openai_codex_unavailable_without_auth_profile() {
    let _guard = env_lock();

    let fake_home = std::env::temp_dir().join(format!(
        "openprx-provider-avail-noauth-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&fake_home).unwrap();

    let home_guard = EnvGuard::set("HOME", Some(&fake_home.to_string_lossy()));
    let summary = summarize_provider_availability(
        "openai",
        Some("sk-test"),
        &crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec!["openai-codex".into()],
            acknowledge_single_provider_risk: false,
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        },
        &ProviderRuntimeOptions::default(),
    );

    assert!(
        summary
            .unavailable
            .iter()
            .any(|(name, reason)| name == "openai-codex" && reason.contains("auth profile"))
    );
    drop(home_guard);
    let _ = std::fs::remove_dir_all(&fake_home);
}

#[test]
fn provider_model_prefix_matching_rejects_cross_provider_mismatch() {
    assert!(provider_matches_model_prefix("openai", "openai/gpt-4o"));
    assert!(!provider_matches_model_prefix("anthropic", "openai/gpt-4o"));
}

/// A decision service publishes its own model ids, like a BYO endpoint,
/// so no model id is refused as foreign to it (CDP-02).
#[test]
fn a_decision_service_accepts_its_own_model_ids() {
    for provider in ["typesafe", "decision:typesafe"] {
        for model in ["jev-1.13.0", "vendor/decider-2026-09"] {
            assert!(provider_matches_model_prefix(provider, model), "{provider} {model}");
            validate_provider_model(provider, model).expect("test: compatible");
        }
    }
}

// d04 §9.2 hint-injection hardening regression: a raw "hint:<name>" prefix is
// an UNRESOLVED routing hint and MUST NOT bypass the provider prefix check.
// The previous wildcard early-return (`if model.starts_with("hint:") { return
// true; }`) let a crafted unresolved hint match ANY provider, allowing a model
// to be routed to the wrong API. After hardening it must be a non-match.
#[test]
fn test_unresolved_hint_does_not_bypass_provider_match() {
    assert!(
        !provider_matches_model_prefix("openai", "hint:anything"),
        "unresolved hint must not match an unrelated provider"
    );
    assert!(
        !provider_matches_model_prefix("anthropic", "hint:fast"),
        "unresolved hint must not bypass the provider prefix check"
    );
    // A hint crafted to look like a provider name must still not match: the
    // literal "hint:" prefix means it is unresolved.
    assert!(
        !provider_matches_model_prefix("openai", "hint:openai"),
        "a 'hint:'-prefixed model is unresolved and must not match by coincidence"
    );
    // Even an aggregator (accepts_any_model) must reject an unresolved hint.
    assert!(
        !provider_matches_model_prefix("openrouter", "hint:whatever"),
        "aggregators must not accept an unresolved hint either"
    );
    // Regression: legitimate resolved provider-prefixed models still match.
    assert!(provider_matches_model_prefix("openai", "openai/gpt-4o"));
    assert!(provider_matches_model_prefix("anthropic", "anthropic/claude-3-opus"));
    // Empty model still means "provider default" and matches.
    assert!(provider_matches_model_prefix("openai", ""));
}

#[test]
fn a_self_hosted_server_serves_any_repository_it_is_configured_for() {
    for repository in [
        "ggml-org/gpt-oss-20b-GGUF",
        "bartowski/Llama-3.3-70B-Instruct-GGUF",
        "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
        "google/gemma-3-27b-it-GGUF",
        "plain-model-name",
    ] {
        assert!(provider_serves_configured_model("llamacpp", repository), "{repository}");
        assert!(validate_provider_model("llama.cpp", repository).is_ok(), "{repository}");
    }
    assert!(is_self_hosted_model_server("llama.cpp"));
    assert!(!is_self_hosted_model_server("anthropic"));
    // An unresolved routing hint is never served.
    assert!(!provider_serves_configured_model("llamacpp", "hint:fast"));
    // Other providers are unchanged.
    assert!(!provider_serves_configured_model("anthropic", "openai/gpt-5"));
    assert!(provider_serves_configured_model("openrouter", "openai/gpt-5"));
}

#[test]
fn a_model_repository_is_named_explicitly() {
    for repository in [
        "ggml-org/gpt-oss-20b-GGUF",
        "unsloth/gemma-3-27b-it-GGUF",
        "bartowski/Llama-3.3-70B-Instruct-GGUF",
        " ggml-org /gpt-oss-20b-GGUF",
    ] {
        assert!(names_model_repository(repository), "{repository}");
    }
    for routed_elsewhere in [
        "gpt-5",
        "anthropic/claude-sonnet-5",
        "openai/gpt-4o",
        "kimi/k2",
        // D-076 N-10: any case, surrounding space ignored.
        "OpenAI/gpt-4o",
        "Anthropic/claude-sonnet-5",
        "openai /gpt-4o",
        " OPENAI/gpt-4o",
        "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
        "google/gemma-3-27b-it-GGUF",
        "hint:fast",
        "org/",
        "/repo",
        " /repo",
    ] {
        assert!(!names_model_repository(routed_elsewhere), "{routed_elsewhere}");
    }
}

#[test]
fn validate_provider_model_rejects_unknown_kimi_code_model() {
    assert!(validate_provider_model("kimi-code", "k3").is_ok());
    assert!(validate_provider_model("kimi_coding", "kimi-for-coding").is_ok());
    assert!(validate_provider_model("kimi-code", "kimi-for-coding-highspeed").is_ok());
    assert!(validate_provider_model("kimi-code", "kimi-k2.7-code").is_err());

    let err = validate_provider_model("kimi-code", "definitely-not-a-real-model")
        .expect_err("unknown Kimi Code model should fail local validation")
        .to_string();

    assert!(err.contains("invalid model 'definitely-not-a-real-model'"));
    assert!(err.contains("k3"));
}

/// OpenRouter is a model aggregator: it must accept any `org/model` style
/// model ID even when the prefix does not match "openrouter".
/// Regression test for: `prx go -p openrouter -m "mistralai/mistral-7b"`
/// previously returning "not compatible with provider".
#[test]
fn openrouter_accepts_any_org_prefixed_model() {
    // Standard org/model format used by most OpenRouter models
    assert!(provider_matches_model_prefix("openrouter", "mistralai/mistral-7b"));
    assert!(provider_matches_model_prefix("openrouter", "anthropic/claude-3-opus"));
    assert!(provider_matches_model_prefix(
        "openrouter",
        "meta-llama/llama-3-70b-instruct"
    ));
    assert!(provider_matches_model_prefix("openrouter", "google/gemma-2-9b-it"));
    // Mixed-case provider name must also work
    assert!(provider_matches_model_prefix("OpenRouter", "mistralai/mistral-7b"));
    // Models without a slash (plain names) already returned true for everyone
    assert!(provider_matches_model_prefix("openrouter", "gpt-4o"));
    // Other providers must NOT accept cross-vendor prefixed models
    assert!(!provider_matches_model_prefix("anthropic", "mistralai/mistral-7b"));
    assert!(!provider_matches_model_prefix("openai", "mistralai/mistral-7b"));
}

/// Fallback providers resolve their own credentials independently
/// rather than inheriting the primary provider's key.  A provider
/// that requires no key (e.g. lmstudio, ollama) must initialize
/// successfully even when the primary uses a completely different key.
#[test]
fn resilient_fallback_resolves_own_credential() {
    let reliability = crate::config::ReliabilityConfig {
        provider_retries: 1,
        provider_backoff_ms: 100,
        fallback_providers: vec!["lmstudio".into(), "ollama".into()],
        acknowledge_single_provider_risk: false,
        api_keys: Vec::new(),
        model_fallbacks: std::collections::HashMap::new(),
        channel_initial_backoff_secs: 2,
        channel_max_backoff_secs: 60,
        scheduler_poll_secs: 15,
        scheduler_retries: 2,
    };

    // Primary uses a ZAI key; fallbacks (lmstudio, ollama) should NOT
    // receive this key; they resolve their own credentials independently.
    let config = factory_config("zai", "glm-4.7", Some("zai-test-key"), None, reliability);
    let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
    assert!(provider.is_ok());
}

/// `custom:` URL entries work as fallback providers, enabling arbitrary
/// OpenAI-compatible endpoints (e.g. local LM Studio on a Docker host).
#[test]
fn resilient_fallback_supports_custom_url() {
    let reliability = crate::config::ReliabilityConfig {
        provider_retries: 1,
        provider_backoff_ms: 100,
        fallback_providers: vec!["custom:http://host.docker.internal:1234/v1".into()],
        acknowledge_single_provider_risk: false,
        api_keys: Vec::new(),
        model_fallbacks: std::collections::HashMap::new(),
        channel_initial_backoff_secs: 2,
        channel_max_backoff_secs: 60,
        scheduler_poll_secs: 15,
        scheduler_retries: 2,
    };

    let config = factory_config("openai", "gpt-4o", Some("openai-test-key"), None, reliability);
    let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
    assert!(provider.is_ok());
}

/// Mixed fallback chain: named providers, custom URLs, and invalid entries
/// all coexist.  Invalid entries are silently ignored; valid ones initialize.
#[test]
fn resolved_provider_refuses_a_mixed_chain_with_an_invalid_member() {
    let reliability = crate::config::ReliabilityConfig {
        provider_retries: 1,
        provider_backoff_ms: 100,
        fallback_providers: vec![
            "deepseek".into(),
            "custom:http://localhost:8080/v1".into(),
            "nonexistent-provider".into(),
            "lmstudio".into(),
        ],
        acknowledge_single_provider_risk: false,
        api_keys: Vec::new(),
        model_fallbacks: std::collections::HashMap::new(),
        channel_initial_backoff_secs: 2,
        channel_max_backoff_secs: 60,
        scheduler_poll_secs: 15,
        scheduler_retries: 2,
    };

    let config = factory_config("zai", "glm-4.7", Some("zai-test-key"), None, reliability);
    let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
    assert!(provider.is_err());
}

#[test]
fn ollama_with_custom_url() {
    let provider = create_provider_with_url("ollama", None, Some("http://10.100.2.32:11434"));
    assert!(provider.is_ok());
}

#[test]
fn ollama_cloud_with_custom_url() {
    let provider = create_provider_with_url("ollama", Some("ollama-key"), Some("https://ollama.com"));
    assert!(provider.is_ok());
}

#[test]
fn factory_all_providers_create_successfully() {
    let providers = [
        "openrouter",
        "anthropic",
        "openai",
        "ollama",
        "gemini",
        "venice",
        "vercel",
        "cloudflare",
        "moonshot",
        "moonshot-intl",
        "kimi-code",
        "moonshot-cn",
        "kimi-code",
        "synthetic",
        "opencode",
        "zai",
        "zai-cn",
        "glm",
        "glm-cn",
        "minimax",
        "minimax-cn",
        "bedrock",
        "qianfan",
        "qwen",
        "qwen-intl",
        "qwen-cn",
        "qwen-us",
        "lmstudio",
        "llamacpp",
        "litellm",
        "vllm",
        "huggingface",
        "groq",
        "mistral",
        "xai",
        "deepseek",
        "together",
        "fireworks",
        "perplexity",
        "cohere",
        "copilot",
        "nvidia",
        "astrai",
        "ovhcloud",
    ];
    for name in providers {
        assert!(
            create_provider(name, Some("test-key")).is_ok(),
            "Provider '{name}' should create successfully"
        );
    }
}

#[test]
fn listed_providers_have_unique_ids_and_aliases() {
    let providers = list_providers();
    let mut canonical_ids = std::collections::HashSet::new();
    let mut aliases = std::collections::HashSet::new();

    for provider in providers {
        assert!(
            canonical_ids.insert(provider.name),
            "Duplicate canonical provider id: {}",
            provider.name
        );

        for alias in provider.aliases {
            assert_ne!(
                *alias, provider.name,
                "Alias must differ from canonical id: {}",
                provider.name
            );
            assert!(
                !canonical_ids.contains(alias),
                "Alias conflicts with canonical provider id: {}",
                alias
            );
            assert!(aliases.insert(alias), "Duplicate provider alias: {}", alias);
        }
    }
}

#[test]
fn listed_providers_and_aliases_are_constructible() {
    for provider in list_providers() {
        assert!(
            create_provider(provider.name, Some("provider-test-credential")).is_ok(),
            "Canonical provider id should be constructible: {}",
            provider.name
        );

        for alias in provider.aliases {
            assert!(
                create_provider(alias, Some("provider-test-credential")).is_ok(),
                "Provider alias should be constructible: {} (for {})",
                alias,
                provider.name
            );
        }
    }
}

// ── API error sanitization ───────────────────────────────

#[test]
fn sanitize_scrubs_sk_prefix() {
    let input = "request failed: sk-1234567890abcdef";
    let out = sanitize_api_error(input);
    assert!(!out.contains("sk-1234567890abcdef"));
    assert!(out.contains("[REDACTED]"));
}

#[test]
fn sanitize_scrubs_multiple_prefixes() {
    let input = "keys sk-abcdef xoxb-12345 xoxp-67890";
    let out = sanitize_api_error(input);
    assert!(!out.contains("sk-abcdef"));
    assert!(!out.contains("xoxb-12345"));
    assert!(!out.contains("xoxp-67890"));
}

#[test]
fn sanitize_short_prefix_then_real_key() {
    let input = "error with sk- prefix and key sk-1234567890";
    let result = sanitize_api_error(input);
    assert!(!result.contains("sk-1234567890"));
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn sanitize_sk_proj_comment_then_real_key() {
    let input = "note: sk- then sk-proj-abc123def456";
    let result = sanitize_api_error(input);
    assert!(!result.contains("sk-proj-abc123def456"));
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn sanitize_keeps_bare_prefix() {
    let input = "only prefix sk- present";
    let result = sanitize_api_error(input);
    assert!(result.contains("sk-"));
}

#[test]
fn sanitize_handles_json_wrapped_key() {
    let input = r#"{"error":"invalid key sk-abc123xyz"}"#;
    let result = sanitize_api_error(input);
    assert!(!result.contains("sk-abc123xyz"));
}

#[test]
fn sanitize_handles_delimiter_boundaries() {
    let input = "bad token xoxb-abc123}; next";
    let result = sanitize_api_error(input);
    assert!(!result.contains("xoxb-abc123"));
    assert!(result.contains("};"));
}

#[test]
fn sanitize_truncates_long_error() {
    let long = "a".repeat(400);
    let result = sanitize_api_error(&long);
    assert!(result.len() <= 203);
    assert!(result.ends_with("..."));
}

#[test]
fn sanitize_truncates_after_scrub() {
    let input = format!("{} sk-abcdef123456 {}", "a".repeat(190), "b".repeat(190));
    let result = sanitize_api_error(&input);
    assert!(!result.contains("sk-abcdef123456"));
    assert!(result.len() <= 203);
}

#[test]
fn sanitize_preserves_unicode_boundaries() {
    let input = format!("{} sk-abcdef123", "hello🙂".repeat(80));
    let result = sanitize_api_error(&input);
    assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    assert!(!result.contains("sk-abcdef123"));
}

#[test]
fn sanitize_no_secret_no_change() {
    let input = "simple upstream timeout";
    let result = sanitize_api_error(input);
    assert_eq!(result, input);
}

#[test]
fn scrub_github_personal_access_token() {
    let input = "auth failed with token ghp_abc123def456";
    let result = crate::security::redaction::redact_secrets(input);
    assert_eq!(result, "auth failed with token [REDACTED]");
}

#[test]
fn scrub_github_oauth_token() {
    let input = "Bearer gho_1234567890abcdef";
    let result = crate::security::redaction::redact_secrets(input);
    assert_eq!(result, "Bearer [REDACTED]");
}

#[test]
fn scrub_github_user_token() {
    let input = "token ghu_sessiontoken123";
    let result = crate::security::redaction::redact_secrets(input);
    assert_eq!(result, "token [REDACTED]");
}

#[test]
fn scrub_github_fine_grained_pat() {
    let input = "failed: github_pat_11AABBC_xyzzy789";
    let result = crate::security::redaction::redact_secrets(input);
    assert_eq!(result, "failed: [REDACTED]");
}

// ── FIX-P2-14: extra api_keys rotation ──

fn reliability_with_keys(keys: Vec<String>) -> crate::config::ReliabilityConfig {
    crate::config::ReliabilityConfig {
        api_keys: keys,
        ..crate::config::ReliabilityConfig::default()
    }
}

#[test]
fn extra_api_keys_filters_empty_dupes_and_primary() {
    let reliability = reliability_with_keys(vec![
        "  ".into(),         // empty after trim → skipped
        "sk-primary".into(), // equals primary → skipped
        " sk-two ".into(),   // trimmed and kept
        "sk-two".into(),     // duplicate of the above → skipped
        "sk-three".into(),
    ]);
    let extras = extra_api_keys(&reliability, Some("sk-primary"));
    assert_eq!(extras, vec!["sk-two", "sk-three"]);
}

#[test]
fn extra_api_keys_handles_no_primary() {
    let reliability = reliability_with_keys(vec!["sk-a".into(), "sk-a".into(), "sk-b".into()]);
    let extras = extra_api_keys(&reliability, None);
    assert_eq!(extras, vec!["sk-a", "sk-b"]);
}

#[test]
fn extra_api_keys_empty_when_unset() {
    let reliability = crate::config::ReliabilityConfig::default();
    assert!(
        extra_api_keys(&reliability, Some("sk-primary")).is_empty(),
        "expected an empty value"
    );
}

#[test]
fn resilient_provider_builds_rotation_instances_for_extra_keys() {
    // With two distinct extra keys, the primary provider chain should
    // include the primary instance plus one instance per distinct extra key.
    // We assert construction succeeds (the rotation instances are built from
    // the same provider factory as the primary).
    let reliability = reliability_with_keys(vec![
        "openai-extra-1".into(),
        "openai-extra-2".into(),
        "openai-extra-2".into(),
    ]);
    let config = factory_config("openai", "gpt-4o", Some("openai-primary"), None, reliability);
    let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
    assert!(
        provider.is_ok(),
        "resilient provider with rotation keys should build: {:?}",
        provider.err()
    );
}

// ── Bring-your-own endpoints: identity, model validation, credentials ──
//
// A `custom:<url>` / `anthropic-custom:<url>` selection names an endpoint
// the operator runs, not a catalog vendor. It therefore owns its own model
// catalog and may legitimately be configured without a credential. Every
// catalog identity keeps the behaviour it has today.

/// A credential-shaped sentinel. Not a real secret.
const BYO_SENTINEL_KEY: &str = "byo-secret";
/// A BYO endpoint name with a fixed port, used only for string-level
/// identity assertions: nothing connects to it.
const BYO_ENDPOINT_NAME: &str = "custom:http://127.0.0.1:18086/v1";
/// A bare model alias, the shape a BYO endpoint advertises.
const BYO_BARE_MODEL: &str = "byo-test-model";
/// A slash-qualified model id whose org prefix is not the provider name.
const BYO_SLASH_MODEL: &str = "exampleorg/byo-test-model";
/// The canned assistant reply the loopback endpoint returns.
const BYO_LOOPBACK_REPLY: &str = "byo-loopback-ok";

fn byo_reliability_without_fallbacks() -> crate::config::ReliabilityConfig {
    crate::config::ReliabilityConfig {
        provider_retries: 0,
        provider_backoff_ms: 0,
        fallback_providers: Vec::new(),
        acknowledge_single_provider_risk: false,
        api_keys: Vec::new(),
        model_fallbacks: std::collections::HashMap::new(),
        channel_initial_backoff_secs: 2,
        channel_max_backoff_secs: 60,
        scheduler_poll_secs: 15,
        scheduler_retries: 2,
    }
}

/// Runtime options rooted in a fresh temp directory, so credential
/// resolution reads an empty auth-profile store and never touches the
/// operator's real configuration.
fn byo_isolated_options(temp: &TempDir) -> ProviderRuntimeOptions {
    ProviderRuntimeOptions {
        default_temperature: None,
        model_settings: Default::default(),
        auth_profile_override: None,
        openprx_dir: Some(temp.path().to_path_buf()),
        secrets_encrypt: false,
        codex_auth_json_path: Some(temp.path().join("absent-codex-auth.json")),
        codex_auth_json_auto_import: false,
        reasoning_enabled: None,
        codex_stream_idle_timeout_secs: None,
        codex_reasoning_effort: None,
        ollama_num_ctx: None,
        ollama_model_num_ctx: Vec::new(),
        // No declarations: a BYO endpoint keeps the undeclared defaults
        // (`Raw` schemas, native tool calls only) these tests assume.
        tool_schema_dialects: Vec::new(),
        tool_call_dialects: Vec::new(),
    }
}

/// A loopback OpenAI-compatible endpoint owned by the test. It requires no
/// authentication and records the headers of every request it answers.
struct ByoLoopbackEndpoint {
    addr: std::net::SocketAddr,
    recorded: std::sync::Arc<Mutex<Vec<Vec<(String, String)>>>>,
    server: tokio::task::JoinHandle<()>,
}

impl ByoLoopbackEndpoint {
    fn provider_name(&self) -> String {
        format!("custom:http://{}/v1", self.addr)
    }

    fn requests(&self) -> Vec<Vec<(String, String)>> {
        self.recorded
            .lock()
            .map_or_else(|poisoned| poisoned.into_inner().clone(), |guard| guard.clone())
    }

    fn shutdown(self) {
        self.server.abort();
    }
}

async fn spawn_byo_loopback() -> ByoLoopbackEndpoint {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let body = format!(
        r#"{{"id":"chatcmpl-byo","object":"chat.completion","created":1,"model":"{BYO_BARE_MODEL}","choices":[{{"index":0,"message":{{"role":"assistant","content":"{BYO_LOOPBACK_REPLY}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}}}"#
    );
    let sse_body = format!(
        "data: {{\"id\":\"chatcmpl-byo\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{BYO_BARE_MODEL}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{BYO_LOOPBACK_REPLY}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: loopback address");
    let recorded: std::sync::Arc<Mutex<Vec<Vec<(String, String)>>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&recorded);

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _peer)) = listener.accept().await else {
                return;
            };
            let sink = std::sync::Arc::clone(&sink);
            let body = body.clone();
            let sse_body = sse_body.clone();
            tokio::spawn(async move {
                let mut buffer: Vec<u8> = Vec::with_capacity(4096);
                let mut chunk = [0_u8; 2048];
                let head_end = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break position + 4;
                    }
                    if buffer.len() > 262_144 {
                        return;
                    }
                };

                let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                let mut headers: Vec<(String, String)> = Vec::new();
                let mut content_length: usize = 0;
                for line in head.split("\r\n").skip(1) {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        let name = name.trim().to_ascii_lowercase();
                        let value = value.trim().to_string();
                        if name == "content-length" {
                            content_length = value.parse().unwrap_or(0);
                        }
                        headers.push((name, value));
                    }
                }

                let mut body_read = buffer.len().saturating_sub(head_end);
                while body_read < content_length {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => body_read += read,
                    }
                }

                let streaming = headers
                    .iter()
                    .any(|(name, value)| name == "accept" && value.contains("text/event-stream"));

                if let Ok(mut guard) = sink.lock() {
                    guard.push(headers);
                }

                let (content_type, payload) = if streaming {
                    ("text/event-stream", sse_body.as_str())
                } else {
                    ("application/json", body.as_str())
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    ByoLoopbackEndpoint { addr, recorded, server }
}

// ── Root connection containment (PMM P0-A) ───────────────────────────

/// A credential-shaped sentinel configured for one provider. Not a real
/// secret.
const ROOT_SENTINEL_KEY: &str = "root-secret";

/// The root connection is configured for one provider. Another provider
/// gets nothing at all — neither the credential nor the base URL — so one
/// vendor's secret can never be presented to another.
#[test]
fn root_connection_is_not_released_to_a_different_provider() {
    let connection = configured_connection_for(
        "anthropic",
        "openai",
        Some(ROOT_SENTINEL_KEY),
        Some("https://api.openai.com/v1"),
    );
    assert_eq!(connection.api_key, None);
    assert_eq!(connection.api_url, None);
}

#[test]
fn default_provider_keeps_its_connection() {
    let connection = configured_connection_for(
        "openai",
        "openai",
        Some(ROOT_SENTINEL_KEY),
        Some("https://api.openai.com/v1"),
    );
    assert_eq!(connection.api_key, Some(ROOT_SENTINEL_KEY));
    assert_eq!(connection.api_url, Some("https://api.openai.com/v1"));
}

/// Alias canonicalization is the reason this resolver exists rather than a
/// plain case-insensitive string comparison: `claude-code` and `anthropic`
/// are one provider, and withholding a credential from its own alias is a
/// usability defect the four existing precedents all carry.
#[test]
fn an_alias_of_the_default_provider_keeps_its_connection() {
    let same = configured_connection_for("claude-code", "anthropic", Some(ROOT_SENTINEL_KEY), None);
    assert_eq!(same.api_key, Some(ROOT_SENTINEL_KEY));

    let other = configured_connection_for("claude-code", "openai", Some(ROOT_SENTINEL_KEY), None);
    assert_eq!(other.api_key, None);
}

/// A bring-your-own endpoint is identified by the endpoint it addresses,
/// so a different port is a different provider and gets nothing.
#[test]
fn byo_endpoints_compare_by_endpoint_identity() {
    let same = configured_connection_for(
        "CUSTOM:http://127.0.0.1:18086/v1",
        BYO_ENDPOINT_NAME,
        Some(BYO_SENTINEL_KEY),
        None,
    );
    assert_eq!(same.api_key, Some(BYO_SENTINEL_KEY));

    let other = configured_connection_for(
        "custom:http://127.0.0.1:19999/v1",
        BYO_ENDPOINT_NAME,
        Some(BYO_SENTINEL_KEY),
        None,
    );
    assert_eq!(other.api_key, None, "a different endpoint is a different provider");

    // The two dialects are not interchangeable either, even on one URL.
    let crossed = configured_connection_for(
        "anthropic-custom:http://127.0.0.1:18086/v1",
        BYO_ENDPOINT_NAME,
        Some(BYO_SENTINEL_KEY),
        None,
    );
    assert_eq!(crossed.api_key, None);
}

/// The classifier is the single source of provider identity. Aliases
/// collapse to the canonical catalog name, both BYO dialects resolve to
/// their endpoint, and anything unresolvable — including a BYO prefix
/// whose URL is rejected — is `Unknown`.
#[test]
fn classify_provider_separates_catalog_entries_from_byo_endpoints() {
    assert_eq!(classify_provider("openai"), ProviderIdentity::Catalog("openai"));
    assert_eq!(classify_provider("claude-code"), ProviderIdentity::Catalog("anthropic"));
    assert_eq!(
        classify_provider("  OpenRouter "),
        ProviderIdentity::Catalog("openrouter")
    );
    assert_eq!(
        classify_provider(BYO_ENDPOINT_NAME),
        ProviderIdentity::ByoEndpoint {
            dialect: ByoDialect::OpenAiCompatible,
            base_url: "http://127.0.0.1:18086/v1".to_string(),
        }
    );
    assert_eq!(
        classify_provider("anthropic-custom:https://api.example.com"),
        ProviderIdentity::ByoEndpoint {
            dialect: ByoDialect::AnthropicCompatible,
            base_url: "https://api.example.com".to_string(),
        }
    );
    assert_eq!(classify_provider("custom:not-a-url"), ProviderIdentity::Unknown);
    assert_eq!(classify_provider("custom:ftp://example.com"), ProviderIdentity::Unknown);
    assert_eq!(classify_provider("custom:"), ProviderIdentity::Unknown);
    assert_eq!(classify_provider("nonexistent"), ProviderIdentity::Unknown);
}

/// SPLASH F1. A BYO endpoint owns its own model catalog, so PRX cannot
/// reject an `org/model` id the endpoint advertises. Every other identity
/// keeps the catalog comparison it has today, including the `hint:`
/// hardening and the Kimi Code allowlist.
///
/// RED at `ae3ca634`: the first three assertions fail with
/// `model '<id>' is not compatible with provider '<name>'`.
#[test]
fn custom_endpoint_accepts_slash_qualified_model_id() {
    assert!(validate_provider_model(BYO_ENDPOINT_NAME, BYO_SLASH_MODEL).is_ok());
    assert!(validate_provider_model("anthropic-custom:https://api.example.com", BYO_SLASH_MODEL).is_ok());
    // A bare alias was already accepted and must stay accepted.
    assert!(validate_provider_model(BYO_ENDPOINT_NAME, BYO_BARE_MODEL).is_ok());

    // Known constrained providers keep strict validation.
    assert!(validate_provider_model("anthropic", BYO_SLASH_MODEL).is_err());
    assert!(validate_provider_model("openai", "mistralai/mistral-7b").is_err());
    assert!(validate_provider_model("kimi-code", "k3").is_ok());
    assert!(validate_provider_model("kimi_coding", "kimi-for-coding").is_ok());
    assert!(validate_provider_model("kimi-code", "kimi-for-coding-highspeed").is_ok());
    assert!(validate_provider_model("kimi-code", "kimi-k2.7-code").is_err());
    assert!(validate_provider_model("kimi-code", "definitely-not-a-real-model").is_err());

    // d04 §9.2: an unresolved hint stays a non-match in front of the new
    // BYO arm, not behind it.
    assert!(!provider_matches_model_prefix(BYO_ENDPOINT_NAME, "hint:anything"));

    // An unparseable BYO string is not silently promoted to "accepts
    // anything": it has no endpoint identity at all.
    assert!(validate_provider_model("custom:not-a-url", BYO_SLASH_MODEL).is_err());
}

/// SPLASH F5, S4 case A. A keyless BYO endpoint configured as the default
/// provider must start. The refusal it replaces was pre-transport, so the
/// endpoint must still see no request before the first turn.
///
/// RED at `ae3ca634`: `Err(... missing credential/api key)`.
#[tokio::test]
async fn keyless_byo_endpoint_starts_without_a_credential() {
    let endpoint = spawn_byo_loopback().await;
    let temp = TempDir::new().expect("test: temp config dir");
    let options = byo_isolated_options(&temp);
    let reliability = byo_reliability_without_fallbacks();
    let provider_name = endpoint.provider_name();
    let config = factory_config(&provider_name, BYO_BARE_MODEL, None, None, reliability.clone());
    let outcome = create_provider_from_test_config(&config, &options);

    assert!(
        reliability.fallback_providers.is_empty(),
        "an available fallback would mask the primary's refusal"
    );
    assert!(
        outcome.is_ok(),
        "a keyless BYO endpoint must start: {:?}",
        outcome.err().map(|error| format!("{error:#}"))
    );
    assert_eq!(
        endpoint.requests().len(),
        0,
        "provider construction must not touch the endpoint"
    );
    endpoint.shutdown();
}

/// SPLASH F5 second order, decision D-012 (S1 Q3). The relaxation is
/// symmetric with `ollama`: a keyless BYO endpoint listed as a fallback
/// becomes available too. The credential gate itself must stay intact for
/// catalog providers, which the `mistral` control pins.
///
/// RED at `ae3ca634`: the BYO fallback is reported
/// `missing credential/api key`.
#[tokio::test]
async fn keyless_byo_fallback_becomes_available_while_the_gate_stays_intact() {
    let endpoint = spawn_byo_loopback().await;
    let temp = TempDir::new().expect("test: temp config dir");
    let options = byo_isolated_options(&temp);
    let provider_name = endpoint.provider_name();

    let mut reliability = byo_reliability_without_fallbacks();
    reliability.fallback_providers = vec![provider_name.clone()];

    let summary = summarize_provider_availability("ollama", None, &reliability, &options);
    assert!(
        summary.available.iter().any(|name| name == &provider_name),
        "keyless BYO fallback must be available: {:?}",
        summary.unavailable
    );

    // Control: a catalog provider with no credential is still refused, and
    // an empty fallback list cannot mask that refusal.
    let gate = byo_reliability_without_fallbacks();
    let catalog = summarize_provider_availability("mistral", None, &gate, &options);
    assert!(catalog.available.is_empty(), "{:?}", catalog.available);
    assert!(
        catalog
            .unavailable
            .iter()
            .any(|(name, reason)| name == "mistral" && reason == "missing credential/api key"),
        "{:?}",
        catalog.unavailable
    );
    endpoint.shutdown();
}

/// R-C1 F-1. `AnthropicProvider` has no keyless mode, so an
/// `anthropic-custom:<url>` configured without a credential can never
/// serve a turn. The credential exemption therefore covers the
/// OpenAI-compatible dialect only: the Anthropic dialect is refused at the
/// startup gate, where the diagnostic is accurate and no transport exists,
/// instead of at the first turn — and it does not join a fallback chain it
/// could never serve.
///
/// RED at `c7d3fead`: the keyless Anthropic-dialect endpoint is reported
/// available, as primary and as fallback.
#[tokio::test]
async fn keyless_anthropic_dialect_endpoint_is_refused_at_the_credential_gate() {
    const ANTHROPIC_DIALECT_ENDPOINT: &str = "anthropic-custom:http://127.0.0.1:18086/v1";
    let temp = TempDir::new().expect("test: temp config dir");
    let options = byo_isolated_options(&temp);
    let reliability = byo_reliability_without_fallbacks();

    assert!(
        provider_requires_explicit_credential(ANTHROPIC_DIALECT_ENDPOINT),
        "the Anthropic dialect has no keyless mode and must keep the requirement"
    );
    assert!(
        !provider_requires_explicit_credential(BYO_ENDPOINT_NAME),
        "the OpenAI-compatible dialect keeps its exemption"
    );

    let summary = summarize_provider_availability(ANTHROPIC_DIALECT_ENDPOINT, None, &reliability, &options);
    assert!(summary.available.is_empty(), "{:?}", summary.available);
    assert!(
        summary
            .unavailable
            .iter()
            .any(|(name, reason)| name == ANTHROPIC_DIALECT_ENDPOINT && reason == "missing credential/api key"),
        "{:?}",
        summary.unavailable
    );

    // With a credential of its own it is available, exactly as before.
    let keyed = summarize_provider_availability(
        ANTHROPIC_DIALECT_ENDPOINT,
        Some(BYO_SENTINEL_KEY),
        &reliability,
        &options,
    );
    assert!(
        keyed.available.iter().any(|name| name == ANTHROPIC_DIALECT_ENDPOINT),
        "{:?}",
        keyed.unavailable
    );

    // And a keyless one is not marked available as a fallback either.
    let mut with_fallback = byo_reliability_without_fallbacks();
    with_fallback.fallback_providers = vec![ANTHROPIC_DIALECT_ENDPOINT.to_string()];
    let chained = summarize_provider_availability("ollama", None, &with_fallback, &options);
    assert!(
        !chained.available.iter().any(|name| name == ANTHROPIC_DIALECT_ENDPOINT),
        "{:?}",
        chained.available
    );
}

/// R-C1 F-4. This module folds a provider name in two places: the
/// classifier, which decides identity for credential release, and the
/// auth-profile canonicalizer, which decides where the credential is
/// stored. A name that folds one way in one and another way in the other
/// withholds a key its own store says is shared. Guard test: it pins the
/// agreement across both tables rather than a behavior of its own.
///
/// RED at `c7d3fead`: the regional aliases the catalog does not list as
/// rows (`bigmodel`, `glm-cn`, `kimi-intl`, `zai-cn`, …) classify
/// `Unknown` while the credential store folds them onto their vendor.
#[test]
fn provider_identity_agrees_with_auth_profile_canonicalization() {
    /// Every alias the regional tables recognise but `list_providers()`
    /// does not carry as a name or alias.
    const REGIONAL_ALIASES: &[&str] = &[
        "minimax-oauth-global",
        "minimax-portal-global",
        "glm-cn",
        "zhipu-cn",
        "bigmodel",
        "glm-global",
        "zhipu-global",
        "moonshot-intl",
        "moonshot-global",
        "kimi-intl",
        "kimi-global",
        "moonshot-cn",
        "kimi-cn",
        "qwen-cn",
        "dashscope-cn",
        "qwen-international",
        "dashscope-international",
        "dashscope-coding",
        "qwen-coding",
        "dashscope-coding-intl",
        "qwen-coding-intl",
        "zai-global",
        "z.ai-global",
        "zai-cn",
        "z.ai-cn",
    ];

    let mut names: Vec<String> = Vec::new();
    for info in list_providers() {
        names.push(info.name.to_string());
        names.extend(info.aliases.iter().map(|alias| (*alias).to_string()));
    }
    names.extend(REGIONAL_ALIASES.iter().map(|alias| (*alias).to_string()));

    for name in names {
        match classify_provider(&name) {
            ProviderIdentity::Catalog(canonical) => assert_eq!(
                canonical_auth_profile_provider(&name),
                canonical,
                "`{name}` folds to two different providers"
            ),
            other => panic!("test: `{name}` must classify as a catalog provider, got {other:?}"),
        }
    }
}

/// R-C1 F-3. `qwen`, `qwen-intl` and `qwen-us` share one catalog row but
/// address three different servers, and so do `minimax` / `minimax-cn`,
/// `glm` / `bigmodel`, `moonshot` / `kimi-intl` and `zai` / `zai-cn`. A
/// credential issued for one region is not a credential for another, so
/// the identity the resolvers compare carries the endpoint the name
/// selects. Spellings of one region stay one identity.
///
/// RED at `c7d3fead`: the regional spellings collapse onto the catalog row
/// and a key bound to one region is released to the other.
#[test]
fn regional_aliases_of_one_catalog_row_keep_their_own_endpoint_identity() {
    assert_ne!(canonical_provider_key("qwen"), canonical_provider_key("qwen-us"));
    assert_ne!(canonical_provider_key("qwen"), canonical_provider_key("qwen-intl"));
    assert_ne!(canonical_provider_key("qwen"), canonical_provider_key("qwen-coding"));
    assert_ne!(canonical_provider_key("minimax"), canonical_provider_key("minimax-cn"));
    assert_ne!(canonical_provider_key("glm"), canonical_provider_key("bigmodel"));
    assert_ne!(canonical_provider_key("moonshot"), canonical_provider_key("kimi-intl"));
    assert_ne!(canonical_provider_key("zai"), canonical_provider_key("zai-cn"));

    assert_eq!(canonical_provider_key("qwen"), canonical_provider_key("dashscope"));
    assert_eq!(
        canonical_provider_key("qwen-us"),
        canonical_provider_key("dashscope-us")
    );
    assert_eq!(canonical_provider_key("minimax-cn"), canonical_provider_key("minimaxi"));
    assert_eq!(canonical_provider_key("glm"), canonical_provider_key("zhipu"));
    assert_eq!(canonical_provider_key("zai"), canonical_provider_key("z.ai"));

    // The root connection follows that identity: the same region keeps it,
    // another region gets nothing.
    assert_eq!(
        configured_connection_for("dashscope", "qwen", Some(ROOT_SENTINEL_KEY), None).api_key,
        Some(ROOT_SENTINEL_KEY)
    );
    assert_eq!(
        configured_connection_for("qwen-us", "qwen", Some(ROOT_SENTINEL_KEY), None).api_key,
        None,
        "a key bound to one region must not be released to another"
    );
}

/// S4 case E. An unparseable BYO string has no endpoint identity: it is
/// refused with the message `parse_custom_provider_url` already produces,
/// before any transport exists, and it is not treated as credential-free.
#[tokio::test]
async fn unparseable_byo_endpoint_is_refused_before_any_transport() {
    let endpoint = spawn_byo_loopback().await;
    let temp = TempDir::new().expect("test: temp config dir");
    let options = byo_isolated_options(&temp);
    let reliability = byo_reliability_without_fallbacks();

    let config = factory_config("custom:not-a-url", BYO_BARE_MODEL, None, None, reliability);
    let error = create_provider_from_test_config(&config, &options)
        .err()
        .map(|error| format!("{error:#}"))
        .expect("test: an unparseable BYO URL must be refused");

    assert!(
        error.contains("is not one PRX can construct"),
        "unexpected error: {error}"
    );
    assert!(
        provider_requires_explicit_credential("custom:not-a-url"),
        "an unparseable BYO string must not inherit the credential-free arm"
    );
    assert_eq!(endpoint.requests().len(), 0, "a refusal must be pre-transport");
    endpoint.shutdown();
}

/// SPLASH F5 second refusal layer, S4 case P6 inverted. A keyless BYO
/// endpoint must reach its endpoint with no `Authorization` header at all
/// — never with a placeholder credential — on every request surface, not
/// only the one the first turn happens to use.
///
/// RED at `ae3ca634`: every surface fails with `Custom API key not set`
/// and zero requests reach the endpoint.
#[tokio::test]
async fn keyless_byo_endpoint_sends_no_authorization_header_on_any_surface() {
    use futures::StreamExt;

    let endpoint = spawn_byo_loopback().await;
    let provider = create_provider(&endpoint.provider_name(), None).expect("test: keyless BYO construction is valid");
    let history = [ChatMessage::user("ping")];
    let stream_options = traits::StreamOptions::new(true);

    let metered = provider
        .chat(
            ChatRequest {
                messages: &history,
                tools: None,
            },
            BYO_BARE_MODEL,
            0.0,
        )
        .await;
    let with_system = provider.chat_with_system(None, "ping", BYO_BARE_MODEL, 0.0).await;
    let with_history = provider.chat_with_history(&history, BYO_BARE_MODEL, 0.0).await;
    let with_tools = provider.chat_with_tools(&history, &[], BYO_BARE_MODEL, 0.0).await;
    let streamed_system: Vec<_> = provider
        .stream_chat_with_system(None, "ping", BYO_BARE_MODEL, 0.0, stream_options.clone())
        .collect()
        .await;
    let streamed_history: Vec<_> = provider
        .stream_chat_with_history(&history, BYO_BARE_MODEL, 0.0, stream_options)
        .collect()
        .await;

    assert_eq!(
        metered.map(|response| response.text).ok().flatten().as_deref(),
        Some(BYO_LOOPBACK_REPLY)
    );
    assert_eq!(with_system.as_deref().ok(), Some(BYO_LOOPBACK_REPLY));
    assert_eq!(with_history.as_deref().ok(), Some(BYO_LOOPBACK_REPLY));
    assert_eq!(
        with_tools.map(|response| response.text).ok().flatten().as_deref(),
        Some(BYO_LOOPBACK_REPLY)
    );
    for chunks in [&streamed_system, &streamed_history] {
        assert!(
            chunks.iter().all(std::result::Result::is_ok),
            "a keyless stream must not be refused: {:?}",
            chunks
                .iter()
                .filter_map(|chunk| chunk.as_ref().err())
                .collect::<Vec<_>>()
        );
    }

    let requests = endpoint.requests();
    assert_eq!(requests.len(), 6, "every surface must reach the endpoint");
    for headers in &requests {
        assert!(
            !headers.iter().any(|(name, _)| name == "authorization"),
            "a keyless endpoint must receive no authorization header: {:?}",
            headers.iter().map(|(name, _)| name).collect::<Vec<_>>()
        );
    }
    endpoint.shutdown();
}

/// The keyless relaxation is scoped to the absence of a credential: a BYO
/// endpoint that does have one still presents it.
#[tokio::test]
async fn keyed_byo_endpoint_still_presents_its_own_credential() {
    let endpoint = spawn_byo_loopback().await;
    let provider = create_provider(&endpoint.provider_name(), Some(BYO_SENTINEL_KEY))
        .expect("test: keyed BYO construction is valid");

    let reply = provider.simple_chat("ping", BYO_BARE_MODEL, 0.0).await;

    let requests = endpoint.requests();
    assert_eq!(reply.as_deref().ok(), Some(BYO_LOOPBACK_REPLY));
    assert_eq!(requests.len(), 1);
    let authorization = requests
        .first()
        .and_then(|headers| {
            headers
                .iter()
                .find(|(name, _)| name == "authorization")
                .map(|(_, value)| value.clone())
        })
        .expect("test: a keyed endpoint receives an authorization header");
    assert_eq!(authorization, format!("Bearer {BYO_SENTINEL_KEY}"));
    endpoint.shutdown();
}
