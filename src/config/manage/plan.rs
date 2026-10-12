//! The redacted semantic plan of one configuration change.
//!
//! A [`ConfigChangeSummary`] lists every leaf setting a change adds, removes
//! or alters, compared on the loaded (decrypted) configurations so that a
//! re-encrypted but unchanged secret is not reported. Every value shown is
//! taken from a copy redacted through the secret-path registry
//! ([`crate::config::secret_paths`]), so a summary never carries a
//! credential, and it is safe to return to an API client, print, or keep in
//! the configuration history.
//!
//! The summary also carries the change's risk classification (D-018 F6):
//! the transaction layer is the one place that sees the complete semantic
//! difference of every write surface, so it classifies; each surface keeps
//! enforcing its own approval policy (see `docs/configuration.md`).

use crate::config::Config;
use crate::config::secret_paths::{self, PathElem};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How one setting changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
}

/// One changed leaf setting. `before`/`after` are redacted display values;
/// `None` means the setting was absent (or null).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChangeEntry {
    /// Concrete path, e.g. `model_routes[0].api_key`.
    pub path: String,
    pub kind: ChangeKind,
    pub before: Option<Value>,
    pub after: Option<Value>,
    /// Whether the path is a secret; its values are then always the mask.
    pub secret: bool,
}

/// Risk of a configuration change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigChangeRisk {
    /// No credential, no encryption switch, no provider endpoint.
    Low,
    /// Writes a credential, turns encryption at rest off, or points a
    /// provider (and so its stored key) at a different endpoint.
    High,
}

/// Why a change is classified [`ConfigChangeRisk::High`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RiskReason {
    /// A secret path was set, replaced or cleared.
    CredentialChanged { path: String },
    /// `secrets.encrypt` went from true to false: later saves store every
    /// credential in plaintext.
    EncryptionDisabled,
    /// A provider endpoint changed, so a stored key is sent to a new host.
    EndpointChanged { path: String },
    /// A rollback would restore a credential the current configuration no
    /// longer holds anywhere (a rotated-out key, a removed paired token).
    CredentialReintroduced { path: String },
    /// A change widened what a principal may do, or turned off a protection
    /// of an inbound path (D-049 amendment): a user policy granting a higher
    /// role, a project, a higher visibility ceiling or fewer blocked
    /// patterns; an `autonomy.scopes` change that is not provably narrower;
    /// a tool newly exposed on the MCP endpoint; `nodes.server.tls_required`
    /// turned off. The narrowing direction of each stays low.
    PrivilegeWidened { path: String },
    /// The stored configuration exists but cannot be loaded, so the change
    /// cannot be compared with what it replaces (D-050): every rule that
    /// looks at the prior value would fail open. Every change is high risk
    /// until the configuration loads again; repair needs confirmation.
    BaselineUnavailable,
    /// A local program that can reach stored credentials is launched
    /// differently: a stdio MCP server was added, or its `transport`,
    /// `command` or `args` changed. It runs with PRX's privileges (it can
    /// read the stored credentials) and receives its `env` values, so this is
    /// high risk whether or not it holds any (D-045).
    CredentialRecipientChanged { path: String },
    /// An MCP server's launch environment (`mcp.servers.<name>.env.<VAR>`)
    /// was set, replaced or cleared. Besides a credential it can be a
    /// setting that changes what the program runs (`NODE_OPTIONS`,
    /// `LD_PRELOAD`, `DYLD_*`, `PYTHONPATH`, `PATH`).
    ProcessEnvironmentChanged { path: String },
    /// A stored credential now goes to a different provider: the default
    /// provider changed identity while the root connection holds a key
    /// (`api_key`, `reliability.api_keys`), or a route, embedding route or
    /// agent changed its provider while it holds its own `api_key` (D-085
    /// F-3). The key is sent to the new provider on its next request.
    CredentialOwnerChanged { path: String },
    /// Who is admitted inbound changed: a secret that admits a client
    /// (D-048: `gateway.paired_tokens[..]`, the inbound webhook token and
    /// signing secret, a channel's verify token or webhook secret, the node
    /// server's bearer token) or a setting that decides trust (D-049:
    /// `secret_paths::ACCESS_CONTROL_PATHS`, e.g. `gateway.require_pairing`,
    /// the MCP JWT key material, a channel's sender allow-list) was set,
    /// replaced or removed.
    AccessGrantChanged { path: String },
    /// Decision state may leave this machine, or goes somewhere else (CDP
    /// 13.1, CXR 4.4): `causal_tree.backend.remote_enabled` turned on, or,
    /// while it is on, the backend `kind`, `endpoint`, `model` or
    /// `credential_ref` changed. Turning it off or selecting `builtin`
    /// narrows and stays low.
    RemoteDecisionEnabled { path: String },
}

impl RiskReason {
    /// The reason as the clause every surface shows ("writes the credential
    /// at `api_key`"): the confirmation refusal and the rendered plan use
    /// this one wording. It names paths only, never a value.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::CredentialChanged { path } => format!("writes the credential at {path}"),
            Self::EncryptionDisabled => "turns secrets.encrypt off".to_string(),
            Self::BaselineUnavailable => {
                "cannot be compared with the stored configuration, which does not load".to_string()
            }
            Self::EndpointChanged { path } => format!("changes the provider endpoint at {path}"),
            Self::CredentialRecipientChanged { path } => {
                format!("adds or relaunches a local program that can reach stored credentials at {path}")
            }
            Self::ProcessEnvironmentChanged { path } => {
                format!("changes the launch environment of an MCP server at {path}")
            }
            Self::AccessGrantChanged { path } => {
                format!("changes who is admitted (a secret or setting that admits clients) at {path}")
            }
            Self::PrivilegeWidened { path } => {
                format!("widens what a principal may do (or turns off an inbound protection) at {path}")
            }
            Self::CredentialReintroduced { path } => {
                format!("restores a credential the current configuration no longer holds at {path}")
            }
            Self::CredentialOwnerChanged { path } => {
                format!("sends a stored credential to a different provider at {path}")
            }
            Self::RemoteDecisionEnabled { path } => {
                format!("sends decision state off this machine, or to another backend, at {path}")
            }
        }
    }
}

/// The path prefix of a change to a credential reference's stored value
/// (the auth-profile store, not a configuration file).
pub const CREDENTIAL_REFERENCES_PATH: &str = "credential_references.";

/// The redacted semantic difference between two configurations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChangeSummary {
    pub changes: Vec<ConfigChangeEntry>,
    pub risk: ConfigChangeRisk,
    pub risk_reasons: Vec<RiskReason>,
    /// Paths of secrets the change creates or changes, or stored credential
    /// references it attaches, compared by identity
    /// (a route's hint, an agent's name, a list value), never by position
    /// (D-101 F-3). The Gateway gates these High.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_writes: Vec<String>,
    /// Paths of secrets the change clears (gated Medium), by identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_clears: Vec<String>,
}

impl ConfigChangeSummary {
    /// Summarize the change from `before` (`None`: no loadable prior
    /// configuration, so every setting counts as added) to `after`.
    pub fn between(before: Option<&Config>, after: &Config) -> Result<Self> {
        let after_raw = serde_json::to_value(after).context("Failed to render the candidate configuration")?;
        let before_raw = before
            .map(serde_json::to_value)
            .transpose()
            .context("Failed to render the current configuration")?;
        let mut after_redacted = after_raw.clone();
        secret_paths::redact(&mut after_redacted);
        let before_redacted = before_raw.as_ref().map(|raw| {
            let mut redacted = raw.clone();
            secret_paths::redact(&mut redacted);
            redacted
        });

        let mut ctx = DiffContext {
            has_before: before.is_some(),
            ..DiffContext::default()
        };
        diff(
            Sides {
                raw: before_raw.as_ref(),
                shown: before_redacted.as_ref(),
            },
            Sides {
                raw: Some(&after_raw),
                shown: Some(&after_redacted),
            },
            &mut Vec::new(),
            &mut Vec::new(),
            Reach {
                inside_secret: false,
                credentialed_key: false,
            },
            &mut ctx,
        );
        let DiffContext {
            out: changes,
            credential_paths,
            attached_reference_paths,
            endpoint_paths,
            has_before: _,
            mcp_launch_changes,
            environment_paths,
            access_paths,
        } = ctx;

        let mut risk_reasons = Vec::new();
        // Secrets are compared by identity, not by position: removing a
        // route before one that holds a key is no secret change (D-101 F-3).
        let before_secrets = before_raw.as_ref().map(secret_leaves).unwrap_or_default();
        let after_secrets = secret_leaves(&after_raw);
        let written = after_secrets
            .iter()
            .filter(|(identity, leaf)| {
                before_secrets
                    .get(*identity)
                    .is_none_or(|before| before.value != leaf.value)
            })
            .map(|(_, leaf)| leaf)
            .collect::<Vec<_>>();
        let cleared = before_secrets
            .iter()
            .filter(|(identity, _)| !after_secrets.contains_key(*identity))
            .map(|(_, leaf)| leaf)
            .collect::<Vec<_>>();
        let mut environment_paths = environment_paths;
        let mut access_paths = access_paths;
        let mut secret_credentials = Vec::new();
        for leaf in written.iter().chain(cleared.iter()) {
            // A secret that is a process environment or an access grant gets
            // its own reason (see `secret_paths::SecretUse`); every other
            // secret is a credential.
            match secret_paths::registered(&leaf.raw).map(|registered| registered.used_as) {
                Some(secret_paths::SecretUse::ProcessEnvironment) => environment_paths.push(leaf.display.clone()),
                Some(secret_paths::SecretUse::AccessGrant) => access_paths.push(leaf.display.clone()),
                Some(secret_paths::SecretUse::Credential) | None => secret_credentials.push(leaf.display.clone()),
            }
        }
        environment_paths.sort();
        environment_paths.dedup();
        access_paths.sort();
        access_paths.dedup();
        let mut secret_writes = written.iter().map(|leaf| leaf.display.clone()).collect::<Vec<_>>();
        secret_writes.extend(attached_reference_paths);
        let decision_reference_added = matches!(
            &after.causal_tree.backend.credential_ref,
            crate::causal_tree::config::CredentialRefSetting::Profile(_)
        ) && before
            .is_none_or(|before| before.causal_tree.backend.credential_ref != after.causal_tree.backend.credential_ref);
        if decision_reference_added {
            secret_writes.push("causal_tree.backend.credential_ref".to_string());
        }
        secret_writes.sort();
        secret_writes.dedup();
        let mut secret_clears = cleared.iter().map(|leaf| leaf.display.clone()).collect::<Vec<_>>();
        secret_clears.sort();
        secret_clears.dedup();
        let mut credentials = secret_credentials
            .into_iter()
            .chain(credential_paths)
            .collect::<Vec<_>>();
        credentials.sort();
        credentials.dedup();
        risk_reasons.extend(
            credentials
                .into_iter()
                .map(|path| RiskReason::CredentialChanged { path }),
        );
        risk_reasons.extend(
            environment_paths
                .into_iter()
                .map(|path| RiskReason::ProcessEnvironmentChanged { path }),
        );
        risk_reasons.extend(
            access_paths
                .into_iter()
                .map(|path| RiskReason::AccessGrantChanged { path }),
        );
        // Direction-aware settings (D-049 amendment): only a change that
        // widens who is admitted or what a principal may do is high risk.
        risk_reasons.extend(
            credential_owner_changes(before, after)
                .into_iter()
                .map(|path| RiskReason::CredentialOwnerChanged { path }),
        );
        risk_reasons.extend(workspace_mcp_widening(before, after));
        risk_reasons.extend(workspace_hooks_widening(before, after));
        if let Some(before) = before {
            risk_reasons.extend(widenings(before, after));
        }
        risk_reasons.extend(decision_plane_changes(before, after));
        if before.is_some_and(|before| before.secrets.encrypt) && !after.secrets.encrypt {
            risk_reasons.push(RiskReason::EncryptionDisabled);
        }
        // A stdio MCP server is a local program run with PRX's privileges: it
        // can read every stored credential, and it receives its `env`
        // values. Adding one, or changing what runs (`transport`, `command`,
        // `args`), is high risk whether or not it holds `env` values (D-045).
        // An HTTP server launches nothing, so only a stdio side counts.
        let is_stdio = |root: Option<&Value>, server: &str| {
            root.and_then(|root| root.pointer(&format!("/mcp/servers/{}/transport", json_pointer_escape(server))))
                .and_then(Value::as_str)
                .is_some_and(|transport| transport.eq_ignore_ascii_case("stdio"))
        };
        let mut relaunched = mcp_launch_changes
            .into_iter()
            .filter(|(server, _)| is_stdio(before_raw.as_ref(), server) || is_stdio(Some(&after_raw), server))
            .map(|(_, path)| path)
            .collect::<Vec<_>>();
        relaunched.dedup();
        risk_reasons.extend(
            relaunched
                .into_iter()
                .map(|path| RiskReason::CredentialRecipientChanged { path }),
        );
        risk_reasons.extend(
            endpoint_paths
                .into_iter()
                .map(|path| RiskReason::EndpointChanged { path }),
        );
        let risk = if risk_reasons.is_empty() {
            ConfigChangeRisk::Low
        } else {
            ConfigChangeRisk::High
        };
        Ok(Self {
            changes,
            risk,
            risk_reasons,
            secret_writes,
            secret_clears,
        })
    }

    /// The summary of replacing the stored value of credential reference
    /// `id` (the auth-profile store, not a configuration file): one secret
    /// entry, high risk. Names the reference only.
    #[must_use]
    pub fn credential_reference_replaced(id: &str) -> Self {
        let path = format!("{CREDENTIAL_REFERENCES_PATH}{id}");
        let mask = || Some(Value::String(secret_paths::REDACTION_MASK.to_string()));
        Self {
            changes: vec![ConfigChangeEntry {
                path: path.clone(),
                kind: ChangeKind::Changed,
                before: mask(),
                after: mask(),
                secret: true,
            }],
            risk: ConfigChangeRisk::High,
            risk_reasons: vec![RiskReason::CredentialChanged { path: path.clone() }],
            secret_writes: vec![path],
            secret_clears: Vec::new(),
        }
    }

