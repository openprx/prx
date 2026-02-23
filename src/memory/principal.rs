use anyhow::Result;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::LazyLock;

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
        let projects = parse_json_array(&projects_raw);
        let blocked_patterns = compile_patterns(parse_json_array(&blocked_raw));

        return Ok(Principal {
            user_id,
            role: Role::from_db(&role_raw),
            projects,
            visibility_ceiling: Visibility::from_db(&ceiling_raw),
            blocked_patterns,
            current_channel,
            current_chat_id,
            current_chat_type,
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
}
