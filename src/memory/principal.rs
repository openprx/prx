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
        Self::parse(value).unwrap_or(Self::Guest)
    }

    /// The canonical reading of a configured role (trimmed, any case), or
    /// `None` for an unknown one. Shared by the runtime, the load-time check
    /// of `[[user_policies]]` and the configuration plan (D-050).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "owner" => Some(Self::Owner),
            "member" => Some(Self::Member),
            "guest" => Some(Self::Guest),
            "anonymous" => Some(Self::Anonymous),
            _ => None,
        }
    }

    /// How much the role may do: anonymous < guest < member < owner.
    #[must_use]
    pub const fn privilege_rank(&self) -> u8 {
        match self {
            Self::Anonymous => 0,
            Self::Guest => 1,
            Self::Member => 2,
            Self::Owner => 3,
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
        Self::parse(value).unwrap_or(Self::Private)
    }

    /// The canonical reading of a configured visibility (trimmed, any case),
    /// or `None` for an unknown one. Shared by the runtime, the load-time
    /// check of `[[user_policies]]` and the configuration plan (D-050).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "system" => Some(Self::System),
            "owner" => Some(Self::Owner),
            "private" => Some(Self::Private),
            "user" => Some(Self::User),
            "group" => Some(Self::Group),
            "project" => Some(Self::Project),
            "public" => Some(Self::Public),
            _ => None,
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
    /// Authenticated, issuer-qualified owner anchor for this read (C03 /
    /// V02-R5).
    ///
    /// Distinct from [`Self::user_id`] (an ACL identity resolved from
    /// `identity_bindings`) and from [`Self::raw_sender`] (a channel-account
    /// anchor). It is only consulted when [`Self::require_owner_match`] is set.
    pub owner_id: Option<String>,
    /// Require `memories.owner_id` equality on every non-public scope arm
    /// (C03 / V02-R5).
    ///
    /// `false` (the historical contract) emits no owner fragment at all, so
    /// every statement stays byte-identical for channel, chat, cron, xin and
    /// sub-agent principals.
    ///
    /// `true` is set ONLY from the trusted runtime scope authored by the
    /// execution service for an owner-isolated origin — today an externally
    /// authenticated MCP caller, whose chat id (`mcp:{subject}`) and raw sender
    /// are the external *subject* only, so two identities presenting the same
    /// subject under different issuers would otherwise resolve to the same
    /// principal. It is never read from model-supplied tool arguments, and it
    /// is never a function of `memory.acl_enabled`.
    pub require_owner_match: bool,
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
    /// Carry the trusted context's owner anchor and isolation bit onto a
    /// principal that was just resolved from policy or built as a fallback.
    ///
    /// Single seam: every backend that resolves a read principal from a
    /// [`MemoryWriteContext`] goes through this, so no construction site can
    /// silently drop the isolation bit or invent its own source for it.
    #[must_use]
    pub fn with_owner_scope(mut self, context: &MemoryWriteContext) -> Self {
        self.owner_id = context.owner_id.clone();
        self.require_owner_match = context.require_owner_match;
        self
    }

    /// The owner id this principal is isolated to, with blank values treated as
    /// absent (same rule as [`crate::memory::owner_predicate::has_owner_id`]).
    fn isolated_owner_id(&self) -> Option<&str> {
        self.owner_id
            .as_deref()
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
    }

    /// The shared owner-isolation suffix, rendered into the dialect-agnostic
    /// marker vocabulary this predicate uses.
    ///
    /// The fragment text and the fail-closed rule come from
    /// [`crate::memory::owner_predicate::owner_match_suffix`] — the single seam
    /// C03 installed — so no second owner-isolation vocabulary can appear here.
    /// Only the placeholder spelling is translated: `owner_match_suffix` emits
    /// a concrete `?N` / `$N` token because its callers write SQL directly,
    /// while this predicate is rendered per backend later and marks its bind
    /// points with [`SCOPE_BIND_MARKER`]. The suffix contains exactly one
    /// placeholder when it binds, so the one-marker-one-param accounting holds.
    fn owner_match_fragment(&self) -> crate::memory::owner_predicate::OwnerMatchPredicate {
        const RENDER_INDEX: usize = 1;
        const RENDER_DIALECT: crate::memory::session_predicate::PlaceholderDialect =
            crate::memory::session_predicate::PlaceholderDialect::Sqlite;
        let mut fragment = crate::memory::owner_predicate::owner_match_suffix(
            RENDER_DIALECT,
            RENDER_INDEX,
            self.require_owner_match,
            self.isolated_owner_id().is_some(),
        );
        fragment.sql = fragment
            .sql
            .replace(&RENDER_DIALECT.token(RENDER_INDEX), SCOPE_BIND_MARKER);
        fragment
    }

    /// Record the owner bind this fragment consumes, if any.
    ///
    /// Called immediately before the fragment's placeholder is emitted, so the
    /// parameter list stays in template order (one marker, one param).
    fn push_owner_param(
        &self,
        fragment: &crate::memory::owner_predicate::OwnerMatchPredicate,
        params: &mut Vec<ScopeParam>,
    ) {
        if fragment.bound_owner {
            params.push(ScopeParam::Text(
                self.isolated_owner_id().unwrap_or_default().to_string(),
            ));
        }
    }

    /// Wrap one non-public visibility condition in its parentheses, owner-
    /// qualified when the caller is owner-isolated.
    ///
    /// `body` is the condition without its enclosing parentheses; the owner
    /// fragment always lands inside them, so it binds that arm alone and never
    /// the `visibility = 'public'` arm beside it.
    fn owner_scoped_group(
        &self,
        fragment: &crate::memory::owner_predicate::OwnerMatchPredicate,
        body: &str,
        params: &mut Vec<ScopeParam>,
    ) -> String {
        self.push_owner_param(fragment, params);
        format!("({body}{})", fragment.sql)
    }

    /// The unconditional (`1=1`) arm, owner-qualified when the caller is
    /// owner-isolated.
    ///
    /// An owner-isolated caller must never reach a truly unconditional
    /// predicate, even when it resolves to an `owner`-role policy row or to a
    /// principal with ACL enforcement off. Without isolation this renders the
    /// historical `"1=1"` byte-for-byte.
    fn unconditional_predicate(&self) -> ScopePredicate {
        let fragment = self.owner_match_fragment();
        let mut params = Vec::new();
        self.push_owner_param(&fragment, &mut params);
        ScopePredicate {
            template: format!("1=1{}", fragment.sql),
            params,
        }
    }

    /// Single-source scope truth table (D11).
    ///
    /// Produces a dialect-agnostic [`ScopePredicate`] from `role` /
    /// `visibility_ceiling`. Both the SQLite and Postgres backends render from
    /// this one function, so the visibility truth table can no longer drift
    /// between backends. Bind points use [`SCOPE_BIND_MARKER`]; the literal SQL
    /// is portable (plain scalar comparisons plus one standard sub-query, no
    /// dialect-specific functions).
    ///
    /// C03 / V02-R5: when [`Principal::require_owner_match`] is set, every
    /// **non-public** arm additionally requires `owner_id` equality. The
    /// `visibility = 'public'` arm and the `sensitivity != 'secret'` guard are
    /// untouched, matching C03's treatment of `workspace` / `global` rows in
    /// the event read model. When the flag is clear no fragment is emitted at
    /// all, so every rendering is byte-identical to the pre-V02-R5 one (locked
    /// by the `golden_build_sql_scope_*` suite).
    pub fn build_scope_predicate(&self) -> ScopePredicate {
        let m = SCOPE_BIND_MARKER;

        if !self.acl_enforced {
            return self.unconditional_predicate();
        }

        match self.role {
            Role::Owner => self.unconditional_predicate(),
            // FIX-P1-06: unify the Anonymous scope with the previously hardcoded
            // SQLite recall/forget branches. An anonymous principal sees public
            // rows plus rows it authored, matched on the
            // `(channel, chat_id, raw_sender)` triple, excluding secrets.
            Role::Anonymous => {
                let fragment = self.owner_match_fragment();
                // The management API principal carries `owner_id` and
                // `owner_isolated` and no channel, chat, or sender. Its rows
                // are stored with those columns NULL, so the channel triple
                // would hide them from the writer. Match that caller on
                // `owner_id` alone. A caller that has any of the three anchors
                // keeps the triple, including the owner suffix when isolated.
                let has_channel_anchor =
                    !self.current_channel.is_empty() || !self.current_chat_id.is_empty() || !self.raw_sender.is_empty();
                if self.require_owner_match && self.isolated_owner_id().is_some() && !has_channel_anchor {
                    let mut params = Vec::new();
                    let owned = self.owner_scoped_group(&fragment, "1=1", &mut params);
                    return ScopePredicate {
                        template: format!("(visibility = 'public' OR {owned}) AND sensitivity != 'secret'"),
                        params,
                    };
                }
                let mut params = vec![
                    ScopeParam::Text(self.current_channel.clone()),
                    ScopeParam::Text(self.current_chat_id.clone()),
                    ScopeParam::Text(self.raw_sender.clone()),
                ];
                let authored = self.owner_scoped_group(
                    &fragment,
                    &format!("channel = {m} AND chat_id = {m} AND raw_sender = {m}"),
                    &mut params,
                );
                ScopePredicate {
                    template: format!(
                        "(visibility = 'public' OR {authored}) \
                     AND sensitivity != 'secret'"
                    ),
                    params,
                }
            }
            Role::Member | Role::Guest => {
                let fragment = self.owner_match_fragment();
                let ceiling_ord = self.visibility_ceiling.ordinal();
                let mut conditions = vec!["visibility = 'public'".to_string()];
                let mut params = Vec::new();

                if ceiling_ord >= Visibility::Private.ordinal() {
                    params.push(ScopeParam::Text(self.current_channel.clone()));
                    params.push(ScopeParam::Text(self.current_chat_id.clone()));
                    conditions.push(self.owner_scoped_group(
                        &fragment,
                        &format!("visibility = 'private' AND chat_type = 'dm' AND channel = {m} AND chat_id = {m}"),
                        &mut params,
                    ));
                }

                if ceiling_ord >= Visibility::User.ordinal() {
                    params.push(ScopeParam::Text(self.user_id.clone()));
                    conditions.push(self.owner_scoped_group(
                        &fragment,
                        &format!("visibility = 'user' AND sender_id = {m}"),
                        &mut params,
                    ));
                }

                if ceiling_ord >= Visibility::Group.ordinal() {
                    params.push(ScopeParam::Text(self.current_channel.clone()));
                    params.push(ScopeParam::Text(self.current_chat_id.clone()));
                    conditions.push(self.owner_scoped_group(
                        &fragment,
                        &format!("visibility = 'group' AND chat_type = 'group' AND channel = {m} AND chat_id = {m}"),
                        &mut params,
                    ));
                }

                if ceiling_ord >= Visibility::Project.ordinal() && !self.projects.is_empty() {
                    let placeholders = (0..self.projects.len()).map(|_| m).collect::<Vec<_>>().join(",");
                    params.extend(self.projects.iter().cloned().map(ScopeParam::Text));
                    params.push(ScopeParam::Text(self.user_id.clone()));
                    conditions.push(self.owner_scoped_group(
                        &fragment,
                        &format!(
                            "visibility = 'project' AND topic_id IN (\
                            SELECT t.id FROM topics t \
                            INNER JOIN topic_participants tp ON tp.topic_id = t.id \
                            WHERE t.project IN ({placeholders}) \
                            AND tp.user_id = {m}\
                        )"
                        ),
                        &mut params,
                    ));
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

    /// Postgres renderer (`$N` placeholders).
    ///
    /// Renders the shared [`build_scope_predicate`](Self::build_scope_predicate)
    /// into Postgres SQL whose bind points start at `$start_index` and increment
    /// by one per bind marker, in template order. The caller appends the returned
    /// [`ScopeParam`] values to its existing `$1..$(start_index - 1)` parameters,
    /// preserving order. Each marker maps to exactly one positional parameter
    /// (no `$N` reuse), matching the one-param-per-marker invariant of the
    /// dialect-agnostic predicate.
    ///
    /// `start_index` must be >= 1 (the index of the first scope parameter in the
    /// caller's full parameter slice). The returned SQL is fully parameterized;
    /// no value is interpolated into the SQL text (iron rule 9).
    pub fn build_sql_scope_pg(&self, start_index: usize) -> (String, Vec<ScopeParam>) {
        let predicate = self.build_scope_predicate();

        // Replace each marker with `$N` left-to-right, N starting at
        // start_index. The marker count equals predicate.params.len() (asserted
        // in tests), so the final index lines up with the appended params.
        let mut sql = String::with_capacity(predicate.template.len());
        let mut next_index = start_index;
        let mut rest = predicate.template.as_str();
        while let Some(pos) = rest.find(SCOPE_BIND_MARKER) {
            sql.push_str(&rest[..pos]);
            sql.push('$');
            sql.push_str(&next_index.to_string());
            next_index += 1;
            rest = &rest[pos + SCOPE_BIND_MARKER.len()..];
        }
        sql.push_str(rest);

        (sql, predicate.params)
    }
}

#[derive(Debug, Clone, Default)]
pub struct MemoryWriteContext {
    /// Canonical workspace supplied by the runtime envelope/trusted tool scope.
    /// PostgreSQL uses it as transaction-local RLS context; it must never be
    /// inferred from model-authored arguments.
    pub workspace_id: Option<String>,
    pub channel: Option<String>,
    pub chat_type: Option<String>,
    pub chat_id: Option<String>,
    pub sender_id: Option<String>,
    pub raw_sender: Option<String>,
    /// Authenticated, issuer-qualified owner id from the trusted runtime scope
    /// (C03 / V02-R5).
    ///
    /// Distinct from [`Self::sender_id`], which is a channel-account anchor
    /// that several callers also use as a write-time owner default.
    pub owner_id: Option<String>,
    /// Require `owner_id` equality on every non-public scope arm
    /// (C03 / V02-R5).
    ///
    /// Set ONLY from the service-authored `_zc_scope.owner_isolated` marker, by
    /// way of [`crate::tools::execution::scope_requires_owner_match`]. Never
    /// from model-supplied arguments, and never from a config flag.
    pub require_owner_match: bool,
    /// Agent partition of the writing turn (MA-01b3), carried from the runtime
    /// envelope or the trusted `_zc_scope`. Never from model-supplied arguments.
    pub agent_partition: Option<crate::memory::AgentPartition>,
    /// Durable session key of the writing turn (MA-01b3, H3), carried from the
    /// runtime envelope or the trusted `_zc_scope`.
    pub session_key: Option<String>,
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
        )
        .with_owner_scope(ctx));
    };

    let Some(raw_sender) = ctx.raw_sender.as_deref() else {
        return Ok(anonymous_principal(
            format!("anonymous:{channel}:unknown"),
            current_channel,
            current_chat_id,
            current_chat_type,
            raw_sender_anchor,
        )
        .with_owner_scope(ctx));
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
        )
        .with_owner_scope(ctx));
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
    )
    .with_owner_scope(ctx))
}

