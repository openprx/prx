//! The single registry of secret-bearing configuration paths.
//!
//! Every surface that stores, reads back or displays configuration decides
//! what a secret is here, and nowhere else:
//!
//! - `Config::save` encrypts, and `Config::load_from_path` decrypts, exactly
//!   the paths registered as [`SecretHandling::EncryptedAtRest`].
//! - The Gateway config API (`GET /api/config`, the raw file API) and
//!   `prx config show` redact every registered path, plus any key whose name
//!   looks like a credential ([`is_secret_key_name`]), so a secret that a
//!   future config section adds without registering it is still masked.
//!
//! Every registered path is encrypted at rest (SECRETS-OTHER moved the last
//! channel, gateway, MCP, webhook, tunnel and node secrets there). A guard
//! test walks the `Config` schema and fails when a credential-named field is
//! neither registered nor exempted with a reason, so a new credential cannot
//! be stored as plaintext unnoticed.

use anyhow::{Context, Result};

/// The value every read surface shows in place of a secret. A request that
/// posts this value back is never stored as a credential.
pub const REDACTION_MASK: &str = "***";

/// One step of a registered path pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// A table key with this exact name.
    Key(&'static str),
    /// Any key of a map (agent names, MCP server names, environment names).
    AnyKey,
    /// Any element of an array.
    AnyIndex,
}

/// How a registered secret is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretHandling {
    /// Encrypted by `Config::save` when `secrets.encrypt = true`, decrypted on
    /// load. A plaintext value that an older release wrote is still read and
    /// is encrypted on the next save. This is the handling of every
    /// registered path today; a secret that must stay readable as written by
    /// some external reader would add its own handling here.
    EncryptedAtRest,
}

/// What a registered secret authenticates. Informational; it groups the
/// registry for review and for the follow-up migrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretDomain {
    /// A model provider credential.
    Provider,
    /// A credential of a built-in tool integration.
    Tool,
    /// A storage connection string.
    Storage,
    /// A channel credential.
    Channel,
    /// A Gateway credential.
    Gateway,
    /// An MCP server environment value.
    Mcp,
    /// A remote node (`prx-node`) credential.
    Node,
    /// Any other credential (tunnels, webhooks, the chat daemon).
    Other,
}

/// One registered secret-bearing path.
#[derive(Debug, Clone, Copy)]
pub struct SecretPath {
    /// Human-readable pattern, e.g. `model_routes[].api_key`.
    pub pattern: &'static str,
    /// The pattern the walkers match.
    pub segments: &'static [Segment],
    /// How the value is stored.
    pub handling: SecretHandling,
    /// What the value authenticates.
    pub domain: SecretDomain,
    /// For a serde alias spelling the loader also accepts, the canonical
    /// pattern it deserializes into. Save always writes the canonical name;
    /// the alias is registered so a hand-written alias is still decrypted,
    /// masked and refused as plaintext like the canonical path.
    pub alias_of: Option<&'static str>,
    /// What writing the value does besides storing a secret; decides the
    /// risk reason the configuration plan gives a change of it.
    pub used_as: SecretUse,
}

/// What a registered secret does once it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretUse {
    /// PRX presents it to authenticate somewhere (a key, a token, a
    /// password). Replacing it rotates the credential.
    Credential,
    /// It becomes the environment of a program PRX launches (an MCP server's
    /// `env`). Besides credentials it can hold settings that change what the
    /// program runs (`NODE_OPTIONS`, `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`,
    /// `PYTHONPATH`, `PATH`), so every change needs confirmation.
    ProcessEnvironment,
    /// Its value admits a client to PRX or to `prx-node` (D-048): the
    /// paired-token list, the inbound webhook token and signing secret, a
    /// channel's verify token, webhook or signing secret, the node server's
    /// bearer token and HMAC secret. Whoever knows a new value is let in, and
    /// replacing one shuts out the clients that hold the old one, so every
    /// change needs confirmation. An outbound credential PRX presents to
    /// someone else is a [`Credential`](Self::Credential).
    AccessGrant,
}

use SecretHandling::EncryptedAtRest;
use Segment::{AnyIndex, AnyKey, Key};

