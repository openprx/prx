use anyhow::Result;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params, types::Value};
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

pub const CURRENT_POLICY_VERSION: i64 = 1;

/// Canonical system-principal identifiers.
///
/// FIX-P0-24 (#17): a single source of truth shared by both the SQLite and
/// Postgres memory backends so `is_system_principal` recognizes the exact same
/// set of names. Adding a name here updates both backends at once.
pub const SYSTEM_PRINCIPAL_IDS: [&str; 4] = ["self_system", "router", "internal", "system"];

/// Returns `true` when `name` is one of the canonical system-principal ids.
///
/// Both memory backends pass `agent_id` / `persona_id` here to decide whether a
/// principal is an internal/system actor that bypasses owner-scoped ACL.
#[must_use]
pub fn is_system_principal(name: &str) -> bool {
    SYSTEM_PRINCIPAL_IDS.contains(&name)
}

/// Canonical owner-centric identity carried by runtime ingress.
///
/// Phase 0 maps this onto existing `sender_id`/`raw_sender` columns so current
/// SQLite/Postgres ACL code can start using a shared owner anchor without a
/// disruptive schema rewrite. Later phases can persist `owner_id` explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerPrincipal {
    pub owner_id: String,
    pub principal_id: String,
    pub workspace_id: String,
    pub source_channel: String,
    pub external_subject: String,
    pub session_key: String,
    pub roles: Vec<Role>,
    pub policy_version: i64,
}

impl OwnerPrincipal {
    #[must_use]
    pub fn new(
        workspace_id: impl Into<String>,
        source_channel: impl Into<String>,
        external_subject: impl Into<String>,
        session_key: impl Into<String>,
        roles: Vec<Role>,
    ) -> Self {
        let workspace_id = workspace_id.into();
        let source_channel = normalize_identity_part(source_channel.into(), "local");
        let external_subject = normalize_identity_part(external_subject.into(), "unknown");
        let session_key = normalize_identity_part(session_key.into(), "session");
        let principal_id = format!("{source_channel}:{external_subject}");
        let owner_id = format!("owner:{workspace_id}:{principal_id}");

        Self {
            owner_id,
            principal_id,
            workspace_id,
            source_channel,
            external_subject,
            session_key,
            roles,
            policy_version: CURRENT_POLICY_VERSION,
        }
    }

    #[must_use]
    pub fn from_write_context(
        workspace_id: impl Into<String>,
        session_key: impl Into<String>,
        ctx: &MemoryWriteContext,
    ) -> Self {
        Self::new(
            workspace_id,
            ctx.channel.as_deref().unwrap_or("local"),
            ctx.sender_id
                .as_deref()
                .or(ctx.raw_sender.as_deref())
                .unwrap_or("unknown"),
            session_key,
            vec![Role::Anonymous],
        )
    }
}

