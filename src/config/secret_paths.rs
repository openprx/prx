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
        pattern: "skills.github_token",
        segments: &[Key("skills"), Key("github_token")],
        handling: EncryptedAtRest,
        domain: SecretDomain::Tool,
        alias_of: None,
        used_as: SecretUse::Credential,
    },
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
    recipient_url(
        "a2a.peers.<*>.address",
        &[Key("a2a"), Key("peers"), AnyKey, Key("address")],
    ),
    // The decision endpoint (CDP-03, CDP-04): validation refuses userinfo
    // there, and this keeps every read surface safe for a hand-edited file.
    // The decision backend's credential is sent to it, so re-pointing it is
    // an endpoint change (high risk).
    recipient_url(
        "decision_engine.backend.endpoint",
        &[Key("decision_engine"), Key("backend"), Key("endpoint")],
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
    access(
        "a2a.peers",
        &[Key("a2a"), Key("peers")],
        "messaging/a2a pinned peer recipient grants",
    ),
    access(
        "a2a.messaging_enabled",
        &[Key("a2a"), Key("messaging_enabled")],
        "messaging/a2a listener activation",
    ),
    access(
        "agent.messaging.inbound",
        &[Key("agent"), Key("messaging"), Key("inbound")],
        "messaging receiver admission",
    ),
    access(
        "agents.<*>.messaging.inbound",
        &[Key("agents"), AnyKey, Key("messaging"), Key("inbound")],
        "named receiver admission",
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
        "channels/inbound_policy.rs, daemon/mod.rs (channel account -> user identity)",
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
        "channels/inbound_policy.rs (direct/group message admission policy)",
    ),
    access(
        "channels_config.signal.group_policy",
        &[Key("channels_config"), Key("signal"), Key("group_policy")],
        "channels/inbound_policy.rs (direct/group message admission policy)",
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
        "channels/inbound_policy.rs (direct/group message admission policy)",
    ),
    access(
        "channels_config.whatsapp.group_policy",
        &[Key("channels_config"), Key("whatsapp"), Key("group_policy")],
        "channels/inbound_policy.rs (direct/group message admission policy)",
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

[channels_config.signal]
account = "+10000000000"
http_url = "http://signal:userinfo-sentinel-signal@127.0.0.1:25"

[channels_config.whatsapp]
ws_url = "ws://wa:userinfo-sentinel-whatsapp@127.0.0.1:26"

[decision_engine.backend]
endpoint = "https://decider:userinfo-sentinel-engine@127.0.0.1:28/v1"
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
#[path = "secret_paths/tests.rs"]
mod tests;