/// Every registered secret-bearing path.
pub static SECRET_PATHS: &[SecretPath] = &[
    // ── Provider credentials ───────────────────────────────────────
    SecretPath {
        pattern: "api_key",
        segments: &[Key("api_key")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Provider,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "agents.<name>.api_key",
        segments: &[Key("agents"), AnyKey, Key("api_key")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Provider,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "model_routes[].api_key",
        segments: &[Key("model_routes"), AnyIndex, Key("api_key")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Provider,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "embedding_routes[].api_key",
        segments: &[Key("embedding_routes"), AnyIndex, Key("api_key")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Provider,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "reliability.api_keys[]",
        segments: &[Key("reliability"), Key("api_keys"), AnyIndex],
        handling: EncryptedAtRest,
        domain: SecretDomain::Provider,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    // ── Tool and storage credentials ───────────────────────────────
    SecretPath {
        pattern: "composio.api_key",
        segments: &[Key("composio"), Key("api_key")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Tool,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "browser.computer_use.api_key",
        segments: &[Key("browser"), Key("computer_use"), Key("api_key")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Tool,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "web_search.brave_api_key",
        segments: &[Key("web_search"), Key("brave_api_key")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Tool,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "storage.provider.config.db_url",
        segments: &[Key("storage"), Key("provider"), Key("config"), Key("db_url")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Storage,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    // serde aliases of `db_url` (see `StorageProviderConfig`).
    SecretPath {
        pattern: "storage.provider.config.dbURL",
        segments: &[Key("storage"), Key("provider"), Key("config"), Key("dbURL")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Storage,
        alias_of: Some("storage.provider.config.db_url"),
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "storage.provider.config.database_url",
        segments: &[Key("storage"), Key("provider"), Key("config"), Key("database_url")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Storage,
        alias_of: Some("storage.provider.config.db_url"),
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "storage.provider.config.databaseUrl",
        segments: &[Key("storage"), Key("provider"), Key("config"), Key("databaseUrl")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Storage,
        alias_of: Some("storage.provider.config.db_url"),
        used_as: SecretUse::Credential,
    },
    // ── Gateway, MCP, channel and other credentials (SECRETS-OTHER) ──
    SecretPath {
        pattern: "gateway.paired_tokens[]",
        segments: &[Key("gateway"), Key("paired_tokens"), AnyIndex],
        handling: EncryptedAtRest,
        domain: SecretDomain::Gateway,
        alias_of: None,
        used_as: SecretUse::AccessGrant,
    },
    SecretPath {
        pattern: "mcp.servers.<name>.env.<var>",
        segments: &[Key("mcp"), Key("servers"), AnyKey, Key("env"), AnyKey],
        handling: EncryptedAtRest,
        domain: SecretDomain::Mcp,
        alias_of: None,
        used_as: SecretUse::ProcessEnvironment,
    },
    SecretPath {
        pattern: "channels_config.telegram.bot_token",
        segments: &[Key("channels_config"), Key("telegram"), Key("bot_token")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Channel,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "channels_config.discord.bot_token",
        segments: &[Key("channels_config"), Key("discord"), Key("bot_token")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Channel,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "channels_config.slack.bot_token",
        segments: &[Key("channels_config"), Key("slack"), Key("bot_token")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Channel,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "channels_config.slack.app_token",
        segments: &[Key("channels_config"), Key("slack"), Key("app_token")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Channel,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "channels_config.mattermost.bot_token",
        segments: &[Key("channels_config"), Key("mattermost"), Key("bot_token")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Channel,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "webhook.token",
        segments: &[Key("webhook"), Key("token")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Other,
        alias_of: None,
        used_as: SecretUse::AccessGrant,
    },
    SecretPath {
        pattern: "tunnel.cloudflare.token",
        segments: &[Key("tunnel"), Key("cloudflare"), Key("token")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Other,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    SecretPath {
        pattern: "chat.daemon.token",
        segments: &[Key("chat"), Key("daemon"), Key("token")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Other,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
    other(
        "channels_config.dingtalk.client_secret",
        &[Key("channels_config"), Key("dingtalk"), Key("client_secret")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.email.password",
        &[Key("channels_config"), Key("email"), Key("password")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.irc.server_password",
        &[Key("channels_config"), Key("irc"), Key("server_password")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.irc.nickserv_password",
        &[Key("channels_config"), Key("irc"), Key("nickserv_password")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.irc.sasl_password",
        &[Key("channels_config"), Key("irc"), Key("sasl_password")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.lark.app_secret",
        &[Key("channels_config"), Key("lark"), Key("app_secret")],
        SecretDomain::Channel,
    ),
    admission(
        "channels_config.lark.encrypt_key",
        &[Key("channels_config"), Key("lark"), Key("encrypt_key")],
        SecretDomain::Channel,
    ),
    admission(
        "channels_config.lark.verification_token",
        &[Key("channels_config"), Key("lark"), Key("verification_token")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.linq.api_token",
        &[Key("channels_config"), Key("linq"), Key("api_token")],
        SecretDomain::Channel,
    ),
    admission(
        "channels_config.linq.signing_secret",
        &[Key("channels_config"), Key("linq"), Key("signing_secret")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.matrix.access_token",
        &[Key("channels_config"), Key("matrix"), Key("access_token")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.nextcloud_talk.app_token",
        &[Key("channels_config"), Key("nextcloud_talk"), Key("app_token")],
        SecretDomain::Channel,
    ),
    admission(
        "channels_config.nextcloud_talk.webhook_secret",
        &[Key("channels_config"), Key("nextcloud_talk"), Key("webhook_secret")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.qq.app_secret",
        &[Key("channels_config"), Key("qq"), Key("app_secret")],
        SecretDomain::Channel,
    ),
    admission(
        "channels_config.wacli.webhook_secret",
        &[Key("channels_config"), Key("wacli"), Key("webhook_secret")],
        SecretDomain::Channel,
    ),
    admission(
        "channels_config.webhook.secret",
        &[Key("channels_config"), Key("webhook"), Key("secret")],
        SecretDomain::Channel,
    ),
    other(
        "channels_config.whatsapp.access_token",
        &[Key("channels_config"), Key("whatsapp"), Key("access_token")],
        SecretDomain::Channel,
    ),
    admission(
        "channels_config.whatsapp.app_secret",
        &[Key("channels_config"), Key("whatsapp"), Key("app_secret")],
        SecretDomain::Channel,
    ),
    admission(
        "channels_config.whatsapp.verify_token",
        &[Key("channels_config"), Key("whatsapp"), Key("verify_token")],
        SecretDomain::Channel,
    ),
    admission(
        "webhook.signing_secret",
        &[Key("webhook"), Key("signing_secret")],
        SecretDomain::Other,
    ),
    other(
        "tunnel.ngrok.auth_token",
        &[Key("tunnel"), Key("ngrok"), Key("auth_token")],
        SecretDomain::Other,
    ),
    // ── Remote node credentials ────────────────────────────────────
    other(
        "nodes.nodes[].bearer_token",
        &[Key("nodes"), Key("nodes"), AnyIndex, Key("bearer_token")],
        SecretDomain::Node,
    ),
    other(
        "nodes.nodes[].hmac_secret",
        &[Key("nodes"), Key("nodes"), AnyIndex, Key("hmac_secret")],
        SecretDomain::Node,
    ),
    admission(
        "nodes.server.bearer_token",
        &[Key("nodes"), Key("server"), Key("bearer_token")],
        SecretDomain::Node,
    ),
    admission(
        "nodes.server.hmac_secret",
        &[Key("nodes"), Key("server"), Key("hmac_secret")],
        SecretDomain::Node,
    ),
    // serde aliases of the node `bearer_token` fields.
    SecretPath {
        alias_of: Some("nodes.nodes[].bearer_token"),
        ..other(
            "nodes.nodes[].token",
            &[Key("nodes"), Key("nodes"), AnyIndex, Key("token")],
            SecretDomain::Node,
        )
    },
    SecretPath {
        alias_of: Some("nodes.server.bearer_token"),
        ..admission(
            "nodes.server.token",
            &[Key("nodes"), Key("server"), Key("token")],
            SecretDomain::Node,
        )
    },
];

/// A registered secret that admits a client to PRX or to `prx-node` (an
/// inbound webhook token or signing secret, a channel's verify token, the
/// node server's bearer token): an access grant (D-048).
const fn admission(pattern: &'static str, segments: &'static [Segment], domain: SecretDomain) -> SecretPath {
    SecretPath {
        used_as: SecretUse::AccessGrant,
        ..other(pattern, segments, domain)
    }
}

/// A registered secret outside the provider, tool and storage domains.
const fn other(pattern: &'static str, segments: &'static [Segment], domain: SecretDomain) -> SecretPath {
    SecretPath {
        pattern,
        segments,
        handling: EncryptedAtRest,
        domain,
        alias_of: None,
        used_as: SecretUse::Credential,
    }
}

/// How a configuration path that names an endpoint is treated.
///
/// This is the one registry of endpoint paths. It decides two things:
///
/// - [`userinfo`](Self::userinfo): the value is a URL (or a provider label
///   that may spell one) and may carry `user:password@`. Every read surface
///   that masks a registered secret shows it with only the userinfo replaced
///   (`custom:http://[redacted]@host/v1`), so the endpoint stays
///   recognisable. A redacted value posted or written back is a mask: it
///   restores the stored value when it equals that value's redacted form,
///   and is refused otherwise. Setting userinfo is a credential change.
/// - [`credential_recipient`](Self::credential_recipient): a stored
///   credential is sent to this endpoint, so re-pointing it is an endpoint
///   change (high risk) in the configuration plan (`config::manage::plan`).
///
/// A guard test fails when a URL-named field of the schema is missing here.
#[derive(Debug, Clone, Copy)]
pub struct EndpointPath {
    /// Human-readable pattern, e.g. `reliability.fallback_providers[]`.
    pub pattern: &'static str,
    /// The pattern the walkers match.
    pub segments: &'static [Segment],
    /// The value may carry URL userinfo.
    pub userinfo: bool,
    /// Re-pointing the endpoint sends a stored credential to a new host.
    pub credential_recipient: bool,
}

const fn userinfo(pattern: &'static str, segments: &'static [Segment]) -> EndpointPath {
    EndpointPath {
        pattern,
        segments,
        userinfo: true,
        credential_recipient: false,
    }
}

const fn recipient_url(pattern: &'static str, segments: &'static [Segment]) -> EndpointPath {
    EndpointPath {
        pattern,
        segments,
        userinfo: true,
        credential_recipient: true,
    }
}

const fn recipient_host(pattern: &'static str, segments: &'static [Segment]) -> EndpointPath {
    EndpointPath {
        pattern,
        segments,
        userinfo: false,
        credential_recipient: true,
    }
}

/// Every configuration path that names a provider or an endpoint.
pub static ENDPOINT_PATHS: &[EndpointPath] = &[
    // Provider labels and the provider URL: a `custom:` spelling carries
    // its endpoint; `plan.rs` treats a change to or from one as an endpoint
    // change (a stored key goes with the provider).
    userinfo("default_provider", &[Key("default_provider")]),
    userinfo("api_url", &[Key("api_url")]),
    userinfo(
        "reliability.fallback_providers[]",
        &[Key("reliability"), Key("fallback_providers"), AnyIndex],
    ),
    userinfo(
        "model_routes[].provider",
        &[Key("model_routes"), AnyIndex, Key("provider")],
    ),
    userinfo(
        "embedding_routes[].provider",
        &[Key("embedding_routes"), AnyIndex, Key("provider")],
    ),
    userinfo("agents.<name>.provider", &[Key("agents"), AnyKey, Key("provider")]),
    userinfo(
        "providers.tool_dialects[].provider",
        &[Key("providers"), Key("tool_dialects"), AnyIndex, Key("provider")],
    ),
    userinfo(
        "providers.models[].provider",
        &[Key("providers"), Key("models"), AnyIndex, Key("provider")],
    ),
    userinfo(
        "router.models[].provider",
        &[Key("router"), Key("models"), AnyIndex, Key("provider")],
    ),
    userinfo("memory.embedding_provider", &[Key("memory"), Key("embedding_provider")]),
    userinfo(
        "smart_group.classifier_provider",
        &[Key("smart_group"), Key("classifier_provider")],
    ),
    // Endpoints that receive a stored credential (a token, a password, the
    // traffic of every credentialed request through a proxy): re-pointing
    // one sends that credential to the new host. The MCP server URL gets no
    // header from this configuration today; re-pointing it redirects tool
    // traffic and any credential a later header field adds. IRC and email
    // hosts are host names, not URLs, so they carry no userinfo.
    recipient_url("proxy.http_proxy", &[Key("proxy"), Key("http_proxy")]),
    recipient_url("proxy.https_proxy", &[Key("proxy"), Key("https_proxy")]),
    recipient_url("proxy.all_proxy", &[Key("proxy"), Key("all_proxy")]),
    recipient_url(
        "mcp.servers.<name>.url",
        &[Key("mcp"), Key("servers"), AnyKey, Key("url")],
    ),
    recipient_url(
        "nodes.nodes[].endpoint",
        &[Key("nodes"), Key("nodes"), AnyIndex, Key("endpoint")],
    ),
    recipient_url(
        "channels_config.mattermost.url",
        &[Key("channels_config"), Key("mattermost"), Key("url")],
    ),
    recipient_url(
        "channels_config.matrix.homeserver",
        &[Key("channels_config"), Key("matrix"), Key("homeserver")],
    ),
    recipient_url(
        "channels_config.signal.http_url",
        &[Key("channels_config"), Key("signal"), Key("http_url")],
    ),
    recipient_url(
        "channels_config.whatsapp.ws_url",
        &[Key("channels_config"), Key("whatsapp"), Key("ws_url")],
    ),
    recipient_url(
        "channels_config.nextcloud_talk.base_url",
        &[Key("channels_config"), Key("nextcloud_talk"), Key("base_url")],
    ),
    recipient_host(
        "channels_config.irc.server",
        &[Key("channels_config"), Key("irc"), Key("server")],
    ),
    recipient_host(
        "channels_config.email.imap_host",
        &[Key("channels_config"), Key("email"), Key("imap_host")],
    ),
    recipient_host(
        "channels_config.email.smtp_host",
        &[Key("channels_config"), Key("email"), Key("smtp_host")],
    ),
    recipient_url(
        "observability.otel_endpoint",
        &[Key("observability"), Key("otel_endpoint")],
    ),
    recipient_url("chat.daemon.url", &[Key("chat"), Key("daemon"), Key("url")]),
    // A connection string is a credential and an endpoint at once: a new
    // value re-points memory storage, so a change is also an endpoint change
    // (D-049 F). Its whole value is a registered secret, so it carries no
    // separate userinfo rule.
    recipient_host(
        "storage.provider.config.db_url",
        &[Key("storage"), Key("provider"), Key("config"), Key("db_url")],
    ),
    // Endpoint URLs that receive no stored credential: only their userinfo
    // is a secret.
    userinfo("media.audio_ollama_url", &[Key("media"), Key("audio_ollama_url")]),
    userinfo(
        "tunnel.custom.health_url",
        &[Key("tunnel"), Key("custom"), Key("health_url")],
    ),
    userinfo("mcp_server.jwt_jwks_uri", &[Key("mcp_server"), Key("jwt_jwks_uri")]),
    userinfo("a2a.card_jwks_uri", &[Key("a2a"), Key("card_jwks_uri")]),
    // The decision endpoint (CDP-03, CDP-04): validation refuses userinfo
    // there, and this keeps every read surface safe for a hand-edited file.
    // The decision backend's credential is sent to it, so re-pointing it is
    // an endpoint change (high risk).
    recipient_url(
        "causal_tree.backend.endpoint",
        &[Key("causal_tree"), Key("backend"), Key("endpoint")],
    ),
    // The endpoint a calibration was applied to (D-094 C-7): its userinfo is
    // redacted like the backend's, and it receives nothing.
    userinfo(
        "causal_tree.policy.calibration_profile.endpoint",
        &[
            Key("causal_tree"),
            Key("policy"),
            Key("calibration_profile"),
            Key("endpoint"),
        ],
    ),
];

/// A setting that decides who is admitted inbound (D-049): not a secret, but
/// whoever it newly trusts is let in. A change at or under one of these
/// paths is an access-grant change, like a change of an admission secret
/// ([`SecretUse::AccessGrant`]), on every write path.
#[derive(Debug, Clone, Copy)]
pub struct AccessControlPath {
    /// Human-readable pattern, e.g. `gateway.require_pairing`.
    pub pattern: &'static str,
    /// The pattern the walkers match; every path under it counts too.
    pub segments: &'static [Segment],
    /// Where the setting is enforced, for review.
    pub enforced_at: &'static str,
}

const fn access(pattern: &'static str, segments: &'static [Segment], enforced_at: &'static str) -> AccessControlPath {
    AccessControlPath {
        pattern,
        segments,
        enforced_at,
    }
}

/// Every setting whose change can widen who is admitted inbound.
pub static ACCESS_CONTROL_PATHS: &[AccessControlPath] = &[
    // ── Gateway ─────────────────────────────────────────────────────
    access(
        "gateway.require_pairing",
        &[Key("gateway"), Key("require_pairing")],
        "gateway/mod.rs (PairingGuard::new), security/pairing.rs",
    ),
    access(
        "gateway.trust_forwarded_headers",
        &[Key("gateway"), Key("trust_forwarded_headers")],
        "gateway/mod.rs (client key), gateway/compat.rs (MCP caller identity headers)",
    ),
    access(
        "gateway.allow_public_bind",
        &[Key("gateway"), Key("allow_public_bind")],
        "gateway/mod.rs (public bind refusal)",
    ),
    access(
        "gateway.host",
        &[Key("gateway"), Key("host")],
        "runtime/mode.rs -> gateway::run_gateway (bind address; a public host exposes the gateway)",
    ),
    // ── MCP server endpoint (`/mcp/v1`) ─────────────────────────────
    access(
        "mcp_server.require_auth",
        &[Key("mcp_server"), Key("require_auth")],
        "gateway/compat.rs (unauthenticated MCP calls)",
    ),
    access(
        "mcp_server.jwt_public_key_pem",
        &[Key("mcp_server"), Key("jwt_public_key_pem")],
        "gateway/compat.rs (bearer JWT verification key)",
    ),
    access(
        "mcp_server.jwt_jwks",
        &[Key("mcp_server"), Key("jwt_jwks")],
        "gateway/compat.rs (inline JWKS)",
    ),
    access(
        "mcp_server.jwt_jwks_uri",
        &[Key("mcp_server"), Key("jwt_jwks_uri")],
        "gateway/compat.rs (remote JWKS)",
    ),
    access(
        "mcp_server.jwt_issuer",
        &[Key("mcp_server"), Key("jwt_issuer")],
        "gateway/compat.rs (accepted `iss`)",
    ),
    access(
        "mcp_server.jwt_audience",
        &[Key("mcp_server"), Key("jwt_audience")],
        "gateway/compat.rs (accepted `aud`)",
    ),
    access(
        "mcp_server.jwt_algorithms",
        &[Key("mcp_server"), Key("jwt_algorithms")],
        "gateway/compat.rs (accepted signature algorithms)",
    ),
    access(
        "mcp_server.allowed_origins",
        &[Key("mcp_server"), Key("allowed_origins")],
        "declared browser-origin allow-list of the MCP endpoint (not read yet)",
    ),
    // ── A2A peers ───────────────────────────────────────────────────
    access(
        "a2a.allowed_peer_issuers",
        &[Key("a2a"), Key("allowed_peer_issuers")],
        "gateway/compat.rs (peer issuer allow-list)",
    ),
    access(
        "a2a.trusted_trust_domains",
        &[Key("a2a"), Key("trusted_trust_domains")],
        "gateway/compat.rs (SPIFFE trust domains)",
    ),
    access(
        "a2a.spiffe_trust_bundle_pem",
        &[Key("a2a"), Key("spiffe_trust_bundle_pem")],
        "gateway/compat.rs (SVID chain verification)",
    ),
    access(
        "a2a.spiffe_strict_validation",
        &[Key("a2a"), Key("spiffe_strict_validation")],
        "gateway/compat.rs (strict SVID validation)",
    ),
    // ── Who a caller is ─────────────────────────────────────────────
    access(
        "identity_bindings",
        &[Key("identity_bindings")],
        "channels/mod.rs, daemon/mod.rs (channel account -> user identity)",
    ),
    // ── Channel sender allow-lists and policies ─────────────────────
    access(
        "channels_config.telegram.allowed_users",
        &[Key("channels_config"), Key("telegram"), Key("allowed_users")],
        "channels/telegram.rs",
    ),
    access(
        "channels_config.discord.allowed_users",
        &[Key("channels_config"), Key("discord"), Key("allowed_users")],
        "channels/discord.rs",
    ),
    access(
        "channels_config.slack.allowed_users",
        &[Key("channels_config"), Key("slack"), Key("allowed_users")],
        "channels/slack.rs",
    ),
    access(
        "channels_config.mattermost.allowed_users",
        &[Key("channels_config"), Key("mattermost"), Key("allowed_users")],
        "channels/mattermost.rs",
    ),
    access(
        "channels_config.imessage.allowed_contacts",
        &[Key("channels_config"), Key("imessage"), Key("allowed_contacts")],
        "channels/imessage.rs",
    ),
    access(
        "channels_config.matrix.allowed_users",
        &[Key("channels_config"), Key("matrix"), Key("allowed_users")],
        "channels/matrix.rs",
    ),
    access(
        "channels_config.signal.allowed_from",
        &[Key("channels_config"), Key("signal"), Key("allowed_from")],
        "channels/signal.rs",
    ),
    access(
        "channels_config.signal.group_allow_from",
        &[Key("channels_config"), Key("signal"), Key("group_allow_from")],
        "channels/signal.rs",
    ),
    access(
        "channels_config.signal.dm_policy",
        &[Key("channels_config"), Key("signal"), Key("dm_policy")],
        "channels/mod.rs (direct/group message admission policy)",
    ),
    access(
        "channels_config.signal.group_policy",
        &[Key("channels_config"), Key("signal"), Key("group_policy")],
        "channels/mod.rs (direct/group message admission policy)",
    ),
    access(
        "channels_config.whatsapp.allowed_numbers",
        &[Key("channels_config"), Key("whatsapp"), Key("allowed_numbers")],
        "channels/whatsapp.rs",
    ),
    access(
        "channels_config.whatsapp.group_allow_from",
        &[Key("channels_config"), Key("whatsapp"), Key("group_allow_from")],
        "channels/whatsapp.rs",
    ),
    access(
        "channels_config.whatsapp.dm_policy",
        &[Key("channels_config"), Key("whatsapp"), Key("dm_policy")],
        "channels/mod.rs (direct/group message admission policy)",
    ),
    access(
        "channels_config.whatsapp.group_policy",
        &[Key("channels_config"), Key("whatsapp"), Key("group_policy")],
        "channels/mod.rs (direct/group message admission policy)",
    ),
    access(
        "channels_config.wacli.allowed_from",
        &[Key("channels_config"), Key("wacli"), Key("allowed_from")],
        "channels/wacli.rs",
    ),
    access(
        "channels_config.wacli.allow_unsigned_loopback",
        &[Key("channels_config"), Key("wacli"), Key("allow_unsigned_loopback")],
        "channels/wacli.rs (accepts unsigned webhook calls)",
    ),
    access(
        "channels_config.linq.allowed_senders",
        &[Key("channels_config"), Key("linq"), Key("allowed_senders")],
        "channels/linq.rs",
    ),
    access(
        "channels_config.nextcloud_talk.allowed_users",
        &[Key("channels_config"), Key("nextcloud_talk"), Key("allowed_users")],
        "channels/nextcloud_talk.rs",
    ),
    access(
        "channels_config.email.allowed_senders",
        &[Key("channels_config"), Key("email"), Key("allowed_senders")],
        "channels/email_channel.rs",
    ),
    access(
        "channels_config.irc.allowed_users",
        &[Key("channels_config"), Key("irc"), Key("allowed_users")],
        "channels/irc.rs",
    ),
    access(
        "channels_config.lark.allowed_users",
        &[Key("channels_config"), Key("lark"), Key("allowed_users")],
        "channels/lark.rs",
    ),
    access(
        "channels_config.dingtalk.allowed_users",
        &[Key("channels_config"), Key("dingtalk"), Key("allowed_users")],
        "channels/dingtalk.rs",
    ),
    access(
        "channels_config.qq.allowed_users",
        &[Key("channels_config"), Key("qq"), Key("allowed_users")],
        "channels/qq.rs",
    ),
];

/// Whether a concrete path is, or lies under, a setting that decides who is
/// admitted inbound.
#[must_use]
pub fn is_access_control_path(path: &[PathElem]) -> bool {
    ACCESS_CONTROL_PATHS.iter().any(|entry| {
        path.get(..entry.segments.len())
            .is_some_and(|prefix| pattern_matches(entry.segments, prefix))
    })
}

/// Tables whose *keys* may spell a provider by URL: `[cost].prices` is keyed
/// `<provider>/<model>`, and a provider may carry `user:password@`. The value
/// walkers never see keys, so these tables are handled by key.
pub static URL_USERINFO_KEY_TABLES: &[EndpointPath] =
    &[userinfo("cost.prices.<provider/model>", &[Key("cost"), Key("prices")])];

/// Whether a concrete path is a table whose keys may carry URL userinfo.
#[must_use]
pub fn is_url_userinfo_key_table(path: &[PathElem]) -> bool {
    URL_USERINFO_KEY_TABLES
        .iter()
        .any(|entry| pattern_matches(entry.segments, path))
}

/// Whether a concrete path names a provider or an endpoint by URL, so its
/// value may carry URL userinfo.
#[must_use]
pub fn is_url_userinfo_path(path: &[PathElem]) -> bool {
    ENDPOINT_PATHS
        .iter()
        .any(|entry| entry.userinfo && pattern_matches(entry.segments, path))
}

/// Whether a concrete path names an endpoint that receives a stored
/// credential, so re-pointing it sends that credential to a new host.
#[must_use]
pub fn is_credential_recipient_endpoint(path: &[PathElem]) -> bool {
    ENDPOINT_PATHS
        .iter()
        .any(|entry| entry.credential_recipient && pattern_matches(entry.segments, path))
}

/// The userinfo `value` carries, as it would be redacted, or `None`.
fn url_userinfo(value: &str) -> Option<&str> {
    let std::borrow::Cow::Owned(redacted) = crate::providers::redact_endpoint_userinfo(value) else {
        return None;
    };
    // `redacted` is `value` with one or more userinfo spans replaced; the
    // common prefix and suffix bound the part that differs.
    let prefix = value
        .bytes()
        .zip(redacted.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = value
        .bytes()
        .rev()
        .zip(redacted.bytes().rev())
        .take_while(|(left, right)| left == right)
        .count()
        .min(value.len().saturating_sub(prefix));
    // A span that does not fall on character boundaries is not expected, but
    // the whole value is then the safe thing to scrub.
    value
        .get(prefix..value.len() - suffix)
        .filter(|span| !span.is_empty())
        .or(Some(value))
}

/// Whether a key name looks like a credential.
///
/// This is the fallback that keeps an unregistered secret masked on every
/// read surface. It never decides what is encrypted: that is only the
/// registry.
#[must_use]
pub fn is_secret_key_name(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "api_key"
            | "api_keys"
            | "auth_token"
            | "token"
            | "secret"
            | "password"
            | "paired_tokens"
            | "db_url"
            | "private_key"
            | "access_key"
            | "credential"
            | "credentials"
            | "connection_string"
            | "signing_secret"
            | "webhook_secret"
            | "app_secret"
    ) || key.ends_with("_api_key")
        || key.ends_with("_api_keys")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_password")
        || key.ends_with("_key")
        || key.ends_with("_credential")
        || key.ends_with("_credentials")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("private_key")
}

/// One concrete step of a path inside a configuration tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathElem {
    /// A table key.
    Key(String),
    /// An array index.
    Index(usize),
}

/// Render a concrete path as `model_routes[0].api_key`.
#[must_use]
pub fn display_path(path: &[PathElem]) -> String {
    let mut rendered = String::new();
    for elem in path {
        match elem {
            PathElem::Key(key) => {
                if !rendered.is_empty() {
                    rendered.push('.');
                }
                rendered.push_str(key);
            }
            PathElem::Index(index) => {
                rendered.push('[');
                rendered.push_str(&index.to_string());
                rendered.push(']');
            }
        }
    }
    rendered
}

/// Whether a concrete path is an instance of a pattern.
#[must_use]
pub fn pattern_matches(segments: &[Segment], path: &[PathElem]) -> bool {
    segments.len() == path.len()
        && segments.iter().zip(path).all(|(segment, elem)| match (segment, elem) {
            (Segment::Key(expected), PathElem::Key(actual)) => expected == actual,
            (Segment::AnyKey, PathElem::Key(_)) | (Segment::AnyIndex, PathElem::Index(_)) => true,
            (Segment::Key(_) | Segment::AnyKey, PathElem::Index(_)) | (Segment::AnyIndex, PathElem::Key(_)) => false,
        })
}

/// The registered path a concrete path is an instance of, if any.
#[must_use]
pub fn registered(path: &[PathElem]) -> Option<&'static SecretPath> {
    SECRET_PATHS
        .iter()
        .find(|secret| pattern_matches(secret.segments, path))
}

/// Whether a concrete path holds a secret: registered, or named like one.
#[must_use]
pub fn is_secret_path(path: &[PathElem]) -> bool {
    registered(path).is_some() || matches!(path.last(), Some(PathElem::Key(key)) if is_secret_key_name(key))
}

fn is_encrypted_at_rest(path: &[PathElem]) -> bool {
    registered(path).is_some_and(|secret| secret.handling == SecretHandling::EncryptedAtRest)
}

fn transform_encrypted_at(
    node: &mut toml::Value,
    path: &mut Vec<PathElem>,
    transform: &mut dyn FnMut(&str, &str) -> Result<Option<String>>,
) -> Result<()> {
    if !path.is_empty() && is_encrypted_at_rest(path) {
        if let Some(raw) = node.as_str() {
            if let Some(next) = transform(&display_path(path), raw)? {
                *node = toml::Value::String(next);
            }
        }
        return Ok(());
    }
    match node {
        toml::Value::Table(table) => {
            for (key, child) in table.iter_mut() {
                path.push(PathElem::Key(key.clone()));
                let result = transform_encrypted_at(child, path, transform);
                path.pop();
                result?;
            }
        }
        toml::Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                path.push(PathElem::Index(index));
                let result = transform_encrypted_at(child, path, transform);
                path.pop();
                result?;
            }
        }
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => {}
    }
    Ok(())
}

/// Encrypt every plaintext value at an [`SecretHandling::EncryptedAtRest`]
/// path of a stored tree. Already-encrypted values are kept; a disabled store
/// (`secrets.encrypt = false`) keeps plaintext.
pub fn encrypt_at_rest(root: &mut toml::Value, store: &crate::security::SecretStore) -> Result<()> {
    transform_encrypted_at(root, &mut Vec::new(), &mut |path, raw| {
        if crate::security::SecretStore::is_encrypted(raw) {
            return Ok(None);
        }
        store
            .encrypt(raw)
            .map(Some)
            .with_context(|| format!("Failed to encrypt config.{path}"))
    })
}

/// Decrypt every encrypted value at an [`SecretHandling::EncryptedAtRest`]
/// path of a stored tree. A plaintext value (written by an older release, or
/// with `secrets.encrypt = false`) is read as it is.
pub fn decrypt_at_rest(root: &mut toml::Value, store: &crate::security::SecretStore) -> Result<()> {
    transform_encrypted_at(root, &mut Vec::new(), &mut |path, raw| {
        if !crate::security::SecretStore::is_encrypted(raw) {
            return Ok(None);
        }
        store
            .decrypt(raw)
            .map(Some)
            .with_context(|| format!("Failed to decrypt config.{path}"))
    })
}

/// Decrypt one table of a stored tree read straight from `config_path` by a
/// reader that does not go through `Config::load` (`prx-node --config` reads
/// `nodes.server` only), with the key beside that file, as `Config::load`
/// does. Nothing outside `table` is decrypted, and a missing key file is
/// never created. Plaintext stays as it is; a value that does not decrypt is
/// an error naming its path, never a value.
pub fn decrypt_file_table(root: &mut toml::Value, table: &[&str], config_path: &std::path::Path) -> Result<()> {
    let Some(node) = table.iter().try_fold(root, |node, key| node.get_mut(*key)) else {
        return Ok(());
    };
    let config_dir = config_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    // Decryption never depends on `secrets.encrypt`: it only turns off
    // encryption on save.
    let store = crate::security::SecretStore::with_existing_key(config_dir);
    let mut path = table.iter().map(|key| PathElem::Key((*key).to_string())).collect();
    transform_encrypted_at(node, &mut path, &mut |path, raw| {
        if !crate::security::SecretStore::is_encrypted(raw) {
            return Ok(None);
        }
        store
            .decrypt(raw)
            .map(Some)
            .with_context(|| format!("Failed to decrypt config.{path}"))
    })
}

/// Re-encrypt every legacy `enc:` value at an encrypted path as `enc2:`.
/// Returns how many values changed. A value that fails to migrate is logged
/// and kept, so one corrupt value does not block the others.
pub fn migrate_legacy_at_rest(root: &mut toml::Value, store: &crate::security::SecretStore) -> Result<usize> {
    let mut migrated = 0_usize;
    transform_encrypted_at(root, &mut Vec::new(), &mut |path, raw| {
        if !crate::security::SecretStore::needs_migration(raw) {
            return Ok(None);
        }
        match store.decrypt_and_migrate(raw) {
            Ok((_, Some(next))) => {
                migrated += 1;
                Ok(Some(next))
            }
            Ok((_, None)) => Ok(None),
            Err(error) => {
                tracing::warn!(path = %path, error = %error, "Failed to migrate legacy secret; keeping it");
                Ok(None)
            }
        }
    })?;
    Ok(migrated)
}

/// A configuration tree the registry can walk: TOML for stored files and
/// `prx config show`, JSON for the Gateway API.
pub trait SecretTree: Sized {
    /// The string value of a scalar string node.
    fn as_secret_str(&self) -> Option<&str>;
    /// Replace this node with a string.
    fn set_string(&mut self, value: String);
    /// Whether this node is an explicit null (JSON only).
    fn is_null_node(&self) -> bool;
    /// Visit every direct child with its path element.
    fn for_each_child<'a>(&'a self, visit: &mut dyn FnMut(PathElem, &'a Self));
    /// Visit every direct child mutably with its path element.
    fn for_each_child_mut(&mut self, visit: &mut dyn FnMut(PathElem, &mut Self));
    /// Whether this node is a table or an array.
    fn is_container(&self) -> bool;
    /// Rename the keys of a table for which `rename` returns a new name. The
    /// value moves with its key. No-op for anything that is not a table.
    fn rename_keys(&mut self, rename: &dyn Fn(&str) -> Option<String>);
    /// The keys of a table (empty for anything else).
    fn table_keys(&self) -> Vec<String>;
}

impl SecretTree for toml::Value {
    fn as_secret_str(&self) -> Option<&str> {
        self.as_str()
    }

    fn set_string(&mut self, value: String) {
        *self = Self::String(value);
    }

    fn is_null_node(&self) -> bool {
        false
    }

    fn for_each_child<'a>(&'a self, visit: &mut dyn FnMut(PathElem, &'a Self)) {
        match self {
            Self::Table(table) => {
                for (key, child) in table {
                    visit(PathElem::Key(key.clone()), child);
                }
            }
            Self::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    visit(PathElem::Index(index), child);
                }
            }
            Self::String(_) | Self::Integer(_) | Self::Float(_) | Self::Boolean(_) | Self::Datetime(_) => {}
        }
    }

    fn for_each_child_mut(&mut self, visit: &mut dyn FnMut(PathElem, &mut Self)) {
        match self {
            Self::Table(table) => {
                for (key, child) in table.iter_mut() {
                    visit(PathElem::Key(key.clone()), child);
                }
            }
            Self::Array(items) => {
                for (index, child) in items.iter_mut().enumerate() {
                    visit(PathElem::Index(index), child);
                }
            }
            Self::String(_) | Self::Integer(_) | Self::Float(_) | Self::Boolean(_) | Self::Datetime(_) => {}
        }
    }

    fn is_container(&self) -> bool {
        matches!(self, Self::Table(_) | Self::Array(_))
    }

    fn rename_keys(&mut self, rename: &dyn Fn(&str) -> Option<String>) {
        if let Self::Table(table) = self {
            let renamed: Vec<(String, String)> = table
                .keys()
                .filter_map(|key| rename(key).map(|new| (key.clone(), new)))
                .collect();
            for (old, new) in renamed {
                if let Some(value) = table.remove(&old) {
                    table.insert(new, value);
                }
            }
        }
    }

    fn table_keys(&self) -> Vec<String> {
        match self {
            Self::Table(table) => table.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }
}

impl SecretTree for serde_json::Value {
    fn as_secret_str(&self) -> Option<&str> {
        self.as_str()
    }

    fn set_string(&mut self, value: String) {
        *self = Self::String(value);
    }

    fn is_null_node(&self) -> bool {
        self.is_null()
    }

    fn for_each_child<'a>(&'a self, visit: &mut dyn FnMut(PathElem, &'a Self)) {
        match self {
            Self::Object(map) => {
                for (key, child) in map {
                    visit(PathElem::Key(key.clone()), child);
                }
            }
            Self::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    visit(PathElem::Index(index), child);
                }
            }
            Self::Null | Self::Bool(_) | Self::Number(_) | Self::String(_) => {}
        }
    }

    fn for_each_child_mut(&mut self, visit: &mut dyn FnMut(PathElem, &mut Self)) {
        match self {
            Self::Object(map) => {
                for (key, child) in map.iter_mut() {
                    visit(PathElem::Key(key.clone()), child);
                }
            }
            Self::Array(items) => {
                for (index, child) in items.iter_mut().enumerate() {
                    visit(PathElem::Index(index), child);
                }
            }
            Self::Null | Self::Bool(_) | Self::Number(_) | Self::String(_) => {}
        }
    }

    fn is_container(&self) -> bool {
        matches!(self, Self::Object(_) | Self::Array(_))
    }

    fn rename_keys(&mut self, rename: &dyn Fn(&str) -> Option<String>) {
        if let Self::Object(map) = self {
            let renamed: Vec<(String, String)> = map
                .keys()
                .filter_map(|key| rename(key).map(|new| (key.clone(), new)))
                .collect();
            for (old, new) in renamed {
                if let Some(value) = map.remove(&old) {
                    map.insert(new, value);
                }
            }
        }
    }

    fn table_keys(&self) -> Vec<String> {
        match self {
            Self::Object(map) => map.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }
}

/// Replace every non-null scalar under `node` with the mask.
fn mask_subtree<T: SecretTree>(node: &mut T) {
    if node.is_container() {
        node.for_each_child_mut(&mut |_, child| mask_subtree(child));
    } else if !node.is_null_node() {
        node.set_string(REDACTION_MASK.to_string());
    }
}

fn redact_at<T: SecretTree>(node: &mut T, path: &mut Vec<PathElem>) {
    if !path.is_empty() && is_secret_path(path) {
        mask_subtree(node);
        return;
    }
    if is_url_userinfo_key_table(path) {
        node.rename_keys(&|key| match crate::security::redaction::redact_url_value(key) {
            std::borrow::Cow::Owned(redacted) => Some(redacted),
            std::borrow::Cow::Borrowed(_) => None,
        });
    }
    if is_url_userinfo_path(path) {
        if let Some(std::borrow::Cow::Owned(redacted)) =
            node.as_secret_str().map(crate::security::redaction::redact_url_value)
        {
            node.set_string(redacted);
        }
        return;
    }
    node.for_each_child_mut(&mut |elem, child| {
        path.push(elem);
        redact_at(child, path);
        path.pop();
    });
}

/// Mask every secret in a configuration tree rooted at the config root.
pub fn redact<T: SecretTree>(root: &mut T) {
    redact_at(root, &mut Vec::new());
}

/// Mask every secret in a subtree that sits at `prefix` inside the config.
pub fn redact_subtree<T: SecretTree>(node: &mut T, prefix: &[PathElem]) {
    let mut path = prefix.to_vec();
    redact_at(node, &mut path);
}

/// What a configuration tree holds at its secret paths.
#[derive(Debug, Default)]
pub struct SecretScan {
    /// Concrete paths of encrypted-at-rest slots that hold plaintext.
    pub plaintext_at_rest: Vec<String>,
    /// Concrete secret paths whose value is the redaction mask.
    pub masked: Vec<String>,
    /// Every non-empty secret value found, with the display path of the
    /// setting that holds it, for scrubbing messages.
    values: Vec<(String, String)>,
}

impl SecretScan {
    /// Whether the tree holds any non-empty secret value.
    #[must_use]
    pub const fn has_secret_values(&self) -> bool {
        !self.values.is_empty()
    }

    /// Every non-empty secret value found, with the display path of the
    /// setting that holds it (the decision outbound guard refuses a request
    /// that carries one, and names the setting of one it cannot scan).
    pub(crate) fn values_with_paths(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(path, value)| (path.as_str(), value.as_str()))
    }

    /// Replace every secret value the scan found in `message` with the mask,
    /// then apply the shape-based redactor. The one error scrubber:
    /// [`scrub_message`].
    #[must_use]
    pub fn scrub(&self, message: &str) -> String {
        scrub_message(Some(self), message)
    }
}

/// The single scrubber for an error message a surface may show. With a
/// [`SecretScan`] every secret value the scan holds is replaced by the mask
/// first; a caller that holds no scan (a credential endpoint, where replacing
/// a short submitted value would garble unrelated words) passes `None`. Both
/// then go through the shape-based redactor, so a surface never differs from
/// another in what it treats as a secret.
#[must_use]
pub fn scrub_message(scan: Option<&SecretScan>, message: &str) -> String {
    let masked = scan.map_or_else(
        || message.to_string(),
        |scan| {
            scan.values.iter().fold(message.to_string(), |scrubbed, (_, value)| {
                scrubbed.replace(value.as_str(), REDACTION_MASK)
            })
        },
    );
    crate::security::redaction::redact_secrets(&masked)
}

fn scan_secret_subtree<T: SecretTree>(node: &T, path: &mut Vec<PathElem>, scan: &mut SecretScan) {
    if let Some(raw) = node.as_secret_str() {
        if raw == REDACTION_MASK {
            scan.masked.push(display_path(path));
        } else if !raw.is_empty() {
            // The leaf itself must be a registered encrypted slot: a name-rule
            // hit on `reliability.api_keys` covers the array, the registered
            // slot is each element.
            if is_encrypted_at_rest(path) && !crate::security::SecretStore::is_encrypted(raw) {
                scan.plaintext_at_rest.push(display_path(path));
            }
            scan.values.push((display_path(path), raw.to_string()));
        }
        return;
    }
    node.for_each_child(&mut |elem, child| {
        path.push(elem);
        scan_secret_subtree(child, path, scan);
        path.pop();
    });
}

fn scan_at<T: SecretTree>(node: &T, path: &mut Vec<PathElem>, scan: &mut SecretScan) {
    if !path.is_empty() && is_secret_path(path) {
        scan_secret_subtree(node, path, scan);
        return;
    }
    if is_url_userinfo_key_table(path) {
        for key in node.table_keys() {
            if crate::security::redaction::holds_redacted_url_part(&key) {
                path.push(PathElem::Key(key));
                scan.masked.push(display_path(path));
                path.pop();
            } else if let Some(userinfo) = url_userinfo(&key) {
                scan.values.push((display_path(path), userinfo.to_string()));
            }
        }
    }
    if is_url_userinfo_path(path) {
        if let Some(raw) = node.as_secret_str() {
            if crate::security::redaction::holds_redacted_url_part(raw) {
                scan.masked.push(display_path(path));
            } else if let Some(userinfo) = url_userinfo(raw) {
                scan.values.push((display_path(path), userinfo.to_string()));
            }
        }
        return;
    }
    node.for_each_child(&mut |elem, child| {
        path.push(elem);
        scan_at(child, path, scan);
        path.pop();
    });
}

/// Scan a configuration tree rooted at the config root: which secret paths
/// hold plaintext at an encrypted slot, which hold the mask, and every secret
/// value (for scrubbing messages that might quote one).
#[must_use]
pub fn scan<T: SecretTree>(root: &T) -> SecretScan {
    let mut result = SecretScan::default();
    scan_at(root, &mut Vec::new(), &mut result);
    result
}

/// Warn about every secret path of a decrypted stored tree that holds the
/// literal redaction mask. Such a value was never a real credential (an
/// earlier `GET`→`POST` round trip stored it) and is sent to the provider as
/// the key until it is set again. The warning names the path, never a value.
pub fn warn_on_stored_masks(root: &toml::Value) {
    let masked = scan(root).masked;
    if !masked.is_empty() {
        tracing::warn!(
            paths = %masked.join(", "),
            "Stored configuration holds the redaction mask `***` as a credential; set it again with \
             PUT /api/config/secrets (provider slots) or by replacing the array"
        );
    }
}

/// `{:?}` of a configuration sub-struct that sits at `prefix` in the
/// configuration (a dotted path; `[]` marks an array element and any segment
/// matches a user-named map key, as in `model_routes[]` or `agents.<name>`).
///
/// It prints the serialized settings with every secret masked the way
/// [`redact`] masks the whole configuration: every registered secret path,
/// every credential-named key and the userinfo of every endpoint URL. A
/// derived `Debug` would print them. It never falls back to the derived
/// output.
pub(crate) fn debug_redacted<T: serde::Serialize>(
    f: &mut std::fmt::Formatter<'_>,
    name: &str,
    value: &T,
    prefix: &str,
) -> std::fmt::Result {
    let mut path = Vec::new();
    for segment in prefix.split('.').filter(|segment| !segment.is_empty()) {
        match segment.strip_suffix("[]") {
            Some(key) => {
                path.push(PathElem::Key(key.to_string()));
                path.push(PathElem::Index(0));
            }
            None => path.push(PathElem::Key(segment.to_string())),
        }
    }
    match serde_json::to_value(value) {
        Ok(mut settings) => {
            redact_subtree(&mut settings, &path);
            write!(f, "{name} {settings}")
        }
        Err(_) => write!(f, "{name} <unserializable>"),
    }
}

/// Whether a comment in stored TOML text assigns a credential-named key, as
/// in `# api_key = "sk-old"` (a common way to keep a previous key around).
/// Checks the text after the first `#` (a later `#` also ends a key), so a
/// trailing comment counts; a `#` inside a string can only cause extra
/// masking, never less.
///
/// A key is the last word before any `=` or `:`, so `# token: abc` counts
/// as well. It is compared ignoring case with `-` read as `_`
/// (`# api-key = …`), and a name ending in `apikey` counts (`# apikey = …`).
/// A credential in free text (`# the old key was sk-…`) is not found.
#[must_use]
pub fn has_commented_secret(text: &str) -> bool {
    const COMMENTED_KEY_LOOKBACK: usize = 256;
    text.lines().any(|line| {
        line.find('#').is_some_and(|index| {
            let comment = line.get(index + 1..).unwrap_or_default();
            comment.match_indices(['=', ':']).any(|(end, _)| {
                // A key name is short; looking back a bounded distance keeps
                // a line of many separators linear.
                let mut start = end.saturating_sub(COMMENTED_KEY_LOOKBACK);
                while !comment.is_char_boundary(start) {
                    start += 1;
                }
                comment
                    .get(start..end)
                    .unwrap_or_default()
                    .split_whitespace()
                    .next_back()
                    .and_then(|key| key.rsplit(['.', '[', ']', '#']).find(|segment| !segment.is_empty()))
                    .map(|key| {
                        key.trim_matches(|c| c == '"' || c == '\'')
                            .to_ascii_lowercase()
                            .replace('-', "_")
                    })
                    .is_some_and(|key| key.ends_with("apikey") || is_secret_key_name(&key))
            })
        })
    })
}

/// The stored form of a configuration with every secret masked. This is what
/// `prx config show` prints.
pub fn redacted_stored_toml(config: &crate::config::Config) -> Result<toml::Value> {
    let mut value = config.to_stored_toml_value()?;
    redact(&mut value);
    Ok(value)
}

/// A configuration fixture that holds a distinct sentinel at every registered
/// path. Shared by the registry guards here and the Gateway API tests.
#[cfg(test)]
pub(crate) mod fixture {
    /// Every sentinel starts with this prefix, so a rendering can be checked
    /// for any leak with one substring search.
    pub(crate) const SENTINEL_PREFIX: &str = "sentinel-";

    pub(crate) const TOML: &str = r#"
api_key = "sentinel-root"

[agents.worker]
provider = "openrouter"
model = "model-test"
api_key = "sentinel-agent"

[[model_routes]]
hint = "fast"
provider = "openrouter"
model = "model-test"
api_key = "sentinel-route"

[[embedding_routes]]
hint = "semantic"
provider = "openai"
model = "text-embedding-3-small"
api_key = "sentinel-embedding"

[reliability]
api_keys = ["sentinel-rotation-0", "sentinel-rotation-1"]

[composio]
api_key = "sentinel-composio"

[browser.computer_use]
api_key = "sentinel-browser"

[web_search]
brave_api_key = "sentinel-brave"

[storage.provider.config]
db_url = "postgres://sentinel-db@localhost/prx"

[gateway]
paired_tokens = ["sentinel-paired"]

[mcp.servers.local]
command = "mcp-local"
env = { MCP_TOKEN = "sentinel-mcp-env" }

[channels_config.telegram]
bot_token = "sentinel-telegram"
allowed_users = []

[channels_config.discord]
bot_token = "sentinel-discord"

[channels_config.slack]
bot_token = "sentinel-slack-bot"
app_token = "sentinel-slack-app"

[channels_config.mattermost]
url = "http://127.0.0.1:1"
bot_token = "sentinel-mattermost"

[channels_config.dingtalk]
client_id = "dingtalk-client"
client_secret = "sentinel-dingtalk"

[channels_config.email]
imap_host = "imap.invalid"
smtp_host = "smtp.invalid"
username = "prx"
password = "sentinel-email"
from_address = "prx@mail.invalid"

[channels_config.irc]
server = "irc.invalid"
nickname = "prx"
server_password = "sentinel-irc-server"
nickserv_password = "sentinel-irc-nickserv"
sasl_password = "sentinel-irc-sasl"

[channels_config.lark]
app_id = "lark-app"
app_secret = "sentinel-lark-app"
encrypt_key = "sentinel-lark-encrypt"
verification_token = "sentinel-lark-verification"

[channels_config.linq]
api_token = "sentinel-linq-api"
from_phone = "+10000000000"
signing_secret = "sentinel-linq-signing"

[channels_config.matrix]
homeserver = "http://127.0.0.1:1"
access_token = "sentinel-matrix"
room_id = "!room:matrix.invalid"
allowed_users = []

[channels_config.nextcloud_talk]
base_url = "http://127.0.0.1:1"
app_token = "sentinel-nextcloud-app"
webhook_secret = "sentinel-nextcloud-webhook"

[channels_config.qq]
app_id = "qq-app"
app_secret = "sentinel-qq"

[channels_config.wacli]
webhook_secret = "sentinel-wacli-webhook"

[channels_config.webhook]
port = 1
secret = "sentinel-channel-webhook"

[channels_config.whatsapp]
access_token = "sentinel-whatsapp-access"
app_secret = "sentinel-whatsapp-app"
verify_token = "sentinel-whatsapp-verify"

[webhook]
token = "sentinel-webhook"
signing_secret = "sentinel-webhook-signing"

[tunnel.cloudflare]
token = "sentinel-cloudflare"

[tunnel.ngrok]
auth_token = "sentinel-ngrok"

[chat.daemon]
token = "sentinel-daemon"

[[nodes.nodes]]
id = "edge"
endpoint = "http://127.0.0.1:1"
bearer_token = "sentinel-node-bearer"
hmac_secret = "sentinel-node-hmac"

[nodes.server]
bearer_token = "sentinel-node-server-bearer"
hmac_secret = "sentinel-node-server-hmac"
"#;

    /// A value at every endpoint URL of `URL_USERINFO_PATHS` outside the
    /// provider labels, each carrying a distinct fixture password.
    pub(crate) const ENDPOINT_USERINFO_TOML: &str = r#"
[proxy]
http_proxy = "http://proxy:userinfo-sentinel-http-proxy@127.0.0.1:11"
https_proxy = "http://proxy:userinfo-sentinel-https-proxy@127.0.0.1:12"
all_proxy = "socks5://proxy:userinfo-sentinel-all-proxy@127.0.0.1:13"

[mcp.servers.remote]
transport = "http"
url = "https://mcp:userinfo-sentinel-mcp@127.0.0.1:14/mcp"

[chat.daemon]
url = "http://chat:userinfo-sentinel-daemon@127.0.0.1:15"

[[nodes.nodes]]
id = "edge"
endpoint = "http://node:userinfo-sentinel-node@127.0.0.1:16"
bearer_token = "node-bearer"

[observability]
otel_endpoint = "http://otel:userinfo-sentinel-otel@127.0.0.1:17"

[media]
audio_ollama_url = "http://ollama:userinfo-sentinel-ollama@127.0.0.1:18"

[tunnel.custom]
start_command = "tunnel-up"
health_url = "http://tunnel:userinfo-sentinel-tunnel@127.0.0.1:19/health"

[mcp_server]
jwt_jwks_uri = "https://jwks:userinfo-sentinel-mcp-jwks@127.0.0.1:20/jwks.json"

[a2a]
card_jwks_uri = "https://jwks:userinfo-sentinel-a2a-jwks@127.0.0.1:21/jwks.json"

[channels_config.mattermost]
url = "http://mm:userinfo-sentinel-mattermost@127.0.0.1:22"
bot_token = "mattermost-token"

[channels_config.nextcloud_talk]
base_url = "http://nc:userinfo-sentinel-nextcloud@127.0.0.1:23"
app_token = "nextcloud-token"

[channels_config.matrix]
homeserver = "http://mx:userinfo-sentinel-matrix@127.0.0.1:24"
access_token = "matrix-token"
room_id = "!room:matrix.invalid"
allowed_users = []

[channels_config.signal]
account = "+10000000000"
http_url = "http://signal:userinfo-sentinel-signal@127.0.0.1:25"

[channels_config.whatsapp]
ws_url = "ws://wa:userinfo-sentinel-whatsapp@127.0.0.1:26"

[causal_tree.backend]
endpoint = "https://decider:userinfo-sentinel-decision@127.0.0.1:27/v1"
"#;

    /// `toml` (a partial configuration) overlaid onto the default tree, as a
    /// `Config` rooted in `dir`.
    pub(crate) fn config_from_overlay(dir: &std::path::Path, toml: &str) -> crate::config::Config {
        let mut tree = toml::Value::try_from(crate::config::Config::default()).expect("test: default config tree");
        overlay(&mut tree, toml::from_str(toml).expect("test: overlay parses"));
        let mut config: crate::config::Config = tree.try_into().expect("test: the overlay must deserialize");
        config.config_path = dir.join("config.toml");
        config.workspace_dir = dir.join("workspace");
        config
    }

    /// Every concrete path in `root` whose string value satisfies `keep`.
    pub(crate) fn string_leaves<T: super::SecretTree>(
        root: &T,
        keep: &dyn Fn(&str) -> bool,
    ) -> std::collections::BTreeSet<Vec<super::PathElem>> {
        fn walk<T: super::SecretTree>(
            node: &T,
            path: &mut Vec<super::PathElem>,
            keep: &dyn Fn(&str) -> bool,
            out: &mut std::collections::BTreeSet<Vec<super::PathElem>>,
        ) {
            if let Some(value) = node.as_secret_str() {
                if keep(value) {
                    out.insert(path.clone());
                }
                return;
            }
            node.for_each_child(&mut |elem, child| {
                path.push(elem);
                walk(child, path, keep, out);
                path.pop();
            });
        }
        let mut out = std::collections::BTreeSet::new();
        walk(root, &mut Vec::new(), keep, &mut out);
        out
    }

    /// Masked concrete paths of a rendering, as display strings.
    pub(crate) fn masked_paths<T: super::SecretTree>(root: &T) -> std::collections::BTreeSet<String> {
        string_leaves(root, &|value| value == super::REDACTION_MASK)
            .iter()
            .map(|path| super::display_path(path))
            .collect()
    }

    /// Held by every test that reaches a log callsite another test captures
    /// (the stored-mask warning, the credential endpoint's success line).
    /// `tracing` caches per-callsite interest process-wide, and a callsite
    /// first reached on a thread without a capturing subscriber can drop the
    /// capturing test's event; serializing these few tests removes the race.
    pub(crate) static LOG_CALLSITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Keep callsite registration on sibling threads aware of this capture.
    /// tracing-core's single-dispatcher fast path uses that thread's default.
    pub(crate) fn capture_dispatch(subscriber: impl tracing::Subscriber + Send + Sync + 'static) -> tracing::Dispatch {
        static INERT: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
        INERT.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
        tracing::Dispatch::new(subscriber)
    }

    /// Deep-merge `patch` into `base`.
    pub(crate) fn overlay(base: &mut toml::Value, patch: toml::Value) {
        match (base, patch) {
            (toml::Value::Table(base), toml::Value::Table(patch)) => {
                for (key, value) in patch {
                    match base.get_mut(&key) {
                        Some(existing) => overlay(existing, value),
                        None => {
                            base.insert(key, value);
                        }
                    }
                }
            }
            (base, patch) => *base = patch,
        }
    }

    /// The fixture as a `Config` rooted in `dir`.
    pub(crate) fn config_in(dir: &std::path::Path) -> crate::config::Config {
        // Overlay the sentinels onto the default tree, so sections whose
        // required fields the fixture does not spell out keep their defaults.
        config_from_overlay(dir, TOML)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn one_scrubber_masks_by_value_and_by_shape_with_or_without_a_scan() {
        let message = "bad key sk-proj-abcdefghijklmnop12345 near sentinel-route";
        let tree: toml::Value = toml::from_str(
            "[[model_routes]]\nhint = \"fast\"\nprovider = \"p\"\nmodel = \"m\"\napi_key = \"sentinel-route\"\n",
        )
        .expect("test: parse");
        let found = scan(&tree);
        let with_scan = scrub_message(Some(&found), message);
        assert!(!with_scan.contains("sk-proj-abcdefghijklmnop12345"), "{with_scan}");
        assert!(!with_scan.contains("sentinel-route"), "{with_scan}");
        let without_scan = scrub_message(None, message);
        assert!(
            !without_scan.contains("sk-proj-abcdefghijklmnop12345"),
            "{without_scan}"
        );
        assert_eq!(found.scrub(message), with_scan);
    }

    fn fixture_tree() -> toml::Value {
        toml::from_str(fixture::TOML).expect("test: fixture parses")
    }

    fn sentinel_paths() -> BTreeSet<Vec<PathElem>> {
        fixture::string_leaves(&fixture_tree(), &|value| value.contains(fixture::SENTINEL_PREFIX))
    }

    /// Resolve `$ref` and `anyOf`/`oneOf`/`allOf` alternatives of a schema node.
    fn alternatives<'a>(root: &'a serde_json::Value, node: &'a serde_json::Value) -> Vec<&'a serde_json::Value> {
        if let Some(reference) = node.get("$ref").and_then(serde_json::Value::as_str) {
            let name = reference.rsplit('/').next().unwrap_or_default();
            return root
                .get("$defs")
                .and_then(|defs| defs.get(name))
                .map(|target| alternatives(root, target))
                .unwrap_or_default();
        }
        let mut found = vec![node];
        for combinator in ["anyOf", "oneOf", "allOf"] {
            if let Some(options) = node.get(combinator).and_then(serde_json::Value::as_array) {
                for option in options {
                    found.extend(alternatives(root, option));
                }
            }
        }
        found
    }

    fn step<'a>(
        root: &'a serde_json::Value,
        node: &'a serde_json::Value,
        segment: Segment,
    ) -> Option<&'a serde_json::Value> {
        alternatives(root, node)
            .into_iter()
            .find_map(|candidate| match segment {
                Segment::Key(key) => candidate.get("properties").and_then(|properties| properties.get(key)),
                Segment::AnyKey => candidate
                    .get("additionalProperties")
                    .filter(|schema| schema.is_object()),
                Segment::AnyIndex => candidate.get("items"),
            })
    }

    fn admits_string(root: &serde_json::Value, node: &serde_json::Value) -> bool {
        alternatives(root, node)
            .into_iter()
            .any(|candidate| match candidate.get("type") {
                Some(serde_json::Value::String(kind)) => kind == "string",
                Some(serde_json::Value::Array(kinds)) => kinds.iter().any(|kind| kind == "string"),
                _ => false,
            })
    }

    /// Every registered pattern names a real string slot of `Config`. A typo
    /// in the registry would otherwise silently protect nothing.
    #[test]
    fn every_registered_path_names_a_string_slot_of_the_config_schema() {
        let root = serde_json::to_value(schemars::schema_for!(crate::config::Config)).expect("test: schema serializes");
        // The JSON schema lists canonical names only; aliases have their
        // own guard below.
        for secret in SECRET_PATHS.iter().filter(|secret| secret.alias_of.is_none()) {
            let leaf = secret
                .segments
                .iter()
                .try_fold(&root, |node, segment| step(&root, node, *segment));
            let leaf = leaf.unwrap_or_else(|| panic!("registered path {} is not in the Config schema", secret.pattern));
            assert!(
                admits_string(&root, leaf),
                "registered path {} does not hold a string",
                secret.pattern
            );
        }
    }

    /// The fixture really covers the registry: each registered pattern has at
    /// least one sentinel instance, and every sentinel sits at a secret path.
    #[test]
    fn fixture_holds_a_sentinel_at_every_registered_path() {
        let sentinels = sentinel_paths();
        // One fixture cannot hold a field and its alias at once.
        for secret in SECRET_PATHS.iter().filter(|secret| secret.alias_of.is_none()) {
            assert!(
                sentinels.iter().any(|path| pattern_matches(secret.segments, path)),
                "the fixture has no sentinel at {}",
                secret.pattern
            );
        }
        for path in &sentinels {
            assert!(
                registered(path).is_some(),
                "sentinel at {} is not registered",
                display_path(path)
            );
        }
    }

    /// Guard: the registry's encrypted set is exactly what `Config::save`
    /// encrypts, through the real save path on a temp config dir.
    #[tokio::test]
    async fn registry_encrypted_set_is_exactly_what_save_encrypts() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_in(dir.path());
        config.save().await.expect("test: save fixture");

        let stored: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config.config_path).expect("test: read stored config"))
                .expect("test: stored config parses");
        let store = crate::security::SecretStore::new(dir.path(), true);

        let encrypted_on_disk = fixture::string_leaves(&stored, &crate::security::SecretStore::is_encrypted);
        let expected_encrypted: BTreeSet<Vec<PathElem>> = sentinel_paths()
            .into_iter()
            .filter(|path| registered(path).is_some_and(|secret| secret.handling == SecretHandling::EncryptedAtRest))
            .collect();
        assert_eq!(
            encrypted_on_disk
                .iter()
                .map(|path| display_path(path))
                .collect::<Vec<_>>(),
            expected_encrypted
                .iter()
                .map(|path| display_path(path))
                .collect::<Vec<_>>(),
            "save must encrypt exactly the registered encrypted-at-rest paths"
        );

        let fixture = fixture_tree();
        for path in &expected_encrypted {
            let stored_value = lookup(&stored, path).expect("test: encrypted path stored");
            let plaintext = lookup(&fixture, path).expect("test: fixture value");
            assert_eq!(
                store.decrypt(stored_value).expect("test: decrypt stored value"),
                plaintext,
                "{} must decrypt to what was saved",
                display_path(path)
            );
        }
    }

    fn lookup<'a>(root: &'a toml::Value, path: &[PathElem]) -> Option<&'a str> {
        path.iter()
            .try_fold(root, |node, elem| match elem {
                PathElem::Key(key) => node.get(key.as_str()),
                PathElem::Index(index) => node.get(*index),
            })
            .and_then(toml::Value::as_str)
    }

    /// `prx config show` renders the stored form with every registered path
    /// masked and no sentinel left.
    #[test]
    fn config_show_rendering_masks_every_registered_path() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_in(dir.path());
        let rendered = redacted_stored_toml(&config).expect("test: render");
        let text = toml::to_string_pretty(&rendered).expect("test: serialize rendering");
        assert!(
            !text.contains(fixture::SENTINEL_PREFIX),
            "config show leaked a sentinel:\n{text}"
        );

        let masked = fixture::masked_paths(&rendered);
        for path in sentinel_paths() {
            assert!(
                masked.contains(&display_path(&path)),
                "{} is not masked by config show",
                display_path(&path)
            );
        }
    }

    // ── SECRETS-OTHER: every registered secret is encrypted at rest ──────

    /// RED before SECRETS-OTHER: with `secrets.encrypt = true`, channel and
    /// bot tokens, paired tokens, MCP environment values, webhook, tunnel,
    /// daemon and node credentials were stored as plaintext. After: every
    /// sentinel of the fixture is `enc2:` on disk, and the load restores it.
    #[tokio::test]
    async fn save_encrypts_every_registered_secret_and_load_restores_it() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_in(dir.path());
        config.save().await.expect("test: save fixture");
        let (raw, stored) = stored_tree(&config);
        assert!(
            !raw.contains(fixture::SENTINEL_PREFIX),
            "a secret is stored as plaintext:\n{raw}"
        );
        for path in sentinel_paths() {
            let value = lookup(&stored, &path).unwrap_or_else(|| panic!("{} is not stored", display_path(&path)));
            assert!(
                crate::security::SecretStore::is_secure_encrypted(value),
                "{} is not enc2: on disk",
                display_path(&path)
            );
        }

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load saved fixture");
        let loaded = toml::Value::try_from(&loaded).expect("test: render loaded");
        let fixture = fixture_tree();
        for path in sentinel_paths() {
            assert_eq!(
                lookup(&loaded, &path),
                lookup(&fixture, &path),
                "{} must load back as written",
                display_path(&path)
            );
        }
    }

    /// Migration (D-018 Q4, no forced migration): a file an older release
    /// wrote holds channel tokens, paired tokens and MCP environment values
    /// as plaintext. They load as they are and are encrypted on the next
    /// save; the values survive every step.
    #[tokio::test]
    async fn plaintext_channel_and_gateway_secrets_from_an_older_release_are_encrypted_on_next_save() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "default_temperature = 0.7\n\n[gateway]\npaired_tokens = [\"sentinel-old-paired\"]\n\n\
             [channels_config]\ncli = true\n\n[channels_config.telegram]\nbot_token = \"sentinel-old-telegram\"\nallowed_users = []\n\n\
             [mcp.servers.local]\ncommand = \"mcp-local\"\nenv = { GITHUB_PAT = \"sentinel-old-mcp\" }\n",
        )
        .expect("test: write legacy config");

        let check = |config: &crate::config::Config| {
            assert_eq!(config.gateway.paired_tokens, vec!["sentinel-old-paired".to_string()]);
            assert_eq!(
                config
                    .channels_config
                    .telegram
                    .as_ref()
                    .map(|telegram| telegram.bot_token.as_str()),
                Some("sentinel-old-telegram")
            );
            assert_eq!(
                config
                    .mcp
                    .servers
                    .get("local")
                    .and_then(|server| server.env.get("GITHUB_PAT"))
                    .map(String::as_str),
                Some("sentinel-old-mcp")
            );
        };
        let loaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .unwrap_or_else(|error| panic!("test: a plaintext legacy file must still load: {error:#}"));
        check(&loaded);
        loaded.save().await.expect("test: next save");
        let (raw, _) = stored_tree(&loaded);
        assert!(
            !raw.contains(fixture::SENTINEL_PREFIX),
            "a secret survived the next save as plaintext:\n{raw}"
        );
        let reloaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: reload re-encrypted config");
        check(&reloaded);
    }

    /// Guard (downgrade path, D-018 Q4): `secrets.encrypt = false` stores
    /// every registered secret as plaintext, so an older release can read
    /// the file again.
    #[tokio::test]
    async fn disabled_encryption_keeps_every_registered_secret_plaintext() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let mut config = fixture::config_in(dir.path());
        config.secrets.encrypt = false;
        config.save().await.expect("test: save fixture");
        let (_, stored) = stored_tree(&config);
        assert!(fixture::string_leaves(&stored, &crate::security::SecretStore::is_encrypted).is_empty());
        let fixture = fixture_tree();
        for path in sentinel_paths() {
            assert_eq!(
                lookup(&stored, &path),
                lookup(&fixture, &path),
                "{}",
                display_path(&path)
            );
        }
    }

    // ── A-2: every provider credential slot is encrypted at rest ──────────

    const ROUTE_SECRET: &str = "sentinel-route-secret";
    const EMBEDDING_SECRET: &str = "sentinel-embedding-secret";
    const ROTATION_SECRETS: [&str; 2] = ["sentinel-rotation-a", "sentinel-rotation-b"];

    fn provider_slots_config(dir: &std::path::Path, route_provider: &str) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.config_path = dir.join("config.toml");
        config.workspace_dir = dir.join("workspace");
        config.model_routes = vec![crate::config::ModelRouteConfig {
            hint: "fast".into(),
            provider: route_provider.into(),
            model: "route-model".into(),
            api_key: Some(ROUTE_SECRET.into()),
            credential_ref: None,
        }];
        config.embedding_routes = vec![crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: "text-embedding-3-small".into(),
            dimensions: None,
            api_key: Some(EMBEDDING_SECRET.into()),
            credential_ref: None,
        }];
        config.reliability.api_keys = ROTATION_SECRETS.iter().map(|key| (*key).to_string()).collect();
        config
    }

    fn stored_tree(config: &crate::config::Config) -> (String, toml::Value) {
        let raw = std::fs::read_to_string(&config.config_path).expect("test: read stored config");
        let tree = toml::from_str(&raw).expect("test: stored config parses");
        (raw, tree)
    }

    fn stored_provider_slots(tree: &toml::Value) -> Vec<String> {
        let mut slots = Vec::new();
        for (section, key) in [("model_routes", "api_key"), ("embedding_routes", "api_key")] {
            if let Some(routes) = tree.get(section).and_then(toml::Value::as_array) {
                slots.extend(
                    routes
                        .iter()
                        .filter_map(|route| route.get(key).and_then(toml::Value::as_str).map(str::to_string)),
                );
            }
        }
        if let Some(keys) = tree
            .get("reliability")
            .and_then(|reliability| reliability.get("api_keys"))
            .and_then(toml::Value::as_array)
        {
            slots.extend(keys.iter().filter_map(toml::Value::as_str).map(str::to_string));
        }
        slots
    }

    /// RED before A-2: `Config::save` wrote route, embedding-route and
    /// rotation keys to disk as plaintext on every write path.
    #[tokio::test]
    async fn save_encrypts_route_embedding_and_rotation_keys() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = provider_slots_config(dir.path(), "openrouter");
        config.save().await.expect("test: save");

        let (raw, tree) = stored_tree(&config);
        for secret in [ROUTE_SECRET, EMBEDDING_SECRET, ROTATION_SECRETS[0], ROTATION_SECRETS[1]] {
            assert!(!raw.contains(secret), "{secret} is stored as plaintext:\n{raw}");
        }
        let stored = stored_provider_slots(&tree);
        assert_eq!(stored.len(), 4, "every slot must still be stored: {stored:?}");
        assert!(
            stored
                .iter()
                .all(|value| crate::security::SecretStore::is_secure_encrypted(value)),
            "every provider slot must be enc2: {stored:?}"
        );

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load saved config");
        assert_eq!(loaded.model_routes[0].api_key.as_deref(), Some(ROUTE_SECRET));
        assert_eq!(loaded.embedding_routes[0].api_key.as_deref(), Some(EMBEDDING_SECRET));
        assert_eq!(loaded.reliability.api_keys, ROTATION_SECRETS.to_vec());
    }

    /// Migration (D-018 Q4): a file an older release wrote holds these keys
    /// as plaintext. The new binary reads them as they are and encrypts them
    /// on the next save; the values survive every step.
    #[tokio::test]
    async fn plaintext_provider_slots_from_an_older_release_load_and_are_encrypted_on_next_save() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config_path = dir.path().join("config.toml");
        let legacy = format!(
            "default_temperature = 0.7\n\n[[model_routes]]\nhint = \"fast\"\nprovider = \"openrouter\"\nmodel = \"route-model\"\napi_key = \"{ROUTE_SECRET}\"\n\n[[embedding_routes]]\nhint = \"semantic\"\nprovider = \"openai\"\nmodel = \"text-embedding-3-small\"\napi_key = \"{EMBEDDING_SECRET}\"\n\n[reliability]\napi_keys = [\"{}\", \"{}\"]\n",
            ROTATION_SECRETS[0], ROTATION_SECRETS[1]
        );
        std::fs::write(&config_path, legacy).expect("test: write legacy config");

        let loaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: a plaintext legacy file must still load");
        assert_eq!(loaded.model_routes[0].api_key.as_deref(), Some(ROUTE_SECRET));
        assert_eq!(loaded.embedding_routes[0].api_key.as_deref(), Some(EMBEDDING_SECRET));
        assert_eq!(loaded.reliability.api_keys, ROTATION_SECRETS.to_vec());

        loaded.save().await.expect("test: next save");
        let (raw, tree) = stored_tree(&loaded);
        for secret in [ROUTE_SECRET, EMBEDDING_SECRET, ROTATION_SECRETS[0], ROTATION_SECRETS[1]] {
            assert!(!raw.contains(secret), "{secret} survived the next save as plaintext");
        }
        assert!(
            stored_provider_slots(&tree)
                .iter()
                .all(|value| crate::security::SecretStore::is_secure_encrypted(value))
        );

        let reloaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: reload re-encrypted config");
        assert_eq!(reloaded.model_routes[0].api_key.as_deref(), Some(ROUTE_SECRET));
        assert_eq!(reloaded.embedding_routes[0].api_key.as_deref(), Some(EMBEDDING_SECRET));
        assert_eq!(reloaded.reliability.api_keys, ROTATION_SECRETS.to_vec());
    }

    /// `secrets.encrypt = false` keeps every slot as plaintext, as before.
    #[tokio::test]
    async fn disabled_encryption_keeps_provider_slots_plaintext() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let mut config = provider_slots_config(dir.path(), "openrouter");
        config.secrets.encrypt = false;
        config.save().await.expect("test: save");
        let (_, tree) = stored_tree(&config);
        assert_eq!(
            stored_provider_slots(&tree),
            vec![
                ROUTE_SECRET.to_string(),
                EMBEDDING_SECRET.to_string(),
                ROTATION_SECRETS[0].to_string(),
                ROTATION_SECRETS[1].to_string()
            ]
        );
    }

    /// A legacy XOR `enc:` route key is decrypted on load and upgraded to
    /// `enc2:` by the startup migration, like the other encrypted slots.
    /// RED before A-2: the route key was neither decrypted nor migrated, so
    /// the literal `enc:` string was the route's bearer credential.
    #[tokio::test]
    async fn legacy_xor_route_key_is_decrypted_and_migrated() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let store = crate::security::SecretStore::new(dir.path(), true);
        let legacy_value = legacy_xor(dir.path(), ROUTE_SECRET);
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "default_temperature = 0.7\n\n[[model_routes]]\nhint = \"fast\"\nprovider = \"openrouter\"\nmodel = \"route-model\"\napi_key = \"{legacy_value}\"\n"
            ),
        )
        .expect("test: write legacy config");

        let loaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: load legacy route key");
        assert_eq!(loaded.model_routes[0].api_key.as_deref(), Some(ROUTE_SECRET));

        crate::config::schema::migrate_config_legacy_secrets(&loaded)
            .await
            .expect("test: legacy migration");
        let (_, tree) = stored_tree(&loaded);
        let stored = stored_provider_slots(&tree);
        assert_eq!(stored.len(), 1);
        assert!(
            crate::security::SecretStore::is_secure_encrypted(&stored[0]),
            "{stored:?}"
        );
        assert_eq!(store.decrypt(&stored[0]).expect("test: decrypt migrated"), ROUTE_SECRET);
    }

    /// RED before PMMH-f (D-101 F-5): with `secrets.encrypt = false` the
    /// startup upgrade rewrote a legacy `enc:` value as plaintext. It is
    /// left as it is now (it keeps loading).
    #[tokio::test]
    async fn a_legacy_value_is_never_rewritten_as_plaintext() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let legacy_value = legacy_xor(dir.path(), ROUTE_SECRET);
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "default_temperature = 0.7\n\n[secrets]\nencrypt = false\n\n[[model_routes]]\nhint = \"fast\"\nprovider = \"openrouter\"\nmodel = \"route-model\"\napi_key = \"{legacy_value}\"\n"
            ),
        )
        .expect("test: write legacy config");
        let before = std::fs::read(&config_path).expect("test: read");

        let loaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: load legacy route key");
        crate::config::schema::migrate_config_legacy_secrets(&loaded)
            .await
            .expect("test: legacy migration");

        let after = std::fs::read(&config_path).expect("test: read");
        assert!(
            !String::from_utf8_lossy(&after).contains(ROUTE_SECRET),
            "the legacy value was rewritten as plaintext"
        );
        assert_eq!(after, before);
        assert_eq!(
            loaded.model_routes.first().and_then(|route| route.api_key.as_deref()),
            Some(ROUTE_SECRET)
        );
    }

    /// A loopback OpenAI-compatible endpoint that answers every chat request
    /// with "ok" and reports each request's `Authorization` header.
    async fn spawn_authorization_recorder() -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback");
        let addr = listener.local_addr().expect("test: loopback addr");
        let (seen_tx, seen_rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                let head_end = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break None,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break Some(position + 4);
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                let authorization = head.split("\r\n").find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
                        .map(|(_, value)| value.trim().to_string())
                });
                let content_length = head
                    .split("\r\n")
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body_read = buffer.len() - head_end;
                while body_read < content_length {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => body_read += read,
                    }
                }
                let _ = seen_tx.send(authorization);
                let body = r#"{"id":"c","object":"chat.completion","created":1,"model":"route-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        (addr, seen_rx, server)
    }

    fn isolated_provider_options(dir: &std::path::Path) -> crate::providers::ProviderRuntimeOptions {
        crate::providers::ProviderRuntimeOptions {
            openprx_dir: Some(dir.to_path_buf()),
            codex_auth_json_path: Some(dir.join("absent-codex-auth.json")),
            codex_auth_json_auto_import: false,
            ..crate::providers::ProviderRuntimeOptions::default()
        }
    }

    /// The legacy XOR `enc:` form of `plaintext` under the key in `dir`.
    fn legacy_xor(dir: &std::path::Path, plaintext: &str) -> String {
        let store = crate::security::SecretStore::new(dir, true);
        store.encrypt("prime-the-key").expect("test: create key file");
        let key_hex = std::fs::read_to_string(dir.join(".secret_key")).expect("test: key file");
        let key_hex = key_hex.trim();
        let key = (0..key_hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&key_hex[index..index + 2], 16).expect("test: key hex"))
            .collect::<Vec<u8>>();
        format!(
            "enc:{}",
            plaintext
                .bytes()
                .enumerate()
                .map(|(index, byte)| format!("{:02x}", byte ^ key[index % key.len()]))
                .collect::<String>()
        )
    }

    /// F10: on the default split layout the route keys live in
    /// `config.d/routing.toml`. A legacy `enc:` route key there is decrypted
    /// on load (the merged tree is decrypted, fragments included), the routed
    /// provider receives the real key, and the next structured save rewrites
    /// the fragment as `enc2:`. The startup migration still rewrites
    /// `config.toml` only.
    #[tokio::test]
    async fn legacy_route_key_in_a_fragment_reaches_the_provider_and_is_upgraded_on_next_save() {
        let (addr, mut seen_rx, server) = spawn_authorization_recorder().await;
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let route_provider = format!("custom:http://{addr}/v1");
        let mut config = provider_slots_config(dir.path(), &route_provider);
        config.reliability.api_keys = Vec::new();
        config.embedding_routes = Vec::new();
        crate::config::files::write_split_config(&config, false)
            .await
            .expect("test: write split layout");
        let fragment = crate::config::files::config_dir_path(&config.config_path).join("routing.toml");
        let stored = std::fs::read_to_string(&fragment).expect("test: routing fragment");
        assert!(
            !std::fs::read_to_string(&config.config_path)
                .expect("test: main")
                .contains("model_routes")
        );

        // Replace the encrypted route key with its legacy XOR form.
        let mut tree: toml::Value = toml::from_str(&stored).expect("test: fragment parses");
        let slot = tree
            .get_mut("model_routes")
            .and_then(|routes| routes.get_mut(0))
            .and_then(|route| route.get_mut("api_key"))
            .expect("test: route key in the fragment");
        *slot = toml::Value::String(legacy_xor(dir.path(), ROUTE_SECRET));
        std::fs::write(&fragment, toml::to_string_pretty(&tree).expect("test: render")).expect("test: write fragment");

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load split layout");
        assert_eq!(
            loaded.model_routes.first().and_then(|r| r.api_key.as_deref()),
            Some(ROUTE_SECRET)
        );

        let mut runtime_config = loaded.clone();
        runtime_config.default_provider = Some(route_provider.clone());
        runtime_config.default_model = Some("route-model".to_string());
        runtime_config.reliability.provider_retries = 0;
        runtime_config.reliability.fallback_providers = Vec::new();
        let provider =
            crate::providers::create_provider_from_test_config(&runtime_config, &isolated_provider_options(dir.path()))
                .expect("test: routed provider");
        let reply = provider.chat_with_system(None, "ping", "hint:fast", 0.0).await;
        assert_eq!(reply.as_deref().ok(), Some("ok"), "unexpected reply: {reply:?}");
        assert_eq!(
            seen_rx.recv().await.flatten().as_deref(),
            Some(format!("Bearer {ROUTE_SECRET}").as_str())
        );

        loaded.save().await.expect("test: next save");
        let upgraded = std::fs::read_to_string(&fragment).expect("test: fragment after save");
        let upgraded: toml::Value = toml::from_str(&upgraded).expect("test: parses");
        let key = upgraded
            .get("model_routes")
            .and_then(|routes| routes.get(0))
            .and_then(|route| route.get("api_key"))
            .and_then(toml::Value::as_str)
            .expect("test: route key after save");
        assert!(
            crate::security::SecretStore::is_secure_encrypted(key),
            "the next save must upgrade to enc2:"
        );
        server.abort();
    }

    /// RED before PMM-H (PMMA F-10): the startup migration upgraded legacy
    /// `enc:` values in `config.toml` only; one in a `config.d` fragment
    /// stayed XOR-encrypted on disk until a later save. It is now upgraded
    /// at startup, and every other file is left byte for byte.
    #[tokio::test]
    async fn legacy_route_key_in_a_fragment_is_upgraded_at_startup() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let mut config = provider_slots_config(dir.path(), "openrouter");
        config.reliability.api_keys = Vec::new();
        config.embedding_routes = Vec::new();
        crate::config::files::write_split_config(&config, false)
            .await
            .expect("test: write split layout");
        let fragment = crate::config::files::config_dir_path(&config.config_path).join("routing.toml");
        let stored = std::fs::read_to_string(&fragment).expect("test: routing fragment");
        let mut tree: toml::Value = toml::from_str(&stored).expect("test: fragment parses");
        let slot = tree
            .get_mut("model_routes")
            .and_then(|routes| routes.get_mut(0))
            .and_then(|route| route.get_mut("api_key"))
            .expect("test: route key in the fragment");
        *slot = toml::Value::String(legacy_xor(dir.path(), ROUTE_SECRET));
        std::fs::write(&fragment, toml::to_string_pretty(&tree).expect("test: render")).expect("test: write fragment");
        let main_before = std::fs::read(&config.config_path).expect("test: main");

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load split layout");
        crate::config::schema::migrate_config_legacy_secrets(&loaded)
            .await
            .expect("test: startup migration");

        let upgraded: toml::Value =
            toml::from_str(&std::fs::read_to_string(&fragment).expect("test: fragment")).expect("test: parses");
        let key = upgraded
            .get("model_routes")
            .and_then(|routes| routes.get(0))
            .and_then(|route| route.get("api_key"))
            .and_then(toml::Value::as_str)
            .expect("test: route key");
        assert!(
            crate::security::SecretStore::is_secure_encrypted(key),
            "the fragment still holds a legacy value: {key}"
        );
        let store = crate::security::SecretStore::new(dir.path(), true);
        assert_eq!(store.decrypt(key).expect("test: decrypt"), ROUTE_SECRET);
        assert_eq!(std::fs::read(&config.config_path).expect("test: main"), main_before);
        // Nothing left to migrate: a second run writes nothing.
        let fragment_after = std::fs::read(&fragment).expect("test: fragment");
        crate::config::schema::migrate_config_legacy_secrets(&loaded)
            .await
            .expect("test: second run");
        assert_eq!(std::fs::read(&fragment).expect("test: fragment"), fragment_after);
    }

    /// PMM-H: a route that names a credential reference instead of an
    /// inline key sends the referenced key to its own provider, and a
    /// reference issued for another provider is never sent.
    #[tokio::test]
    async fn a_route_credential_ref_reaches_its_owner_provider_only() {
        let (addr, mut seen_rx, server) = spawn_authorization_recorder().await;
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let route_provider = format!("custom:http://{addr}/v1");
        let mut config = provider_slots_config(dir.path(), &route_provider);
        config.reliability.api_keys = Vec::new();
        config.embedding_routes = Vec::new();
        let references =
            crate::config::manage::credentials::CredentialReferences::for_config(&config).expect("test: references");
        references
            .set(&route_provider, "route", ROUTE_SECRET)
            .expect("test: reference for the route's provider");
        references
            .set("anthropic", "other", "sentinel-other-provider-secret")
            .expect("test: reference for another provider");
        config.model_routes = vec![
            crate::config::ModelRouteConfig {
                hint: "fast".into(),
                provider: route_provider.clone(),
                model: "route-model".into(),
                api_key: None,
                credential_ref: Some("cred-route".into()),
            },
            crate::config::ModelRouteConfig {
                hint: "wrong".into(),
                provider: route_provider.clone(),
                model: "route-model".into(),
                api_key: None,
                credential_ref: Some("cred-other".into()),
            },
        ];
        config.save().await.expect("test: save");
        let stored = std::fs::read_to_string(&config.config_path).expect("test: stored");
        assert!(stored.contains("credential_ref = \"cred-route\""), "{stored}");
        assert!(!stored.contains(ROUTE_SECRET), "{stored}");

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load");
        let routes = loaded.model_routes_with_references();
        assert_eq!(routes[0].api_key.as_deref(), Some(ROUTE_SECRET));
        assert_eq!(routes[1].api_key, None, "another provider's reference was released");

        let mut runtime_config = loaded.clone();
        runtime_config.default_provider = Some(route_provider.clone());
        runtime_config.default_model = Some("route-model".to_string());
        runtime_config.model_routes = routes.to_vec();
        runtime_config.reliability.provider_retries = 0;
        runtime_config.reliability.fallback_providers = Vec::new();
        let provider =
            crate::providers::create_provider_from_test_config(&runtime_config, &isolated_provider_options(dir.path()))
                .expect("test: routed provider");
        let reply = provider.chat_with_system(None, "ping", "hint:fast", 0.0).await;
        assert_eq!(reply.as_deref().ok(), Some("ok"), "unexpected reply: {reply:?}");
        assert_eq!(
            seen_rx.recv().await.flatten().as_deref(),
            Some(format!("Bearer {ROUTE_SECRET}").as_str())
        );
        server.abort();
    }

    /// The route credential reaches its provider as plaintext after the
    /// encrypted round trip: loopback `Authorization` equals the sentinel.
    #[tokio::test]
    async fn routed_provider_built_from_the_loaded_config_presents_the_plaintext_route_key() {
        let (addr, mut seen_rx, server) = spawn_authorization_recorder().await;

        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let route_provider = format!("custom:http://{addr}/v1");
        let mut config = provider_slots_config(dir.path(), &route_provider);
        config.reliability.api_keys = Vec::new();
        config.save().await.expect("test: save");
        let (raw, _) = stored_tree(&config);
        assert!(!raw.contains(ROUTE_SECRET), "the route key must be stored encrypted");

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load");
        let mut runtime_config = loaded.clone();
        runtime_config.default_provider = Some(route_provider.clone());
        runtime_config.default_model = Some("route-model".to_string());
        runtime_config.reliability.provider_retries = 0;
        runtime_config.reliability.fallback_providers = Vec::new();
        let options = isolated_provider_options(dir.path());
        let provider = crate::providers::create_provider_from_test_config(&runtime_config, &options)
            .expect("test: routed provider");
        let reply = provider.chat_with_system(None, "ping", "hint:fast", 0.0).await;
        assert_eq!(reply.as_deref().ok(), Some("ok"), "unexpected reply: {reply:?}");
        assert_eq!(
            seen_rx.recv().await.flatten().as_deref(),
            Some(format!("Bearer {ROUTE_SECRET}").as_str())
        );
        server.abort();
    }

    /// The scan names plaintext at every registered encrypted slot, the
    /// elements of `reliability.api_keys` and `gateway.paired_tokens`
    /// included, and never flags ciphertext.
    #[test]
    fn scan_finds_plaintext_at_encrypted_slots_and_masks_anywhere() {
        let tree: toml::Value = toml::from_str(
            r#"
api_key = "enc2:00"
[reliability]
api_keys = ["enc2:00", "sentinel-rotation"]
[[model_routes]]
hint = "fast"
provider = "p"
model = "m"
api_key = "sentinel-route"
[gateway]
paired_tokens = ["sentinel-paired"]
[channels_config.telegram]
bot_token = "***"
"#,
        )
        .expect("test: parse");
        let mut found = scan(&tree);
        found.plaintext_at_rest.sort();
        assert_eq!(
            found.plaintext_at_rest,
            vec![
                "gateway.paired_tokens[0]".to_string(),
                "model_routes[0].api_key".to_string(),
                "reliability.api_keys[1]".to_string()
            ]
        );
        assert_eq!(found.masked, vec!["channels_config.telegram.bot_token".to_string()]);
        assert!(found.has_secret_values());
        assert_eq!(found.scrub("x sentinel-route y sentinel-paired"), "x *** y ***");
    }

    /// F1b, RED before PMMA-f: decryption ran on the stored tree before
    /// serde mapped the alias, and the alias was not registered, so an
    /// encrypted `db_url` written under an alias reached the storage backend
    /// as the literal ciphertext.
    #[test]
    fn encrypted_db_url_under_every_alias_decrypts_on_load() {
        for alias in ["db_url", "dbURL", "database_url", "databaseUrl"] {
            let dir = tempfile::TempDir::new().expect("test: temp config dir");
            let store = crate::security::SecretStore::new(dir.path(), true);
            let secret = "postgres://u:sentinel-db-alias@h/db";
            let encrypted = store.encrypt(secret).expect("test: encrypt");
            let config_path = dir.path().join("config.toml");
            std::fs::write(
                &config_path,
                format!("default_temperature = 0.7\n\n[storage.provider.config]\n{alias} = \"{encrypted}\"\n"),
            )
            .expect("test: write config");
            let loaded =
                crate::config::Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
            assert_eq!(
                loaded.storage.provider.config.db_url.as_deref(),
                Some(secret),
                "{alias}"
            );
        }
    }

    /// D-049 A: every inbound-trust setting names a real slot of the
    /// `Config` schema (of any type), is documented with where it is
    /// enforced, and is listed once. A typo would silently classify nothing.
    #[test]
    fn every_access_control_path_names_a_slot_of_the_config_schema() {
        let root = serde_json::to_value(schemars::schema_for!(crate::config::Config)).expect("test: schema serializes");
        let mut seen = BTreeSet::new();
        for entry in ACCESS_CONTROL_PATHS {
            assert!(seen.insert(entry.pattern), "{} is listed twice", entry.pattern);
            assert!(
                !entry.enforced_at.is_empty(),
                "{} names no enforcement site",
                entry.pattern
            );
            assert!(
                entry
                    .segments
                    .iter()
                    .try_fold(&root, |node, segment| step(&root, node, *segment))
                    .is_some(),
                "access-control path {} is not in the Config schema",
                entry.pattern
            );
            // None of them is a secret: they are public trust material or
            // switches, so they are not encrypted, only classified.
            assert!(
                !SECRET_PATHS.iter().any(|secret| secret.segments == entry.segments),
                "{} is registered as a secret too",
                entry.pattern
            );
        }
        for listed in [
            "gateway.require_pairing",
            "gateway.trust_forwarded_headers",
            "mcp_server.require_auth",
            "mcp_server.jwt_public_key_pem",
            "mcp_server.jwt_jwks",
            "mcp_server.jwt_jwks_uri",
            "mcp_server.jwt_issuer",
            "mcp_server.jwt_audience",
            "mcp_server.jwt_algorithms",
            "a2a.allowed_peer_issuers",
            "a2a.trusted_trust_domains",
            "a2a.spiffe_trust_bundle_pem",
            "a2a.spiffe_strict_validation",
        ] {
            assert!(seen.contains(listed), "D-049 names {listed}");
        }
        // Paths under a setting count as the setting.
        fn path(elems: &[&str]) -> Vec<PathElem> {
            elems.iter().map(|key| PathElem::Key((*key).to_string())).collect()
        }
        let mut element = path(&["channels_config", "telegram", "allowed_users"]);
        element.push(PathElem::Index(3));
        assert!(is_access_control_path(&element));
        assert!(is_access_control_path(&path(&["gateway", "require_pairing"])));
        assert!(!is_access_control_path(&path(&["gateway", "port"])));
        assert!(!is_access_control_path(&path(&[
            "channels_config",
            "telegram",
            "bot_token"
        ])));
    }

    /// Every registered secret whose value admits a client (D-048), with what
    /// it admits. Reclassifying a path, or registering a new admission
    /// secret, has to change this table: the guard below compares it with
    /// the registry both ways.
    const ADMISSION_SECRETS: &[(&str, &str)] = &[
        ("gateway.paired_tokens[]", "Gateway API clients (bearer tokens)"),
        ("webhook.token", "senders of the standalone webhook"),
        ("webhook.signing_secret", "signed requests to the standalone webhook"),
        (
            "channels_config.webhook.secret",
            "signed requests to the webhook channel",
        ),
        (
            "channels_config.whatsapp.verify_token",
            "the WhatsApp Cloud API webhook handshake",
        ),
        (
            "channels_config.whatsapp.app_secret",
            "signed WhatsApp Cloud API webhook calls",
        ),
        ("channels_config.lark.verification_token", "Lark/Feishu event callbacks"),
        (
            "channels_config.lark.encrypt_key",
            "encrypted Lark/Feishu event callbacks",
        ),
        ("channels_config.linq.signing_secret", "signed Linq webhook calls"),
        (
            "channels_config.nextcloud_talk.webhook_secret",
            "signed Nextcloud Talk bot calls",
        ),
        ("channels_config.wacli.webhook_secret", "signed wacli webhook calls"),
        ("nodes.server.bearer_token", "`prx-node` RPC clients"),
        ("nodes.server.hmac_secret", "signed `prx-node` RPC requests"),
    ];

    /// D-048: the registry marks exactly the admission secrets as access
    /// grants; every other canonical path is an outbound credential or the
    /// MCP launch environment. An alias carries its canonical path's kind.
    #[test]
    fn exactly_the_admission_secrets_are_access_grants() {
        let expected: BTreeSet<&str> = ADMISSION_SECRETS.iter().map(|(pattern, _)| *pattern).collect();
        let registered: BTreeSet<&str> = SECRET_PATHS
            .iter()
            .filter(|secret| secret.alias_of.is_none() && secret.used_as == SecretUse::AccessGrant)
            .map(|secret| secret.pattern)
            .collect();
        assert_eq!(registered, expected);
        let environment: Vec<&str> = SECRET_PATHS
            .iter()
            .filter(|secret| secret.used_as == SecretUse::ProcessEnvironment)
            .map(|secret| secret.pattern)
            .collect();
        assert_eq!(environment, vec!["mcp.servers.<name>.env.<var>"]);
        // Outbound credentials PRX presents to others stay credentials.
        for outbound in [
            "channels_config.telegram.bot_token",
            "channels_config.whatsapp.access_token",
            "channels_config.linq.api_token",
            "nodes.nodes[].bearer_token",
            "nodes.nodes[].hmac_secret",
            "chat.daemon.token",
        ] {
            assert!(
                SECRET_PATHS
                    .iter()
                    .any(|secret| secret.pattern == outbound && secret.used_as == SecretUse::Credential),
                "{outbound} must stay an outbound credential"
            );
        }
    }

    /// Every registered alias is a real serde alias of its canonical path, so
    /// registering it protects a spelling the loader actually accepts. The
    /// fixture's sentinel at the canonical path is moved to the alias key and
    /// must land back in the canonical field.
    #[test]
    fn every_registered_alias_deserializes_into_its_canonical_field() {
        let sentinels = sentinel_paths();
        for secret in SECRET_PATHS.iter().filter(|secret| secret.alias_of.is_some()) {
            let Some(canonical) = secret.alias_of else { continue };
            let canonical_path = SECRET_PATHS
                .iter()
                .find(|other| {
                    other.pattern == canonical
                        && other.alias_of.is_none()
                        && other.handling == secret.handling
                        && other.used_as == secret.used_as
                })
                .unwrap_or_else(|| panic!("{} names an unregistered canonical path {canonical}", secret.pattern));
            let Some(Segment::Key(alias_key)) = secret.segments.last() else {
                panic!("alias {} must end in a key", secret.pattern);
            };
            let concrete = sentinels
                .iter()
                .find(|path| pattern_matches(canonical_path.segments, path))
                .unwrap_or_else(|| panic!("the fixture has no sentinel at {canonical}"));
            let Some((PathElem::Key(canonical_key), parent)) = concrete.split_last() else {
                panic!("{canonical} must end in a key");
            };
            // The default tree holds the canonical key too (a default value),
            // so the rename happens on the merged tree the loader would see.
            let mut tree = toml::Value::try_from(crate::config::Config::default()).expect("test: default tree");
            fixture::overlay(&mut tree, fixture_tree());
            let table = parent
                .iter()
                .try_fold(&mut tree, |node, elem| match elem {
                    PathElem::Key(key) => node.get_mut(key.as_str()),
                    PathElem::Index(index) => node.get_mut(*index),
                })
                .and_then(toml::Value::as_table_mut)
                .expect("test: parent table of the sentinel");
            let value = table.remove(canonical_key).expect("test: sentinel value");
            let expected = value.clone();
            table.insert((*alias_key).to_string(), value);

            let config: crate::config::Config = tree
                .try_into()
                .unwrap_or_else(|error| panic!("{} is not accepted by the loader: {error}", secret.pattern));
            let rendered = toml::Value::try_from(&config).expect("test: render config");
            assert_eq!(
                lookup(&rendered, concrete),
                expected.as_str(),
                "{} does not land in {canonical}",
                secret.pattern
            );
        }
    }

    /// The source files of the structs the `Config` schema is made of. A serde
    /// alias of a credential field in any of them must be registered.
    const SCHEMA_SOURCES: [(&str, &str); 2] = [
        ("config/schema.rs", include_str!("schema.rs")),
        (
            "channels/email_channel.rs",
            include_str!("../channels/email_channel.rs"),
        ),
    ];

    /// Every `(alias, field)` pair of a `#[serde(alias = "...")]` attribute in
    /// the schema sources, with the field it decorates.
    fn serde_aliases() -> Vec<(String, String, &'static str)> {
        let mut found = Vec::new();
        for (file, source) in SCHEMA_SOURCES {
            let lines: Vec<&str> = source.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if !trimmed.starts_with("#[serde(") || !trimmed.contains("alias = \"") {
                    continue;
                }
                let Some(field) = lines.iter().skip(index + 1).take(6).find_map(|next| {
                    next.trim_start()
                        .strip_prefix("pub ")
                        .and_then(|rest| rest.split_once(':'))
                        .map(|(name, _)| name.trim().to_string())
                }) else {
                    continue;
                };
                for piece in trimmed.split("alias = \"").skip(1) {
                    if let Some((alias, _)) = piece.split_once('"') {
                        found.push((alias.to_string(), field.clone(), file));
                    }
                }
            }
        }
        found
    }

    /// a39 (R-PMMAf R1): a serde alias of a secret field is a second spelling
    /// the loader accepts. Unless it is registered with `alias_of`, a value
    /// written under it is not encrypted, not decrypted when encrypted, and
    /// not refused as plaintext. RED before SECRETS-OTHER: the node
    /// `bearer_token` aliases `token` were not registered.
    #[test]
    fn every_serde_alias_of_a_secret_field_is_registered() {
        let aliases = serde_aliases();
        assert!(
            aliases
                .iter()
                .any(|(alias, field, _)| alias == "dbURL" && field == "db_url"),
            "the alias scan must see the db_url aliases: {aliases:?}"
        );
        let canonical_names: BTreeSet<&str> = SECRET_PATHS
            .iter()
            .filter(|secret| secret.alias_of.is_none())
            .filter_map(|secret| match secret.segments.last() {
                Some(Segment::Key(key)) => Some(*key),
                _ => None,
            })
            .collect();
        let mut missing = Vec::new();
        for (alias, field, file) in &aliases {
            if !(is_secret_key_name(field) || canonical_names.contains(field.as_str())) {
                continue;
            }
            let occurrences = aliases
                .iter()
                .filter(|(other_alias, other_field, _)| other_alias == alias && other_field == field)
                .count();
            let registered = SECRET_PATHS
                .iter()
                .filter(|secret| {
                    matches!(secret.segments.last(), Some(Segment::Key(key)) if key == alias)
                        && secret
                            .alias_of
                            .is_some_and(|canonical| canonical.rsplit('.').next() == Some(field.as_str()))
                })
                .count();
            if registered < occurrences {
                missing.push(format!("{file}: alias `{alias}` of `{field}`"));
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "serde aliases of secret fields without an alias_of row: {missing:#?}"
        );
    }

    /// One step of a schema slot, owned (the schema's keys are not static).
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    enum Step {
        Key(String),
        AnyKey,
        AnyIndex,
    }

    fn display_steps(steps: &[Step]) -> String {
        let mut rendered = String::new();
        for step in steps {
            match step {
                Step::Key(key) => {
                    if !rendered.is_empty() {
                        rendered.push('.');
                    }
                    rendered.push_str(key);
                }
                Step::AnyKey => rendered.push_str(".<*>"),
                Step::AnyIndex => rendered.push_str("[]"),
            }
        }
        rendered
    }

    fn steps_match(segments: &[Segment], steps: &[Step]) -> bool {
        segments.len() == steps.len()
            && segments.iter().zip(steps).all(|(segment, step)| match (segment, step) {
                (Segment::Key(expected), Step::Key(actual)) => expected == actual,
                (Segment::AnyKey, Step::AnyKey) | (Segment::AnyIndex, Step::AnyIndex) => true,
                _ => false,
            })
    }

    /// Every slot of the `Config` schema that admits a string, as a pattern.
    fn schema_string_slots() -> BTreeSet<Vec<Step>> {
        fn walk(
            root: &serde_json::Value,
            node: &serde_json::Value,
            path: &mut Vec<Step>,
            out: &mut BTreeSet<Vec<Step>>,
        ) {
            // Recursive schema types cannot nest this deep in a real config.
            if path.len() > 16 {
                return;
            }
            if !path.is_empty() && admits_string(root, node) {
                out.insert(path.clone());
            }
            for candidate in alternatives(root, node) {
                if let Some(properties) = candidate.get("properties").and_then(serde_json::Value::as_object) {
                    for (key, child) in properties {
                        path.push(Step::Key(key.clone()));
                        walk(root, child, path, out);
                        path.pop();
                    }
                }
                if let Some(child) = candidate
                    .get("additionalProperties")
                    .filter(|schema| schema.is_object())
                {
                    path.push(Step::AnyKey);
                    walk(root, child, path, out);
                    path.pop();
                }
                if let Some(child) = candidate.get("items").filter(|schema| schema.is_object()) {
                    path.push(Step::AnyIndex);
                    walk(root, child, path, out);
                    path.pop();
                }
            }
        }
        let root = serde_json::to_value(schemars::schema_for!(crate::config::Config)).expect("test: schema serializes");
        let mut out = BTreeSet::new();
        walk(&root, &root, &mut Vec::new(), &mut out);
        out
    }

    /// The field name of a slot: its last key, looking through list indices
    /// (`reliability.api_keys[]` is named `api_keys`). A map value has no
    /// field name of its own.
    fn slot_name(steps: &[Step]) -> Option<&str> {
        steps
            .iter()
            .rev()
            .find(|step| !matches!(step, Step::AnyIndex))
            .and_then(|step| match step {
                Step::Key(key) => Some(key.as_str()),
                Step::AnyKey | Step::AnyIndex => None,
            })
    }

    /// Schema slots whose name looks like a credential but that hold none.
    /// Each carries the reason; the guard below fails when one disappears
    /// from the schema, so the list cannot go stale.
    const NOT_SECRET: &[(&str, &str)] = &[(
        "nodes.server.tls_key",
        "a filesystem path to the PEM file (like nodes.server.tls_cert), not the key material",
    )];

    /// Registry completeness (R-PMMA F5, D-018 Q6): every string slot of the
    /// `Config` schema whose name looks like a credential is registered or
    /// explicitly exempt. A credential field added later fails here until it
    /// is registered, so it cannot be stored in plaintext unnoticed. RED
    /// before SECRETS-OTHER: about 30 channel, node, tunnel and webhook
    /// credentials were masked by the name rule only.
    #[test]
    fn every_secret_named_string_slot_of_the_schema_is_registered_or_exempt() {
        let slots = schema_string_slots();
        assert!(
            slots
                .iter()
                .any(|slot| display_steps(slot) == "channels_config.telegram.bot_token"),
            "the schema walk must reach the channel sections"
        );
        let unregistered: Vec<String> = slots
            .iter()
            .filter(|slot| slot_name(slot).is_some_and(is_secret_key_name))
            .filter(|slot| {
                !SECRET_PATHS
                    .iter()
                    .any(|secret| secret.alias_of.is_none() && steps_match(secret.segments, slot))
            })
            .map(|slot| display_steps(slot))
            .filter(|pattern| !NOT_SECRET.iter().any(|(exempt, _)| exempt == pattern))
            .collect();
        assert!(
            unregistered.is_empty(),
            "credential-named config fields missing from SECRET_PATHS (register them, or add them to NOT_SECRET \
             with a reason): {unregistered:#?}"
        );
        for (exempt, reason) in NOT_SECRET {
            assert!(!reason.is_empty(), "expected a non-empty value");
            assert!(
                slots.iter().any(|slot| display_steps(slot) == *exempt),
                "NOT_SECRET entry {exempt} is not a string slot of the schema any more"
            );
        }
    }

    /// Whether a field name says it holds a URL (or a proxy URL).
    fn url_named(name: &str) -> bool {
        name == "homeserver"
            || name
                .split('_')
                .any(|token| matches!(token, "url" | "uri" | "endpoint" | "proxy"))
    }

    /// URL-named slots that are not endpoint URLs.
    const NOT_URL: &[(&str, &str)] = &[
        (
            "proxy.no_proxy[]",
            "host and domain names that bypass the proxy, never a URL with userinfo",
        ),
        (
            "tunnel.custom.url_pattern",
            "a pattern matched against the tunnel command's output, not a URL PRX connects to",
        ),
    ];

    /// Every URL-named string slot of the schema either is a registered
    /// secret (a connection string such as `storage.provider.config.db_url`)
    /// or has its userinfo redacted as a `URL_USERINFO_PATHS` entry. RED
    /// before SECRETS-OTHER: proxy, MCP, node, OTEL, daemon and channel
    /// endpoint URLs were shown with their `user:password@` in clear.
    #[test]
    fn every_url_named_string_slot_of_the_schema_redacts_its_userinfo() {
        let slots = schema_string_slots();
        let uncovered: Vec<String> = slots
            .iter()
            .filter(|slot| slot_name(slot).is_some_and(url_named))
            .filter(|slot| {
                !SECRET_PATHS
                    .iter()
                    .any(|secret| secret.alias_of.is_none() && steps_match(secret.segments, slot))
                    && !ENDPOINT_PATHS
                        .iter()
                        .any(|entry| entry.userinfo && steps_match(entry.segments, slot))
            })
            .map(|slot| display_steps(slot))
            .filter(|pattern| !NOT_URL.iter().any(|(exempt, _)| exempt == pattern))
            .collect();
        assert!(
            uncovered.is_empty(),
            "URL-named config fields missing from ENDPOINT_PATHS (with userinfo): {uncovered:#?}"
        );
        for (exempt, _) in NOT_URL {
            assert!(
                slots.iter().any(|slot| display_steps(slot) == *exempt),
                "NOT_URL entry {exempt} is not a string slot of the schema any more"
            );
        }
        for entry in ENDPOINT_PATHS {
            assert!(
                slots.iter().any(|slot| steps_match(entry.segments, slot)),
                "ENDPOINT_PATHS entry {} is not a string slot of the schema",
                entry.pattern
            );
        }
    }

    /// R-SECRETS F3, RED before SECRETS-f: userinfo in `a2a.card_jwks_uri`
    /// loaded and was published in the public Agent Card. After: the load
    /// refuses it, naming the field and never the password.
    #[test]
    fn a_published_jwks_uri_with_userinfo_is_refused() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_temperature = 0.7\n\n[a2a]\ncard_jwks_uri = \"https://u:userinfo-sentinel-jwks@h.invalid/jwks.json\"\n",
        )
        .expect("test: write config");
        let error = crate::config::Config::load_from_path(&path, dir.path().join("workspace"))
            .expect_err("test: userinfo in a published URI must not load");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("a2a.card_jwks_uri"), "{rendered}");
        assert!(!rendered.contains("userinfo-sentinel"), "{rendered}");

        std::fs::write(
            &path,
            "default_temperature = 0.7\n\n[a2a]\ncard_jwks_uri = \"https://h.invalid/jwks.json\"\n",
        )
        .expect("test: write config");
        crate::config::Config::load_from_path(&path, dir.path().join("workspace")).expect("test: plain URI loads");
    }

    /// `prx config show` and the Gateway redaction keep every endpoint URL
    /// readable and replace only its userinfo.
    #[test]
    fn config_show_redacts_url_userinfo_in_every_endpoint_url() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_from_overlay(dir.path(), fixture::ENDPOINT_USERINFO_TOML);
        let rendered = redacted_stored_toml(&config)
            .expect("test: config show rendering")
            .to_string();
        assert!(
            !rendered.contains("userinfo-sentinel"),
            "config show leaked URL userinfo: {rendered}"
        );
        for endpoint in [
            "http://[redacted]@127.0.0.1:11",
            "socks5://[redacted]@127.0.0.1:13",
            "https://[redacted]@127.0.0.1:14/mcp",
            "http://[redacted]@127.0.0.1:16",
            "http://[redacted]@127.0.0.1:19/health",
            "ws://[redacted]@127.0.0.1:26",
            "https://[redacted]@127.0.0.1:27/v1",
        ] {
            assert!(rendered.contains(endpoint), "{endpoint} is missing from: {rendered}");
        }

        let mut json = serde_json::to_value(&config).expect("test: json");
        redact(&mut json);
        let json = json.to_string();
        assert!(!json.contains("userinfo-sentinel"), "GET /api/config leaked: {json}");

        let raw: toml::Value = toml::from_str(fixture::ENDPOINT_USERINFO_TOML).expect("test: parses");
        let found = scan(&raw);
        assert!(found.has_secret_values(), "the userinfo must be scrubbed from messages");
        assert!(
            !found
                .scrub(fixture::ENDPOINT_USERINFO_TOML)
                .contains("userinfo-sentinel")
        );
    }

    /// CDP-04 (V6.09 endpoint half): the decision endpoint written by hand
    /// with user information never shows it on a read surface: `prx config
    /// show`, `GET /api/config`, a scrubbed message or the section's `Debug`.
    #[test]
    fn the_decision_endpoint_is_redacted_on_every_read_surface() {
        const SENTINEL: &str = "userinfo-sentinel-cdp04";
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let overlay = format!("[causal_tree.backend]\nendpoint = \"https://decider:{SENTINEL}@127.0.0.1:27/v1\"\n");
        let config = fixture::config_from_overlay(dir.path(), &overlay);
        let rendered = redacted_stored_toml(&config)
            .expect("test: config show rendering")
            .to_string();
        assert!(!rendered.contains(SENTINEL), "config show leaked: {rendered}");
        assert!(rendered.contains("https://[redacted]@127.0.0.1:27/v1"), "{rendered}");
        let mut json = serde_json::to_value(&config).expect("test: json");
        redact(&mut json);
        assert!(!json.to_string().contains(SENTINEL), "GET /api/config leaked: {json}");
        let found = scan(&toml::from_str::<toml::Value>(&overlay).expect("test: overlay"));
        assert!(!found.scrub(&overlay).contains(SENTINEL));
        let debug = format!("{:?}", config.causal_tree);
        assert!(!debug.contains(SENTINEL), "{debug}");
        assert!(is_credential_recipient_endpoint(&[
            PathElem::Key("causal_tree".to_string()),
            PathElem::Key("backend".to_string()),
            PathElem::Key("endpoint".to_string()),
        ]));
    }

    /// F2, RED before PMMA-f: a credential stored as the literal mask (left
    /// by the pre-A-4 round trip, possibly encrypted) loaded silently and was
    /// sent to the provider as the key. After: the load warns, naming the
    /// path and never a value.
    #[test]
    fn load_warns_about_a_stored_mask_by_path_only() {
        #[derive(Clone, Default)]
        struct Sink(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let store = crate::security::SecretStore::new(dir.path(), true);
        let encrypted_mask = store.encrypt(REDACTION_MASK).expect("test: encrypt mask");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "default_temperature = 0.7\n\n[[model_routes]]\nhint = \"fast\"\nprovider = \"openrouter\"\nmodel = \"m\"\napi_key = \"{encrypted_mask}\"\n\n[gateway]\npaired_tokens = [\"***\", \"sentinel-paired-ok\"]\n"
            ),
        )
        .expect("test: write config");

        let _serial = fixture::LOG_CALLSITE_LOCK.blocking_lock();
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let loaded = tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
        })
        .expect("test: a stored mask must not stop the load");
        assert_eq!(
            loaded.model_routes.first().and_then(|r| r.api_key.as_deref()),
            Some(REDACTION_MASK)
        );

        let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
        assert!(
            logs.contains("model_routes[0].api_key"),
            "the stored mask must be reported: {logs}"
        );
        assert!(
            logs.contains("gateway.paired_tokens[0]"),
            "the stored mask must be reported: {logs}"
        );
        assert!(
            !logs.contains("sentinel-paired-ok"),
            "the warning must not carry a value: {logs}"
        );
        assert!(!logs.contains("enc2:"), "the warning must not carry a value: {logs}");
    }

    /// F7, RED before PMMA-f: `Config`'s `Debug` redacted only the root key;
    /// route, agent, rotation, tool, storage and channel secrets printed in
    /// clear. After: every registry path is masked and ordinary values stay.
    #[test]
    fn config_debug_masks_every_registered_path() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_in(dir.path());
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains(fixture::SENTINEL_PREFIX),
            "Debug leaked a secret: {rendered}"
        );
        assert!(
            rendered.contains("model-test"),
            "Debug must keep ordinary values: {rendered}"
        );
        assert!(
            rendered.contains("config_path"),
            "Debug must keep the path fields: {rendered}"
        );
    }

    #[test]
    fn name_rule_masks_an_unregistered_credential_and_keeps_ordinary_values() {
        let mut value = serde_json::json!({
            "future_section": { "client_secret": "sentinel-future", "enabled": true, "count": 3 },
            "default_model": "kimi-k2",
            "nested": { "credential": { "username": "user", "password": "pass" } },
        });
        redact(&mut value);
        assert_eq!(
            value.pointer("/future_section/client_secret"),
            Some(&serde_json::json!(REDACTION_MASK))
        );
        assert_eq!(value.pointer("/future_section/enabled"), Some(&serde_json::json!(true)));
        assert_eq!(value.pointer("/default_model"), Some(&serde_json::json!("kimi-k2")));
        assert_eq!(
            value.pointer("/nested/credential/username"),
            Some(&serde_json::json!(REDACTION_MASK))
        );
        assert_eq!(
            value.pointer("/nested/credential/password"),
            Some(&serde_json::json!(REDACTION_MASK))
        );
    }

    /// Every configuration path that names a provider or an endpoint by URL.
    /// Each value carries a distinct fixture password, so one search finds a
    /// leak from any of them.
    const USERINFO_TOML: &str = r#"
