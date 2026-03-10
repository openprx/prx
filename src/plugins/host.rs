//! Host state and host function implementations for WASM plugins.
//!
//! `HostState` is the per-instance state stored in each wasmtime `Store`.
//! Host functions provide plugins with controlled access to logging, config,
//! KV storage, memory system, and HTTP requests.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};

use crate::memory::traits::Memory;
#[cfg(feature = "wasm-plugins")]
use crate::plugins::event_bus::EventBus;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
#[cfg(feature = "wasm-plugins")]
use wasmtime::component::ResourceTable;
#[cfg(feature = "wasm-plugins")]
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder};

pub(crate) type WebSocketSessionMap =
    Arc<Mutex<HashMap<u64, Arc<Mutex<Box<dyn WebSocketConnection>>>>>>;

#[async_trait]
pub trait WebSocketConnection: Send + Sync {
    async fn send_text(&mut self, message: String) -> Result<(), String>;
    async fn recv_text(&mut self) -> Result<Option<String>, String>;
    async fn close(&mut self) -> Result<(), String>;
}

#[async_trait]
pub trait WebSocketConnector: Send + Sync {
    async fn connect(&self, url: &str) -> Result<Box<dyn WebSocketConnection>, String>;
}

struct TungsteniteConnector;

struct TungsteniteConnection {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

#[async_trait]
impl WebSocketConnector for TungsteniteConnector {
    async fn connect(&self, url: &str) -> Result<Box<dyn WebSocketConnection>, String> {
        let (stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| format!("websocket connect failed: {e}"))?;
        Ok(Box::new(TungsteniteConnection { stream }))
    }
}

#[async_trait]
impl WebSocketConnection for TungsteniteConnection {
    async fn send_text(&mut self, message: String) -> Result<(), String> {
        self.stream
            .send(tokio_tungstenite::tungstenite::Message::Text(
                message.into(),
            ))
            .await
            .map_err(|e| format!("websocket send failed: {e}"))
    }

    async fn recv_text(&mut self) -> Result<Option<String>, String> {
        loop {
            match self.stream.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                    return Ok(Some(text.to_string()));
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes))) => {
                    let text = String::from_utf8(bytes.to_vec())
                        .map_err(|e| format!("websocket binary payload is not utf-8: {e}"))?;
                    return Ok(Some(text));
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                    return Ok(None);
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_)))
                | Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_)))
                | Some(Ok(tokio_tungstenite::tungstenite::Message::Frame(_))) => {}
                Some(Err(e)) => return Err(format!("websocket receive failed: {e}")),
            }
        }
    }

    async fn close(&mut self) -> Result<(), String> {
        self.stream
            .close(None)
            .await
            .map_err(|e| format!("websocket close failed: {e}"))
    }
}

/// Per-plugin-instance state stored in the wasmtime `Store<HostState>`.
///
/// Each loaded plugin gets its own `HostState` with isolated KV namespace,
/// permission enforcement, and a copy of its configuration.
pub struct HostState {
    /// Unique plugin identifier.
    pub plugin_name: String,

    /// Plugin-specific configuration (from plugin.toml `[config]` section).
    pub config: HashMap<String, String>,

    /// Granted permissions (checked on every host function call).
    pub granted_permissions: HashSet<String>,

    /// Optional permissions the plugin can request at runtime.
    pub optional_permissions: HashSet<String>,

    /// Outbound URL allowlist patterns (used by HTTP and WebSocket host functions).
    pub http_allowlist: Vec<String>,

    /// Namespaced in-memory KV store (plugin-isolated).
    pub kv_store: Arc<RwLock<HashMap<String, Vec<u8>>>>,

    /// Resource limits.
    pub timeout_ms: u64,

    /// Active outbound WebSocket sessions owned by this plugin instance.
    pub websocket_sessions: WebSocketSessionMap,

    /// Monotonic session id generator for outbound WebSocket handles.
    pub next_websocket_session_id: Arc<AtomicU64>,

    /// Connector implementation, overridable in tests.
    pub websocket_connector: Arc<dyn WebSocketConnector>,

    /// Memory backend reference for prx:host/memory host functions.
    pub memory: Option<Arc<dyn Memory>>,

