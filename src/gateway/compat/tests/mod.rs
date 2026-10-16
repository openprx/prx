#![cfg(test)]
use super::*;
use axum::http::{HeaderMap, HeaderValue};
use ring::signature::{ED25519, UnparsedPublicKey};
use tempfile::TempDir;

mod card_jwt;
mod document_tools;
mod handler;
mod idempotency;
mod mcp_isolation;
mod memory_delete;
mod memory_tools;
mod session_registry;
mod svid;

/// Fixture tool for the `POST /mcp/v1/tools/call` production path.
///
/// The schema is deliberately closed (`additionalProperties: false`) and
/// carries an `enum`, because those are exactly the constraints the
/// pre-0.8.126 direct `execute_named` call never enforced.
#[derive(Clone)]
struct McpFixtureTool {
    name: &'static str,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    arguments: Arc<parking_lot::Mutex<Vec<Value>>>,
}

impl McpFixtureTool {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            arguments: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn last_arguments(&self) -> Value {
        self.arguments.lock().last().cloned().unwrap_or(Value::Null)
    }
}

#[async_trait::async_trait]
impl crate::tools::Tool for McpFixtureTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "mcp gateway fixture tool"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "enum": ["read", "write"]},
                "note": {"type": "string"}
            },
            "required": ["mode"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.arguments.lock().push(args);
        Ok(crate::tools::ToolResult {
            success: true,
            output: "fixture-ran".to_string(),
            error: None,
        })
    }
}

/// Name the fixture registers under for the service-pipeline tests.
///
/// Every MCP caller runs owner-isolated, and an isolated caller may run
/// only a tool on the owner-isolation-aware list (V02-R9), so the fixture
/// borrows a listed name. `memory_forget` is also an Act-effect name, which
/// the policy and idempotency tests below depend on.
const MCP_FIXTURE_ACT_TOOL: &str = "memory_forget";

fn mcp_test_identity() -> ExternalAgentIdentity {
    external_identity_for("issuer.example", "bearer", "subject-1", "ws-1")
}

fn mcp_test_config(workspace: &Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config
}

use jsonwebtoken::{EncodingKey, Header, encode};

use serde::Serialize;

const TEST_RSA_PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC2XB5X630ehklg\n\
cK2tKCnNj1BhEaeNNU3+ElKnQK2v8r4ctb28JtgiTIe5SmGpp2CXSwuePEJp/KJ5\n\
tbL2wlcOfBEgV1R/zJ9dpR5OgZHRSdwcpHQ1WnGwyU2PA7XDAczHUM7bDjFZee02\n\
N7lSvZKJFRgInPm2i6CKZpD9eFYfACl0fFkZvlWe4cH5Qu4L1e6A551vaFzE7nAC\n\
5BVeTy+4glWzxqriXoBvfDwvA0fkP93b4kuDfjHQIl5UYTIKJldCSgX9fKcyeQQK\n\
mQM0uD7jLBNup3z52Le8SOqUC2+porAx17NbglVNSSTFo4mUlcKtAos/ZZKkG3Cw\n\
t4XmItGrAgMBAAECggEABD+7OUM0ZByu4v9dHJa9+psnPUvDAqhGS/AzWtBH+7yF\n\
L3gQlqy1Bn+P/gI8qk5LEttdxu8SgROw2XMhW1yF2MGNSJqgTmX/JJhvT1b3hc/Z\n\
wBUY3BFiasMV+ooUlxmYouBAIZyAC9yqaUP9Hm9qVaVXLQQzUUyJjg/7utwt7YTy\n\
Xyh9uGKWeE/LG+7m8SR1fXq5Lwf/1SgPBb2oVoO8Dpn2DnAJ/JpxCbulSu5GAzSs\n\
AyjEuLW9Zalqh+3hTvWXtUzT3KHuX4elDhmv9vu5UbZGrjMPC9IEI6WdQu4qO3fw\n\
D99RkuLZanhqw9FgJDQhDghKUJTuf5MIShJyfHP36QKBgQDfwt+HHcYSeomcXJLm\n\
Us6t5bSvv6ZMaODyr4LEK+r6d8BfxFF4g0B0RIBotT5krEyFv4OY8cCenNMmFFXu\n\
FDK5XNXZJQdpX2u18PxhY4eYK7JPaW76F4rUZRB77NavZNwh8GOYZhWF6z5E9EOW\n\
E9VLs31T3OJSKiiQsWpIjfozVQKBgQDQojY9TOSBBXPn9our1/nm7KP6+NKELcy/\n\
PtAetKR9SSMWOYrrhBiTQ4NzJgRvoMi5iw4UD5FMN7MbMGI8Rpt9OQVVARBQ2+8y\n\
pforWDMdgbppk+8KMm4VW0kLezXlt0BY+aLN+TXFRTwfpxqNI9Q0OStMXMV1tDOc\n\
P+QHtqLw/wKBgQCP7p4yH6jFQiU6eyHTHfjsSxHK6xBhniT0dok6/rULn/QSpglx\n\
55uSLm4a7FrSDzK55dMUko1AecgoenQ7zKpEKb81CmiWE1cJlZYCXy9dZt4vzrYg\n\
EPywWsIbtODzuYEQI70szp2RoxxO5oCDDQbqxu4a/75k89FcIYMoYItMcQKBgCN4\n\
UIxfdEHTMX9wVRKkJ2JxPPfAMdozBypEfZGa2JRMSODQa6Pa02rGAaUkA4EO7tFM\n\
qNoUQ3mXxqWKtkjVID5L4XIwOhvlKGeN/Fg+KIKNuamVcwBizoBnAqYnDmS9oPz/\n\
hARMqC2ftbcT69mvC7bNOWVEKHX4awXXfucoz871AoGASv++umarnx2XAOPR+4pd\n\
Dv3qazMWKWp+P6Lh4BxhJqKnmcicyaEWf7ermG29tjLFvTyCTWBz/wtRKyS+PNSg\n\
LKi6p/oD7FKDLwRlNR14bCfuo57HplU0N3/xVkwOCBq7+u/tj8aUxQQa25Tgs1BY\n\
2EPW+JVwoGPd+e3y/0uZ90o=\n\
-----END PRIVATE KEY-----\n";