    /// Whether nothing changed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// One secret value of a rendered configuration.
struct SecretLeaf {
    /// Its concrete path, for the registry.
    raw: Vec<PathElem>,
    /// Its display path (a URL-keyed table's key redacted).
    display: String,
    value: String,
}

/// Every non-empty secret value of a rendered configuration, keyed by its
/// identity: the path with every array element named by what identifies it
/// (a table's `hint`, a string's own value) instead of its position, so a
/// removed or reordered neighbour changes no secret (D-101 F-3). Identities
/// are compared in memory only; nothing shows them.
fn secret_leaves(root: &Value) -> std::collections::BTreeMap<String, SecretLeaf> {
    fn walk(
        node: &Value,
        raw: &mut Vec<PathElem>,
        display: &mut Vec<PathElem>,
        identity: &mut Vec<String>,
        inside_secret: bool,
        out: &mut std::collections::BTreeMap<String, SecretLeaf>,
    ) {
        let inside_secret = inside_secret || (!raw.is_empty() && secret_paths::is_secret_path(raw));
        match node {
            Value::Object(map) => {
                let key_table = secret_paths::is_url_userinfo_key_table(raw);
                for (key, child) in map {
                    let shown = if key_table {
                        crate::security::redaction::redact_url_value(key).into_owned()
                    } else {
                        key.clone()
                    };
                    raw.push(PathElem::Key(key.clone()));
                    display.push(PathElem::Key(shown));
                    identity.push(format!(".{key}"));
                    walk(child, raw, display, identity, inside_secret, out);
                    identity.pop();
                    display.pop();
                    raw.pop();
                }
            }
            Value::Array(items) => {
                let hint_of = |item: &Value| item.get("hint").and_then(Value::as_str).map(str::to_string);
                for (index, child) in items.iter().enumerate() {
                    let named = match child {
                        // A hint that repeats in the array names its element
                        // together with its position, so no two elements
                        // share an identity (D-104 C-1).
                        Value::Object(fields) => fields.get("hint").and_then(Value::as_str).map(|hint| {
                            let repeated = items
                                .iter()
                                .filter(|item| hint_of(item).as_deref() == Some(hint))
                                .count()
                                > 1;
                            if repeated {
                                format!("[hint={hint}#{index}]")
                            } else {
                                format!("[hint={hint}]")
                            }
                        }),
                        Value::String(value) if inside_secret => Some(format!("[={value}]")),
                        _ => None,
                    };
                    raw.push(PathElem::Index(index));
                    display.push(PathElem::Index(index));
                    identity.push(named.unwrap_or_else(|| format!("[{index}]")));
                    walk(child, raw, display, identity, inside_secret, out);
                    identity.pop();
                    display.pop();
                    raw.pop();
                }
            }
            Value::String(value) if inside_secret && !value.is_empty() => {
                out.insert(
                    identity.concat(),
                    SecretLeaf {
                        raw: raw.clone(),
                        display: secret_paths::display_path(display),
                        value: value.clone(),
                    },
                );
            }
            _ => {}
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, &mut Vec::new(), &mut Vec::new(), &mut Vec::new(), false, &mut out);
    out
}

/// One side of the comparison: the unredacted value decides whether
/// something changed, the redacted one is what is shown.
#[derive(Clone, Copy)]
struct Sides<'a> {
    raw: Option<&'a Value>,
    shown: Option<&'a Value>,
}

impl<'a> Sides<'a> {
    /// The child at `raw_elem` on the unredacted side and at `shown_elem`
    /// on the redacted side (they differ for a redacted userinfo key).
    fn child(self, raw_elem: &PathElem, shown_elem: &PathElem) -> Self {
        let pick = |value: Option<&'a Value>, elem: &PathElem| {
            value.and_then(|value| match (elem, value) {
                (PathElem::Key(key), Value::Object(map)) => map.get(key),
                (PathElem::Index(index), Value::Array(items)) => items.get(*index),
                _ => None,
            })
        };
        Self {
            raw: pick(self.raw, raw_elem),
            shown: pick(self.shown, shown_elem),
        }
    }

    /// The value, treating an explicit null as absent.
    fn present(self) -> Option<&'a Value> {
        self.raw.filter(|value| !value.is_null())
    }
}

/// What the walk collects besides the change entries.
#[derive(Default)]
struct DiffContext {
    /// Whether there is a prior configuration. Without one (a first write),
    /// every setting is "added", and a default token endpoint coming into
    /// existence re-points nothing.
    has_before: bool,
    out: Vec<ConfigChangeEntry>,
    /// Display paths of changes that write URL userinfo: a provider or
    /// endpoint value carrying `user:password@`, or any setting under a
    /// credentialed `cost.prices` key.
    credential_paths: Vec<String>,
    /// Stored credential references newly attached to a route or agent.
    attached_reference_paths: Vec<String>,
    /// Display paths of changes that point a provider at another endpoint.
    endpoint_paths: Vec<String>,
    /// MCP servers whose launch (`command`, `args`, `transport`) changed:
    /// (server name, display path).
    mcp_launch_changes: Vec<(String, String)>,
    /// Display paths of changed secrets registered as a process environment.
    environment_paths: Vec<String>,
    /// Display paths of changed secrets registered as an access grant.
    access_paths: Vec<String>,
}

/// How the walk reached a node.
#[derive(Clone, Copy)]
struct Reach {
    /// Under a secret path: every leaf is a secret.
    inside_secret: bool,
    /// Under a `cost.prices` key that carries URL userinfo.
    credentialed_key: bool,
}

/// `raw` is the real path (it decides secrets and registries); `display` is
/// the same path with every key of a userinfo key table redacted, so no
/// credentialed key ever reaches a plan, a history record or a tool output.
fn diff(
    before: Sides<'_>,
    after: Sides<'_>,
    raw: &mut Vec<PathElem>,
    display: &mut Vec<PathElem>,
    reach: Reach,
    ctx: &mut DiffContext,
) {
    let reach = Reach {
        inside_secret: reach.inside_secret || (!raw.is_empty() && secret_paths::is_secret_path(raw)),
        ..reach
    };
    let key_table = secret_paths::is_url_userinfo_key_table(raw);
    let descend = |elem: PathElem, ctx: &mut DiffContext, raw: &mut Vec<PathElem>, display: &mut Vec<PathElem>| {
        let (shown_elem, credentialed_key) = match &elem {
            // The display key goes through the one URL redactor (D-088 C-6);
            // only URL userinfo makes the key itself a credential.
            PathElem::Key(key) if key_table => (
                PathElem::Key(crate::security::redaction::redact_url_value(key).into_owned()),
                matches!(
                    crate::providers::redact_endpoint_userinfo(key),
                    std::borrow::Cow::Owned(_)
                ),
            ),
            _ => (elem.clone(), false),
        };
        let child_reach = Reach {
            credentialed_key: reach.credentialed_key || credentialed_key,
            ..reach
        };
        let (left, right) = (before.child(&elem, &shown_elem), after.child(&elem, &shown_elem));
        raw.push(elem);
        display.push(shown_elem);
        diff(left, right, raw, display, child_reach, ctx);
        raw.pop();
        display.pop();
    };
    match (before.present(), after.present()) {
        (Some(Value::Object(left)), Some(Value::Object(right))) => {
            let mut keys = left.keys().chain(right.keys()).cloned().collect::<Vec<_>>();
            keys.sort();
            keys.dedup();
            for key in keys {
                descend(PathElem::Key(key), ctx, raw, display);
            }
        }
        (Some(Value::Array(left)), Some(Value::Array(right))) => {
            for index in 0..left.len().max(right.len()) {
                descend(PathElem::Index(index), ctx, raw, display);
            }
        }
        // A container on one side only: report its leaves one by one, so
        // each carries its own redaction.
        (None, Some(Value::Object(_) | Value::Array(_))) | (Some(Value::Object(_) | Value::Array(_)), None) => {
            for elem in child_elems(before.present().or_else(|| after.present())) {
                descend(elem, ctx, raw, display);
            }
        }
        (None, None) => {}
        (left, right) => {
            if left == right {
                return;
            }
            let kind = match (left, right) {
                (None, Some(_)) => ChangeKind::Added,
                (Some(_), None) => ChangeKind::Removed,
                _ => ChangeKind::Changed,
            };
            // A credential is a string. A switch or a number under a
            // secret-named table (`secrets.encrypt`) is a setting, and an
            // empty string holds no credential.
            let holds_credential =
                |value: Option<&Value>| value.and_then(Value::as_str).is_some_and(|text| !text.is_empty());
            let secret = reach.inside_secret && (holds_credential(left) || holds_credential(right));
            let shown = |side: Sides<'_>| {
                side.present().map(|_| {
                    if secret {
                        Value::String(secret_paths::REDACTION_MASK.to_string())
                    } else {
                        side.shown.cloned().unwrap_or(Value::Null)
                    }
                })
            };
            let path = secret_paths::display_path(display);
            let userinfo_path = secret_paths::is_url_userinfo_path(raw);
            let carries_userinfo = |value: Option<&Value>| {
                value.and_then(Value::as_str).is_some_and(|text| {
                    matches!(
                        crate::providers::redact_endpoint_userinfo(text),
                        std::borrow::Cow::Owned(_)
                    )
                })
            };
            if reach.credentialed_key || (userinfo_path && (carries_userinfo(left) || carries_userinfo(right))) {
                ctx.credential_paths.push(path.clone());
            }
            // A token endpoint counts when it is re-pointed or set on an
            // existing configuration; every other endpoint rule as before.
            // An empty value names no endpoint: a section that starts being
            // written with its empty default re-points nothing.
            let names_endpoint = |value: Option<&Value>| {
                value
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
            };
            let token_endpoint = secret_paths::is_credential_recipient_endpoint(raw)
                && (left.is_some() || ctx.has_before)
                && (names_endpoint(left) || names_endpoint(right));
            if token_endpoint || is_endpoint_change(raw, userinfo_path, left, right) {
                ctx.endpoint_paths.push(path.clone());
            }
            // A setting that decides who is admitted inbound (D-049 A: pairing,
            // JWT and SPIFFE trust, sender allow-lists, identity bindings) is
            // an access grant like an admission secret. A first write
            // (no prior configuration) grants nothing new.
            if ctx.has_before && secret_paths::is_access_control_path(raw) && !ctx.access_paths.contains(&path) {
                ctx.access_paths.push(path.clone());
            }
            if let Some(server) = crate::config::mcp_launch::launch_server(raw) {
                ctx.mcp_launch_changes.push((server.to_string(), path.clone()));
            }
            // A route or agent `credential_ref` naming a stored credential
            // hands it to the selection's provider (PMM-H). The decision
            // backend's reference is classified by `decision_plane_changes`
            // so it contributes exactly one reason under the I2 contract.
            // `""` and `"none"` name no credential on these generic paths.
            let names_credential = right.and_then(Value::as_str).is_some_and(|value| {
                let value = value.trim();
                !value.is_empty() && value != "none"
            });
            if names_credential
                && path != "causal_tree.backend.credential_ref"
                && matches!(raw.last(), Some(PathElem::Key(key)) if key == "credential_ref")
            {
                ctx.credential_paths.push(path.clone());
                ctx.attached_reference_paths.push(path.clone());
            }
            ctx.out.push(ConfigChangeEntry {
                path,
                kind,
                before: shown(before),
                after: shown(after),
                secret,
            });
        }
    }
}

fn child_elems(value: Option<&Value>) -> Vec<PathElem> {
    match value {
        Some(Value::Object(map)) => map.keys().cloned().map(PathElem::Key).collect(),
        Some(Value::Array(items)) => (0..items.len()).map(PathElem::Index).collect(),
        _ => Vec::new(),
    }
}

/// Keys that name the endpoint a provider credential is sent to, wherever
/// they appear.
const ENDPOINT_KEYS: &[&str] = &["api_url", "base_url"];

/// Provider spellings that carry their own endpoint URL.
const ENDPOINT_PROVIDER_PREFIXES: &[&str] = &["custom:", "anthropic-custom:"];

/// Escape a key for a JSON pointer (RFC 6901).
fn json_pointer_escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Whether a changed leaf points a provider at another endpoint: an
/// `api_url`/`base_url` anywhere, or a provider field of the URL registry
/// (`secret_paths::ENDPOINT_PATHS`: default, fallback, route, agent,
/// embedding and classifier providers, ...) or any `provider` key, whose old
/// or new value names a `custom:` endpoint. Credential-recipient endpoints
/// (`secret_paths::ENDPOINT_PATHS`) are checked by the diff, which knows
/// whether a prior configuration exists.
fn is_endpoint_change(raw: &[PathElem], userinfo_path: bool, left: Option<&Value>, right: Option<&Value>) -> bool {
    let key = match raw.last() {
        Some(PathElem::Key(key)) => Some(key.as_str()),
        _ => None,
    };
    if key.is_some_and(|key| ENDPOINT_KEYS.contains(&key)) {
        return true;
    }
    if !userinfo_path && !matches!(key, Some("provider" | "default_provider")) {
        return false;
    }
    [left, right].into_iter().any(|value| {
        value.and_then(Value::as_str).is_some_and(|provider| {
            let provider = provider.trim().to_ascii_lowercase();
            ENDPOINT_PROVIDER_PREFIXES
                .iter()
                .any(|prefix| provider.starts_with(prefix))
        })
    })
}