    /// Event bus reference for prx:host/events host functions.
    #[cfg(feature = "wasm-plugins")]
    pub event_bus: Option<Arc<EventBus>>,
    /// WASI resource table (required by WasiView).
    #[cfg(feature = "wasm-plugins")]
    pub wasi_table: ResourceTable,
    /// WASI context (required by WasiView).
    #[cfg(feature = "wasm-plugins")]
    pub wasi_ctx: WasiCtx,
}

impl HostState {
    /// Create a new `HostState` for a plugin.
    /// Create a new `HostState` for a plugin.
    pub fn new(
        plugin_name: String,
        config: HashMap<String, String>,
        granted_permissions: HashSet<String>,
        optional_permissions: HashSet<String>,
        http_allowlist: Vec<String>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            plugin_name,
            config,
            granted_permissions,
            optional_permissions,
            http_allowlist,
            kv_store: Arc::new(RwLock::new(HashMap::new())),
            timeout_ms,
            websocket_sessions: Arc::new(Mutex::new(HashMap::new())),
            next_websocket_session_id: Arc::new(AtomicU64::new(1)),
            websocket_connector: Arc::new(TungsteniteConnector),
            memory: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
            #[cfg(feature = "wasm-plugins")]
            wasi_table: ResourceTable::new(),
            #[cfg(feature = "wasm-plugins")]
            wasi_ctx: WasiCtxBuilder::new().build(),
        }
    }

    /// Create a new `HostState` with a memory backend reference.
    pub fn with_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Override the websocket connector implementation.
    pub fn with_websocket_connector(mut self, connector: Arc<dyn WebSocketConnector>) -> Self {
        self.websocket_connector = connector;
        self
    }

    /// Inject an event bus reference into this host state.
    #[cfg(feature = "wasm-plugins")]
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Check if a permission is granted. Returns an error if denied.
    pub fn check_permission(&self, interface: &str) -> Result<(), String> {
        // Basic interfaces are always allowed.
        if matches!(interface, "log" | "config" | "clock") {
            return Ok(());
        }
        if self.granted_permissions.contains(interface) {
            Ok(())
        } else if self.optional_permissions.contains(interface) {
            Err(format!(
                "permission '{interface}' not yet granted for plugin '{}' (optional, needs approval)",
                self.plugin_name
            ))
        } else {
            Err(format!(
                "permission '{interface}' denied for plugin '{}'",
                self.plugin_name
            ))
        }
    }

    /// Check if a URL is allowed by the http_allowlist.
    pub fn check_url_allowed(&self, url: &str) -> bool {
        if self.http_allowlist.is_empty() {
            // No allowlist = all URLs allowed (if http-outbound permission is granted)
            return true;
        }
        for pattern in &self.http_allowlist {
            if pattern.ends_with('*') {
                let prefix = &pattern[..pattern.len() - 1];
                if url.starts_with(prefix) {
                    return true;
                }
            } else if url == pattern {
                return true;
            }
        }
        false
    }
}

fn websocket_timeout(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms.max(1))
}

async fn websocket_session(
    sessions: &WebSocketSessionMap,
    session_id: u64,
) -> Result<Arc<Mutex<Box<dyn WebSocketConnection>>>, String> {
    let sessions_guard = sessions.lock().await;
    sessions_guard
        .get(&session_id)
        .cloned()
        .ok_or_else(|| format!("websocket session {session_id} not found"))
}

pub(crate) async fn drop_websocket_session(sessions: &WebSocketSessionMap, session_id: u64) {
    let mut sessions_guard = sessions.lock().await;
    sessions_guard.remove(&session_id);
}

pub(crate) async fn websocket_connect_with(
    connector: Arc<dyn WebSocketConnector>,
    sessions: WebSocketSessionMap,
    next_session_id: Arc<AtomicU64>,
    timeout_ms: u64,
    url: String,
) -> Result<u64, String> {
    let timeout = websocket_timeout(timeout_ms);
    let connection = tokio::time::timeout(timeout, connector.connect(&url))
        .await
        .map_err(|_| format!("websocket connect timed out after {timeout_ms}ms"))??;

    let session_id = next_session_id.fetch_add(1, Ordering::Relaxed);
    let mut sessions_guard = sessions.lock().await;
    sessions_guard.insert(session_id, Arc::new(Mutex::new(connection)));
    Ok(session_id)
}