fn normalize_identity_part(value: impl Into<String>, fallback: &str) -> String {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.replace(['\n', '\r', '\t'], " ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Owner,
    Member,
    Guest,
    Anonymous,
}

impl Role {
    fn from_db(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "owner" => Self::Owner,
            "member" => Self::Member,
            "anonymous" => Self::Anonymous,
            _ => Self::Guest,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Visibility {
    System,
    Owner,
    Private,
    User,
    Group,
    Project,
    Public,
}

impl Visibility {
    pub const fn ordinal(&self) -> u8 {
        match self {
            Self::System => 0,
            Self::Owner => 1,
            Self::Private => 2,
            Self::User => 3,
            Self::Group => 4,
            Self::Project => 5,
            Self::Public => 6,
        }
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Owner => "owner",
            Self::Private => "private",
            Self::User => "user",
            Self::Group => "group",
            Self::Project => "project",
            Self::Public => "public",
        }
    }

    fn from_db(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "system" => Self::System,
            "owner" => Self::Owner,
            "user" => Self::User,
            "group" => Self::Group,
            "project" => Self::Project,
            "public" => Self::Public,
            _ => Self::Private,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sensitivity {
    Normal,
    Sensitive,
    Secret,
}

impl Sensitivity {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Sensitive => "sensitive",
            Self::Secret => "secret",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatType {
    Dm,
    Group,
    Webhook,
    Cron,
    Internal,
}

impl ChatType {
    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "group" => Self::Group,
            "webhook" => Self::Webhook,
            "cron" => Self::Cron,
            "internal" => Self::Internal,
            "dm" | "direct" | "private" => Self::Dm,
            _ => Self::Dm,
        }
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Dm => "dm",
            Self::Group => "group",
            Self::Webhook => "webhook",
            Self::Cron => "cron",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub role: Role,
    pub projects: Vec<String>,
    pub visibility_ceiling: Visibility,
    pub blocked_patterns: Vec<Regex>,
    pub current_channel: String,
    pub current_chat_id: String,
    pub current_chat_type: ChatType,
    /// Raw channel-account anchor for the originating sender (FIX-P1-06).
    ///
    /// The Anonymous scope matches durable rows on the
    /// `(channel, chat_id, raw_sender)` triple so an unbound sender can still
    /// reach memories they themselves produced. Empty string means "no anchor".
    pub raw_sender: String,
    pub acl_enforced: bool,
}

/// Dialect-agnostic scope parameter value.
///
/// D11: the single-source scope predicate (`build_scope_predicate`) emits a
/// SQL template plus an ordered list of these values. Every backend renderer
/// (`build_sql_scope` for SQLite `?`, `build_sql_scope_pg` for Postgres `$N`)
/// binds them through parameterized queries only — never string-interpolated
/// into SQL (iron rule 9). All current scope binds are plain text columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeParam {
    Text(String),
}

/// Placeholder marker used inside a `ScopePredicate::template`.
///
/// Each occurrence corresponds positionally to one entry in
/// `ScopePredicate::params`. Backend renderers replace these markers with their
/// dialect-specific placeholder (`?` for SQLite, `$N` for Postgres). The marker
/// must never collide with literal SQL text; `{}` is safe here because the
/// canonical predicate contains no `format!`-style braces in its literals.
const SCOPE_BIND_MARKER: &str = "{}";

/// Dialect-agnostic scope predicate: the single source of truth for the
/// memory-visibility truth table shared by every backend.
///
/// The `template` is a WHERE-clause fragment whose bind points are marked with
/// [`SCOPE_BIND_MARKER`]; `params` lists the bound values in template order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopePredicate {
    pub template: String,
    pub params: Vec<ScopeParam>,
}

impl Principal {
    /// Single-source scope truth table (D11).
    ///
    /// Produces a dialect-agnostic [`ScopePredicate`] from `role` /
    /// `visibility_ceiling`. Both the SQLite and Postgres backends render from
    /// this one function, so the visibility truth table can no longer drift
    /// between backends. Bind points use [`SCOPE_BIND_MARKER`]; the literal SQL
    /// is portable (plain scalar comparisons plus one standard sub-query, no
    /// dialect-specific functions).
    pub fn build_scope_predicate(&self) -> ScopePredicate {
        let m = SCOPE_BIND_MARKER;

        if !self.acl_enforced {
            return ScopePredicate {
                template: "1=1".to_string(),
                params: Vec::new(),
            };
        }

        match self.role {
            Role::Owner => ScopePredicate {
                template: "1=1".to_string(),
                params: Vec::new(),
            },
            // FIX-P1-06: unify the Anonymous scope with the previously hardcoded
            // SQLite recall/forget branches. An anonymous principal sees public
            // rows plus rows it authored, matched on the
            // `(channel, chat_id, raw_sender)` triple, excluding secrets.
            Role::Anonymous => ScopePredicate {
                template: format!(
                    "(visibility = 'public' OR (channel = {m} AND chat_id = {m} AND raw_sender = {m})) \
                     AND sensitivity != 'secret'"
                ),
                params: vec![
                    ScopeParam::Text(self.current_channel.clone()),
                    ScopeParam::Text(self.current_chat_id.clone()),
                    ScopeParam::Text(self.raw_sender.clone()),
                ],
            },
            Role::Member | Role::Guest => {
                let ceiling_ord = self.visibility_ceiling.ordinal();
                let mut conditions = vec!["visibility = 'public'".to_string()];
                let mut params = Vec::new();

                if ceiling_ord >= Visibility::Private.ordinal() {
                    conditions.push(format!(
                        "(visibility = 'private' AND chat_type = 'dm' AND channel = {m} AND chat_id = {m})"
                    ));
                    params.push(ScopeParam::Text(self.current_channel.clone()));
                    params.push(ScopeParam::Text(self.current_chat_id.clone()));
                }

                if ceiling_ord >= Visibility::User.ordinal() {
                    conditions.push(format!("(visibility = 'user' AND sender_id = {m})"));
                    params.push(ScopeParam::Text(self.user_id.clone()));
                }

                if ceiling_ord >= Visibility::Group.ordinal() {
                    conditions.push(format!(
                        "(visibility = 'group' AND chat_type = 'group' AND channel = {m} AND chat_id = {m})"
                    ));
                    params.push(ScopeParam::Text(self.current_channel.clone()));
                    params.push(ScopeParam::Text(self.current_chat_id.clone()));
                }

                if ceiling_ord >= Visibility::Project.ordinal() && !self.projects.is_empty() {
                    let placeholders = (0..self.projects.len()).map(|_| m).collect::<Vec<_>>().join(",");
                    conditions.push(format!(
                        "(visibility = 'project' AND topic_id IN (\
                            SELECT t.id FROM topics t \
                            INNER JOIN topic_participants tp ON tp.topic_id = t.id \
                            WHERE t.project IN ({placeholders}) \
                            AND tp.user_id = {m}\
                        ))"
                    ));
                    params.extend(self.projects.iter().cloned().map(ScopeParam::Text));
                    params.push(ScopeParam::Text(self.user_id.clone()));
                }

                ScopePredicate {
                    template: format!("({}) AND sensitivity != 'secret'", conditions.join(" OR ")),
                    params,
                }
            }
        }
    }

    /// SQLite renderer (`?` placeholders). Thin wrapper over
    /// [`build_scope_predicate`](Self::build_scope_predicate): each
    /// [`SCOPE_BIND_MARKER`] becomes a `?`, each [`ScopeParam`] becomes a
    /// `rusqlite` [`Value`]. Behavior is byte-for-byte identical to the prior
    /// hand-written implementation (locked by the `golden_build_sql_scope_*`
    /// tests).
    pub fn build_sql_scope(&self) -> (String, Vec<Value>) {
        let predicate = self.build_scope_predicate();
        let sql = predicate.template.replace(SCOPE_BIND_MARKER, "?");
        let params = predicate
            .params
            .into_iter()
            .map(|param| match param {
                ScopeParam::Text(text) => Value::from(text),
            })
            .collect();
        (sql, params)
    }
}

#[derive(Debug, Clone, Default)]
pub struct MemoryWriteContext {
    pub channel: Option<String>,
    pub chat_type: Option<String>,
    pub chat_id: Option<String>,
    pub sender_id: Option<String>,
    pub raw_sender: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MemoryClassification {
    pub visibility: Visibility,
    pub sensitivity: Sensitivity,
    pub risk_signals: Vec<String>,
    pub policy_version: i64,
}

pub fn post_filter<T, F>(memories: Vec<T>, principal: &Principal, mut content_of: F) -> Vec<T>
where
    F: FnMut(&T) -> &str,
{
    if principal.role == Role::Owner || principal.blocked_patterns.is_empty() {
        return memories;
    }

    memories
        .into_iter()
        .filter(|memory| {
            let normalized = content_of(memory).nfkc().collect::<String>().to_lowercase();
            let no_space = normalized.replace(' ', "");
            !principal
                .blocked_patterns
                .iter()
                .any(|re| re.is_match(&normalized) || re.is_match(&no_space))
        })
        .collect()
}

pub fn log_access(
    conn: &Connection,
    principal: &Principal,
    action: &str,
    query: Option<&str>,
    memory_id: Option<&str>,
    policy_rule: Option<&str>,
    result: &str,
) {
    if principal.role == Role::Owner {
        return;
    }

    conn.execute(
        "INSERT INTO access_audit_log (id, timestamp, requester, action, query, memory_id, policy_rule, result)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            Uuid::new_v4().to_string(),
            chrono::Utc::now().to_rfc3339(),
            &principal.user_id,
            action,
            query,
            memory_id,
            policy_rule,
            result,
        ],
    )
    .ok();
}

pub fn resolve_principal(conn: &Connection, ctx: &MemoryWriteContext) -> Result<Principal> {
    let current_channel = ctx.channel.clone().unwrap_or_default();
    let current_chat_id = ctx.chat_id.clone().unwrap_or_default();
    let current_chat_type = ctx.chat_type.as_deref().map(ChatType::from_str).unwrap_or(ChatType::Dm);

    let raw_sender_anchor = ctx.raw_sender.clone().unwrap_or_default();

    let Some(channel) = ctx.channel.as_deref() else {
        return Ok(anonymous_principal(
            "anonymous:unknown:unknown".to_string(),
            current_channel,
            current_chat_id,
            current_chat_type,
            raw_sender_anchor,
        ));
    };

    let Some(raw_sender) = ctx.raw_sender.as_deref() else {
        return Ok(anonymous_principal(
            format!("anonymous:{channel}:unknown"),
            current_channel,
            current_chat_id,
            current_chat_type,
            raw_sender_anchor,
        ));
    };

    let binding_user_id: Option<String> = conn
        .query_row(
            "SELECT user_id FROM identity_bindings WHERE channel = ?1 AND channel_account = ?2",
            params![channel, raw_sender],
            |row| row.get(0),
        )
        .optional()?;

    let Some(user_id) = binding_user_id else {
        return Ok(anonymous_principal(
            format!("anonymous:{channel}:{raw_sender}"),
            current_channel,
            current_chat_id,
            current_chat_type,
            raw_sender_anchor,
        ));
    };

    let policy: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT role, projects, visibility_ceiling, blocked_patterns \
             FROM user_policies WHERE user_id = ?1",
            params![&user_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

    Ok(principal_from_policy(
        user_id,
        policy,
        current_channel,
        current_chat_id,
        current_chat_type,
        raw_sender_anchor,
    ))
}

/// Build a [`Principal`] from a resolved user id and optional `user_policies`
/// row, sharing the exact role/visibility/blocked-pattern logic used by
/// [`resolve_principal`]. Backends that resolve identity through their own SQL
/// (for example Postgres) call this so policy interpretation stays identical.
///
/// `policy` is `(role, projects_json, visibility_ceiling, blocked_patterns_json)`.
#[must_use]
pub fn principal_from_policy(
    user_id: String,
    policy: Option<(String, String, String, String)>,
    current_channel: String,
    current_chat_id: String,
    current_chat_type: ChatType,
    raw_sender: String,
) -> Principal {
    if let Some((role_raw, projects_raw, ceiling_raw, blocked_raw)) = policy {
        let role = Role::from_db(&role_raw);
        let projects = parse_json_array(&projects_raw);
        let blocked_patterns = compile_patterns(parse_json_array(&blocked_raw));
        let acl_enforced = is_acl_enforced_for_role(&role);

        return Principal {
            user_id,
            role,
            projects,
            visibility_ceiling: Visibility::from_db(&ceiling_raw),
            blocked_patterns,
            current_channel,
            current_chat_id,
            current_chat_type,
            raw_sender,
            acl_enforced,
        };
    }

    Principal {
        user_id,
        role: Role::Guest,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Private,
        blocked_patterns: Vec::new(),
        current_channel,
        current_chat_id,
        current_chat_type,
        raw_sender,
        acl_enforced: is_acl_enforced_for_role(&Role::Guest),
    }
}

pub fn classify_memory(ctx: &MemoryWriteContext, content: &str, principal: &Principal) -> MemoryClassification {
    let mut risk_signals = Vec::new();
    if matches_sensitive_patterns(content) {
        risk_signals.push("sensitive_keyword_match".to_string());
    }
    if contains_pii(content) {
        risk_signals.push("pii_detected".to_string());
    }

    let chat_type = ctx.chat_type.as_deref().map(ChatType::from_str).unwrap_or(ChatType::Dm);

    let base_visibility = match (&principal.role, &chat_type) {
        (Role::Owner, ChatType::Dm) => Visibility::Owner,
        (_, ChatType::Webhook | ChatType::Cron | ChatType::Internal) => Visibility::Owner,
        (_, ChatType::Group) => Visibility::Group,
        (Role::Member, ChatType::Dm) => Visibility::Private,
        _ => Visibility::Private,
    };

    let visibility = if !risk_signals.is_empty() && base_visibility > Visibility::Owner {
        Visibility::Owner
    } else {
        base_visibility
    };

    let sensitivity = if risk_signals.is_empty() {
        Sensitivity::Normal
    } else {
        Sensitivity::Sensitive
    };

    MemoryClassification {
        visibility,
        sensitivity,
        risk_signals,
        policy_version: CURRENT_POLICY_VERSION,
    }
}

pub fn matches_sensitive_patterns(content: &str) -> bool {
    let normalized = content.nfkc().collect::<String>().to_lowercase();
    let no_space = normalized.replace(' ', "");

    SENSITIVE_PATTERNS
        .iter()
        .any(|re| re.is_match(&normalized) || re.is_match(&no_space))
}

fn contains_pii(content: &str) -> bool {
    EMAIL_RE.is_match(content) || IPV4_RE.is_match(content)
}

fn parse_json_array(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn compile_patterns(patterns: Vec<String>) -> Vec<Regex> {
    patterns
        .into_iter()
        .filter_map(|pattern| Regex::new(&pattern).ok())
        .collect()
}

const fn anonymous_principal(
    user_id: String,
    current_channel: String,
    current_chat_id: String,
    current_chat_type: ChatType,
    raw_sender: String,
) -> Principal {
    Principal {
        user_id,
        role: Role::Anonymous,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Private,
        blocked_patterns: Vec::new(),
        current_channel,
        current_chat_id,
        current_chat_type,
        raw_sender,
        acl_enforced: is_acl_enforced_for_role(&Role::Anonymous),
    }
}

const ACL_ENFORCE_ANONYMOUS: bool = true;
const ACL_ENFORCE_GUEST: bool = true;
const ACL_ENFORCE_MEMBER: bool = true;

const fn is_acl_enforced_for_role(role: &Role) -> bool {
    match role {
        Role::Owner => false,
        Role::Anonymous => ACL_ENFORCE_ANONYMOUS,
        Role::Guest => ACL_ENFORCE_GUEST,
        Role::Member => ACL_ENFORCE_MEMBER,
    }
}

#[allow(clippy::expect_used)]
static SENSITIVE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"\bssh\b").expect("compile regex: ssh keyword"),
        Regex::new(r"\bapi[_-]?key\b").expect("compile regex: api key keyword"),
        Regex::new(r"密钥|私钥|秘钥").expect("compile regex: Chinese key/secret keywords"),
        Regex::new(r"\bpassw(or)?d\b").expect("compile regex: password keyword"),
        Regex::new(r"\btok(en)?\b").expect("compile regex: token keyword"),
        Regex::new(r"\bsecret\b").expect("compile regex: secret keyword"),
        Regex::new(r"im-ops|服务器地址").expect("compile regex: server address keywords"),
        Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").expect("compile regex: IPv4 address pattern"),
        Regex::new(r"\bprivate[_\s]?key\b").expect("compile regex: private key keyword"),
    ]
});