default_provider = "custom:http://alice:userinfo-sentinel-default@127.0.0.1:1/v1"
api_url = "http://alice:userinfo-sentinel-api-url@127.0.0.1:2/v1"

[reliability]
fallback_providers = ["custom:https://bob:userinfo-sentinel-fallback@127.0.0.1:3/v1", "openrouter"]

[[model_routes]]
hint = "fast"
provider = "custom:http://carol:userinfo-sentinel-route@127.0.0.1:4/v1"
model = "route-model"

[[embedding_routes]]
hint = "semantic"
provider = "custom:http://dave:userinfo-sentinel-embedding@127.0.0.1:5/v1"
model = "embed-model"

[agents.worker]
provider = "custom:http://erin:userinfo-sentinel-agent@127.0.0.1:6/v1"
model = "agent-model"

[[providers.tool_dialects]]
provider = "custom:http://frank:userinfo-sentinel-dialect@127.0.0.1:7/v1"
tool_schema = "no_root_combinators"

[smart_group]
classifier_provider = "custom:http://grace:userinfo-sentinel-classifier@127.0.0.1:8/v1"
"#;

    fn userinfo_config(dir: &std::path::Path) -> crate::config::Config {
        let mut tree = toml::Value::try_from(crate::config::Config::default()).expect("test: default config tree");
        fixture::overlay(
            &mut tree,
            toml::from_str(USERINFO_TOML).expect("test: userinfo fixture parses"),
        );
        let mut config: crate::config::Config = tree.try_into().expect("test: the userinfo fixture must deserialize");
        config.config_path = dir.join("config.toml");
        config.workspace_dir = dir.join("workspace");
        config
    }

    /// R-C2Kf S-2: `prx config show` printed every provider label verbatim,
    /// so a `custom:` URL's `user:password@` reached the terminal. The
    /// endpoints stay readable; only their userinfo is replaced.
    #[test]
    fn config_show_redacts_url_userinfo_in_every_provider_and_endpoint_field() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let rendered = redacted_stored_toml(&userinfo_config(dir.path()))
            .expect("test: config show rendering")
            .to_string();
        assert!(
            !rendered.contains("userinfo-sentinel"),
            "config show leaked URL userinfo: {rendered}"
        );
        for endpoint in [
            "custom:http://[redacted]@127.0.0.1:1/v1",
            "http://[redacted]@127.0.0.1:2/v1",
            "custom:https://[redacted]@127.0.0.1:3/v1",
            "custom:http://[redacted]@127.0.0.1:4/v1",
            "custom:http://[redacted]@127.0.0.1:5/v1",
            "custom:http://[redacted]@127.0.0.1:6/v1",
            "custom:http://[redacted]@127.0.0.1:7/v1",
            "custom:http://[redacted]@127.0.0.1:8/v1",
        ] {
            assert!(rendered.contains(endpoint), "{endpoint} is missing from: {rendered}");
        }
        assert!(
            rendered.contains("\"openrouter\""),
            "an ordinary label must stay: {rendered}"
        );
    }

    /// A redacted label posted or written back is a mask: it must be found by
    /// the scan that refuses masks, never stored as a provider.
    #[test]
    fn a_redacted_endpoint_label_counts_as_a_mask() {
        let tree: toml::Value = toml::from_str(
            "default_provider = \"custom:http://[redacted]@127.0.0.1:1/v1\"\n\
             [reliability]\nfallback_providers = [\"openrouter\", \"custom:http://[redacted]@h.invalid/v1\"]\n",
        )
        .expect("test: masked tree parses");
        let masked: BTreeSet<String> = scan(&tree).masked.into_iter().collect();
        assert_eq!(
            masked,
            BTreeSet::from([
                "default_provider".to_string(),
                "reliability.fallback_providers[1]".to_string()
            ])
        );
        // Userinfo is a secret value for scrubbing messages, and a file that
        // holds one is served masked.
        let raw: toml::Value = toml::from_str(USERINFO_TOML).expect("test: userinfo fixture parses");
        let found = scan(&raw);
        assert!(found.has_secret_values());
        assert!(
            !found
                .scrub("rejected custom:http://alice:userinfo-sentinel-default@127.0.0.1:1/v1")
                .contains("userinfo-sentinel")
        );
    }

    /// R-C2BU F-3: the memory embedding provider also takes a `custom:` URL.
    #[test]
    fn config_show_redacts_url_userinfo_in_the_memory_embedding_provider() {
        let mut tree: toml::Value = toml::from_str(
            "[memory]\nembedding_provider = \"custom:https://u:userinfo-sentinel-embed@embed.invalid/v1\"\n",
        )
        .expect("test: fixture parses");
        let found = scan(&tree);
        assert!(
            found.has_secret_values(),
            "the userinfo must be a secret value for scrubbing"
        );
        redact(&mut tree);
        let rendered = tree.to_string();
        assert!(!rendered.contains("userinfo-sentinel"), "{rendered}");
        assert!(
            rendered.contains("custom:https://[redacted]@embed.invalid/v1"),
            "{rendered}"
        );
    }

    /// D-027: two credential-bearing `[cost].prices` keys that redact to the
    /// same spelling are a configuration error. The error names the keys in
    /// their redacted form only.
    #[test]
    fn colliding_credentialed_price_keys_fail_validation_without_printing_credentials() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_temperature = 0.7\n\n[cost.prices.\"custom:http://alice:pwA-d027@h.invalid/v1/m\"]\ninput = 1.0\noutput = 1.0\n\n\
             [cost.prices.\"custom:http://bob:pwB-d027@h.invalid/v1/m\"]\ninput = 9.0\noutput = 9.0\n",
        )
        .expect("test: write config");
        let error = crate::config::Config::load_from_path(&path, dir.path().join("workspace"))
            .expect_err("test: colliding price keys must not load");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("custom:http://[redacted]@h.invalid/v1/m"),
            "{rendered}"
        );
        assert!(rendered.contains("cost.prices"), "{rendered}");
        assert!(
            !rendered.contains("pwA-d027") && !rendered.contains("pwB-d027"),
            "{rendered}"
        );
        assert!(!rendered.contains("alice") && !rendered.contains("bob"), "{rendered}");

        // Distinct endpoints, or one credentialed key, still load.
        std::fs::write(
            &path,
            "default_temperature = 0.7\n\n[cost.prices.\"custom:http://alice:pwA-d027@h.invalid/v1/m\"]\ninput = 1.0\noutput = 1.0\n\n\
             [cost.prices.\"custom:http://bob:pwB-d027@other.invalid/v1/m\"]\ninput = 9.0\noutput = 9.0\n",
        )
        .expect("test: write config");
        crate::config::Config::load_from_path(&path, dir.path().join("workspace"))
            .expect("test: distinct endpoints load");
    }

    /// The `{:?}` of every table-valued section of `config`, by its key.
    fn section_debug(config: &crate::config::Config) -> Vec<(&'static str, String)> {
        macro_rules! sections {
            ($($field:ident),* $(,)?) => {
                vec![$((stringify!($field), format!("{:?}", config.$field))),*]
            };
        }
        sections!(
            providers,
            observability,
            autonomy,
            runtime,
            reliability,
            scheduler,
            agent,
            sessions_spawn,
            chat,
            self_system,
            skills,
            skill_rag,
            skills_lifecycle,
            model_routes,
            embedding_routes,
            query_classification,
            task_routing,
            router,
            smart_group,
            heartbeat,
            xin,
            cron,
            channels_config,
            memory,
            identity_bindings,
            user_policies,
            storage,
            tunnel,
            gateway,
            webhook,
            composio,
            mcp,
            mcp_server,
            a2a,
            auth,
            secrets,
            browser,
            http_request,
            multimodal,
            web_search,
            proxy,
            identity,
            cost,
            nodes,
            agents,
            media,
            causal_tree,
            security,
            compliance,
            tool_tiering,
        )
    }

    /// Provider labels with URL userinfo at the label paths the endpoint
    /// fixture leaves out.
    const LABEL_USERINFO_TOML: &str = r#"