pub(crate) async fn websocket_send_with(
    sessions: WebSocketSessionMap,
    timeout_ms: u64,
    session_id: u64,
    message: String,
) -> Result<(), String> {
    let session = websocket_session(&sessions, session_id).await?;
    let timeout = websocket_timeout(timeout_ms);
    let result = tokio::time::timeout(timeout, async {
        let mut guard = session.lock().await;
        guard.send_text(message).await
    })
    .await;

    match result {
        Err(_) => {
            // Timeout — clean up stale session to prevent resource leak.
            drop_websocket_session(&sessions, session_id).await;
            Err(format!("websocket send timed out after {timeout_ms}ms"))
        }
        Ok(Err(e)) => {
            drop_websocket_session(&sessions, session_id).await;
            Err(e)
        }
        Ok(Ok(())) => Ok(()),
    }
}

pub(crate) async fn websocket_receive_with(
    sessions: WebSocketSessionMap,
    timeout_ms: u64,
    session_id: u64,
) -> Result<String, String> {
    let session = websocket_session(&sessions, session_id).await?;
    let timeout = websocket_timeout(timeout_ms);
    let result = tokio::time::timeout(timeout, async {
        let mut guard = session.lock().await;
        guard.recv_text().await
    })
    .await;

    match result {
        Err(_) => {
            // Timeout — clean up stale session to prevent resource leak.
            drop_websocket_session(&sessions, session_id).await;
            Err(format!("websocket receive timed out after {timeout_ms}ms"))
        }
        Ok(Ok(Some(message))) => Ok(message),
        Ok(Ok(None)) => {
            drop_websocket_session(&sessions, session_id).await;
            Err(format!("websocket session {session_id} closed"))
        }
        Ok(Err(error)) => {
            drop_websocket_session(&sessions, session_id).await;
            Err(error)
        }
    }
}

pub(crate) async fn websocket_close_with(
    sessions: WebSocketSessionMap,
    timeout_ms: u64,
    session_id: u64,
) -> Result<(), String> {
    let session = websocket_session(&sessions, session_id).await?;
    let timeout = websocket_timeout(timeout_ms);
    let result = tokio::time::timeout(timeout, async {
        let mut guard = session.lock().await;
        guard.close().await
    })
    .await
    .map_err(|_| format!("websocket close timed out after {timeout_ms}ms"))?;
    drop_websocket_session(&sessions, session_id).await;
    result
}

// ── WASI trait implementations ──

#[cfg(feature = "wasm-plugins")]
impl wasmtime_wasi::IoView for HostState {
    fn table(&mut self) -> &mut wasmtime::component::ResourceTable {
        &mut self.wasi_table
    }
}

#[cfg(feature = "wasm-plugins")]
impl wasmtime_wasi::WasiView for HostState {
    fn ctx(&mut self) -> &mut wasmtime_wasi::WasiCtx {
        &mut self.wasi_ctx
    }
}

// ── Host function implementations ──

/// Log a message on behalf of a plugin.
/// Maps to `prx:host/log.log(level, message)`.
pub fn host_log(state: &HostState, level: &str, message: &str) {
    match level {
        "trace" => tracing::trace!(plugin = %state.plugin_name, "{message}"),
        "debug" => tracing::debug!(plugin = %state.plugin_name, "{message}"),
        "info" => tracing::info!(plugin = %state.plugin_name, "{message}"),
        "warn" => tracing::warn!(plugin = %state.plugin_name, "{message}"),
        "error" => tracing::error!(plugin = %state.plugin_name, "{message}"),
        _ => tracing::info!(plugin = %state.plugin_name, level = level, "{message}"),
    }
}

/// Retrieve a config value for the plugin.
/// Maps to `prx:host/config.get(key)`.
pub fn host_config_get(state: &HostState, key: &str) -> Option<String> {
    state.config.get(key).cloned()
}

/// Get a value from the plugin's KV store.
/// Maps to `prx:host/kv.get(key)`.
pub async fn host_kv_get(state: &HostState, key: &str) -> Option<Vec<u8>> {
    let store = state.kv_store.read().await;
    store.get(key).cloned()
}

