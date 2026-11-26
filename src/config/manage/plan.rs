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
    /// 13.1, CXR 4.4): `decision_engine.backend.remote_enabled` turned on, or,
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
        let before_secrets = before_raw
            .as_ref()
            .map(|tree| secret_leaves(tree, SecretArrayIdentity::Value))
            .unwrap_or_default();
        let after_secrets = secret_leaves(&after_raw, SecretArrayIdentity::Value);
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
        let after_reference = &crate::config::schema::decision_engine::effective_engine_backend(after).credential_ref;
        let decision_reference_added = matches!(
            after_reference,
            crate::config::schema::decision_engine::CredentialRefSetting::Profile(_)
        ) && before.is_none_or(|before| {
            &crate::config::schema::decision_engine::effective_engine_backend(before).credential_ref != after_reference
        });
        if decision_reference_added {
            secret_writes.push("decision_engine.backend.credential_ref".to_string());
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

#[derive(Clone, Copy)]
enum SecretArrayIdentity {
    Value,
    PopulatedSlot,
}

/// Every non-empty secret value of a rendered configuration, keyed by its
/// identity: the path with every array element named by what identifies it
/// (a table's `hint`, a string's own value for risk classification) instead
/// of its position, so a removed or reordered neighbour changes no secret
/// (D-101 F-3). Omission checks use populated string-array slots instead.
/// Identities are compared in memory only; nothing shows them.
fn secret_leaves(root: &Value, array_identity: SecretArrayIdentity) -> std::collections::BTreeMap<String, SecretLeaf> {
    fn walk(
        node: &Value,
        raw: &mut Vec<PathElem>,
        display: &mut Vec<PathElem>,
        identity: &mut Vec<String>,
        inside_secret: bool,
        array_identity: SecretArrayIdentity,
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
                    walk(child, raw, display, identity, inside_secret, array_identity, out);
                    identity.pop();
                    display.pop();
                    raw.pop();
                }
            }
            Value::Array(items) => {
                let hint_of = |item: &Value| item.get("hint").and_then(Value::as_str).map(str::to_string);
                let mut populated = 0;
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
                        Value::String(value) if inside_secret => match array_identity {
                            SecretArrayIdentity::Value => Some(format!("[={value}]")),
                            SecretArrayIdentity::PopulatedSlot => {
                                if value.is_empty() || value == secret_paths::REDACTION_MASK {
                                    continue;
                                }
                                let slot = populated;
                                populated += 1;
                                Some(format!("[populated={slot}]"))
                            }
                        },
                        _ => None,
                    };
                    raw.push(PathElem::Index(index));
                    display.push(PathElem::Index(index));
                    identity.push(named.unwrap_or_else(|| format!("[{index}]")));
                    walk(child, raw, display, identity, inside_secret, array_identity, out);
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
    walk(
        root,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        false,
        array_identity,
        &mut out,
    );
    out
}

/// Raw omissions retain keyed-route identities but compare string arrays by
/// populated count: rotation is a credential change, not an omitted slot.
pub(super) fn omitted_secret_paths(before: &Value, after: &Value) -> Vec<String> {
    let after = secret_leaves(after, SecretArrayIdentity::PopulatedSlot);
    secret_leaves(before, SecretArrayIdentity::PopulatedSlot)
        .into_iter()
        .filter(|(identity, leaf)| leaf.value != secret_paths::REDACTION_MASK && !after.contains_key(identity))
        .map(|(_, leaf)| leaf.display)
        .collect()
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
                && path != "decision_engine.backend.credential_ref"
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
    let default_section = crate::config::schema::decision_engine::DecisionEngineConfig::default();
    let old_section = before.map_or(&default_section, |before| &before.decision_engine);
    let new_section = &after.decision_engine;
    let old = old_section
        .backend
        .as_ref()
        .unwrap_or(&crate::config::schema::decision_engine::DEFAULT_BACKEND);
    let new = new_section
        .backend
        .as_ref()
        .unwrap_or(&crate::config::schema::decision_engine::DEFAULT_BACKEND);
    let mut reasons = Vec::new();
    let path = |field: &str| format!("decision_engine.backend.{field}");
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
    } else if sends_remotely(new_section.enabled, new) {
        if sends_remotely(old_section.enabled, old) {
            // Still remote: a change to where or how decisions are sent.
            reasons.extend(
                target_changes
                    .iter()
                    .filter(|(_, changed)| *changed)
                    .map(|(field, _)| RiskReason::RemoteDecisionEnabled { path: path(field) }),
            );
        } else if !old_section.enabled {
            // L-1: enabling the engine starts remote I/O to a configured
            // remote target.
            reasons.push(RiskReason::RemoteDecisionEnabled {
                path: "decision_engine.enabled".to_string(),
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

/// Whether a backend sends decision state off this machine (D-079 M-2): the
/// engine is enabled, remote processing is on, and the configured backend is
/// remote by the factory's own rule (`configured_kind`, `locality_of`;
/// D-082 N-5): a hosted kind, or a compatible endpoint that is not a literal
/// loopback address. The builtin, `auto` without an endpoint and a loopback
/// endpoint are local.
fn sends_remotely(enabled: bool, backend: &crate::config::schema::decision_engine::DecisionBackendConfig) -> bool {
    enabled
        && backend.remote_enabled
        && crate::config::schema::decision_engine::configured_kind(backend).is_some_and(|kind| {
            crate::config::schema::decision_engine::locality_of(kind, backend.endpoint.trim())
                == crate::config::schema::decision_engine::BackendLocality::Remote
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
        allowlist_widened(&old_server.allowed_commands, &new_server.allowed_commands),
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
        after
            .runtime
            .shell_env_allowlist
            .iter()
            .any(|name| !before.runtime.shell_env_allowlist.contains(name)),
        "runtime.shell_env_allowlist",
    );
    push(
        before.runtime.kind == "docker" && after.runtime.kind != "docker",
        "runtime.kind",
    );
    push(before.runtime.workdir != after.runtime.workdir, "runtime.workdir");
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

/// Whether a node command allowlist became wider. An empty list or a `"*"`
/// entry allows any command, so it is the widest state.
fn allowlist_widened(before: &[String], after: &[String]) -> bool {
    let allows_any = |list: &[String]| list.is_empty() || list.iter().any(|entry| entry == "*");
    if allows_any(before) {
        return false;
    }
    allows_any(after) || adds_entries(before, after)
}

/// Whether `before` holds an entry `after` dropped.
fn drops_entries(before: &[String], after: &[String]) -> bool {
    adds_entries(after, before)
}

/// Whether a sender allow-list is narrower than any listening scope: it has
/// no `"*"`, and it names senders or, where the channel reads an empty list
/// as "nobody" (Discord, Slack, Mattermost, IRC), is empty. Signal
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
mod tests;