[[router.models]]
provider = "custom:http://router:sentinel-label-router@127.0.0.1:31/v1"
model_id = "m"

[[providers.models]]
provider = "custom:http://models:sentinel-label-models@127.0.0.1:32/v1"
model = "m"

[[providers.tool_dialects]]
provider = "custom:http://dialect:sentinel-label-dialect@127.0.0.1:33/v1"
tool_schema = "raw"

[memory]
embedding_provider = "custom:http://embed:sentinel-label-memory@127.0.0.1:34/v1"

[smart_group]
classifier_provider = "custom:http://group:sentinel-label-group@127.0.0.1:35/v1"

[[model_routes]]
hint = "fast"
provider = "custom:http://route:sentinel-label-route@127.0.0.1:36/v1"
model = "m"

[reliability]
fallback_providers = ["custom:http://fallback:sentinel-label-fallback@127.0.0.1:37/v1"]

[cost.prices."custom:http://price:sentinel-label-price@127.0.0.1:38/v1/m"]
input = 1.0
output = 1.0
"#;

    /// The display path of every table in `tree` that has a key carrying a
    /// fixture sentinel (the value leaves alone miss a secret in a key).
    fn sentinel_key_owners(tree: &toml::Value) -> BTreeSet<String> {
        fn walk(node: &toml::Value, path: &mut Vec<PathElem>, out: &mut BTreeSet<String>) {
            match node {
                toml::Value::Table(table) => {
                    if table.keys().any(|key| key.contains(fixture::SENTINEL_PREFIX)) {
                        out.insert(display_path(path));
                    }
                    for (key, child) in table {
                        path.push(PathElem::Key(key.clone()));
                        walk(child, path, out);
                        path.pop();
                    }
                }
                toml::Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        path.push(PathElem::Index(index));
                        walk(child, path, out);
                        path.pop();
                    }
                }
                _ => {}
            }
        }
        let mut out = BTreeSet::new();
        walk(tree, &mut Vec::new(), &mut out);
        out
    }

    /// `{:?}` of the sub-struct that owns the value at `owner` (the display
    /// path of a sentinel's parent), or `None` when no probe is written for it.
    fn owner_debug(config: &crate::config::Config, owner: &str) -> Option<String> {
        fn d(value: &dyn std::fmt::Debug) -> String {
            format!("{value:?}")
        }
        let c = config;
        let channels = &c.channels_config;
        Some(match owner {
            "" => d(c),
            "agents.worker" => d(c.agents.get("worker")?),
            "model_routes[0]" => d(c.model_routes.first()?),
            "embedding_routes[0]" => d(c.embedding_routes.first()?),
            "reliability" | "reliability.api_keys" | "reliability.fallback_providers" => d(&c.reliability),
            "composio" => d(&c.composio),
            "browser.computer_use" => d(&c.browser.computer_use),
            "web_search" => d(&c.web_search),
            "storage.provider.config" => d(&c.storage.provider.config),
            "gateway" | "gateway.paired_tokens" => d(&c.gateway),
            "mcp.servers.local" | "mcp.servers.local.env" => d(c.mcp.servers.get("local")?),
            "mcp.servers.remote" => d(c.mcp.servers.get("remote")?),
            "channels_config.telegram" => d(channels.telegram.as_ref()?),
            "channels_config.discord" => d(channels.discord.as_ref()?),
            "channels_config.slack" => d(channels.slack.as_ref()?),
            "channels_config.mattermost" => d(channels.mattermost.as_ref()?),
            "channels_config.dingtalk" => d(channels.dingtalk.as_ref()?),
            "channels_config.email" => d(channels.email.as_ref()?),
            "channels_config.irc" => d(channels.irc.as_ref()?),
            "channels_config.lark" => d(channels.lark.as_ref()?),
            "channels_config.linq" => d(channels.linq.as_ref()?),
            "channels_config.matrix" => d(channels.matrix.as_ref()?),
            "channels_config.nextcloud_talk" => d(channels.nextcloud_talk.as_ref()?),
            "channels_config.qq" => d(channels.qq.as_ref()?),
            "channels_config.wacli" => d(channels.wacli.as_ref()?),
            "channels_config.webhook" => d(channels.webhook.as_ref()?),
            "channels_config.whatsapp" => d(channels.whatsapp.as_ref()?),
            "channels_config.signal" => d(channels.signal.as_ref()?),
            "webhook" => d(&c.webhook),
            "tunnel.cloudflare" => d(c.tunnel.cloudflare.as_ref()?),
            "tunnel.ngrok" => d(c.tunnel.ngrok.as_ref()?),
            "tunnel.custom" => d(c.tunnel.custom.as_ref()?),
            "chat.daemon" => d(&c.chat.daemon),
            "causal_tree.backend" => d(&c.causal_tree.backend),
            "nodes.nodes[0]" => d(c.nodes.nodes.first()?),
            "nodes.server" => d(&c.nodes.server),
            "proxy" => d(&c.proxy),
            "observability" => d(&c.observability),
            "media" => d(&c.media),
            "mcp_server" => d(&c.mcp_server),
            "a2a" => d(&c.a2a),
            "router.models[0]" => d(c.router.models.first()?),
            "providers.models[0]" => d(c.providers.models.first()?),
            "providers.tool_dialects[0]" => d(c.providers.tool_dialects.first()?),
            "memory" => d(&c.memory),
            "smart_group" => d(&c.smart_group),
            "cost.prices" => d(&c.cost),
            _ => return None,
        })
    }

    /// INT-2B a40 (R-PMMAf sub-struct Debug), RED before: the configuration
    /// sub-structs derived `Debug`, so `{:?}` of a model route, a channel
    /// config or a delegate agent printed its key, and of an endpoint its URL
    /// password. After: for every sentinel of the registry fixture, the
    /// endpoint fixture and a provider-label fixture, `{:?}` of the sub-struct
    /// that owns it prints no sentinel (a sentinel whose owner has no probe
    /// fails the test), and neither does `{:?}` of any table-valued section.
    #[test]
    fn no_configuration_struct_debug_prints_a_secret() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let mut unprobed = BTreeSet::new();
        let mut leaks = BTreeSet::new();
        for overlay in [fixture::TOML, fixture::ENDPOINT_USERINFO_TOML, LABEL_USERINFO_TOML] {
            let config = fixture::config_from_overlay(dir.path(), overlay);
            let tree: toml::Value = toml::from_str(overlay).expect("test: overlay parses");
            let sentinels = fixture::string_leaves(&tree, &|value| value.contains(fixture::SENTINEL_PREFIX));
            assert!(!sentinels.is_empty());
            // A secret in a map key (a credentialed `cost.prices` key) is
            // owned by the struct that holds the map.
            let key_owners = sentinel_key_owners(&tree);
            let owners = sentinels
                .into_iter()
                .map(|path| {
                    let mut owner = path;
                    if matches!(owner.pop(), Some(PathElem::Index(_))) {
                        owner.pop();
                    }
                    display_path(&owner)
                })
                .chain(key_owners);
            for owner in owners {
                match owner_debug(&config, &owner) {
                    Some(rendered) if rendered.contains(fixture::SENTINEL_PREFIX) => {
                        leaks.insert(owner);
                    }
                    Some(_) => {}
                    None => {
                        unprobed.insert(owner);
                    }
                }
            }

            let sections = section_debug(&config);
            let serialized = serde_json::to_value(&config).expect("test: serialize");
            let probed: BTreeSet<&str> = sections.iter().map(|(name, _)| *name).collect();
            for (key, value) in serialized.as_object().expect("test: config is an object") {
                if value.is_object() || value.is_array() {
                    assert!(probed.contains(key.as_str()), "section `{key}` has no Debug probe");
                }
            }
            for (name, rendered) in &sections {
                if rendered.contains(fixture::SENTINEL_PREFIX) {
                    leaks.insert((*name).to_string());
                }
            }
        }
        assert!(unprobed.is_empty(), "add a Debug probe for: {unprobed:?}");
        assert!(leaks.is_empty(), "these print a secret with {{:?}}: {leaks:?}");
    }

    /// INT-2B a41 (R-PMMAf R5, D-056 DC-4), RED before: a commented-out
    /// credential was found only as `name = value` with the exact credential
    /// name, so `# api-key = …`, `# apikey = …` and `# token: …` were served
    /// verbatim. After: `-` reads as `_`, `apikey` counts, and `:` separates
    /// like `=`. Ordinary comments still do not count.
    #[test]
    fn a_commented_credential_is_found_in_its_common_spellings() {
        for text in [
            "# api-key = \"sk-old\"",
            "# apikey = \"sk-old\"",
            "# OpenAI-ApiKey = \"sk-old\"",
            "# api_key: sk-old",
            "#token: abc",
            "model = \"m\" # bot-token: abc",
            "# Password = hunter2",
            "# api_key = \"sk-old\"",
            "# note: api_key = \"sk-old\"",
        ] {
            assert!(has_commented_secret(text), "{text:?} must count as a commented secret");
        }
        for text in [
            "# see the API documentation",
            "# key points: none",
            "# timeout = 5",
            "# model: gpt-4o",
            "# http://example.invalid/a=b",
            "default_model = \"m\"",
        ] {
            assert!(!has_commented_secret(text), "{text:?} is an ordinary comment");
        }
    }
}