/// Set a value in the plugin's KV store.
/// Maps to `prx:host/kv.set(key, value)`.
pub async fn host_kv_set(state: &HostState, key: String, value: Vec<u8>) {
    let mut store = state.kv_store.write().await;
    store.insert(key, value);
}

/// Delete a value from the plugin's KV store.
/// Maps to `prx:host/kv.delete(key)`. Returns `true` if key existed.
pub async fn host_kv_delete(state: &HostState, key: &str) -> bool {
    let mut store = state.kv_store.write().await;
    store.remove(key).is_some()
}

/// List keys matching a prefix in the plugin's KV store.
/// Maps to `prx:host/kv.list-keys(prefix)`.
pub async fn host_kv_list_keys(state: &HostState, prefix: &str) -> Vec<String> {
    let store = state.kv_store.read().await;
    store
        .keys()
        .filter(|k| k.starts_with(prefix))
        .cloned()
        .collect()
}

/// Open a websocket-outbound connection and return an opaque session id.
pub async fn host_websocket_connect(state: &HostState, url: &str) -> Result<u64, String> {
    state.check_permission("websocket-outbound")?;
    if !state.check_url_allowed(url) {
        return Err(format!("URL not in allowlist: {url}"));
    }

    websocket_connect_with(
        Arc::clone(&state.websocket_connector),
        Arc::clone(&state.websocket_sessions),
        Arc::clone(&state.next_websocket_session_id),
        state.timeout_ms,
        url.to_string(),
    )
    .await
}

/// Send a UTF-8 message on an open websocket session.
pub async fn host_websocket_send(
    state: &HostState,
    session_id: u64,
    message: String,
) -> Result<(), String> {
    state.check_permission("websocket-outbound")?;
    websocket_send_with(
        Arc::clone(&state.websocket_sessions),
        state.timeout_ms,
        session_id,
        message,
    )
    .await
}

/// Receive a UTF-8 message from an open websocket session.
pub async fn host_websocket_receive(state: &HostState, session_id: u64) -> Result<String, String> {
    state.check_permission("websocket-outbound")?;
    websocket_receive_with(
        Arc::clone(&state.websocket_sessions),
        state.timeout_ms,
        session_id,
    )
    .await
}