const TEST_RSA_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAtlweV+t9HoZJYHCtrSgp\n\
zY9QYRGnjTVN/hJSp0Ctr/K+HLW9vCbYIkyHuUphqadgl0sLnjxCafyiebWy9sJX\n\
DnwRIFdUf8yfXaUeToGR0UncHKR0NVpxsMlNjwO1wwHMx1DO2w4xWXntNje5Ur2S\n\
iRUYCJz5tougimaQ/XhWHwApdHxZGb5VnuHB+ULuC9XugOedb2hcxO5wAuQVXk8v\n\
uIJVs8aq4l6Ab3w8LwNH5D/d2+JLg34x0CJeVGEyCiZXQkoF/XynMnkECpkDNLg+\n\
4ywTbqd8+di3vEjqlAtvqaKwMdezW4JVTUkkxaOJlJXCrQKLP2WSpBtwsLeF5iLR\n\
qwIDAQAB\n\
-----END PUBLIC KEY-----\n";

#[derive(Serialize)]
struct TestClaims {
    iss: String,
    sub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    aud: Option<String>,
    exp: usize,
}

fn future_exp() -> usize {
    usize::try_from(chrono::Utc::now().timestamp() + 3_600).unwrap_or(usize::MAX)
}

fn sign_rs256(claims: &TestClaims, kid: Option<&str>) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(ToString::to_string);
    let key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_PEM.as_bytes()).expect("test: rsa private pem");
    encode(&header, claims, &key).expect("test: encode rs256")
}