/// Every place where a stored credential now goes to a different provider
/// (D-085 F-3), by the path of the provider field that now receives it.
///
/// The check follows the credential, not the slot (D-088 C-1): each key
/// value held before (the root connection under the default provider,
/// every route, embedding route and agent) is mapped to the providers that
/// held it, and every holder after the change whose key was held before by
/// other providers only is reported. A hint rename, an index shift or a key
/// copied into another slot cannot move a key unseen. Providers are compared
/// by identity, so a respelling is no change. A first write hands nothing
/// over; a key that is new is a credential change, reported as such.
fn credential_owner_changes(before: Option<&Config>, after: &Config) -> Vec<String> {
    use crate::providers::ProviderId;
    use std::collections::HashMap;
    let Some(before) = before else {
        return Vec::new();
    };
    let mut owners: HashMap<&str, Vec<ProviderId>> = HashMap::new();
    for (_, provider, key) in credential_holders(before) {
        let entry = owners.entry(key).or_default();
        if !entry.contains(&provider) {
            entry.push(provider);
        }
    }
    let mut paths: Vec<String> = Vec::new();
    for (path, provider, key) in credential_holders(after) {
        let moved = owners.get(key).is_some_and(|previous| !previous.contains(&provider));
        if moved && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// Every stored provider credential of `config` with the provider it is
/// sent to and the path of that provider field: the root connection
/// (`api_key`, each `reliability.api_keys` entry) under the default
/// provider, and each route, embedding route and agent that holds its own
/// `api_key`. Empty values hold nothing.
fn credential_holders(config: &Config) -> Vec<(String, crate::providers::ProviderId, &str)> {
    use crate::providers::ProviderId;
    fn held(key: Option<&String>) -> Option<&str> {
        key.map(|key| key.trim()).filter(|key| !key.is_empty())
    }
    let mut holders = Vec::new();
    let default = ProviderId::of(crate::providers::selection::default_provider(config));
    for key in held(config.api_key.as_ref())
        .into_iter()
        .chain(config.reliability.api_keys.iter().filter_map(|key| held(Some(key))))
    {
        holders.push(("default_provider".to_string(), default.clone(), key));
    }
    for (index, route) in config.model_routes.iter().enumerate() {
        if let Some(key) = held(route.api_key.as_ref()) {
            holders.push((
                format!("model_routes[{index}].provider"),
                ProviderId::of(&route.provider),
                key,
            ));
        }
    }
    for (index, route) in config.embedding_routes.iter().enumerate() {
        if let Some(key) = held(route.api_key.as_ref()) {
            holders.push((
                format!("embedding_routes[{index}].provider"),
                ProviderId::of(&route.provider),
                key,
            ));
        }
    }
    let mut agents = config.agents.iter().collect::<Vec<_>>();
    agents.sort_by(|left, right| left.0.cmp(right.0));
    for (name, agent) in agents {
        if let Some(key) = held(agent.api_key.as_ref()) {
            holders.push((format!("agents.{name}.provider"), ProviderId::of(&agent.provider), key));
        }
    }
    holders
}

/// Turning `security.workspace_hooks` on (HOOKS-ENV): the workspace
/// `hooks.json`, which the agent's file tools can write, then starts
/// programs that run as PRX's user and can read the stored credentials. A
/// first write counts from `off`.
fn workspace_hooks_widening(before: Option<&Config>, after: &Config) -> Option<RiskReason> {
    use crate::config::WorkspaceHooks;
    let before = before.map_or(WorkspaceHooks::Off, |before| before.security.workspace_hooks);
    (after.security.workspace_hooks > before).then(|| RiskReason::CredentialRecipientChanged {
        path: "security.workspace_hooks".to_string(),
    })
}

/// Widening `mcp.workspace_servers` (D-085 F-1): letting a workspace
/// `mcp.json` add stdio servers lets it start programs that run as PRX's
/// user (they can read the stored credentials); letting it add HTTP servers
/// widens what the agent's tools reach. A first write counts from `off`.
fn workspace_mcp_widening(before: Option<&Config>, after: &Config) -> Option<RiskReason> {
    use crate::config::WorkspaceMcpServers;
    let before = before.map_or(WorkspaceMcpServers::Off, |before| before.mcp.workspace_servers);
    let after = after.mcp.workspace_servers;
    if after <= before {
        return None;
    }
    let path = "mcp.workspace_servers".to_string();
    Some(if after == WorkspaceMcpServers::Stdio {
        RiskReason::CredentialRecipientChanged { path }
    } else {
        RiskReason::PrivilegeWidened { path }
    })
}

/// The direction-aware part of the classification (D-049 amendment). Each
/// setting here is compared on the typed configuration, and only its
/// widening direction is a risk reason; the narrowing direction stays low.
/// Where a change cannot be shown to narrow (a reordered or rewritten scope
/// rule), it counts as widening.
fn widenings(before: &Config, after: &Config) -> Vec<RiskReason> {
    let mut reasons = Vec::new();
    listen_scope_widenings(before, after, &mut reasons);
    user_policy_widenings(before, after, &mut reasons);
    scope_widenings(&before.autonomy.scopes, &after.autonomy.scopes, &mut reasons);
    if adds_entries(&before.mcp_server.exposed_tools, &after.mcp_server.exposed_tools) {
        reasons.push(RiskReason::PrivilegeWidened {
            path: "mcp_server.exposed_tools".to_string(),
        });
    }
    if before.nodes.server.tls_required && !after.nodes.server.tls_required {
        reasons.push(RiskReason::PrivilegeWidened {
            path: "nodes.server.tls_required".to_string(),
        });
    }
    exposure_and_execution_widenings(before, after, &mut reasons);
    reasons
}

/// The decision plane's own high-risk changes (CDP-04, SP-CDP-A 5.4), read
/// from the typed sections so a rename cannot hide one. With no loadable
/// prior configuration the prior backend is the default: local, nothing
/// remote. The endpoint is a credential recipient in
/// [`secret_paths::ENDPOINT_PATHS`], so re-pointing it is an
/// [`RiskReason::EndpointChanged`] from the tree walk above.
fn decision_plane_changes(before: Option<&Config>, after: &Config) -> Vec<RiskReason> {
    let default_section = crate::causal_tree::config::CausalTreeConfig::default();
    let old_section = before.map_or(&default_section, |before| &before.causal_tree);
    let (old, new) = (&old_section.backend, &after.causal_tree.backend);
    let mut reasons = Vec::new();
    let path = |field: &str| format!("causal_tree.backend.{field}");
    let target_changes = [
        ("kind", old.kind != new.kind),
        ("endpoint", old.endpoint.trim() != new.endpoint.trim()),
        ("model", old.model.trim() != new.model.trim()),
        ("credential_ref", old.credential_ref != new.credential_ref),
    ];
    if !old.remote_enabled && new.remote_enabled {
        // Switching remote processing on is high whatever the target.
        reasons.push(RiskReason::RemoteDecisionEnabled {
            path: path("remote_enabled"),
        });
    } else if sends_remotely(&after.causal_tree) {
        if sends_remotely(old_section) {
            // Still remote: a change to where or how decisions are sent.
            reasons.extend(
                target_changes
                    .iter()
                    .filter(|(_, changed)| *changed)
                    .map(|(field, _)| RiskReason::RemoteDecisionEnabled { path: path(field) }),
            );
        } else if !old_section.enabled {
            // L-1: enabling the plane starts remote I/O to a configured
            // remote target.
            reasons.push(RiskReason::RemoteDecisionEnabled {
                path: "causal_tree.enabled".to_string(),
            });
        } else {
            // The target became remote.
            let changed = target_changes
                .iter()
                .find(|(_, changed)| *changed)
                .map_or("kind", |(field, _)| field);
            reasons.push(RiskReason::RemoteDecisionEnabled { path: path(changed) });
        }
    }
    // Which credential the decision backend uses is a credential change,
    // whether or not remote processing is on.
    if old.credential_ref != new.credential_ref {
        reasons.push(RiskReason::CredentialChanged {
            path: path("credential_ref"),
        });
    }
    reasons
}

/// Whether `section` sends decision state off this machine (D-079 M-2): the
/// plane is enabled, remote processing is on, and the configured backend is
/// remote by the factory's own rule (`configured_kind`, `locality_of`;
/// D-082 N-5): a hosted kind, or a compatible endpoint that is not a literal
/// loopback address. The builtin, `auto` without an endpoint and a loopback
/// endpoint are local.
fn sends_remotely(section: &crate::causal_tree::config::CausalTreeConfig) -> bool {
    section.enabled
        && section.backend.remote_enabled
        && crate::decision::factory::configured_kind(section).is_some_and(|kind| {
            crate::decision::backend::locality_of(kind, section.backend.endpoint.trim())
                == crate::decision::BackendLocality::Remote
        })
}

/// How much an autonomy level lets the agent do.
const fn autonomy_rank(level: crate::security::AutonomyLevel) -> u8 {
    match level {
        crate::security::AutonomyLevel::ReadOnly => 0,
        crate::security::AutonomyLevel::Supervised => 1,
        crate::security::AutonomyLevel::Full => 2,
    }
}

/// D-050 item 4, each in its widening direction only:
///
/// - public exposure: `tunnel.provider` switched to a provider (anything but
///   `none`; a tailnet or a public tunnel both reach the gateway from
///   outside), and `tunnel.tailscale.funnel` turned on (publishes it);
/// - `tunnel.custom.start_command` set or changed: a command the daemon
///   runs, like an MCP server launch (D-045); clearing it narrows;
/// - `autonomy.level` raised, `autonomy.workspace_only` turned off, an
///   `autonomy.forbidden_paths` entry removed;
/// - `nodes.server.allowed_commands` given an entry, a
///   `nodes.server.blocked_commands` entry removed.
fn exposure_and_execution_widenings(before: &Config, after: &Config, reasons: &mut Vec<RiskReason>) {
    let (old_tunnel, new_tunnel) = (&before.tunnel, &after.tunnel);
    if new_tunnel.provider != old_tunnel.provider && new_tunnel.provider != "none" {
        reasons.push(RiskReason::AccessGrantChanged {
            path: "tunnel.provider".to_string(),
        });
    }
    let funnel = |tunnel: &crate::config::TunnelConfig| tunnel.tailscale.as_ref().is_some_and(|ts| ts.funnel);
    if funnel(new_tunnel) && !funnel(old_tunnel) {
        reasons.push(RiskReason::AccessGrantChanged {
            path: "tunnel.tailscale.funnel".to_string(),
        });
    }
    let command = |tunnel: &crate::config::TunnelConfig| {
        tunnel
            .custom
            .as_ref()
            .map(|custom| custom.start_command.trim().to_string())
            .filter(|command| !command.is_empty())
    };
    if let Some(new_command) = command(new_tunnel) {
        if command(old_tunnel).as_deref() != Some(new_command.as_str()) {
            reasons.push(RiskReason::PrivilegeWidened {
                path: "tunnel.custom.start_command".to_string(),
            });
        }
    }

    let mut push = |widened: bool, path: &str| {
        if widened {
            reasons.push(RiskReason::PrivilegeWidened { path: path.to_string() });
        }
    };
    let (old_autonomy, new_autonomy) = (&before.autonomy, &after.autonomy);
    push(
        autonomy_rank(new_autonomy.level) > autonomy_rank(old_autonomy.level),
        "autonomy.level",
    );
    push(
        old_autonomy.workspace_only && !new_autonomy.workspace_only,
        "autonomy.workspace_only",
    );
    push(
        drops_entries(&old_autonomy.forbidden_paths, &new_autonomy.forbidden_paths),
        "autonomy.forbidden_paths",
    );
    let (old_server, new_server) = (&before.nodes.server, &after.nodes.server);
    push(
        adds_entries(&old_server.allowed_commands, &new_server.allowed_commands),
        "nodes.server.allowed_commands",
    );
    push(
        drops_entries(&old_server.blocked_commands, &new_server.blocked_commands),
        "nodes.server.blocked_commands",
    );
    containment_widenings(before, after, reasons);
}

/// The tools a channel session is not offered, with the runtime precedence
/// (D-055): every `always_exclude` name, which capability publication applies before
/// `always_include` on every surface, plus each `channel_exclude` name that
/// `always_include` does not re-admit (`ToolTieringConfig::for_channel_surface`).
fn channel_excluded_tools(tiering: &crate::config::ToolTieringConfig) -> Vec<String> {
    tiering.for_channel_surface().always_exclude
}

/// How far a Docker network lets the sandbox reach, as `runtime::docker`
/// passes it (trimmed; empty means Docker's default bridge): `none` < any
/// named network < `host`.
fn docker_network_rank(network: &str) -> u8 {
    match network.trim() {
        "none" => 0,
        "host" => 2,
        _ => 1,
    }
}

/// D-051, each in its widening direction only (`privilege_widened`):
///
/// - `runtime.kind` leaving `docker` (the sandbox is removed);
/// - `runtime.docker.network` reaching further: `none` to any network, any
///   network to `host`, or one named network to another (a different set of
///   reachable containers; the default bridge is the empty value);
/// - a tool newly offered to channel sessions, judged on the effective
///   channel surface (`ToolTieringConfig::for_channel_surface`, D-052 and
///   D-055): `always_exclude` wins on every surface, and `always_include`
///   overrides only a `channel_exclude` name. So an `always_exclude` entry
///   removed, a `channel_exclude` entry removed, or an `always_include`
///   entry that re-admits a `channel_exclude` name is a widening;
/// - the audit trail weakened (D-052): `security.audit.max_size_mb`
///   lowered (a smaller size rotates the trail away sooner; zero is
///   refused at load) or `security.audit.log_path` moved;
/// - D-051 amendment: `runtime.docker.read_only_rootfs` turned off;
///   `runtime.docker.allowed_workspace_roots` given a root or emptied (an
///   empty list lets any workspace be mounted, `runtime::docker`);
///   `browser.allowed_domains` given a domain, a wildcard included, or
///   emptied (an empty list lets `web_fetch` reach any public host),
///   compared after the tools' own normalisation;
/// - D-052: `runtime.docker.mount_workspace` turned on (the container gains
///   a read-write mount of the host workspace).
fn containment_widenings(before: &Config, after: &Config, reasons: &mut Vec<RiskReason>) {
    let mut push = |widened: bool, path: &str| {
        if widened {
            reasons.push(RiskReason::PrivilegeWidened { path: path.to_string() });
        }
    };
    push(
        before.runtime.kind == "docker" && after.runtime.kind != "docker",
        "runtime.kind",
    );
    let (old_network, new_network) = (
        before.runtime.docker.network.trim(),
        after.runtime.docker.network.trim(),
    );
    let (old_rank, new_rank) = (docker_network_rank(old_network), docker_network_rank(new_network));
    push(
        new_rank > old_rank || (new_rank == 1 && old_rank == 1 && new_network != old_network),
        "runtime.docker.network",
    );
    push(
        drops_entries(
            &channel_excluded_tools(&before.tool_tiering),
            &channel_excluded_tools(&after.tool_tiering),
        ),
        "tool_tiering",
    );
    push(
        after.security.audit.max_size_mb < before.security.audit.max_size_mb,
        "security.audit.max_size_mb",
    );
    push(
        after.security.audit.log_path != before.security.audit.log_path,
        "security.audit.log_path",
    );
    let (old_docker, new_docker) = (&before.runtime.docker, &after.runtime.docker);
    push(
        old_docker.read_only_rootfs && !new_docker.read_only_rootfs,
        "runtime.docker.read_only_rootfs",
    );
    push(
        !old_docker.mount_workspace && new_docker.mount_workspace,
        "runtime.docker.mount_workspace",
    );
    push(
        tools_allow_widens(&old_docker.allowed_workspace_roots, &new_docker.allowed_workspace_roots),
        "runtime.docker.allowed_workspace_roots",
    );
    let domains =
        |config: &Config| crate::tools::web_fetch::normalize_allowed_domains(config.browser.allowed_domains.clone());
    push(
        tools_allow_widens(&domains(before), &domains(after)),
        "browser.allowed_domains",
    );
}

/// Whether `after` holds an entry `before` does not.
fn adds_entries(before: &[String], after: &[String]) -> bool {
    after.iter().any(|entry| !before.contains(entry))
}

/// Whether `before` holds an entry `after` dropped.
fn drops_entries(before: &[String], after: &[String]) -> bool {
    adds_entries(after, before)
}

/// Whether a sender allow-list is narrower than any listening scope: it has
/// no `"*"`, and it names senders or, where the channel reads an empty list
/// as "nobody" (Discord, Slack, Mattermost, Matrix, IRC), is empty. Signal
/// reads an empty `allowed_from` through its DM and group policies, so an
/// empty list there does not count as narrow.
fn names_its_senders(allow_list: &[String], empty_admits_nobody: bool) -> bool {
    !allow_list.iter().any(|entry| entry.trim() == "*") && (empty_admits_nobody || !allow_list.is_empty())
}

/// An optional listening scope (`None` listens everywhere) admits a new
/// audience when it is dropped or moved; setting one where there was none
/// narrows.
fn optional_scope_widens(before: Option<&str>, after: Option<&str>) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => before.trim() != after.trim(),
        (Some(_), None) => true,
        (None, Some(_) | None) => false,
    }
}

/// A channel's listening scope (the guild, room or channel it serves)
/// admits a new audience, unless the channel's own sender allow-list is
/// narrower than any scope (D-049 amendment). A channel added or removed as
/// a whole is classified by its credentials, not here.
fn listen_scope_widenings(before: &Config, after: &Config, reasons: &mut Vec<RiskReason>) {
    let mut push = |widened: bool, narrow_senders: bool, path: &str| {
        if widened && !narrow_senders {
            reasons.push(RiskReason::AccessGrantChanged { path: path.to_string() });
        }
    };
    let (b, a) = (&before.channels_config, &after.channels_config);
    if let (Some(b), Some(a)) = (&b.discord, &a.discord) {
        push(
            optional_scope_widens(b.guild_id.as_deref(), a.guild_id.as_deref()),
            names_its_senders(&a.allowed_users, true),
            "channels_config.discord.guild_id",
        );
    }
    if let (Some(b), Some(a)) = (&b.slack, &a.slack) {
        push(
            optional_scope_widens(b.channel_id.as_deref(), a.channel_id.as_deref()),
            names_its_senders(&a.allowed_users, true),
            "channels_config.slack.channel_id",
        );
    }
    if let (Some(b), Some(a)) = (&b.mattermost, &a.mattermost) {
        push(
            optional_scope_widens(b.channel_id.as_deref(), a.channel_id.as_deref()),
            names_its_senders(&a.allowed_users, true),
            "channels_config.mattermost.channel_id",
        );
    }
    if let (Some(b), Some(a)) = (&b.matrix, &a.matrix) {
        push(
            b.room_id.trim() != a.room_id.trim(),
            names_its_senders(&a.allowed_users, true),
            "channels_config.matrix.room_id",
        );
    }
    if let (Some(b), Some(a)) = (&b.signal, &a.signal) {
        push(
            optional_scope_widens(b.group_id.as_deref(), a.group_id.as_deref()),
            names_its_senders(&a.allowed_from, false),
            "channels_config.signal.group_id",
        );
    }
    if let (Some(b), Some(a)) = (&b.irc, &a.irc) {
        push(
            adds_entries(&b.channels, &a.channels),
            names_its_senders(&a.allowed_users, true),
            "channels_config.irc.channels",
        );
    }
}