/// Close an open websocket session and release its host-side state.
pub async fn host_websocket_close(state: &HostState, session_id: u64) -> Result<(), String> {
    state.check_permission("websocket-outbound")?;
    websocket_close_with(
        Arc::clone(&state.websocket_sessions),
        state.timeout_ms,
        session_id,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::time::sleep;

    enum MockConnectBehavior {
        Connect(Result<MockWebSocketConnection, String>),
        DelayThenConnect {
            delay_ms: u64,
            result: Result<MockWebSocketConnection, String>,
        },
    }

    enum MockReceiveBehavior {
        Message(String),
        Delay(u64),
        Closed,
        Error(String),
    }

    #[derive(Clone, Default)]
    struct MockConnector {
        plans: Arc<Mutex<HashMap<String, VecDeque<MockConnectBehavior>>>>,
    }

    struct MockWebSocketConnection {
        sent_messages: Arc<Mutex<Vec<String>>>,
        receive_plan: VecDeque<MockReceiveBehavior>,
        close_error: Option<String>,
        fail_send_after_close: bool,
        is_closed: bool,
    }

    #[async_trait]
    impl WebSocketConnector for MockConnector {
        async fn connect(&self, url: &str) -> Result<Box<dyn WebSocketConnection>, String> {
            let behavior = {
                let mut plans = self.plans.lock().await;
                plans
                    .get_mut(url)
                    .and_then(VecDeque::pop_front)
                    .ok_or_else(|| format!("no mock plan for url: {url}"))?
            };

            match behavior {
                MockConnectBehavior::Connect(result) => {
                    result.map(|conn| Box::new(conn) as Box<dyn WebSocketConnection>)
                }
                MockConnectBehavior::DelayThenConnect { delay_ms, result } => {
                    sleep(Duration::from_millis(delay_ms)).await;
                    result.map(|conn| Box::new(conn) as Box<dyn WebSocketConnection>)
                }
            }
        }
    }

    #[async_trait]
    impl WebSocketConnection for MockWebSocketConnection {
        async fn send_text(&mut self, message: String) -> Result<(), String> {
            if self.is_closed && self.fail_send_after_close {
                return Err("websocket already closed".to_string());
            }
            self.sent_messages.lock().await.push(message.clone());
            if self.receive_plan.is_empty() {
                self.receive_plan
                    .push_back(MockReceiveBehavior::Message(message));
            }
            Ok(())
        }

        async fn recv_text(&mut self) -> Result<Option<String>, String> {
            match self.receive_plan.pop_front() {
                Some(MockReceiveBehavior::Message(message)) => Ok(Some(message)),
                Some(MockReceiveBehavior::Delay(delay_ms)) => {
                    sleep(Duration::from_millis(delay_ms)).await;
                    Ok(Some("delayed-message".to_string()))
                }
                Some(MockReceiveBehavior::Closed) | None => {
                    self.is_closed = true;
                    Ok(None)
                }
                Some(MockReceiveBehavior::Error(error)) => {
                    self.is_closed = true;
                    Err(error)
                }
            }
        }

        async fn close(&mut self) -> Result<(), String> {
            self.is_closed = true;
            match &self.close_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    impl MockConnector {
        async fn push_plan(&self, url: &str, behavior: MockConnectBehavior) {
            let mut plans = self.plans.lock().await;
            plans
                .entry(url.to_string())
                .or_default()
                .push_back(behavior);
        }
    }

    fn mock_connection(receive_plan: Vec<MockReceiveBehavior>) -> MockWebSocketConnection {
        MockWebSocketConnection {
            sent_messages: Arc::new(Mutex::new(Vec::new())),
            receive_plan: receive_plan.into(),
            close_error: None,
            fail_send_after_close: true,
            is_closed: false,
        }
    }

    fn test_state() -> HostState {
        let mut config = HashMap::new();
        config.insert("api_key".to_string(), "test-key".to_string());
        HostState::new(
            "test-plugin".to_string(),
            config,
            HashSet::from([
                "log".to_string(),
                "kv".to_string(),
                "http-outbound".to_string(),
                "websocket-outbound".to_string(),
            ]),
            HashSet::from(["llm".to_string()]),
            vec![
                "https://api.example.com/*".to_string(),
                "ws://mock.example/*".to_string(),
            ],
            30_000,
        )
    }

    #[test]
    fn host_state_creation() {
        let state = test_state();
        assert_eq!(state.plugin_name, "test-plugin");
        assert_eq!(
            host_config_get(&state, "api_key"),
            Some("test-key".to_string())
        );
        assert_eq!(host_config_get(&state, "missing"), None);
    }

    #[test]
    fn permission_check() {
        let state = test_state();
        // Always-allowed
        assert!(state.check_permission("log").is_ok());
        assert!(state.check_permission("config").is_ok());
        // Granted
        assert!(state.check_permission("kv").is_ok());
        assert!(state.check_permission("http-outbound").is_ok());
        assert!(state.check_permission("websocket-outbound").is_ok());
        // Optional but not granted
        assert!(state.check_permission("llm").is_err());
        // Not declared at all
        assert!(state.check_permission("browser").is_err());
    }

    #[test]
    fn url_allowlist_check() {
        let state = test_state();
        assert!(state.check_url_allowed("https://api.example.com/v1/data"));
        assert!(!state.check_url_allowed("https://evil.com/hack"));
    }

    #[test]
    fn url_allowlist_empty_allows_all() {
        let state = HostState::new(
            "test".to_string(),
            HashMap::new(),
            HashSet::new(),
            HashSet::new(),
            vec![],
            5000,
        );
        assert!(state.check_url_allowed("https://anything.com"));
    }

    #[test]
    fn host_log_levels() {
        let state = test_state();
        host_log(&state, "trace", "trace msg");
        host_log(&state, "debug", "debug msg");
        host_log(&state, "info", "info msg");
        host_log(&state, "warn", "warn msg");
        host_log(&state, "error", "error msg");
        host_log(&state, "unknown", "unknown level");
    }

    #[tokio::test]
    async fn kv_operations() {
        let state = test_state();
        assert_eq!(host_kv_get(&state, "key1").await, None);

        host_kv_set(&state, "key1".to_string(), b"value1".to_vec()).await;
        assert_eq!(host_kv_get(&state, "key1").await, Some(b"value1".to_vec()));

        let deleted = host_kv_delete(&state, "key1").await;
        assert!(deleted);
        assert_eq!(host_kv_get(&state, "key1").await, None);
    }

    #[tokio::test]
    async fn kv_list_keys_with_prefix() {
        let state = test_state();
        host_kv_set(&state, "weather:tokyo".to_string(), b"sunny".to_vec()).await;
        host_kv_set(&state, "weather:london".to_string(), b"rainy".to_vec()).await;
        host_kv_set(&state, "config:api_key".to_string(), b"key".to_vec()).await;

        let mut keys = host_kv_list_keys(&state, "weather:").await;
        keys.sort();
        assert_eq!(keys, vec!["weather:london", "weather:tokyo"]);
    }

    #[tokio::test]
    async fn websocket_connect_send_receive_success() {
        let connector = Arc::new(MockConnector::default());
        connector
            .push_plan(
                "ws://mock.example/echo",
                MockConnectBehavior::Connect(Ok(mock_connection(vec![]))),
            )
            .await;
        let state = test_state().with_websocket_connector(connector);

        let session_id = host_websocket_connect(&state, "ws://mock.example/echo")
            .await
            .unwrap();
        host_websocket_send(&state, session_id, "hello".to_string())
            .await
            .unwrap();
        let message = host_websocket_receive(&state, session_id).await.unwrap();
        assert_eq!(message, "hello");
        host_websocket_close(&state, session_id).await.unwrap();
    }

    #[tokio::test]
    async fn websocket_connect_failure_invalid_url() {
        let connector = Arc::new(MockConnector::default());
        connector
            .push_plan(
                "ws://mock.example/bad",
                MockConnectBehavior::Connect(Err("invalid websocket url".to_string())),
            )
            .await;
        let state = test_state().with_websocket_connector(connector);

        let error = host_websocket_connect(&state, "ws://mock.example/bad")
            .await
            .unwrap_err();
        assert!(error.contains("invalid websocket url"));
    }

    #[tokio::test]
    async fn websocket_connect_timeout() {
        let connector = Arc::new(MockConnector::default());
        connector
            .push_plan(
                "ws://mock.example/slow",
                MockConnectBehavior::DelayThenConnect {
                    delay_ms: 50,
                    result: Ok(mock_connection(vec![])),
                },
            )
            .await;
        let state = HostState::new(
            "test-plugin".to_string(),
            HashMap::new(),
            HashSet::from(["websocket-outbound".to_string()]),
            HashSet::new(),
            vec!["ws://mock.example/*".to_string()],
            10,
        )
        .with_websocket_connector(connector);

        let error = host_websocket_connect(&state, "ws://mock.example/slow")
            .await
            .unwrap_err();
        assert!(error.contains("timed out"));
    }

    #[tokio::test]
    async fn websocket_reconnect_after_disconnect() {
        let connector = Arc::new(MockConnector::default());
        connector
            .push_plan(
                "ws://mock.example/retry",
                MockConnectBehavior::Connect(Ok(mock_connection(vec![
                    MockReceiveBehavior::Closed,
                ]))),
            )
            .await;
        connector
            .push_plan(
                "ws://mock.example/retry",
                MockConnectBehavior::Connect(Ok(mock_connection(vec![]))),
            )
            .await;
        let state = test_state().with_websocket_connector(connector);

        let first_session = host_websocket_connect(&state, "ws://mock.example/retry")
            .await
            .unwrap();
        let first_error = host_websocket_receive(&state, first_session)
            .await
            .unwrap_err();
        assert!(first_error.contains("closed"));
        assert!(
            host_websocket_send(&state, first_session, "stale".to_string())
                .await
                .is_err()
        );

        let second_session = host_websocket_connect(&state, "ws://mock.example/retry")
            .await
            .unwrap();
        host_websocket_send(&state, second_session, "retry-ok".to_string())
            .await
            .unwrap();
        let message = host_websocket_receive(&state, second_session)
            .await
            .unwrap();
        assert_eq!(message, "retry-ok");
    }
}
