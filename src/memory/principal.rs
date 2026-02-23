use anyhow::Result;
use regex::Regex;
use rusqlite::{params, types::Value, Connection, OptionalExtension};
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

pub const CURRENT_POLICY_VERSION: i64 = 1;

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
    pub acl_enforced: bool,
}

impl Principal {
    pub fn build_sql_scope(&self) -> (String, Vec<Value>) {
        if !self.acl_enforced {
            return ("1=1".to_string(), Vec::new());
        }

        match self.role {
            Role::Owner => ("1=1".to_string(), Vec::new()),
            Role::Anonymous => (
                "visibility = 'public' AND sensitivity = 'normal'".to_string(),
                Vec::new(),
            ),
            Role::Member | Role::Guest => {
                let ceiling_ord = self.visibility_ceiling.ordinal();
                let mut conditions = vec!["visibility = 'public'".to_string()];
                let mut params = Vec::new();

                if ceiling_ord >= Visibility::Private.ordinal() {
                    conditions.push(
                        "(visibility = 'private' AND chat_type = 'dm' AND channel = ? AND chat_id = ?)"
                            .to_string(),
                    );
                    params.push(Value::from(self.current_channel.clone()));
                    params.push(Value::from(self.current_chat_id.clone()));
                }

                if ceiling_ord >= Visibility::User.ordinal() {
                    conditions.push("(visibility = 'user' AND sender_id = ?)".to_string());
                    params.push(Value::from(self.user_id.clone()));
                }

                if ceiling_ord >= Visibility::Group.ordinal() {
                    conditions.push(
                        "(visibility = 'group' AND chat_type = 'group' AND channel = ? AND chat_id = ?)"
                            .to_string(),
                    );
                    params.push(Value::from(self.current_channel.clone()));
                    params.push(Value::from(self.current_chat_id.clone()));
                }

                if ceiling_ord >= Visibility::Project.ordinal() && !self.projects.is_empty() {
                    let placeholders = (0..self.projects.len())
                        .map(|_| "?")
                        .collect::<Vec<_>>()
                        .join(",");
                    conditions.push(format!(
                        "(visibility = 'project' AND topic_id IN (\
                            SELECT t.id FROM topics t \
                            INNER JOIN topic_participants tp ON tp.topic_id = t.id \
                            WHERE t.project IN ({placeholders}) \
                            AND tp.user_id = ?\
                        ))"
                    ));
                    params.extend(self.projects.iter().cloned().map(Value::from));
                    params.push(Value::from(self.user_id.clone()));
                }

                (
                    format!("({}) AND sensitivity != 'secret'", conditions.join(" OR ")),
                    params,
                )
            }
        }
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
    let current_chat_type = ctx
        .chat_type
        .as_deref()
        .map(ChatType::from_str)
        .unwrap_or(ChatType::Dm);

    let Some(channel) = ctx.channel.as_deref() else {
        return Ok(anonymous_principal(
            "anonymous:unknown:unknown".to_string(),
            current_channel,
            current_chat_id,
            current_chat_type,
        ));
    };

    let Some(raw_sender) = ctx.raw_sender.as_deref() else {
        return Ok(anonymous_principal(
            format!("anonymous:{channel}:unknown"),
            current_channel,
            current_chat_id,
            current_chat_type,
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

    if let Some((role_raw, projects_raw, ceiling_raw, blocked_raw)) = policy {
        let role = Role::from_db(&role_raw);
        let projects = parse_json_array(&projects_raw);
        let blocked_patterns = compile_patterns(parse_json_array(&blocked_raw));

        return Ok(Principal {
            user_id,
            role: role.clone(),
            projects,
            visibility_ceiling: Visibility::from_db(&ceiling_raw),
            blocked_patterns,
            current_channel,
            current_chat_id,
            current_chat_type,
            acl_enforced: is_acl_enforced_for_role(&role),
        });
    }

    Ok(Principal {
        user_id,
        role: Role::Guest,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Private,
        blocked_patterns: Vec::new(),
        current_channel,
        current_chat_id,
        current_chat_type,
        acl_enforced: is_acl_enforced_for_role(&Role::Guest),
    })
}

pub fn classify_memory(
    ctx: &MemoryWriteContext,
    content: &str,
    principal: &Principal,
) -> MemoryClassification {
    let mut risk_signals = Vec::new();
    if matches_sensitive_patterns(content) {
        risk_signals.push("sensitive_keyword_match".to_string());
    }
    if contains_pii(content) {
        risk_signals.push("pii_detected".to_string());
    }

    let chat_type = ctx
        .chat_type
        .as_deref()
        .map(ChatType::from_str)
        .unwrap_or(ChatType::Dm);

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
    let normalized = content.to_ascii_lowercase();
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

fn anonymous_principal(
    user_id: String,
    current_channel: String,
    current_chat_id: String,
    current_chat_type: ChatType,
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
        acl_enforced: is_acl_enforced_for_role(&Role::Anonymous),
    }
}

const ACL_ENFORCE_ANONYMOUS: bool = true;
const ACL_ENFORCE_GUEST: bool = true;
const ACL_ENFORCE_MEMBER: bool = true;

fn is_acl_enforced_for_role(role: &Role) -> bool {
    match role {
        Role::Owner => false,
        Role::Anonymous => ACL_ENFORCE_ANONYMOUS,
        Role::Guest => ACL_ENFORCE_GUEST,
        Role::Member => ACL_ENFORCE_MEMBER,
    }
}

static SENSITIVE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"\bssh\b").unwrap(),
        Regex::new(r"\bapi[_-]?key\b").unwrap(),
        Regex::new(r"密钥|私钥|秘钥").unwrap(),
        Regex::new(r"\bpassw(or)?d\b").unwrap(),
        Regex::new(r"\btok(en)?\b").unwrap(),
        Regex::new(r"\bsecret\b").unwrap(),
        Regex::new(r"im-ops|服务器地址").unwrap(),
        Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").unwrap(),
        Regex::new(r"\bprivate[_\s]?key\b").unwrap(),
    ]
});

static EMAIL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b").unwrap());
static IPV4_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").unwrap());

#[cfg(test)]
mod tests {
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
    fn build_sql_scope_anonymous_is_public_normal_only() {
        let principal = base_principal(Role::Anonymous, Visibility::Private);
        let (scope, params) = principal.build_sql_scope();
        assert_eq!(scope, "visibility = 'public' AND sensitivity = 'normal'");
        assert!(params.is_empty());
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
        assert_eq!(
            filtered,
            vec!["normal text".to_string(), "hello".to_string()]
        );
    }

    #[test]
    fn post_filter_skips_owner() {
        let mut principal = base_principal(Role::Owner, Visibility::Public);
        principal.blocked_patterns = vec![Regex::new("secret").unwrap()];
        let inputs = vec!["secret".to_string()];
        let filtered = post_filter(inputs.clone(), &principal, |s| s.as_str());
        assert_eq!(filtered, inputs);
    }
}