fn pem_config() -> crate::config::McpServerRuntimeConfig {
    crate::config::McpServerRuntimeConfig {
        jwt_issuer: Some("https://issuer.example".to_string()),
        jwt_audience: Some("prx-mcp".to_string()),
        jwt_public_key_pem: Some(TEST_RSA_PUBLIC_PEM.to_string()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------
// C03 / V02-R4: owner-qualified identity isolation for MCP reads.
//
// `mcp_execution_context` keys the envelope on the external *subject*
// (`mcp:{subject}`, sender = subject). Two MCP identities that present the
// same subject under different issuers therefore share a session key and a
// sender, and only `prx_owner_id` distinguishes them. These tests pin the
// production tool output of the default-exposed `sessions_list` /
// `sessions_history` tools, not just the principal structure.
// ---------------------------------------------------------------------

const ISOLATION_WORKSPACE: &str = "ws-isolation";

const SHARED_SUBJECT: &str = "shared-subject";

fn isolation_identity(issuer: &str) -> ExternalAgentIdentity {
    external_identity_for(issuer, "bearer", SHARED_SUBJECT, ISOLATION_WORKSPACE)
}

/// Append one message event attributed to `identity`, exactly as the MCP
/// envelope would attribute it (session key `mcp:{subject}`, sender =
/// subject, owner = `prx_owner_id`).
async fn seed_owned_message_event(
    memory: &Arc<crate::memory::SqliteMemory>,
    identity: &ExternalAgentIdentity,
    visibility: crate::memory::MemoryVisibility,
    run_id: &str,
    content: &str,
) {
    use crate::memory::Memory as _;
    let input: crate::memory::MessageEventInput = serde_json::from_value(serde_json::json!({
        "workspace_id": identity.workspace_id,
        "owner_id": identity.prx_owner_id,
        "source": "sessions_spawn",
        "channel": "mcp",
        "session_key": format!("mcp:{}", identity.external_subject),
        "run_id": run_id,
        "sender": identity.external_subject,
        "role": "user",
        "event_type": "message.user",
        "visibility": visibility.as_str(),
        "content": content,
    }))
    .expect("test: message event input");
    memory
        .append_message_event(input)
        .await
        .expect("test: append message event");
}

// ---------------------------------------------------------------------
// V02-R5: owner isolation for the memory-table tools the MCP surface
// exposes by default (`memory_search`, `memory_get`, `memory_recall`).
//
// The block above covers the event/registry read model. The `memories`
// table was never part of that boundary: the read mode was chosen from the
// global `memory.acl_enabled` flag alone, so an authenticated MCP caller
// read the whole store at the default configuration, and with the flag on
// it resolved to a principal keyed on the external *subject* — identical
// for two issuers. These tests drive the same production seam
// (`execute_mcp_tool_call` -> `ToolExecutionService` -> tool -> SQLite).
// ---------------------------------------------------------------------

/// One seeded row, named so it cannot be confused with a real note and so
/// its content carries no keyword the memory classifier treats as risky.
const MEMORY_MARKER_KEY: &str = "issuer-a-note";

const MEMORY_MARKER_CONTENT: &str = "alpha rollout note authored by the first issuer";

const MEMORY_MARKER_QUERY: &str = "rollout";

/// Store one `memories` row attributed to `identity` exactly as the MCP
/// envelope attributes it: channel `mcp`, chat id `mcp:{subject}`, raw
/// sender = subject, owner = `prx_owner_id`.
///
/// Only the owner differs between two issuers presenting the same subject,
/// which is the whole point of the fixture.
async fn seed_owned_memory(
    memory: &Arc<crate::memory::SqliteMemory>,
    identity: &ExternalAgentIdentity,
    key: &str,
    content: &str,
) {
    use crate::memory::Memory as _;
    let context = crate::memory::principal::MemoryWriteContext {
        workspace_id: Some(identity.workspace_id.clone()),
        channel: Some("mcp".to_string()),
        chat_type: Some("gateway".to_string()),
        chat_id: Some(format!("mcp:{}", identity.external_subject)),
        sender_id: Some(identity.prx_owner_id.clone()),
        raw_sender: Some(identity.external_subject.clone()),
        owner_id: Some(identity.prx_owner_id.clone()),
        require_owner_match: false,
    };
    memory
        .store_with_context_and_metadata(
            key,
            content,
            crate::memory::MemoryCategory::Core,
            None,
            Some(&context),
            crate::memory::MemoryStoreMetadata {
                workspace_id: Some(identity.workspace_id.clone()),
                owner_id: Some(identity.prx_owner_id.clone()),
                channel: Some("mcp".to_string()),
                ..crate::memory::MemoryStoreMetadata::default()
            },
        )
        .await
        .expect("test: store an owned memory row");
}

// ---------------------------------------------------------------------
// V02-R7: the same C03 guarantees, driven through the registry an actual
// gateway process resolves against.
//
// The block above builds a two-tool fixture registry. That proves the tool
// bodies and the envelope, but says nothing about the registry the handler
// resolves against —
// and until the shared registrar published the two session read tools,
// `POST /mcp/v1/tools/call` answered 404 for both names, so none of those
// guarantees had anything to run over on this seam. These tests build the
// production registry with `build_gateway_turn_runtime` and assert both
// halves at once: the tool is reached, and it is still isolated.
// ---------------------------------------------------------------------

/// A config the turn-runtime builder accepts.
///
/// The builder constructs a real provider chain, which refuses to come up
/// with no credential at all, and the closed loopback port makes certain
/// that no sub-agent started here can reach anything. Nothing in these
/// tests calls out.
fn production_mcp_test_config(workspace: &Path) -> crate::config::Config {
    let mut config = mcp_test_config(workspace);
    config.api_key = Some("test-key".to_string());
    config.api_url = Some("http://127.0.0.1:1".to_string());
    // A sub-agent a test spawns builds its provider from this default: a
    // closed loopback endpoint, so it never reaches a real host (D-081).
    config.default_provider = Some("custom:http://127.0.0.1:1/v1".to_string());
    config
}

fn production_spawn_test_config(workspace: &Path) -> crate::config::Config {
    let mut config = production_mcp_test_config(workspace);
    config.causal_tree.enabled = false;
    config.tool_tiering.always_include.push("sessions_spawn".to_owned());
    config.default_model = Some("prx-gateway-owner-vis".to_owned());
    let provider_name = crate::providers::selection::default_provider(&config).to_owned();
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider": provider_name,
            "model": "prx-gateway-owner-vis",
            "operations": ["chat"],
            "native_tools": true
        }))
        .expect("test: gateway owner-visibility model declaration"),
    );
    config.cost.prices.insert(
        format!("{provider_name}/prx-gateway-owner-vis"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let catalog = crate::providers::selection::catalog_for(&config);
    crate::providers::selection::check_with(
        &config,
        &catalog,
        &provider_name,
        "prx-gateway-owner-vis",
        crate::providers::selection::credential_for(&config, &provider_name),
        &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )
    .unwrap_or_else(|error| panic!("test: gateway fixture model must verify native tools: {error:#}"));
    config
}

/// The MCP identity an owner-isolated caller presents against the
/// production registry.
///
/// The registrar binds the session read tools to `config.workspace_dir`,
/// which is the workspace id their memory projection queries under, so the
/// caller's workspace id has to be that same path for the seeded rows to be
/// addressable at all. Only the issuer differs between callers, exactly as
/// in the fixture block above.
fn production_isolation_identity(issuer: &str, workspace: &Path) -> ExternalAgentIdentity {
    external_identity_for(issuer, "bearer", SHARED_SUBJECT, &workspace.to_string_lossy())
}

/// The production gateway tool registry, sealed exactly as the daemon seals
/// it, over the memory these tests seed.
fn production_sessions_registry(
    config: &crate::config::Config,
    memory: &Arc<crate::memory::SqliteMemory>,
) -> Arc<Vec<Box<dyn crate::tools::Tool>>> {
    production_sessions_runtime(config, memory).tools_registry
}

fn production_sessions_runtime(
    config: &crate::config::Config,
    memory: &Arc<crate::memory::SqliteMemory>,
) -> crate::gateway::TurnRuntimeGeneration {
    let manager = crate::config::new_shared(config.clone());
    let shared: Arc<dyn crate::memory::Memory> = memory.clone();
    let hooks = Arc::new(crate::hooks::HookManager::new(config.workspace_dir.clone()));
    crate::gateway::build_gateway_turn_runtime(
        manager.pin(),
        manager.clone(),
        shared,
        None,
        None,
        None,
        None,
        hooks,
        #[cfg(feature = "wasm-plugins")]
        None,
    )
    .expect("test: the production gateway turn runtime must build")
}

/// Drive one MCP tool call against `registry` and return the wire response,
/// so a caller can assert on the error flag as well as the text.
async fn run_sessions_tool_on(
    registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    config: &crate::config::Config,
    identity: &ExternalAgentIdentity,
    tool_name: &str,
    args: Value,
) -> McpToolCallResponse {
    let outcome = execute_mcp_tool_call(registry, config, tool_name, args, identity, None).await;
    mcp_tool_call_response(tool_name.to_string(), identity.clone(), outcome)
}

fn response_text(response: &McpToolCallResponse) -> String {
    response
        .content
        .first()
        .map(|block| block.text.clone())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------
// H-1: the real `POST /mcp/v1/tools/call` handler.
//
// Every MCP seam test above calls `execute_mcp_tool_call` directly, which
// skips everything `mcp_tools_call` decides before the job runs: caller
// identity derivation, the rate limit, the exposure gate, the identity
// binding and registry admission. The harness below drives the axum handler
// itself over the production gateway turn runtime, the way the daemon wires
// it, so those decisions are under test too.
// ---------------------------------------------------------------------

/// Serializes every user of the process-wide MCP rate limiter.
///
/// `mcp_tools_call` applies the configured budget to the one global limiter
/// in place, so a handler call made with a budget of `0` would switch off the
/// limit a concurrent budget test is spending, and that test's budget of `2`
/// would throttle the handler call. Each side holds this lock for as long as
/// it depends on the global budget.
static MCP_GLOBAL_RATE_LIMITER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