/// Build a [`Principal`] from a resolved user id and optional `user_policies`
/// row, sharing the exact role/visibility/blocked-pattern logic used by
/// [`resolve_principal`]. Backends that resolve identity through their own SQL
/// (for example Postgres) call this so policy interpretation stays identical.
///
/// `policy` is `(role, projects_json, visibility_ceiling, blocked_patterns_json)`.
///
/// The returned principal carries no owner anchor: owner isolation is a
/// property of the *trusted context*, not of the policy row, so callers apply
/// [`Principal::with_owner_scope`] to the result.
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
            owner_id: None,
            require_owner_match: false,
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
        owner_id: None,
        require_owner_match: false,
    }
}

pub fn classify_memory(ctx: &MemoryWriteContext, content: &str, principal: &Principal) -> MemoryClassification {
    let risk_signals =
        classification_risk_signals(content, sensitive_patterns().map(Vec::as_slice), email_re(), ipv4_re());

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

fn classification_risk_signals(
    content: &str,
    patterns: std::result::Result<&[Regex], &regex::Error>,
    email: std::result::Result<&Regex, &regex::Error>,
    ip: std::result::Result<&Regex, &regex::Error>,
) -> Vec<String> {
    if patterns.is_err() || email.is_err() || ip.is_err() {
        return vec!["pattern_unavailable".to_string()];
    }
    let mut signals = Vec::new();
    if sensitive_patterns_or_closed(content, patterns) {
        signals.push("sensitive_keyword_match".to_string());
    }
    if contains_pii(content, email, ip) {
        signals.push("pii_detected".to_string());
    }
    signals
}

pub fn matches_sensitive_patterns(content: &str) -> bool {
    sensitive_patterns_or_closed(content, sensitive_patterns().map(Vec::as_slice))
}

fn sensitive_patterns_or_closed(content: &str, patterns: std::result::Result<&[Regex], &regex::Error>) -> bool {
    sensitive_match(content, patterns).unwrap_or(true)
}

fn sensitive_match<'a>(
    content: &str,
    patterns: std::result::Result<&[Regex], &'a regex::Error>,
) -> std::result::Result<bool, &'a regex::Error> {
    let patterns = patterns?;
    let normalized = content.nfkc().collect::<String>().to_lowercase();
    let no_space = normalized.replace(' ', "");
    Ok(patterns
        .iter()
        .any(|re| re.is_match(&normalized) || re.is_match(&no_space)))
}