/// The rank of a `user_policies` role, read by `memory::principal` itself
/// (the load refuses an unknown one, D-050).
fn role_rank(role: &str) -> u8 {
    crate::memory::principal::Role::parse(role)
        .unwrap_or(crate::memory::principal::Role::Guest)
        .privilege_rank()
}

/// The rank of a visibility ceiling, read by `memory::principal` itself.
fn visibility_rank(ceiling: &str) -> u8 {
    crate::memory::principal::Visibility::parse(ceiling)
        .unwrap_or(crate::memory::principal::Visibility::Private)
        .ordinal()
}

/// A user policy widens what its user may do when it grants a higher role,
/// a project, a higher visibility ceiling or drops a blocked pattern. A user
/// without a policy is a guest with no projects, a `private` ceiling and no
/// blocked patterns, so adding or removing a whole policy is compared
/// against that default.
fn user_policy_widenings(before: &Config, after: &Config, reasons: &mut Vec<RiskReason>) {
    let default_policy = crate::config::schema::UserPolicyConfig {
        user_id: String::new(),
        role: "guest".to_string(),
        projects: Vec::new(),
        visibility_ceiling: "private".to_string(),
        blocked_patterns: Vec::new(),
    };
    let policy_of = |policies: &[crate::config::schema::UserPolicyConfig], user: &str| {
        policies.iter().rposition(|policy| policy.user_id == user)
    };
    let mut users = before
        .user_policies
        .iter()
        .chain(&after.user_policies)
        .map(|policy| policy.user_id.as_str())
        .collect::<Vec<_>>();
    users.sort_unstable();
    users.dedup();
    for user in users {
        let before_index = policy_of(before.user_policies.as_slice(), user);
        let after_index = policy_of(after.user_policies.as_slice(), user);
        let old = before_index
            .and_then(|index| before.user_policies.get(index))
            .unwrap_or(&default_policy);
        let new = after_index
            .and_then(|index| after.user_policies.get(index))
            .unwrap_or(&default_policy);
        let base = match (after_index, before_index) {
            (Some(index), _) | (None, Some(index)) => format!("user_policies[{index}]"),
            (None, None) => continue,
        };
        for (widened, field) in [
            (role_rank(&new.role) > role_rank(&old.role), "role"),
            (adds_entries(&old.projects, &new.projects), "projects"),
            (
                visibility_rank(&new.visibility_ceiling) > visibility_rank(&old.visibility_ceiling),
                "visibility_ceiling",
            ),
            (
                drops_entries(&old.blocked_patterns, &new.blocked_patterns),
                "blocked_patterns",
            ),
        ] {
            if widened {
                reasons.push(RiskReason::PrivilegeWidened {
                    path: format!("{base}.{field}"),
                });
            }
        }
    }
}

/// `tools_allow`: an empty list means every tool (the first matching rule
/// decides, `SecurityPolicy::is_tool_allowed`). It widens when emptied or
/// given a tool a non-empty list did not hold; introducing a list narrows.
fn tools_allow_widens(before: &[String], after: &[String]) -> bool {
    if after.is_empty() {
        !before.is_empty()
    } else {
        !before.is_empty() && adds_entries(before, after)
    }
}

/// `send_allow`: an empty list says nothing; the first matching rule with a
/// non-empty list *is* the whitelist, and without one only the reply channel
/// is reachable (`SecurityPolicy::is_outbound_allowed_for_turn`). So any
/// added entry widens, an empty list included (`[]` to `["*"]` grants every
/// recipient), and emptying widens too: the decision falls through to a
/// later whitelist or to the reply channel, which the old list may not have
/// named. Only removing entries while some remain narrows.
fn send_allow_widens(before: &[String], after: &[String]) -> bool {
    adds_entries(before, after) || (after.is_empty() && !before.is_empty())
}

/// Whether two scope rules can match the same request: they are disjoint
/// only when some criterion names two different concrete values
/// (`rule_matches` treats `None` and `"*"` as "any").
fn matchers_may_overlap(a: &crate::config::schema::ScopeRule, b: &crate::config::schema::ScopeRule) -> bool {
    let compatible = |x: &Option<String>, y: &Option<String>| match (x.as_deref(), y.as_deref()) {
        (Some(x), Some(y)) => x == "*" || y == "*" || x == y,
        _ => true,
    };
    compatible(&a.user, &b.user) && compatible(&a.channel, &b.channel) && compatible(&a.chat_type, &b.chat_type)
}

/// `assign_allow`: assignment is default-deny, and the first matching rule
/// with a non-empty list is the whitelist (`is_assignment_allowed`). An
/// added entry widens. Emptying rule `index`'s list hands the decision to
/// the next matching rule with a list, which may be wider: it widens when a
/// later rule that can match the same requests carries one; with none, the
/// request falls through to deny, which narrows.
fn assign_allow_widens(after: &crate::config::schema::ScopeConfig, index: usize, before: &[String]) -> bool {
    let Some(rule) = after.rules.get(index) else {
        return true;
    };
    if adds_entries(before, &rule.assign_allow) {
        return true;
    }
    rule.assign_allow.is_empty()
        && !before.is_empty()
        && after
            .rules
            .iter()
            .skip(index + 1)
            .any(|later| !later.assign_allow.is_empty() && matchers_may_overlap(rule, later))
}

/// Whether a scope rule matches the same requests in both configurations.
fn same_matcher(before: &crate::config::schema::ScopeRule, after: &crate::config::schema::ScopeRule) -> bool {
    before.user == after.user && before.channel == after.channel && before.chat_type == after.chat_type
}