#[allow(clippy::expect_used)]
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b").expect("compile regex: email address pattern")
});
#[allow(clippy::expect_used)]
static IPV4_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").expect("compile regex: IPv4 address pattern"));

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal,
        clippy::trivial_regex
    )]
    use super::*;

    #[test]
    fn resolve_principal_returns_anonymous_without_binding() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE identity_bindings (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, channel TEXT NOT NULL, channel_account TEXT NOT NULL, display_name TEXT, bound_at TEXT NOT NULL, bound_by TEXT NOT NULL, UNIQUE(channel, channel_account));
             CREATE TABLE user_policies (user_id TEXT PRIMARY KEY, role TEXT NOT NULL DEFAULT 'guest', projects TEXT NOT NULL DEFAULT '[]', visibility_ceiling TEXT NOT NULL DEFAULT 'private', blocked_patterns TEXT NOT NULL DEFAULT '[]', policy_version INTEGER NOT NULL DEFAULT 1, updated_at TEXT NOT NULL);",
        )
        .unwrap();

        let principal = resolve_principal(
            &conn,
            &MemoryWriteContext {
                channel: Some("signal".into()),
                chat_type: Some("dm".into()),
                chat_id: Some("chat-1".into()),
                sender_id: None,
                raw_sender: Some("sender-a".into()),
            },
        )
        .unwrap();

        assert_eq!(principal.role, Role::Anonymous);
        assert_eq!(principal.user_id, "anonymous:signal:sender-a");
    }

    #[test]
    fn owner_principal_from_write_context_uses_sender_id_as_stable_anchor() {
        let ctx = MemoryWriteContext {
            channel: Some("signal".into()),
            chat_type: Some("dm".into()),
            chat_id: Some("chat-1".into()),
            sender_id: Some("+15551234567".into()),
            raw_sender: Some("display-name".into()),
        };

        let owner = OwnerPrincipal::from_write_context("workspace-a", "signal_display-name", &ctx);

        assert_eq!(owner.workspace_id, "workspace-a");
        assert_eq!(owner.source_channel, "signal");
        assert_eq!(owner.external_subject, "+15551234567");
        assert_eq!(owner.principal_id, "signal:+15551234567");
        assert_eq!(owner.owner_id, "owner:workspace-a:signal:+15551234567");
    }

    #[test]
    fn resolve_principal_loads_policy_for_bound_user() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE identity_bindings (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, channel TEXT NOT NULL, channel_account TEXT NOT NULL, display_name TEXT, bound_at TEXT NOT NULL, bound_by TEXT NOT NULL, UNIQUE(channel, channel_account));
             CREATE TABLE user_policies (user_id TEXT PRIMARY KEY, role TEXT NOT NULL DEFAULT 'guest', projects TEXT NOT NULL DEFAULT '[]', visibility_ceiling TEXT NOT NULL DEFAULT 'private', blocked_patterns TEXT NOT NULL DEFAULT '[]', policy_version INTEGER NOT NULL DEFAULT 1, updated_at TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by) VALUES ('1', 'ak', 'signal', 'sender-ak', '2026-02-23T00:00:00Z', 'system')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at) VALUES ('ak', 'owner', '[\"alpha\"]', 'public', '[\"token\"]', '2026-02-23T00:00:00Z')",
            [],
        )
        .unwrap();

        let principal = resolve_principal(
            &conn,
            &MemoryWriteContext {
                channel: Some("signal".into()),
                chat_type: Some("dm".into()),
                chat_id: Some("chat-ak".into()),
                sender_id: None,
                raw_sender: Some("sender-ak".into()),
            },
        )
        .unwrap();

        assert_eq!(principal.user_id, "ak");
        assert_eq!(principal.role, Role::Owner);
        assert_eq!(principal.projects, vec!["alpha".to_string()]);
        assert_eq!(principal.visibility_ceiling, Visibility::Public);
        assert_eq!(principal.blocked_patterns.len(), 1);
        assert!(!principal.acl_enforced);
    }

    #[test]
    fn resolve_principal_sets_acl_rollout_flags() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE identity_bindings (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, channel TEXT NOT NULL, channel_account TEXT NOT NULL, display_name TEXT, bound_at TEXT NOT NULL, bound_by TEXT NOT NULL, UNIQUE(channel, channel_account));
             CREATE TABLE user_policies (user_id TEXT PRIMARY KEY, role TEXT NOT NULL DEFAULT 'guest', projects TEXT NOT NULL DEFAULT '[]', visibility_ceiling TEXT NOT NULL DEFAULT 'private', blocked_patterns TEXT NOT NULL DEFAULT '[]', policy_version INTEGER NOT NULL DEFAULT 1, updated_at TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by) VALUES ('1', 'member_u', 'signal', 'sender-member', '2026-02-23T00:00:00Z', 'system')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at) VALUES ('member_u', 'member', '[]', 'private', '[]', '2026-02-23T00:00:00Z')",
            [],
        )
        .unwrap();

        let anonymous = resolve_principal(
            &conn,
            &MemoryWriteContext {
                channel: Some("signal".into()),
                chat_type: Some("dm".into()),
                chat_id: Some("chat-a".into()),
                sender_id: None,
                raw_sender: Some("unknown".into()),
            },
        )
        .unwrap();
        let member = resolve_principal(
            &conn,
            &MemoryWriteContext {
                channel: Some("signal".into()),
                chat_type: Some("dm".into()),
                chat_id: Some("chat-m".into()),
                sender_id: None,
                raw_sender: Some("sender-member".into()),
            },
        )
        .unwrap();

        assert_eq!(anonymous.role, Role::Anonymous);
        assert!(anonymous.acl_enforced);
        assert_eq!(member.role, Role::Member);
        assert!(member.acl_enforced);
    }

    #[test]
    fn classify_memory_promotes_sensitive_content_to_owner() {
        let principal = Principal {
            user_id: "u1".into(),
            role: Role::Member,
            projects: Vec::new(),
            visibility_ceiling: Visibility::Private,
            blocked_patterns: Vec::new(),
            current_channel: "signal".into(),
            current_chat_id: "group:1".into(),
            current_chat_type: ChatType::Group,
            raw_sender: "sender-1".into(),
            acl_enforced: true,
        };
        let ctx = MemoryWriteContext {
            channel: Some("signal".into()),
            chat_type: Some("group".into()),
            chat_id: Some("group:1".into()),
            sender_id: None,
            raw_sender: Some("sender-1".into()),
        };

        let classified = classify_memory(&ctx, "my api_key is 123456", &principal);

        assert_eq!(classified.visibility, Visibility::Owner);
        assert_eq!(classified.sensitivity, Sensitivity::Sensitive);
        assert!(!classified.risk_signals.is_empty());
    }

    #[test]
    fn classify_memory_owner_dm_stays_owner_without_risk() {
        let principal = Principal {
            user_id: "ak".into(),
            role: Role::Owner,
            projects: Vec::new(),
            visibility_ceiling: Visibility::Public,
            blocked_patterns: Vec::new(),
            current_channel: "signal".into(),
            current_chat_id: "chat-ak".into(),
            current_chat_type: ChatType::Dm,
            raw_sender: "sender-ak".into(),
            acl_enforced: false,
        };
        let ctx = MemoryWriteContext {
            channel: Some("signal".into()),
            chat_type: Some("dm".into()),
            chat_id: Some("chat-ak".into()),
            sender_id: None,
            raw_sender: Some("sender-ak".into()),
        };

        let classified = classify_memory(&ctx, "normal project update", &principal);
        assert_eq!(classified.visibility, Visibility::Owner);
        assert_eq!(classified.sensitivity, Sensitivity::Normal);
        assert_eq!(classified.risk_signals, Vec::<String>::new());
    }

    #[test]
    fn matches_sensitive_patterns_detects_fullwidth_api_key() {
        assert!(matches_sensitive_patterns("凭证是 ａｐｉ＿ｋｅｙ=abc123"));
    }

    #[test]
    fn visibility_ordinal_is_stable() {
        assert!(Visibility::Owner.ordinal() < Visibility::Private.ordinal());
        assert!(Visibility::Private.ordinal() < Visibility::Public.ordinal());
    }

    fn base_principal(role: Role, ceiling: Visibility) -> Principal {
        let acl_enforced = !matches!(role, Role::Owner);
        Principal {
            user_id: "u1".into(),
            role,
            projects: vec!["proj-a".into(), "proj-b".into()],
            visibility_ceiling: ceiling,
            blocked_patterns: Vec::new(),
            current_channel: "telegram".into(),
            current_chat_id: "chat-1".into(),
            current_chat_type: ChatType::Dm,
            raw_sender: "sender-1".into(),
            acl_enforced,
        }
    }

    #[test]
    fn build_sql_scope_owner_is_unrestricted() {
        let principal = base_principal(Role::Owner, Visibility::Public);
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(scope, "1=1");
        assert!(params.is_empty());
    }

    #[test]
    fn build_sql_scope_anonymous_includes_self_authored_triple() {
        // FIX-P1-06: the Anonymous scope now matches public rows OR rows the
        // sender authored on the (channel, chat_id, raw_sender) triple, and the
        // three values are bound as parameters.
        let principal = base_principal(Role::Anonymous, Visibility::Private);
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(
            scope,
            "(visibility = 'public' OR (channel = ? AND chat_id = ? AND raw_sender = ?)) \
             AND sensitivity != 'secret'"
        );
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn build_sql_scope_private_ceiling_excludes_user_group_project() {
        let principal = base_principal(Role::Guest, Visibility::Private);
        let (scope, params) = principal.build_sql_scope();
        assert!(scope.contains("visibility = 'private'"));
        assert!(!scope.contains("visibility = 'user'"));
        assert!(!scope.contains("visibility = 'group'"));
        assert!(!scope.contains("visibility = 'project'"));
        assert!(scope.contains("sensitivity != 'secret'"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn build_sql_scope_user_ceiling_includes_sender_match() {
        let principal = base_principal(Role::Member, Visibility::User);
        let (scope, params) = principal.build_sql_scope();
        assert!(scope.contains("visibility = 'user' AND sender_id = ?"));
        assert!(!scope.contains("visibility = 'group'"));
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn build_sql_scope_group_ceiling_includes_group_triplet() {
        let principal = base_principal(Role::Member, Visibility::Group);
        let (scope, params) = principal.build_sql_scope();
        assert!(scope.contains("visibility = 'group' AND chat_type = 'group'"));
        assert_eq!(params.len(), 5);
    }

    #[test]
    fn build_sql_scope_project_ceiling_includes_project_participant_guard() {
        let principal = base_principal(Role::Member, Visibility::Project);
        let (scope, params) = principal.build_sql_scope();
        assert!(scope.contains("visibility = 'project'"));
        assert!(scope.contains("topic_participants tp"));
        assert!(scope.contains("t.project IN (?,?)"));
        assert_eq!(params.len(), 8);
    }

    /// Extract the textual payload of a rusqlite `Value` for exact golden
    /// assertions. The scope predicate only ever binds `Value::Text`, so any
    /// other variant is a regression and fails the test loudly.
    fn text_of(value: &Value) -> &str {
        match value {
            Value::Text(s) => s.as_str(),
            other => panic!("test: expected Value::Text in scope params, got {other:?}"),
        }
    }

    fn texts(params: &[Value]) -> Vec<&str> {
        params.iter().map(text_of).collect()
    }

    // ----------------------------------------------------------------------
    // D11 A0: byte-exact golden tests for build_sql_scope. These assert the
    // COMPLETE SQL string and the ORDERED parameter values (not just
    // `contains` + length), forming the safety net that proves the A1
    // refactor keeps build_sql_scope byte-for-byte identical.
    // ----------------------------------------------------------------------

    #[test]
    fn golden_build_sql_scope_owner_exact() {
        let principal = base_principal(Role::Owner, Visibility::Public);
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(scope, "1=1");
        assert!(params.is_empty());
    }

    #[test]
    fn golden_build_sql_scope_acl_disabled_exact() {
        // A non-owner with acl_enforced = false short-circuits to "1=1".
        let mut principal = base_principal(Role::Member, Visibility::Public);
        principal.acl_enforced = false;
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(scope, "1=1");
        assert!(params.is_empty());
    }

    #[test]
    fn golden_build_sql_scope_anonymous_exact() {
        let principal = base_principal(Role::Anonymous, Visibility::Private);
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(
            scope,
            "(visibility = 'public' OR (channel = ? AND chat_id = ? AND raw_sender = ?)) \
             AND sensitivity != 'secret'"
        );
        assert_eq!(texts(&params), vec!["telegram", "chat-1", "sender-1"]);
    }

    #[test]
    fn golden_build_sql_scope_private_ceiling_exact() {
        let principal = base_principal(Role::Guest, Visibility::Private);
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(
            scope,
            "(visibility = 'public' OR \
             (visibility = 'private' AND chat_type = 'dm' AND channel = ? AND chat_id = ?)) \
             AND sensitivity != 'secret'"
        );
        assert_eq!(texts(&params), vec!["telegram", "chat-1"]);
    }

    #[test]
    fn golden_build_sql_scope_user_ceiling_exact() {
        let principal = base_principal(Role::Member, Visibility::User);
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(
            scope,
            "(visibility = 'public' OR \
             (visibility = 'private' AND chat_type = 'dm' AND channel = ? AND chat_id = ?) OR \
             (visibility = 'user' AND sender_id = ?)) \
             AND sensitivity != 'secret'"
        );
        assert_eq!(texts(&params), vec!["telegram", "chat-1", "u1"]);
    }

    #[test]
    fn golden_build_sql_scope_group_ceiling_exact() {
        let principal = base_principal(Role::Member, Visibility::Group);
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(
            scope,
            "(visibility = 'public' OR \
             (visibility = 'private' AND chat_type = 'dm' AND channel = ? AND chat_id = ?) OR \
             (visibility = 'user' AND sender_id = ?) OR \
             (visibility = 'group' AND chat_type = 'group' AND channel = ? AND chat_id = ?)) \
             AND sensitivity != 'secret'"
        );
        assert_eq!(texts(&params), vec!["telegram", "chat-1", "u1", "telegram", "chat-1"]);
    }

    #[test]
    fn golden_build_sql_scope_project_ceiling_exact() {
        // Exercises the dynamically built `?,?` placeholder run in the project
        // sub-query (the most refactor-fragile segment).
        let principal = base_principal(Role::Member, Visibility::Project);
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(
            scope,
            "(visibility = 'public' OR \
             (visibility = 'private' AND chat_type = 'dm' AND channel = ? AND chat_id = ?) OR \
             (visibility = 'user' AND sender_id = ?) OR \
             (visibility = 'group' AND chat_type = 'group' AND channel = ? AND chat_id = ?) OR \
             (visibility = 'project' AND topic_id IN (\
SELECT t.id FROM topics t \
INNER JOIN topic_participants tp ON tp.topic_id = t.id \
WHERE t.project IN (?,?) \
AND tp.user_id = ?\
))) \
             AND sensitivity != 'secret'"
        );
        assert_eq!(
            texts(&params),
            vec![
                "telegram", "chat-1", "u1", "telegram", "chat-1", "proj-a", "proj-b", "u1"
            ]
        );
    }

    #[test]
    fn golden_build_sql_scope_project_ceiling_empty_projects_omits_subquery() {
        // Project ceiling but no project memberships -> the project branch is
        // omitted entirely (no dangling placeholders).
        let mut principal = base_principal(Role::Member, Visibility::Project);
        principal.projects = Vec::new();
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(
            scope,
            "(visibility = 'public' OR \
             (visibility = 'private' AND chat_type = 'dm' AND channel = ? AND chat_id = ?) OR \
             (visibility = 'user' AND sender_id = ?) OR \
             (visibility = 'group' AND chat_type = 'group' AND channel = ? AND chat_id = ?)) \
             AND sensitivity != 'secret'"
        );
        assert_eq!(texts(&params), vec!["telegram", "chat-1", "u1", "telegram", "chat-1"]);
    }

    // ----------------------------------------------------------------------
    // D11 A1: dialect-agnostic predicate invariants. The number of bind
    // markers in the template must always equal the number of params, and the
    // SQLite renderer must reproduce the predicate's markers as `?`.
    // ----------------------------------------------------------------------

    fn marker_count(template: &str) -> usize {
        template.matches(SCOPE_BIND_MARKER).count()
    }

    #[test]
    fn scope_predicate_marker_count_matches_param_count_all_branches() {
        let cases = [
            base_principal(Role::Owner, Visibility::Public),
            base_principal(Role::Anonymous, Visibility::Private),
            base_principal(Role::Guest, Visibility::Private),
            base_principal(Role::Member, Visibility::User),
            base_principal(Role::Member, Visibility::Group),
            base_principal(Role::Member, Visibility::Project),
        ];
        for principal in cases {
            let predicate = principal.build_scope_predicate();
            assert_eq!(
                marker_count(&predicate.template),
                predicate.params.len(),
                "marker/param mismatch for role {:?} ceiling {:?}",
                principal.role,
                principal.visibility_ceiling
            );
        }
    }

    #[test]
    fn scope_predicate_sqlite_renders_markers_as_question_marks() {
        // The SQLite renderer must leave no bind markers behind and must emit
        // exactly one `?` per param.
        let principal = base_principal(Role::Member, Visibility::Project);
        let predicate = principal.build_scope_predicate();
        let (sql, params) = principal.build_sql_scope();
        assert!(
            !sql.contains(SCOPE_BIND_MARKER),
            "template marker leaked into SQLite SQL"
        );
        assert_eq!(sql.matches('?').count(), predicate.params.len());
        assert_eq!(params.len(), predicate.params.len());
    }

    #[test]
    fn scope_predicate_owner_and_acl_disabled_are_unconditional() {
        let owner = base_principal(Role::Owner, Visibility::Public).build_scope_predicate();
        assert_eq!(owner.template, "1=1");
        assert!(owner.params.is_empty());

        let mut member = base_principal(Role::Member, Visibility::Public);
        member.acl_enforced = false;
        let disabled = member.build_scope_predicate();
        assert_eq!(disabled.template, "1=1");
        assert!(disabled.params.is_empty());
    }

    #[test]
    fn post_filter_applies_nfkc_and_regex() {
        let mut principal = base_principal(Role::Guest, Visibility::Public);
        principal.blocked_patterns = vec![Regex::new("api[_-]?key").unwrap()];

        let inputs = vec![
            "normal text".to_string(),
            "ＡＰＩＫＥＹ leaked".to_string(),
            "hello".to_string(),
        ];
        let filtered = post_filter(inputs, &principal, |s| s.as_str());
        assert_eq!(filtered, vec!["normal text".to_string(), "hello".to_string()]);
    }

    #[test]
    fn is_system_principal_recognizes_all_four_canonical_ids() {
        for name in ["self_system", "router", "internal", "system"] {
            assert!(super::is_system_principal(name), "{name} should be a system principal");
        }
        assert_eq!(SYSTEM_PRINCIPAL_IDS.len(), 4);
    }

    #[test]
    fn is_system_principal_rejects_non_system_ids() {
        for name in ["alice", "agent", "", "System", "ROUTER", "self", "user"] {
            assert!(
                !super::is_system_principal(name),
                "{name} must not be a system principal"
            );
        }
    }

    #[test]
    fn principal_from_policy_applies_role_and_blocked_patterns() {
        let principal = principal_from_policy(
            "ak".to_string(),
            Some((
                "member".to_string(),
                "[\"alpha\"]".to_string(),
                "user".to_string(),
                "[\"token\"]".to_string(),
            )),
            "signal".to_string(),
            "chat-1".to_string(),
            ChatType::Dm,
            "sender-1".to_string(),
        );
        assert_eq!(principal.role, Role::Member);
        assert_eq!(principal.projects, vec!["alpha".to_string()]);
        assert_eq!(principal.visibility_ceiling, Visibility::User);
        assert_eq!(principal.blocked_patterns.len(), 1);
        assert!(principal.acl_enforced);
    }

    #[test]
    fn principal_from_policy_defaults_to_guest_without_policy() {
        let principal = principal_from_policy(
            "u1".to_string(),
            None,
            "signal".to_string(),
            "chat-1".to_string(),
            ChatType::Dm,
            "sender-1".to_string(),
        );
        assert_eq!(principal.role, Role::Guest);
        assert!(principal.blocked_patterns.is_empty());
        assert!(principal.acl_enforced);
    }

    #[test]
    fn post_filter_skips_owner() {
        let mut principal = base_principal(Role::Owner, Visibility::Public);
        principal.blocked_patterns = vec![Regex::new("secret").unwrap()];
        let inputs = vec!["secret".to_string()];
        let filtered = post_filter(inputs.clone(), &principal, |s| s.as_str());
        assert_eq!(filtered, inputs);
    }

    #[test]
    fn is_system_principal_recognizes_all_four_canonical_names() {
        // FIX-P0-24 (#17): the canonical helper shared by the SQLite and Postgres
        // backends must recognize exactly the same four system-principal ids so
        // their owner-ACL bypass behaves identically.
        for name in ["self_system", "router", "internal", "system"] {
            assert!(is_system_principal(name), "{name} must be a system principal");
        }
        assert_eq!(SYSTEM_PRINCIPAL_IDS.len(), 4);
        for name in ["owner:alice", "anonymous:signal:bob", "user", "", "System", "ROUTER"] {
            assert!(!is_system_principal(name), "{name} must not be a system principal");
        }
    }
}