fn contains_pii(
    content: &str,
    email: std::result::Result<&Regex, &regex::Error>,
    ip: std::result::Result<&Regex, &regex::Error>,
) -> bool {
    pii_match(content, email, ip).unwrap_or(true)
}

fn pii_match<'a>(
    content: &str,
    email: std::result::Result<&Regex, &'a regex::Error>,
    ip: std::result::Result<&Regex, &'a regex::Error>,
) -> std::result::Result<bool, &'a regex::Error> {
    let email = email?;
    let ip = ip?;
    Ok(email.is_match(content) || ip.is_match(content))
}

fn principal_pattern<'a, T>(
    pattern: &'a std::result::Result<T, regex::Error>,
    logged: &std::sync::atomic::AtomicBool,
) -> std::result::Result<&'a T, &'a regex::Error> {
    if pattern.is_err() && !logged.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::error!("principal pattern unavailable");
    }
    pattern.as_ref()
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
        owner_id: None,
        require_owner_match: false,
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

static SENSITIVE_PATTERNS: LazyLock<std::result::Result<Vec<Regex>, regex::Error>> = LazyLock::new(|| {
    Ok(vec![
        Regex::new(r"\bssh\b")?,
        Regex::new(r"\bapi[_-]?key\b")?,
        Regex::new(r"\bcredential(s)?\b")?,
        Regex::new(r"\bpassw(or)?d\b")?,
        Regex::new(r"\btok(en)?\b")?,
        Regex::new(r"\bsecret\b")?,
        Regex::new(r"im-ops|\bserver\s+address\b|\bhost(name)?\s+address\b")?,
        Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b")?,
        Regex::new(r"\bprivate[_\s]?key\b")?,
    ])
});

static EMAIL_RE: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b"));
static IPV4_RE: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b"));

fn sensitive_patterns() -> std::result::Result<&'static Vec<Regex>, &'static regex::Error> {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    principal_pattern(&SENSITIVE_PATTERNS, &LOGGED)
}
fn email_re() -> std::result::Result<&'static Regex, &'static regex::Error> {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    principal_pattern(&EMAIL_RE, &LOGGED)
}
fn ipv4_re() -> std::result::Result<&'static Regex, &'static regex::Error> {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    principal_pattern(&IPV4_RE, &LOGGED)
}

#[cfg(test)]
mod tests;