/// An `autonomy.scopes` change widens what a principal may do unless it is
/// provably narrower (D-049 amendment):
///
/// - `default` from `deny` to anything else widens;
/// - an added `assign_owners` entry widens;
/// - for a rule kept in place with the same matcher, each list by its
///   runtime meaning (D-050): `tools_allow` emptied or given a new tool;
///   `send_allow` given any entry (from empty too) or emptied;
///   `assign_allow` given an entry, or emptied while a later overlapping
///   rule carries one; a dropped `tools_deny`, `send_deny` or `assign_deny`
///   entry;
/// - rules appended after the unchanged ones narrow only when each carries
///   no allow list and `default` stays `allow` (a new rule is first-match
///   for tools, so under `deny` it would admit what `default` refused);
/// - any other rule change (removed, reordered, matcher changed) is not
///   provably narrower and counts as widening.
fn scope_widenings(
    before: &crate::config::schema::ScopeConfig,
    after: &crate::config::schema::ScopeConfig,
    reasons: &mut Vec<RiskReason>,
) {
    let mut push = |path: String| reasons.push(RiskReason::PrivilegeWidened { path });
    if !before.default.allows_unmatched() && after.default.allows_unmatched() {
        push("autonomy.scopes.default".to_string());
    }
    if adds_entries(&before.assign_owners, &after.assign_owners) {
        push("autonomy.scopes.assign_owners".to_string());
    }
    let kept = before.rules.len().min(after.rules.len());
    let prefix_kept = before
        .rules
        .iter()
        .zip(&after.rules)
        .all(|(old, new)| same_matcher(old, new));
    if after.rules.len() < before.rules.len() || !prefix_kept {
        push("autonomy.scopes.rules".to_string());
        return;
    }
    for (index, (old, new)) in before.rules.iter().zip(&after.rules).enumerate().take(kept) {
        for (widened, field) in [
            (tools_allow_widens(&old.tools_allow, &new.tools_allow), "tools_allow"),
            (drops_entries(&old.tools_deny, &new.tools_deny), "tools_deny"),
            (send_allow_widens(&old.send_allow, &new.send_allow), "send_allow"),
            (drops_entries(&old.send_deny, &new.send_deny), "send_deny"),
            (assign_allow_widens(after, index, &old.assign_allow), "assign_allow"),
            (drops_entries(&old.assign_deny, &new.assign_deny), "assign_deny"),
        ] {
            if widened {
                push(format!("autonomy.scopes.rules[{index}].{field}"));
            }
        }
    }
    for (index, added) in after.rules.iter().enumerate().skip(kept) {
        let deny_only = added.tools_allow.is_empty() && added.send_allow.is_empty() && added.assign_allow.is_empty();
        if !deny_only || !after.default.allows_unmatched() {
            push(format!("autonomy.scopes.rules[{index}]"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::secret_paths::fixture;

    fn loaded_fixture(dir: &std::path::Path) -> Config {
        fixture::config_in(dir)
    }

    /// Guard (D-1): a plan over a configuration that holds a sentinel at
    /// every registered secret path renders no sentinel, while every one of
    /// those paths is still reported as a changed secret.
    #[test]
    fn a_plan_over_every_secret_path_renders_no_secret() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let with_secrets = loaded_fixture(dir.path());
        let mut empty = Config::default();
        empty.config_path = with_secrets.config_path.clone();
        empty.workspace_dir = with_secrets.workspace_dir.clone();

        for summary in [
            ConfigChangeSummary::between(Some(&empty), &with_secrets).expect("test: add secrets"),
            ConfigChangeSummary::between(Some(&with_secrets), &empty).expect("test: remove secrets"),
            ConfigChangeSummary::between(None, &with_secrets).expect("test: no prior config"),
        ] {
            let rendered = serde_json::to_string(&summary).expect("test: render summary");
            assert!(
                !rendered.contains(fixture::SENTINEL_PREFIX),
                "a plan leaked a secret: {rendered}"
            );
            assert_eq!(summary.risk, ConfigChangeRisk::High);
            let mut secret_paths = summary
                .changes
                .iter()
                .filter(|change| change.secret)
                .map(|change| change.path.clone())
                .collect::<Vec<_>>();
            secret_paths.sort();
            let sentinel_paths =
                fixture::string_leaves(&serde_json::to_value(&with_secrets).expect("test: json"), &|value| {
                    value.starts_with(fixture::SENTINEL_PREFIX)
                });
            assert!(!sentinel_paths.is_empty(), "test precondition: sentinels present");
            for path in sentinel_paths.iter().map(|path| secret_paths::display_path(path)) {
                assert!(
                    secret_paths.contains(&path),
                    "the plan does not report the secret at {path}: {secret_paths:?}"
                );
            }
        }
    }

    #[test]
    fn an_unchanged_configuration_has_an_empty_low_risk_plan() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = loaded_fixture(dir.path());
        let summary = ConfigChangeSummary::between(Some(&config), &config).expect("test: summary");
        assert!(summary.is_empty(), "{summary:?}");
        assert_eq!(summary.risk, ConfigChangeRisk::Low);
    }

    #[test]
    fn a_plain_setting_shows_its_values_and_is_low_risk() {
        let before = Config::default();
        let mut after = before.clone();
        after.default_temperature = 0.25;
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::Low);
        assert_eq!(
            summary.changes,
            vec![ConfigChangeEntry {
                path: "default_temperature".into(),
                kind: ChangeKind::Changed,
                before: Some(serde_json::json!(before.default_temperature)),
                after: Some(serde_json::json!(0.25)),
                secret: false,
            }]
        );
    }

    /// CDP-04 (SP-CDP-A 5.4): turning remote decision processing on, or
    /// changing where it goes while it is on, is high risk through the one
    /// classifier; the decision endpoint is a credential recipient; a
    /// credential reference change is a credential change; narrowing stays
    /// low.
    #[test]
    fn remote_decision_processing_changes_are_high_risk_and_narrowing_is_low() {
        use crate::causal_tree::config::{BackendSelection, CredentialRefSetting};
        use crate::decision::BackendKind;
        let reasons = |before: &Config, after: &Config| {
            let summary = ConfigChangeSummary::between(Some(before), after).expect("test: summary");
            assert_eq!(
                summary.risk,
                if summary.risk_reasons.is_empty() {
                    ConfigChangeRisk::Low
                } else {
                    ConfigChangeRisk::High
                }
            );
            summary.risk_reasons
        };
        let remote = |field: &str| RiskReason::RemoteDecisionEnabled {
            path: format!("causal_tree.backend.{field}"),
        };
        let local = Config::default();
        let mut remote_on = local.clone();
        remote_on.causal_tree.enabled = true;
        remote_on.causal_tree.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        remote_on.causal_tree.backend.endpoint = "https://decisions.example.com/v1".into();
        remote_on.causal_tree.backend.remote_enabled = true;
        let turned_on = reasons(&local, &remote_on);
        assert!(turned_on.contains(&remote("remote_enabled")), "{turned_on:?}");
        assert!(
            turned_on.contains(&RiskReason::EndpointChanged {
                path: "causal_tree.backend.endpoint".into()
            }),
            "{turned_on:?}"
        );
        assert!(
            ConfigChangeSummary::between(None, &remote_on)
                .expect("test: first write")
                .risk_reasons
                .contains(&remote("remote_enabled"))
        );

        for (field, change) in [
            (
                "model",
                (|config: &mut Config| config.causal_tree.backend.model = "decider-2".into()) as fn(&mut Config),
            ),
            ("kind", |config| {
                config.causal_tree.backend.kind = BackendSelection::Auto
            }),
            ("endpoint", |config| {
                config.causal_tree.backend.endpoint = "https://other.example.com/v1".into();
            }),
            ("credential_ref", |config| {
                config.causal_tree.backend.credential_ref = CredentialRefSetting::None
            }),
        ] {
            let mut after = remote_on.clone();
            change(&mut after);
            let found = reasons(&remote_on, &after);
            assert!(found.contains(&remote(field)), "{field}: {found:?}");
        }

        // Narrowing: remote off, and builtin, are low.
        let mut off = remote_on.clone();
        off.causal_tree.backend.remote_enabled = false;
        assert_eq!(reasons(&remote_on, &off), Vec::new());
        // D-079 M-2: with remote on, a move to the builtin, and a change on a
        // local target, send nothing off the machine and stay low.
        let mut to_builtin = remote_on.clone();
        to_builtin.causal_tree.backend.kind = BackendSelection::Fixed(BackendKind::Builtin);
        assert_eq!(
            reasons(&remote_on, &to_builtin),
            Vec::new(),
            "kind -> builtin is a narrowing"
        );
        let mut local_remote_on = local.clone();
        local_remote_on.causal_tree.enabled = true;
        local_remote_on.causal_tree.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        local_remote_on.causal_tree.backend.endpoint = "http://127.0.0.1:8900/v1".into();
        local_remote_on.causal_tree.backend.remote_enabled = true;
        let mut local_model = local_remote_on.clone();
        local_model.causal_tree.backend.model = "decider-3".into();
        assert_eq!(
            reasons(&local_remote_on, &local_model),
            Vec::new(),
            "a local model change is low"
        );
        // D-082 N-5: a remote target moved to a loopback endpoint sends
        // nothing off the machine; only the re-pointed credential recipient
        // is high.
        let mut to_loopback = remote_on.clone();
        to_loopback.causal_tree.backend.endpoint = "http://127.0.0.1:8900/v1".into();
        assert_eq!(
            reasons(&remote_on, &to_loopback),
            vec![RiskReason::EndpointChanged {
                path: "causal_tree.backend.endpoint".into()
            }]
        );
        // A local target that becomes remote while the switch is on is high.
        let mut becomes_remote = local_remote_on.clone();
        becomes_remote.causal_tree.backend.endpoint = "https://decisions.example.com/v1".into();
        assert!(reasons(&local_remote_on, &becomes_remote).contains(&remote("endpoint")));
        // D-079 L-1: enabling the plane with a remote target configured is
        // high; disabling it is low.
        let mut disabled = remote_on.clone();
        disabled.causal_tree.enabled = false;
        assert_eq!(reasons(&remote_on, &disabled), Vec::new());
        assert_eq!(
            reasons(&disabled, &remote_on),
            vec![RiskReason::RemoteDecisionEnabled {
                path: "causal_tree.enabled".into()
            }]
        );
        let mut builtin = local.clone();
        builtin.causal_tree.backend.kind = BackendSelection::Fixed(BackendKind::Builtin);
        assert_eq!(reasons(&local, &builtin), Vec::new());
        let mut enabled = local.clone();
        enabled.causal_tree.enabled = true;
        assert_eq!(reasons(&local, &enabled), Vec::new(), "the local decision plane is low");

        // With remote off: a new endpoint re-points the backend's credential,
        // and a credential reference change is a credential change.
        let mut endpoint = local.clone();
        endpoint.causal_tree.backend.endpoint = "http://127.0.0.1:8900/v1".into();
        assert_eq!(
            reasons(&local, &endpoint),
            vec![RiskReason::EndpointChanged {
                path: "causal_tree.backend.endpoint".into()
            }]
        );
        let mut credential = local.clone();
        credential.causal_tree.backend.credential_ref = CredentialRefSetting::None;
        assert_eq!(
            reasons(&local, &credential),
            vec![RiskReason::CredentialChanged {
                path: "causal_tree.backend.credential_ref".into()
            }]
        );
    }

    #[test]
    fn disabling_encryption_and_redirecting_an_endpoint_are_high_risk() {
        let before = Config::default();
        let mut after = before.clone();
        after.secrets.encrypt = false;
        after.api_url = Some("https://elsewhere.example/v1".into());
        after.default_provider = Some("custom:https://proxy.example/v1".into());
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High);
        assert!(summary.risk_reasons.contains(&RiskReason::EncryptionDisabled));
        assert!(
            summary
                .risk_reasons
                .contains(&RiskReason::EndpointChanged { path: "api_url".into() })
        );
        assert!(summary.risk_reasons.contains(&RiskReason::EndpointChanged {
            path: "default_provider".into()
        }));
    }

    #[test]
    fn an_added_route_reports_each_leaf_and_masks_its_key() {
        let before = Config::default();
        let mut after = before.clone();
        after.model_routes.push(crate::config::ModelRouteConfig {
            hint: "fast".into(),
            provider: "openrouter".into(),
            model: "m".into(),
            api_key: Some("sentinel-added-route".into()),
            credential_ref: None,
        });
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        let key = summary
            .changes
            .iter()
            .find(|change| change.path == "model_routes[0].api_key")
            .expect("test: route key reported");
        assert_eq!(key.kind, ChangeKind::Added);
        assert!(key.secret);
        assert_eq!(key.after, Some(serde_json::json!(secret_paths::REDACTION_MASK)));
        let hint = summary
            .changes
            .iter()
            .find(|change| change.path == "model_routes[0].hint")
            .expect("test: route hint reported");
        assert_eq!(hint.after, Some(serde_json::json!("fast")));
        assert!(summary.risk_reasons.contains(&RiskReason::CredentialChanged {
            path: "model_routes[0].api_key".into()
        }));
    }

    /// PMMD-g (N-3): URL userinfo in provider and endpoint values and in a
    /// `cost.prices` key never reaches a plan path or value, and each such
    /// change is a credential change.
    #[test]
    fn a_plan_over_url_userinfo_renders_no_userinfo() {
        const PW: &str = "userinfo-sentinel-pw";
        let url = format!("custom:http://alice:{PW}@127.0.0.1:1/v1");
        let before = Config::default();
        let mut after = before.clone();
        after.default_provider = Some(url.clone());
        after.api_url = Some(format!("http://bob:{PW}@127.0.0.1:2/v1"));
        after.reliability.fallback_providers = vec![url.clone()];
        after.cost.prices.insert(
            format!("{url}/model-a"),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 2.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );

        for summary in [
            ConfigChangeSummary::between(Some(&before), &after).expect("test: add"),
            ConfigChangeSummary::between(Some(&after), &before).expect("test: remove"),
        ] {
            let rendered = serde_json::to_string(&summary).expect("test: json");
            assert!(!rendered.contains(PW), "a plan leaked userinfo: {rendered}");
            assert_eq!(summary.risk, ConfigChangeRisk::High);
            for path in [
                "default_provider",
                "api_url",
                "reliability.fallback_providers[0]",
                "cost.prices.custom:http://[redacted]@127.0.0.1:1/v1/model-a.input",
            ] {
                assert!(
                    summary
                        .risk_reasons
                        .contains(&RiskReason::CredentialChanged { path: path.into() }),
                    "{path}: {:?}",
                    summary.risk_reasons
                );
            }
        }
        let price = ConfigChangeSummary::between(Some(&before), &after)
            .expect("test: add")
            .changes
            .into_iter()
            .find(|change| change.path.ends_with("model-a.input"))
            .expect("test: price change");
        assert_eq!(
            price.after,
            Some(serde_json::json!(1.0)),
            "the value is shown, the key redacted"
        );
    }

    /// RED before PMMD-h (G-3): re-pointing an endpoint that receives a
    /// stored token (MCP server, remote node, channel, telemetry, chat
    /// daemon) classified low.
    #[test]
    fn token_receiving_endpoints_are_high_risk() {
        let (old, new) = (
            serde_json::json!("https://a.example"),
            serde_json::json!("https://b.example"),
        );
        let key = |name: &str| PathElem::Key(name.to_string());
        for path in [
            vec![key("mcp"), key("servers"), key("remote"), key("url")],
            vec![key("nodes"), key("nodes"), PathElem::Index(0), key("endpoint")],
            vec![key("channels_config"), key("mattermost"), key("url")],
            vec![key("channels_config"), key("matrix"), key("homeserver")],
            vec![key("channels_config"), key("signal"), key("http_url")],
            vec![key("channels_config"), key("whatsapp"), key("ws_url")],
            vec![key("observability"), key("otel_endpoint")],
            vec![key("chat"), key("daemon"), key("url")],
            vec![key("proxy"), key("https_proxy")],
            vec![key("channels_config"), key("irc"), key("server")],
        ] {
            assert!(
                secret_paths::is_credential_recipient_endpoint(&path)
                    || is_endpoint_change(&path, secret_paths::is_url_userinfo_path(&path), Some(&old), Some(&new)),
                "{}",
                secret_paths::display_path(&path)
            );
        }

        let before = Config::default();
        let mut after = before.clone();
        after.observability.otel_endpoint = Some("https://collector.example:4318".into());
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");

        // A first write (no prior configuration) re-points nothing: default
        // token endpoints coming into existence are not endpoint changes.
        let first = ConfigChangeSummary::between(None, &Config::default()).expect("test: first write");
        assert!(
            !first
                .risk_reasons
                .iter()
                .any(|reason| matches!(reason, RiskReason::EndpointChanged { .. })),
            "{:?}",
            first.risk_reasons
        );
    }

    /// RED before PMMD-i (H-2): a stdio MCP server receives its stored `env`
    /// credentials; pointing its `command` or `args` at another executable
    /// hands them over and was classified low.
    #[test]
    fn relaunching_an_mcp_server_that_holds_credentials_is_high_risk() {
        let mut before = Config::default();
        before.mcp.servers.insert(
            "local".into(),
            serde_json::from_value(serde_json::json!({
                "command": "mcp-local",
                "args": ["--serve"],
                "env": { "MCP_TOKEN": "sentinel-mcp-env" }
            }))
            .expect("test: server"),
        );
        let relaunch: [fn(&mut crate::config::McpServerConfig); 2] = [
            |server| server.command = Some("/tmp/other-binary".into()),
            |server| server.args = vec!["--exfiltrate".into()],
        ];
        for change in relaunch {
            let mut after = before.clone();
            change(after.mcp.servers.get_mut("local").expect("test: server"));
            let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
            assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
            assert!(
                !serde_json::to_string(&summary)
                    .expect("test: json")
                    .contains("sentinel-")
            );
        }
    }

    /// RED before PMME-f (D-085 F-3): switching the default provider to
    /// another catalog provider while the root `api_key` is set hands that
    /// key to the new vendor; it was low risk on every write path. The same
    /// for a route or an agent that moves its own key to another provider.
    #[test]
    fn handing_a_stored_key_to_another_provider_is_high_risk() {
        let mut before = Config::default();
        before.default_provider = Some("openai".into());
        before.api_key = Some("sentinel-root-key".into());
        before.model_routes = vec![crate::config::ModelRouteConfig {
            hint: "fast".into(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            api_key: Some("sentinel-route-key".into()),
            credential_ref: None,
        }];
        let owner_changes = |summary: &ConfigChangeSummary| {
            summary
                .risk_reasons
                .iter()
                .filter_map(|reason| match reason {
                    RiskReason::CredentialOwnerChanged { path } => Some(path.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        let mut switched = before.clone();
        switched.default_provider = Some("anthropic".into());
        if let Some(route) = switched.model_routes.first_mut() {
            route.provider = "openrouter".into();
        }
        let summary = ConfigChangeSummary::between(Some(&before), &switched).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
        assert_eq!(
            owner_changes(&summary),
            ["default_provider", "model_routes[0].provider"]
        );

        // A respelling of the same provider, or a switch with no key held,
        // hands nothing over.
        let mut respelled = before.clone();
        respelled.default_provider = Some("OpenAI".into());
        let summary = ConfigChangeSummary::between(Some(&before), &respelled).expect("test: summary");
        assert!(owner_changes(&summary).is_empty(), "{summary:?}");
        let mut keyless = before;
        keyless.api_key = None;
        keyless.model_routes.clear();
        let mut keyless_switch = keyless.clone();
        keyless_switch.default_provider = Some("anthropic".into());
        let summary = ConfigChangeSummary::between(Some(&keyless), &keyless_switch).expect("test: summary");
        assert!(owner_changes(&summary).is_empty(), "{summary:?}");
        // `reliability.api_keys` is root material too.
        let mut rotated = keyless.clone();
        rotated.reliability.api_keys = vec!["sentinel-rotation".into()];
        let mut rotated_switch = rotated.clone();
        rotated_switch.default_provider = Some("anthropic".into());
        let summary = ConfigChangeSummary::between(Some(&rotated), &rotated_switch).expect("test: summary");
        assert_eq!(owner_changes(&summary), ["default_provider"]);
    }

    /// RED before PMME-g (D-088 C-1): the owner check matched routes by hint,
    /// so renaming a route's hint while moving it to another provider (key
    /// kept) handed the key over at low risk. The check now follows the key.
    #[test]
    fn a_key_moved_under_a_renamed_route_or_embedding_route_is_high_risk() {
        let route = |hint: &str, provider: &str| crate::config::ModelRouteConfig {
            hint: hint.into(),
            provider: provider.into(),
            model: "some-model".into(),
            api_key: Some("sentinel-moving-route-key".into()),
            credential_ref: None,
        };
        let mut before = Config::default();
        before.model_routes = vec![route("fast", "openai")];
        let mut after = before.clone();
        after.model_routes = vec![route("fast2", "anthropic")];
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert!(
            summary.risk_reasons.contains(&RiskReason::CredentialOwnerChanged {
                path: "model_routes[0].provider".into()
            }),
            "{summary:?}"
        );
        assert!(
            !serde_json::to_string(&summary)
                .expect("test: json")
                .contains("sentinel-")
        );

        let embedding = |hint: &str, provider: &str| crate::config::EmbeddingRouteConfig {
            hint: hint.into(),
            provider: provider.into(),
            model: "text-embedding-3-small".into(),
            dimensions: None,
            api_key: Some("sentinel-moving-embedding-key".into()),
            credential_ref: None,
        };
        let mut before = Config::default();
        before.embedding_routes = vec![embedding("semantic", "openai")];
        let mut after = before.clone();
        after.embedding_routes = vec![embedding("archive", "custom:https://embed.example/v1")];
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert!(
            summary.risk_reasons.contains(&RiskReason::CredentialOwnerChanged {
                path: "embedding_routes[0].provider".into()
            }),
            "{summary:?}"
        );

        // A key that stays with its provider while the hint is renamed, or
        // an index shift, moves nothing.
        let mut before = Config::default();
        before.model_routes = vec![route("fast", "openai")];
        let mut renamed = before.clone();
        renamed.model_routes = vec![route("other", "openai"), route("fast", "openai")];
        let summary = ConfigChangeSummary::between(Some(&before), &renamed).expect("test: summary");
        assert!(
            !summary
                .risk_reasons
                .iter()
                .any(|reason| matches!(reason, RiskReason::CredentialOwnerChanged { .. })),
            "{summary:?}"
        );
    }

    /// RED before PMME-g (D-088 C-6): a plan (and so the history record)
    /// showed a provider URL's query values and fragment verbatim.
    #[test]
    fn a_plan_redacts_url_query_values_and_fragments() {
        let before = Config::default();
        let mut after = before.clone();
        after.api_url = Some("https://llm.example/v1?sig=sentinel-plan-sig#sentinel-plan-fragment".into());
        after.reliability.fallback_providers = vec!["custom:https://h.example/v1?sentinel-bare-token".into()];
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        let rendered = serde_json::to_string(&summary).expect("test: json");
        assert!(!rendered.contains("sentinel-"), "{rendered}");
        assert!(
            rendered.contains("https://llm.example/v1?sig=[redacted]#[redacted]"),
            "{rendered}"
        );
    }

    /// RED before HOOKS-ENV: `security.workspace_hooks` did not exist, and
    /// workspace hooks ran without any operator consent. Turning them on is
    /// now high risk; turning them off is not.
    fn route(hint: &str, api_key: Option<&str>) -> crate::config::ModelRouteConfig {
        crate::config::ModelRouteConfig {
            hint: hint.into(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            api_key: api_key.map(str::to_string),
            credential_ref: None,
        }
    }

    /// RED before PMMH-f (D-101 F-3): secrets were compared by position, so
    /// removing a route in front of one that holds a key read as that key
    /// moving (a credential write, High). They are compared by identity now:
    /// no secret changed, and the plan says so; writing a key is a write,
    /// clearing one a clear.
    #[test]
    fn secrets_are_compared_by_identity_not_position() {
        let mut before = Config::default();
        before.model_routes = vec![route("a", None), route("b", Some("pmmh-identity-sentinel"))];
        let mut after = before.clone();
        after.model_routes.remove(0);

        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert!(summary.secret_writes.is_empty(), "{:?}", summary.secret_writes);
        assert!(summary.secret_clears.is_empty(), "{:?}", summary.secret_clears);
        assert!(
            !summary
                .risk_reasons
                .iter()
                .any(|reason| matches!(reason, RiskReason::CredentialChanged { .. })),
            "{:?}",
            summary.risk_reasons
        );

        let mut rotated = after.clone();
        if let Some(route) = rotated.model_routes.first_mut() {
            route.api_key = Some("pmmh-identity-rotated".into());
        }
        let summary = ConfigChangeSummary::between(Some(&after), &rotated).expect("test: summary");
        assert_eq!(summary.secret_writes, ["model_routes[0].api_key"]);
        assert!(summary.secret_clears.is_empty(), "expected an empty value");

        let mut cleared = after.clone();
        if let Some(route) = cleared.model_routes.first_mut() {
            route.api_key = None;
        }
        let summary = ConfigChangeSummary::between(Some(&after), &cleared).expect("test: summary");
        assert!(summary.secret_writes.is_empty(), "expected an empty value");
        assert_eq!(summary.secret_clears, ["model_routes[0].api_key"]);

        // RED before PMMH-g (D-104 C-1): two embedding routes sharing a
        // hint shared one identity, so changing the first one's key was no
        // secret change at all.
        let embedding = |api_key: &str| crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: "text-embedding-3-small".into(),
            dimensions: None,
            api_key: Some(api_key.into()),
            credential_ref: None,
        };
        let mut duplicated = Config::default();
        duplicated.embedding_routes = vec![embedding("pmmh-dup-one"), embedding("pmmh-dup-two")];
        let mut changed = duplicated.clone();
        if let Some(route) = changed.embedding_routes.first_mut() {
            route.api_key = Some("pmmh-dup-rotated".into());
        }
        let summary = ConfigChangeSummary::between(Some(&duplicated), &changed).expect("test: summary");
        assert_eq!(summary.secret_writes, ["embedding_routes[0].api_key"]);
        assert_eq!(summary.risk, ConfigChangeRisk::High);

        // A list of secrets compares as a set: reordering changes nothing.
        let mut keyed = Config::default();
        keyed.reliability.api_keys = vec!["pmmh-key-one".into(), "pmmh-key-two".into()];
        let mut reordered = keyed.clone();
        reordered.reliability.api_keys.reverse();
        let summary = ConfigChangeSummary::between(Some(&keyed), &reordered).expect("test: summary");
        assert!(summary.secret_writes.is_empty() && summary.secret_clears.is_empty());
    }

    #[test]
    fn turning_workspace_hooks_on_is_high_risk() {
        let before = Config::default();
        let mut on = before.clone();
        on.security.workspace_hooks = crate::config::WorkspaceHooks::On;
        let summary = ConfigChangeSummary::between(Some(&before), &on).expect("test: summary");
        assert!(
            summary.risk_reasons.contains(&RiskReason::CredentialRecipientChanged {
                path: "security.workspace_hooks".into()
            }),
            "{summary:?}"
        );
        let summary = ConfigChangeSummary::between(Some(&on), &before).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::Low, "{summary:?}");
    }

    /// D-085 F-1: letting a workspace `mcp.json` add stdio servers is high
    /// risk; narrowing it back is not.
    #[test]
    fn widening_the_workspace_mcp_opt_in_is_high_risk() {
        use crate::config::WorkspaceMcpServers;
        let before = Config::default();
        let mut stdio = before.clone();
        stdio.mcp.workspace_servers = WorkspaceMcpServers::Stdio;
        let summary = ConfigChangeSummary::between(Some(&before), &stdio).expect("test: summary");
        assert!(
            summary.risk_reasons.contains(&RiskReason::CredentialRecipientChanged {
                path: "mcp.workspace_servers".into()
            }),
            "{summary:?}"
        );
        let mut http = before.clone();
        http.mcp.workspace_servers = WorkspaceMcpServers::HttpOnly;
        let summary = ConfigChangeSummary::between(Some(&before), &http).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
        let summary = ConfigChangeSummary::between(Some(&stdio), &before).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::Low, "{summary:?}");
    }

    /// RED before PMM-E (D-045): a stdio MCP server runs with PRX's
    /// privileges and can read every stored credential, so adding one or
    /// re-pointing its launch is high risk even when it holds no `env`
    /// value. Before, such a change was classified low.
    #[test]
    fn adding_or_relaunching_a_stdio_server_without_env_is_high_risk() {
        let server = |value: serde_json::Value| -> crate::config::McpServerConfig {
            serde_json::from_value(value).expect("test: server")
        };
        let mut bare = Config::default();
        bare.mcp.servers.insert(
            "local".into(),
            server(serde_json::json!({ "command": "mcp-local", "args": ["--serve"] })),
        );
        let relaunch: [fn(&mut crate::config::McpServerConfig); 3] = [
            |server| server.command = Some("/tmp/other-binary".into()),
            |server| server.args = vec!["--exfiltrate".into()],
            |server| server.args.push("--extra".into()),
        ];
        for change in relaunch {
            let mut after = bare.clone();
            change(after.mcp.servers.get_mut("local").expect("test: server"));
            let summary = ConfigChangeSummary::between(Some(&bare), &after).expect("test: summary");
            assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
            assert!(
                summary
                    .risk_reasons
                    .iter()
                    .any(|reason| matches!(reason, RiskReason::CredentialRecipientChanged { .. })),
                "{summary:?}"
            );
        }

        // Adding a stdio server without env.
        let mut added = bare.clone();
        added.mcp.servers.insert(
            "fresh".into(),
            server(serde_json::json!({ "command": "npx", "args": ["some-mcp"] })),
        );
        let summary = ConfigChangeSummary::between(Some(&bare), &added).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");

        // Switching an HTTP server to stdio.
        let mut http = bare.clone();
        http.mcp.servers.insert(
            "remote".into(),
            server(serde_json::json!({ "transport": "http", "url": "https://mcp.example.com/mcp" })),
        );
        let mut switched = http.clone();
        let remote = switched.mcp.servers.get_mut("remote").expect("test: server");
        remote.transport = crate::config::McpTransport::Stdio;
        remote.command = Some("mcp-remote-local".into());
        let summary = ConfigChangeSummary::between(Some(&http), &switched).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");

        // What does not change the program stays low: a timeout, a tool
        // filter. Adding an HTTP server launches nothing (its URL is an
        // endpoint, classified as such, not a launch).
        let mut tuned = bare.clone();
        let local = tuned.mcp.servers.get_mut("local").expect("test: server");
        local.startup_timeout_ms = 20_000;
        local.deny_tools = vec!["dangerous".into()];
        let summary = ConfigChangeSummary::between(Some(&bare), &tuned).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::Low, "{summary:?}");
        let mut with_http = bare.clone();
        with_http.mcp.servers.insert(
            "remote".into(),
            server(serde_json::json!({ "transport": "http", "url": "https://mcp.example.com/mcp" })),
        );
        let summary = ConfigChangeSummary::between(Some(&bare), &with_http).expect("test: summary");
        assert!(
            !summary
                .risk_reasons
                .iter()
                .any(|reason| matches!(reason, RiskReason::CredentialRecipientChanged { .. })),
            "{summary:?}"
        );
    }

    /// RED before SECRETS-OTHER: a proxy, MCP, node, OTEL, daemon or channel
    /// endpoint URL carrying `user:password@` was a low-risk change whose
    /// plan showed the password.
    #[test]
    fn a_plan_over_endpoint_url_userinfo_is_a_credential_change_and_renders_no_userinfo() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let after = fixture::config_from_overlay(dir.path(), fixture::ENDPOINT_USERINFO_TOML);
        let mut before = Config::default();
        before.config_path = after.config_path.clone();
        before.workspace_dir = after.workspace_dir.clone();
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: plan");
        let rendered = serde_json::to_string(&summary).expect("test: json");
        assert!(
            !rendered.contains("userinfo-sentinel"),
            "a plan leaked userinfo: {rendered}"
        );
        assert_eq!(summary.risk, ConfigChangeRisk::High);
        for path in [
            "proxy.http_proxy",
            "proxy.all_proxy",
            "mcp.servers.remote.url",
            "chat.daemon.url",
            "nodes.nodes[0].endpoint",
            "observability.otel_endpoint",
            "channels_config.matrix.homeserver",
            "channels_config.whatsapp.ws_url",
        ] {
            assert!(
                summary
                    .risk_reasons
                    .contains(&RiskReason::CredentialChanged { path: path.into() }),
                "{path}: {:?}",
                summary.risk_reasons
            );
        }
    }

    /// R-SECRETS (TOKEN_ENDPOINTS/URL_USERINFO_PATHS overlap), RED before
    /// SECRETS-f: re-pointing a proxy, through which every credentialed
    /// request travels, was a low-risk change. The unified endpoint registry
    /// marks it a credential recipient.
    #[test]
    fn re_pointing_a_proxy_is_an_endpoint_change() {
        let mut before = Config::default();
        before.proxy.https_proxy = Some("http://proxy-a.example:3128".into());
        let mut after = before.clone();
        after.proxy.https_proxy = Some("http://proxy-b.example:3128".into());
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
        assert!(
            summary.risk_reasons.contains(&RiskReason::EndpointChanged {
                path: "proxy.https_proxy".into()
            }),
            "{:?}",
            summary.risk_reasons
        );
    }

    /// R-SECRETS F1: an MCP launch environment value and a paired token are
    /// not credentials alone; the plan gives each its own reason, so a
    /// surface that confirms credential rotation implicitly (the credential
    /// endpoint) cannot confirm them. A route key stays a credential.
    #[test]
    fn process_environment_and_access_grant_changes_have_their_own_reasons() {
        let mut before = Config::default();
        before.mcp.servers.insert(
            "local".into(),
            serde_json::from_value(serde_json::json!({"command": "mcp-local"})).expect("test: server"),
        );
        let mut after = before.clone();
        after
            .mcp
            .servers
            .get_mut("local")
            .expect("test: server")
            .env
            .insert("NODE_OPTIONS".into(), "--require /tmp/x.js".into());
        after.gateway.paired_tokens = vec!["zc_planted".into()];
        after.api_key = Some("sk-rotated".into());

        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High);
        assert_eq!(
            summary.risk_reasons,
            vec![
                RiskReason::CredentialChanged { path: "api_key".into() },
                RiskReason::ProcessEnvironmentChanged {
                    path: "mcp.servers.local.env.NODE_OPTIONS".into()
                },
                RiskReason::AccessGrantChanged {
                    path: "gateway.paired_tokens[0]".into()
                },
            ]
        );
        let rendered = serde_json::to_string(&summary).expect("test: json");
        assert!(
            !rendered.contains("/tmp/x.js") && !rendered.contains("zc_planted"),
            "{rendered}"
        );
    }

    /// D-048, RED before SECRETS-g: changing an admission secret other than a
    /// paired token (the webhook token, a channel verify token, the node
    /// server's bearer token, ...) was a plain credential change, so the
    /// credential endpoint confirmed it implicitly. After: every registry
    /// path marked as an access grant gives `AccessGrantChanged` for its own
    /// path and nothing else; an outbound credential stays
    /// `CredentialChanged`.
    #[test]
    fn every_admission_secret_change_is_an_access_grant_change() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let before = fixture::config_in(dir.path());
        let before_json = serde_json::to_value(&before).expect("test: render fixture");
        let fixture_tree: toml::Value = toml::from_str(fixture::TOML).expect("test: fixture parses");
        let sentinels = fixture::string_leaves(&fixture_tree, &|value| value.contains(fixture::SENTINEL_PREFIX));
        let pointer = |path: &[PathElem]| {
            path.iter()
                .map(|elem| match elem {
                    PathElem::Key(key) => format!("/{key}"),
                    PathElem::Index(index) => format!("/{index}"),
                })
                .collect::<String>()
        };
        let change = |path: &[PathElem]| {
            let mut changed = before_json.clone();
            let slot = changed
                .pointer_mut(&pointer(path))
                .unwrap_or_else(|| panic!("test: {} in the fixture", secret_paths::display_path(path)));
            *slot = serde_json::json!("rotated-admission-value");
            let mut after: Config = serde_json::from_value(changed).expect("test: changed config");
            after.config_path.clone_from(&before.config_path);
            after.workspace_dir.clone_from(&before.workspace_dir);
            ConfigChangeSummary::between(Some(&before), &after).expect("test: summary")
        };

        let mut grants = std::collections::BTreeSet::new();
        for path in &sentinels {
            let Some(registered) = secret_paths::registered(path) else {
                continue;
            };
            let display = secret_paths::display_path(path);
            let summary = change(path.as_slice());
            assert_eq!(summary.risk, ConfigChangeRisk::High, "{display}");
            let expected = match registered.used_as {
                secret_paths::SecretUse::AccessGrant => {
                    grants.insert(registered.pattern);
                    RiskReason::AccessGrantChanged { path: display.clone() }
                }
                secret_paths::SecretUse::ProcessEnvironment => {
                    RiskReason::ProcessEnvironmentChanged { path: display.clone() }
                }
                secret_paths::SecretUse::Credential => RiskReason::CredentialChanged { path: display.clone() },
            };
            let mut expected = vec![expected];
            // A connection string is also an endpoint (D-049 F).
            if secret_paths::is_credential_recipient_endpoint(path) {
                expected.push(RiskReason::EndpointChanged { path: display.clone() });
            }
            assert_eq!(summary.risk_reasons, expected, "{display}");
        }
        let registered: std::collections::BTreeSet<&str> = secret_paths::SECRET_PATHS
            .iter()
            .filter(|secret| secret.alias_of.is_none() && secret.used_as == secret_paths::SecretUse::AccessGrant)
            .map(|secret| secret.pattern)
            .collect();
        assert_eq!(grants, registered, "the fixture must reach every access grant");
    }

    /// D-049 A, RED before SECRETS-h: a change of a setting that decides who
    /// is admitted inbound was Low. After: each is `AccessGrantChanged` for
    /// its own path; a first write (no prior configuration) grants nothing
    /// new; an unrelated setting stays Low.
    #[test]
    fn inbound_trust_settings_are_access_grants() {
        let before = Config::default();
        let before_json = serde_json::to_value(&before).expect("test: render");
        let changes: &[(&str, serde_json::Value, &str)] = &[
            (
                "/gateway/require_pairing",
                serde_json::json!(false),
                "gateway.require_pairing",
            ),
            (
                "/gateway/trust_forwarded_headers",
                serde_json::json!(true),
                "gateway.trust_forwarded_headers",
            ),
            (
                "/gateway/allow_public_bind",
                serde_json::json!(true),
                "gateway.allow_public_bind",
            ),
            ("/gateway/host", serde_json::json!("0.0.0.0"), "gateway.host"),
            (
                "/mcp_server/require_auth",
                serde_json::json!(false),
                "mcp_server.require_auth",
            ),
            (
                "/mcp_server/jwt_public_key_pem",
                serde_json::json!("-----BEGIN PUBLIC KEY-----"),
                "mcp_server.jwt_public_key_pem",
            ),
            (
                "/mcp_server/jwt_jwks",
                serde_json::json!("{\"keys\":[]}"),
                "mcp_server.jwt_jwks",
            ),
            (
                "/mcp_server/jwt_jwks_uri",
                serde_json::json!("https://jwks.invalid/keys"),
                "mcp_server.jwt_jwks_uri",
            ),
            (
                "/mcp_server/jwt_issuer",
                serde_json::json!("https://issuer.invalid"),
                "mcp_server.jwt_issuer",
            ),
            (
                "/mcp_server/jwt_audience",
                serde_json::json!("prx"),
                "mcp_server.jwt_audience",
            ),
            (
                "/mcp_server/jwt_algorithms",
                serde_json::json!(["HS256"]),
                "mcp_server.jwt_algorithms[0]",
            ),
            (
                "/a2a/allowed_peer_issuers",
                serde_json::json!(["spiffe://peer"]),
                "a2a.allowed_peer_issuers[0]",
            ),
            (
                "/a2a/trusted_trust_domains",
                serde_json::json!(["peer.invalid"]),
                "a2a.trusted_trust_domains[0]",
            ),
            (
                "/a2a/spiffe_trust_bundle_pem",
                serde_json::json!("-----BEGIN CERTIFICATE-----"),
                "a2a.spiffe_trust_bundle_pem",
            ),
            (
                "/a2a/spiffe_strict_validation",
                serde_json::json!(true),
                "a2a.spiffe_strict_validation",
            ),
            (
                "/identity_bindings",
                serde_json::json!([{"user_id": "owner", "channel": "telegram", "channel_account": "42"}]),
                "identity_bindings[0].",
            ),
        ];
        for (pointer, value, path) in changes {
            let mut changed = before_json.clone();
            *changed
                .pointer_mut(pointer)
                .unwrap_or_else(|| panic!("test: {pointer} in the default configuration")) = value.clone();
            let after: Config = serde_json::from_value(changed).expect("test: changed config");
            let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
            assert_eq!(summary.risk, ConfigChangeRisk::High, "{path}: {summary:?}");
            assert!(
                summary.risk_reasons.iter().any(|reason| matches!(
                    reason,
                    RiskReason::AccessGrantChanged { path: reported } if reported.starts_with(path)
                )),
                "{path}: {:?}",
                summary.risk_reasons
            );
        }

        let mut unrelated = before.clone();
        unrelated.gateway.port = 17000;
        let summary = ConfigChangeSummary::between(Some(&before), &unrelated).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::Low, "{summary:?}");

        let first = ConfigChangeSummary::between(None, &Config::default()).expect("test: first write");
        assert!(
            !first
                .risk_reasons
                .iter()
                .any(|reason| matches!(reason, RiskReason::AccessGrantChanged { .. })),
            "{:?}",
            first.risk_reasons
        );
    }

    fn section<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).expect("test: channel section")
    }

    fn policy(
        user: &str,
        role: &str,
        projects: &[&str],
        ceiling: &str,
        blocked: &[&str],
    ) -> crate::config::UserPolicyConfig {
        crate::config::UserPolicyConfig {
            user_id: user.to_string(),
            role: role.to_string(),
            projects: projects.iter().map(|p| (*p).to_string()).collect(),
            visibility_ceiling: ceiling.to_string(),
            blocked_patterns: blocked.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    fn strings(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_string()).collect()
    }

    fn rule(channel: &str) -> crate::config::ScopeRule {
        crate::config::ScopeRule {
            channel: Some(channel.to_string()),
            ..crate::config::ScopeRule::default()
        }
    }

    /// One direction-aware case: the setup both sides share, the change, and
    /// the path a widening reports (`None`: the change narrows, so it is low).
    type Case = (&'static str, fn(&mut Config), fn(&mut Config), Option<&'static str>);

    /// D-049 amendment, RED before SECRETS-h: each of these settings changed
    /// as Low in both directions. After: the widening direction is high risk
    /// with a reason naming the setting, and the narrowing direction stays
    /// low, for every setting, explicitly.
    #[test]
    fn direction_aware_settings_are_high_only_when_they_widen() {
        fn discord(config: &mut Config, allowed: &[&str]) {
            config.channels_config.discord = Some(section(serde_json::json!({
                "bot_token": "t", "guild_id": "g1", "allowed_users": allowed
            })));
        }
        let cases: &[Case] = &[
            // ── Channel listening scope ──
            (
                "discord guild moved, everybody admitted",
                |c| discord(c, &["*"]),
                |c| c.channels_config.discord.as_mut().expect("t").guild_id = Some("g2".into()),
                Some("channels_config.discord.guild_id"),
            ),
            (
                "discord guild dropped (all guilds)",
                |c| discord(c, &["*"]),
                |c| c.channels_config.discord.as_mut().expect("t").guild_id = None,
                Some("channels_config.discord.guild_id"),
            ),
            (
                "discord guild moved, senders named",
                |c| discord(c, &["alice"]),
                |c| c.channels_config.discord.as_mut().expect("t").guild_id = Some("g2".into()),
                None,
            ),
            (
                "discord guild set where there was none (narrows)",
                |c| {
                    discord(c, &["*"]);
                    c.channels_config.discord.as_mut().expect("t").guild_id = None;
                },
                |c| c.channels_config.discord.as_mut().expect("t").guild_id = Some("g1".into()),
                None,
            ),
            (
                "slack channel moved, everybody admitted",
                |c| {
                    c.channels_config.slack = Some(section(serde_json::json!({
                        "bot_token": "t", "channel_id": "C1", "allowed_users": ["*"]
                    })));
                },
                |c| c.channels_config.slack.as_mut().expect("t").channel_id = Some("C2".into()),
                Some("channels_config.slack.channel_id"),
            ),
            (
                "mattermost channel moved, nobody named (empty list admits nobody)",
                |c| {
                    c.channels_config.mattermost = Some(section(serde_json::json!({
                        "url": "http://127.0.0.1:1", "bot_token": "t", "channel_id": "c1", "allowed_users": []
                    })));
                },
                |c| c.channels_config.mattermost.as_mut().expect("t").channel_id = Some("c2".into()),
                None,
            ),
            (
                "matrix room moved, everybody admitted",
                |c| {
                    c.channels_config.matrix = Some(section(serde_json::json!({
                        "homeserver": "http://127.0.0.1:1", "access_token": "t",
                        "room_id": "!a:x", "allowed_users": ["*"]
                    })));
                },
                |c| c.channels_config.matrix.as_mut().expect("t").room_id = "!b:x".into(),
                Some("channels_config.matrix.room_id"),
            ),
            (
                "irc channel joined, everybody admitted",
                |c| {
                    c.channels_config.irc = Some(section(serde_json::json!({
                        "server": "irc.invalid", "nickname": "prx", "channels": ["#a"], "allowed_users": ["*"]
                    })));
                },
                |c| c.channels_config.irc.as_mut().expect("t").channels.push("#b".into()),
                Some("channels_config.irc.channels"),
            ),
            (
                "irc channel left (narrows)",
                |c| {
                    c.channels_config.irc = Some(section(serde_json::json!({
                        "server": "irc.invalid", "nickname": "prx", "channels": ["#a", "#b"], "allowed_users": ["*"]
                    })));
                },
                |c| {
                    c.channels_config.irc.as_mut().expect("t").channels.pop();
                },
                None,
            ),
            (
                "signal group scope dropped, empty allow-list",
                |c| {
                    c.channels_config.signal = Some(section(serde_json::json!({
                        "account": "+10000000000", "group_id": "grp"
                    })));
                },
                |c| c.channels_config.signal.as_mut().expect("t").group_id = None,
                Some("channels_config.signal.group_id"),
            ),
            (
                "signal group scope dropped, senders named",
                |c| {
                    c.channels_config.signal = Some(section(serde_json::json!({
                        "account": "+10000000000", "group_id": "grp", "allowed_from": ["+15550001111"]
                    })));
                },
                |c| c.channels_config.signal.as_mut().expect("t").group_id = None,
                None,
            ),
            // ── user_policies ──
            (
                "a new policy grants owner",
                |_| {},
                |c| c.user_policies.push(policy("u1", "owner", &[], "private", &[])),
                Some("user_policies[0].role"),
            ),
            (
                "role lowered (narrows)",
                |c| c.user_policies.push(policy("u1", "owner", &[], "private", &[])),
                |c| c.user_policies.first_mut().expect("t").role = "guest".into(),
                None,
            ),
            (
                "project granted",
                |c| c.user_policies.push(policy("u1", "member", &["a"], "private", &[])),
                |c| c.user_policies.first_mut().expect("t").projects.push("b".into()),
                Some("user_policies[0].projects"),
            ),
            (
                "project withdrawn (narrows)",
                |c| {
                    c.user_policies
                        .push(policy("u1", "member", &["a", "b"], "private", &[]))
                },
                |c| c.user_policies.first_mut().expect("t").projects.truncate(1),
                None,
            ),
            (
                "visibility ceiling raised",
                |c| c.user_policies.push(policy("u1", "member", &[], "private", &[])),
                |c| c.user_policies.first_mut().expect("t").visibility_ceiling = "public".into(),
                Some("user_policies[0].visibility_ceiling"),
            ),
            (
                "visibility ceiling lowered (narrows)",
                |c| c.user_policies.push(policy("u1", "member", &[], "public", &[])),
                |c| c.user_policies.first_mut().expect("t").visibility_ceiling = "private".into(),
                None,
            ),
            (
                "blocked pattern dropped",
                |c| c.user_policies.push(policy("u1", "member", &[], "private", &["token"])),
                |c| c.user_policies.first_mut().expect("t").blocked_patterns.clear(),
                Some("user_policies[0].blocked_patterns"),
            ),
            (
                "blocked pattern added (narrows)",
                |c| c.user_policies.push(policy("u1", "member", &[], "private", &[])),
                |c| {
                    c.user_policies
                        .first_mut()
                        .expect("t")
                        .blocked_patterns
                        .push("token".into())
                },
                None,
            ),
            (
                "a restricting policy removed (its blocked patterns go)",
                |c| c.user_policies.push(policy("u1", "guest", &[], "private", &["token"])),
                |c| c.user_policies.clear(),
                Some("user_policies[0].blocked_patterns"),
            ),
            (
                "a member policy removed (back to guest, narrows)",
                |c| c.user_policies.push(policy("u1", "member", &[], "private", &[])),
                |c| c.user_policies.clear(),
                None,
            ),
            // ── autonomy.scopes ──
            (
                "default deny to allow",
                |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Deny,
                |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Allow,
                Some("autonomy.scopes.default"),
            ),
            (
                "default allow to deny (narrows)",
                |_| {},
                |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Deny,
                None,
            ),
            (
                "assignment owner added",
                |_| {},
                |c| c.autonomy.scopes.assign_owners.push("telegram:alice".into()),
                Some("autonomy.scopes.assign_owners"),
            ),
            (
                "assignment owner removed (narrows)",
                |c| c.autonomy.scopes.assign_owners.push("telegram:alice".into()),
                |c| c.autonomy.scopes.assign_owners.clear(),
                None,
            ),
            (
                "tools_deny entry dropped",
                |c| {
                    let mut r = rule("signal");
                    r.tools_deny = strings(&["shell"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| c.autonomy.scopes.rules.first_mut().expect("t").tools_deny.clear(),
                Some("autonomy.scopes.rules[0].tools_deny"),
            ),
            (
                "tools_deny entry added (narrows)",
                |c| c.autonomy.scopes.rules.push(rule("signal")),
                |c| {
                    c.autonomy
                        .scopes
                        .rules
                        .first_mut()
                        .expect("t")
                        .tools_deny
                        .push("shell".into())
                },
                None,
            ),
            (
                "tools_allow whitelist gains a tool",
                |c| {
                    let mut r = rule("signal");
                    r.tools_allow = strings(&["memory_recall"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| {
                    c.autonomy
                        .scopes
                        .rules
                        .first_mut()
                        .expect("t")
                        .tools_allow
                        .push("shell".into())
                },
                Some("autonomy.scopes.rules[0].tools_allow"),
            ),
            (
                "tools_allow whitelist emptied (every tool)",
                |c| {
                    let mut r = rule("signal");
                    r.tools_allow = strings(&["memory_recall"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| c.autonomy.scopes.rules.first_mut().expect("t").tools_allow.clear(),
                Some("autonomy.scopes.rules[0].tools_allow"),
            ),
            (
                "tools_allow whitelist introduced (narrows)",
                |c| c.autonomy.scopes.rules.push(rule("signal")),
                |c| {
                    c.autonomy
                        .scopes
                        .rules
                        .first_mut()
                        .expect("t")
                        .tools_allow
                        .push("memory_recall".into())
                },
                None,
            ),
            (
                "send_deny entry dropped",
                |c| {
                    let mut r = rule("wacli");
                    r.send_deny = strings(&["*:+15550001111"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| c.autonomy.scopes.rules.first_mut().expect("t").send_deny.clear(),
                Some("autonomy.scopes.rules[0].send_deny"),
            ),
            (
                "send_allow whitelist gains a recipient",
                |c| {
                    let mut r = rule("wacli");
                    r.send_allow = strings(&["wacli:1"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| {
                    c.autonomy
                        .scopes
                        .rules
                        .first_mut()
                        .expect("t")
                        .send_allow
                        .push("wacli:2".into())
                },
                Some("autonomy.scopes.rules[0].send_allow"),
            ),
            (
                "assign_allow entry added (assignment is default-deny)",
                |c| c.autonomy.scopes.rules.push(rule("telegram")),
                |c| {
                    c.autonomy
                        .scopes
                        .rules
                        .first_mut()
                        .expect("t")
                        .assign_allow
                        .push("*:*".into())
                },
                Some("autonomy.scopes.rules[0].assign_allow"),
            ),
            (
                "assign_allow emptied with no later rule (falls through to default-deny, narrows)",
                |c| {
                    let mut r = rule("telegram");
                    r.assign_allow = strings(&["*:*"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| c.autonomy.scopes.rules.first_mut().expect("t").assign_allow.clear(),
                None,
            ),
            (
                "assign_deny entry dropped",
                |c| {
                    let mut r = rule("telegram");
                    r.assign_deny = strings(&["*:*"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| c.autonomy.scopes.rules.first_mut().expect("t").assign_deny.clear(),
                Some("autonomy.scopes.rules[0].assign_deny"),
            ),
            (
                "deny-only rule appended under default allow (narrows)",
                |_| {},
                |c| {
                    let mut r = rule("signal");
                    r.tools_deny = strings(&["shell"]);
                    c.autonomy.scopes.rules.push(r);
                },
                None,
            ),
            (
                "rule appended under default deny (admits what default refused)",
                |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Deny,
                |c| {
                    let mut r = rule("signal");
                    r.tools_deny = strings(&["shell"]);
                    c.autonomy.scopes.rules.push(r);
                },
                Some("autonomy.scopes.rules[0]"),
            ),
            (
                "rule with a whitelist appended",
                |_| {},
                |c| {
                    let mut r = rule("signal");
                    r.send_allow = strings(&["signal:*"]);
                    c.autonomy.scopes.rules.push(r);
                },
                Some("autonomy.scopes.rules[0]"),
            ),
            (
                "rule removed",
                |c| {
                    let mut r = rule("signal");
                    r.tools_deny = strings(&["shell"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| c.autonomy.scopes.rules.clear(),
                Some("autonomy.scopes.rules"),
            ),
            (
                "rule matcher changed",
                |c| c.autonomy.scopes.rules.push(rule("signal")),
                |c| c.autonomy.scopes.rules.first_mut().expect("t").channel = Some("telegram".into()),
                Some("autonomy.scopes.rules"),
            ),
            // ── MCP endpoint and node server ──
            (
                "a tool newly exposed on the MCP endpoint",
                |_| {},
                |c| c.mcp_server.exposed_tools.push("shell".into()),
                Some("mcp_server.exposed_tools"),
            ),
            (
                "a tool no longer exposed (narrows)",
                |_| {},
                |c| {
                    c.mcp_server.exposed_tools.pop();
                },
                None,
            ),
            (
                "node server TLS requirement turned off",
                |c| c.nodes.server.tls_required = true,
                |c| c.nodes.server.tls_required = false,
                Some("nodes.server.tls_required"),
            ),
            (
                "node server TLS requirement turned on (narrows)",
                |c| c.nodes.server.tls_required = false,
                |c| c.nodes.server.tls_required = true,
                None,
            ),
            // ── D-050 item 2: list fields by their runtime meaning ──
            (
                "send_allow introduced on a tools-only rule (R-SECRETSfg P21)",
                |c| {
                    let mut r = rule("signal");
                    r.user = Some("alice".into());
                    r.tools_deny = strings(&["x"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| c.autonomy.scopes.rules.first_mut().expect("t").send_allow = strings(&["*"]),
                Some("autonomy.scopes.rules[0].send_allow"),
            ),
            (
                "send_allow entry removed, one left (narrows)",
                |c| {
                    let mut r = rule("wacli");
                    r.send_allow = strings(&["wacli:1", "wacli:2"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| {
                    c.autonomy.scopes.rules.first_mut().expect("t").send_allow.pop();
                },
                None,
            ),
            (
                "send_allow emptied (falls through to a later whitelist or the reply channel)",
                |c| {
                    let mut r = rule("wacli");
                    r.send_allow = strings(&["wacli:1"]);
                    c.autonomy.scopes.rules.push(r);
                },
                |c| c.autonomy.scopes.rules.first_mut().expect("t").send_allow.clear(),
                Some("autonomy.scopes.rules[0].send_allow"),
            ),
            (
                "assign_allow emptied while a later overlapping rule has one (R-SECRETSfg P22)",
                |c| {
                    let mut alice = crate::config::ScopeRule {
                        user: Some("alice".into()),
                        ..crate::config::ScopeRule::default()
                    };
                    alice.assign_allow = strings(&["lab:s1"]);
                    let mut telegram = rule("telegram");
                    telegram.assign_allow = strings(&["*:*"]);
                    c.autonomy.scopes.rules.push(alice);
                    c.autonomy.scopes.rules.push(telegram);
                },
                |c| c.autonomy.scopes.rules.first_mut().expect("t").assign_allow.clear(),
                Some("autonomy.scopes.rules[0].assign_allow"),
            ),
            (
                "assign_allow emptied while the later rule cannot match the same requests (narrows)",
                |c| {
                    let mut alice = crate::config::ScopeRule {
                        user: Some("alice".into()),
                        ..crate::config::ScopeRule::default()
                    };
                    alice.assign_allow = strings(&["lab:s1"]);
                    let mut bob = crate::config::ScopeRule {
                        user: Some("bob".into()),
                        ..crate::config::ScopeRule::default()
                    };
                    bob.assign_allow = strings(&["*:*"]);
                    c.autonomy.scopes.rules.push(alice);
                    c.autonomy.scopes.rules.push(bob);
                },
                |c| c.autonomy.scopes.rules.first_mut().expect("t").assign_allow.clear(),
                None,
            ),
            // ── D-050 item 4: exposure and execution ──
            (
                "a tunnel switched on",
                |_| {},
                |c| c.tunnel.provider = "tailscale".into(),
                Some("tunnel.provider"),
            ),
            (
                "the tunnel switched off (narrows)",
                |c| c.tunnel.provider = "tailscale".into(),
                |c| c.tunnel.provider = "none".into(),
                None,
            ),
            (
                "Tailscale funnel turned on (public)",
                |c| {
                    c.tunnel.provider = "tailscale".into();
                    c.tunnel.tailscale = Some(section(serde_json::json!({"funnel": false})));
                },
                |c| c.tunnel.tailscale = Some(section(serde_json::json!({"funnel": true}))),
                Some("tunnel.tailscale.funnel"),
            ),
            (
                "Tailscale funnel turned off (narrows)",
                |c| {
                    c.tunnel.provider = "tailscale".into();
                    c.tunnel.tailscale = Some(section(serde_json::json!({"funnel": true})));
                },
                |c| c.tunnel.tailscale = Some(section(serde_json::json!({"funnel": false}))),
                None,
            ),
            (
                "custom tunnel command set",
                |_| {},
                |c| c.tunnel.custom = Some(section(serde_json::json!({"start_command": "tunnel-up"}))),
                Some("tunnel.custom.start_command"),
            ),
            (
                "custom tunnel command changed",
                |c| c.tunnel.custom = Some(section(serde_json::json!({"start_command": "tunnel-up"}))),
                |c| c.tunnel.custom = Some(section(serde_json::json!({"start_command": "/tmp/other"}))),
                Some("tunnel.custom.start_command"),
            ),
            (
                "custom tunnel removed (narrows)",
                |c| c.tunnel.custom = Some(section(serde_json::json!({"start_command": "tunnel-up"}))),
                |c| c.tunnel.custom = None,
                None,
            ),
            (
                "autonomy raised from supervised to full",
                |c| c.autonomy.level = crate::security::AutonomyLevel::Supervised,
                |c| c.autonomy.level = crate::security::AutonomyLevel::Full,
                Some("autonomy.level"),
            ),
            (
                "autonomy lowered to read-only (narrows)",
                |_| {},
                |c| c.autonomy.level = crate::security::AutonomyLevel::ReadOnly,
                None,
            ),
            (
                "workspace_only turned off",
                |c| c.autonomy.workspace_only = true,
                |c| c.autonomy.workspace_only = false,
                Some("autonomy.workspace_only"),
            ),
            (
                "workspace_only turned on (narrows)",
                |c| c.autonomy.workspace_only = false,
                |c| c.autonomy.workspace_only = true,
                None,
            ),
            (
                "a forbidden path removed",
                |_| {},
                |c| {
                    c.autonomy.forbidden_paths.pop();
                },
                Some("autonomy.forbidden_paths"),
            ),
            (
                "a forbidden path added (narrows)",
                |_| {},
                |c| c.autonomy.forbidden_paths.push("/srv".into()),
                None,
            ),
            (
                "a node command allowed",
                |_| {},
                |c| c.nodes.server.allowed_commands.push("sh".into()),
                Some("nodes.server.allowed_commands"),
            ),
            (
                "a node command no longer allowed (narrows)",
                |c| c.nodes.server.allowed_commands = strings(&["echo", "ls"]),
                |c| c.nodes.server.allowed_commands = strings(&["echo"]),
                None,
            ),
            (
                "a node command unblocked",
                |c| c.nodes.server.blocked_commands = strings(&["rm"]),
                |c| c.nodes.server.blocked_commands.clear(),
                Some("nodes.server.blocked_commands"),
            ),
            (
                "a node command blocked (narrows)",
                |_| {},
                |c| c.nodes.server.blocked_commands.push("rm".into()),
                None,
            ),
            // ── D-051: containment ──
            (
                "sandbox removed: runtime.kind docker to native (R-SECRETSfg P27)",
                |c| c.runtime.kind = "docker".into(),
                |c| c.runtime.kind = "native".into(),
                Some("runtime.kind"),
            ),
            (
                "sandbox added: runtime.kind native to docker (narrows)",
                |c| c.runtime.kind = "native".into(),
                |c| c.runtime.kind = "docker".into(),
                None,
            ),
            (
                "docker network opened to the host (R-SECRETSfg P27)",
                |c| c.runtime.docker.network = "none".into(),
                |c| c.runtime.docker.network = "host".into(),
                Some("runtime.docker.network"),
            ),
            (
                "docker network from none to the default bridge",
                |c| c.runtime.docker.network = "none".into(),
                |c| c.runtime.docker.network = String::new(),
                Some("runtime.docker.network"),
            ),
            (
                "docker network moved to another named network",
                |c| c.runtime.docker.network = "sandbox-a".into(),
                |c| c.runtime.docker.network = "sandbox-b".into(),
                Some("runtime.docker.network"),
            ),
            (
                "docker network from host to none (narrows)",
                |c| c.runtime.docker.network = "host".into(),
                |c| c.runtime.docker.network = "none".into(),
                None,
            ),
            (
                "docker network from host to a named network (narrows)",
                |c| c.runtime.docker.network = "host".into(),
                |c| c.runtime.docker.network = "sandbox-a".into(),
                None,
            ),
            (
                "operator tools exposed to channel sessions (R-SECRETSfg P27)",
                |_| {},
                |c| c.tool_tiering.channel_exclude.clear(),
                Some("tool_tiering"),
            ),
            (
                "an operator tool hidden from channel sessions (narrows)",
                |_| {},
                |c| c.tool_tiering.channel_exclude.push("shell".into()),
                None,
            ),
            (
                "docker root filesystem made writable",
                |c| c.runtime.docker.read_only_rootfs = true,
                |c| c.runtime.docker.read_only_rootfs = false,
                Some("runtime.docker.read_only_rootfs"),
            ),
            (
                "docker root filesystem made read-only (narrows)",
                |c| c.runtime.docker.read_only_rootfs = false,
                |c| c.runtime.docker.read_only_rootfs = true,
                None,
            ),
            (
                "a docker workspace root added",
                |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a"]),
                |c| c.runtime.docker.allowed_workspace_roots.push("/".into()),
                Some("runtime.docker.allowed_workspace_roots"),
            ),
            (
                "docker workspace roots emptied (any workspace)",
                |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a"]),
                |c| c.runtime.docker.allowed_workspace_roots.clear(),
                Some("runtime.docker.allowed_workspace_roots"),
            ),
            (
                "a docker workspace root removed, one left (narrows)",
                |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a", "/srv/b"]),
                |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a"]),
                None,
            ),
            (
                "docker workspace roots introduced (narrows)",
                |_| {},
                |c| c.runtime.docker.allowed_workspace_roots = strings(&["/srv/a"]),
                None,
            ),
            (
                "a fetchable domain added",
                |c| c.browser.allowed_domains = strings(&["example.com"]),
                |c| c.browser.allowed_domains.push("attacker.invalid".into()),
                Some("browser.allowed_domains"),
            ),
            (
                "a wildcard domain added",
                |c| c.browser.allowed_domains = strings(&["example.com"]),
                |c| c.browser.allowed_domains.push("*".into()),
                Some("browser.allowed_domains"),
            ),
            (
                "fetchable domains emptied (any public host)",
                |c| c.browser.allowed_domains = strings(&["example.com"]),
                |c| c.browser.allowed_domains.clear(),
                Some("browser.allowed_domains"),
            ),
            (
                "a fetchable domain respelled the way the tool normalises it (no change)",
                |c| c.browser.allowed_domains = strings(&["example.com"]),
                |c| c.browser.allowed_domains = strings(&["https://Example.com/"]),
                None,
            ),
            (
                "a fetchable domain removed, one left (narrows)",
                |c| c.browser.allowed_domains = strings(&["example.com", "example.org"]),
                |c| c.browser.allowed_domains = strings(&["example.com"]),
                None,
            ),
            (
                "fetchable domains introduced (narrows)",
                |_| {},
                |c| c.browser.allowed_domains = strings(&["example.com"]),
                None,
            ),
            (
                "an excluded tool re-admitted through always_include (R-SECRETSj P27b)",
                |_| {},
                |c| c.tool_tiering.always_include.push("proxy_config".into()),
                Some("tool_tiering"),
            ),
            (
                "an always_include entry removed (narrows)",
                |c| c.tool_tiering.always_include.push("proxy_config".into()),
                |c| c.tool_tiering.always_include.clear(),
                None,
            ),
            (
                "an always_exclude entry removed (offered on every surface)",
                |c| c.tool_tiering.always_exclude.push("shell".into()),
                |c| c.tool_tiering.always_exclude.clear(),
                Some("tool_tiering"),
            ),
            (
                "always_exclude cleared while always_include names the same tool (R-SECRETSj K-1)",
                |c| {
                    c.tool_tiering.always_exclude.push("shell".into());
                    c.tool_tiering.always_include.push("shell".into());
                },
                |c| c.tool_tiering.always_exclude.clear(),
                Some("tool_tiering"),
            ),
            (
                "always_include naming an always_exclude tool changes nothing (K-1)",
                |c| c.tool_tiering.always_exclude.push("shell".into()),
                |c| c.tool_tiering.always_include.push("shell".into()),
                None,
            ),
            (
                "an always_exclude entry added (narrows)",
                |_| {},
                |c| c.tool_tiering.always_exclude.push("shell".into()),
                None,
            ),
            (
                "the host workspace mounted into the container",
                |c| c.runtime.docker.mount_workspace = false,
                |c| c.runtime.docker.mount_workspace = true,
                Some("runtime.docker.mount_workspace"),
            ),
            (
                "the host workspace no longer mounted (narrows)",
                |c| c.runtime.docker.mount_workspace = true,
                |c| c.runtime.docker.mount_workspace = false,
                None,
            ),
            (
                "the audit trail shortened",
                |c| c.security.audit.max_size_mb = 100,
                |c| c.security.audit.max_size_mb = 1,
                Some("security.audit.max_size_mb"),
            ),
            (
                "the audit trail lengthened (narrows)",
                |c| c.security.audit.max_size_mb = 10,
                |c| c.security.audit.max_size_mb = 100,
                None,
            ),
            (
                "the audit log moved",
                |_| {},
                |c| c.security.audit.log_path = "logs/other.log".into(),
                Some("security.audit.log_path"),
            ),
            // ── D-050 item 1: one canonical reading ──
            (
                "scope default from deny to allow",
                |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Deny,
                |c| c.autonomy.scopes.default = crate::config::ScopeDefault::Allow,
                Some("autonomy.scopes.default"),
            ),
        ];
        for (name, setup, change, widened) in cases {
            let mut before = Config::default();
            setup(&mut before);
            let mut after = before.clone();
            change(&mut after);
            let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
            match widened {
                Some(path) => {
                    assert_eq!(summary.risk, ConfigChangeRisk::High, "{name}: {summary:?}");
                    assert!(
                        summary.risk_reasons.iter().any(|reason| matches!(
                            reason,
                            RiskReason::AccessGrantChanged { path: reported }
                                | RiskReason::PrivilegeWidened { path: reported } if reported == path
                        )),
                        "{name}: {:?}",
                        summary.risk_reasons
                    );
                }
                None => assert_eq!(summary.risk, ConfigChangeRisk::Low, "{name}: {summary:?}"),
            }
        }
    }
}
