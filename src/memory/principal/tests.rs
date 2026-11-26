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
            agent_partition: None,
            session_key: None,
            workspace_id: Some("workspace-a".into()),
            channel: Some("signal".into()),
            chat_type: Some("dm".into()),
            chat_id: Some("chat-1".into()),
            sender_id: None,
            raw_sender: Some("sender-a".into()),
            owner_id: None,
            require_owner_match: false,
        },
    )
    .unwrap();

    assert_eq!(principal.role, Role::Anonymous);
    assert_eq!(principal.user_id, "anonymous:signal:sender-a");
}

#[test]
fn owner_principal_from_write_context_uses_sender_id_as_stable_anchor() {
    let ctx = MemoryWriteContext {
        agent_partition: None,
        session_key: None,
        workspace_id: Some("workspace-a".into()),
        channel: Some("signal".into()),
        chat_type: Some("dm".into()),
        chat_id: Some("chat-1".into()),
        sender_id: Some("+15551234567".into()),
        raw_sender: Some("display-name".into()),
        owner_id: None,
        require_owner_match: false,
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
            agent_partition: None,
            session_key: None,
            workspace_id: Some("workspace-a".into()),
            channel: Some("signal".into()),
            chat_type: Some("dm".into()),
            chat_id: Some("chat-ak".into()),
            sender_id: None,
            raw_sender: Some("sender-ak".into()),
            owner_id: None,
            require_owner_match: false,
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
            agent_partition: None,
            session_key: None,
            workspace_id: Some("workspace-a".into()),
            channel: Some("signal".into()),
            chat_type: Some("dm".into()),
            chat_id: Some("chat-a".into()),
            sender_id: None,
            raw_sender: Some("unknown".into()),
            owner_id: None,
            require_owner_match: false,
        },
    )
    .unwrap();
    let member = resolve_principal(
        &conn,
        &MemoryWriteContext {
            agent_partition: None,
            session_key: None,
            workspace_id: Some("workspace-a".into()),
            channel: Some("signal".into()),
            chat_type: Some("dm".into()),
            chat_id: Some("chat-m".into()),
            sender_id: None,
            raw_sender: Some("sender-member".into()),
            owner_id: None,
            require_owner_match: false,
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
        owner_id: None,
        require_owner_match: false,
    };
    let ctx = MemoryWriteContext {
        agent_partition: None,
        session_key: None,
        workspace_id: Some("workspace-a".into()),
        channel: Some("signal".into()),
        chat_type: Some("group".into()),
        chat_id: Some("group:1".into()),
        sender_id: None,
        raw_sender: Some("sender-1".into()),
        owner_id: None,
        require_owner_match: false,
    };

    let classified = classify_memory(&ctx, "my api_key is 123456", &principal);

    assert_eq!(classified.visibility, Visibility::Owner);
    assert_eq!(classified.sensitivity, Sensitivity::Sensitive);
    assert!(!classified.risk_signals.is_empty(), "expected a non-empty value");
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
        owner_id: None,
        require_owner_match: false,
    };
    let ctx = MemoryWriteContext {
        agent_partition: None,
        session_key: None,
        workspace_id: Some("workspace-a".into()),
        channel: Some("signal".into()),
        chat_type: Some("dm".into()),
        chat_id: Some("chat-ak".into()),
        sender_id: None,
        raw_sender: Some("sender-ak".into()),
        owner_id: None,
        require_owner_match: false,
    };

    let classified = classify_memory(&ctx, "normal project update", &principal);
    assert_eq!(classified.visibility, Visibility::Owner);
    assert_eq!(classified.sensitivity, Sensitivity::Normal);
    assert_eq!(classified.risk_signals, Vec::<String>::new());
}

/// Detection runs after NFKC, so compatibility forms of the very same
/// keyword must not slip past the table.
#[test]
fn matches_sensitive_patterns_detects_compatibility_forms_of_api_key() {
    assert!(matches_sensitive_patterns(
        "the value is \u{24d0}\u{24df}\u{24d8}_\u{24da}\u{24d4}\u{24e8}=abc123"
    ));
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
        owner_id: None,
        require_owner_match: false,
    }
}

/// The same fixture, owner-isolated (C03 / V02-R5).
fn isolated_principal(role: Role, ceiling: Visibility, owner_id: Option<&str>) -> Principal {
    Principal {
        owner_id: owner_id.map(str::to_string),
        require_owner_match: true,
        ..base_principal(role, ceiling)
    }
}

#[test]
fn build_sql_scope_owner_is_unrestricted() {
    let principal = base_principal(Role::Owner, Visibility::Public);
    let (scope, params) = principal.build_sql_scope();
    assert_eq!(scope, "1=1");
    assert!(params.is_empty(), "expected an empty value");
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
    assert!(params.is_empty(), "expected an empty value");
}

#[test]
fn golden_build_sql_scope_acl_disabled_exact() {
    // A non-owner with acl_enforced = false short-circuits to "1=1".
    let mut principal = base_principal(Role::Member, Visibility::Public);
    principal.acl_enforced = false;
    let (scope, params) = principal.build_sql_scope();
    assert_eq!(scope, "1=1");
    assert!(params.is_empty(), "expected an empty value");
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
// C03 / V02-R5: owner-isolated renderings. The golden block above is the
// proof that a non-isolated caller is untouched — every one of those
// principals has `require_owner_match = false`. These pin what the
// isolated rendering adds, equally byte-exactly.
// ----------------------------------------------------------------------

const TEST_OWNER: &str = "owner:ws-isolation:agent:issuer-a.example:shared-subject";

#[test]
fn owner_isolated_scope_appends_the_owner_match_to_every_non_public_arm() {
    let anonymous = isolated_principal(Role::Anonymous, Visibility::Private, Some(TEST_OWNER));
    let (scope, params) = anonymous.build_sql_scope();
    assert_eq!(
        scope,
        "(visibility = 'public' OR \
         (channel = ? AND chat_id = ? AND raw_sender = ? AND owner_id = ?)) \
         AND sensitivity != 'secret'"
    );
    assert_eq!(texts(&params), vec!["telegram", "chat-1", "sender-1", TEST_OWNER]);

    // No channel triple: the management API scope. The non-public arm is
    // owner equality, and a channel-bearing caller above stays on the triple.
    let mut api_principal = isolated_principal(Role::Anonymous, Visibility::Private, Some(TEST_OWNER));
    api_principal.current_channel.clear();
    api_principal.current_chat_id.clear();
    api_principal.raw_sender.clear();
    let (scope, params) = api_principal.build_sql_scope();
    assert_eq!(
        scope,
        "(visibility = 'public' OR (1=1 AND owner_id = ?)) AND sensitivity != 'secret'"
    );
    assert_eq!(texts(&params), vec![TEST_OWNER]);

    let member = isolated_principal(Role::Member, Visibility::Project, Some(TEST_OWNER));
    let (scope, params) = member.build_sql_scope();
    assert_eq!(
        scope,
        "(visibility = 'public' OR \
         (visibility = 'private' AND chat_type = 'dm' AND channel = ? AND chat_id = ? AND owner_id = ?) OR \
         (visibility = 'user' AND sender_id = ? AND owner_id = ?) OR \
         (visibility = 'group' AND chat_type = 'group' AND channel = ? AND chat_id = ? AND owner_id = ?) OR \
         (visibility = 'project' AND topic_id IN (\
SELECT t.id FROM topics t \
INNER JOIN topic_participants tp ON tp.topic_id = t.id \
WHERE t.project IN (?,?) \
AND tp.user_id = ?\
) AND owner_id = ?)) \
         AND sensitivity != 'secret'"
    );
    assert_eq!(
        texts(&params),
        vec![
            "telegram", "chat-1", TEST_OWNER, "u1", TEST_OWNER, "telegram", "chat-1", TEST_OWNER, "proj-a",
            "proj-b", "u1", TEST_OWNER,
        ]
    );
}

#[test]
fn owner_isolated_scope_without_an_owner_id_is_false_on_every_non_public_arm() {
    // Fail closed: no owner id means no owned row at all, and — critically —
    // nothing is bound, so the arm can never degrade to matching on the
    // channel/sender triple that two issuers share.
    for owner in [None, Some("   ")] {
        let anonymous = isolated_principal(Role::Anonymous, Visibility::Private, owner);
        let (scope, params) = anonymous.build_sql_scope();
        assert_eq!(
            scope,
            "(visibility = 'public' OR \
             (channel = ? AND chat_id = ? AND raw_sender = ? AND FALSE)) \
             AND sensitivity != 'secret'",
            "owner {owner:?}"
        );
        assert_eq!(
            texts(&params),
            vec!["telegram", "chat-1", "sender-1"],
            "owner {owner:?}"
        );

        let member = isolated_principal(Role::Member, Visibility::User, owner);
        let (scope, params) = member.build_sql_scope();
        assert!(
            scope.contains("visibility = 'user' AND sender_id = ? AND FALSE"),
            "{scope}"
        );
        assert_eq!(texts(&params), vec!["telegram", "chat-1", "u1"], "owner {owner:?}");
    }
}

#[test]
fn owner_isolated_owner_role_is_not_unconditional() {
    // An operator who has bound the MCP channel to an `owner`-role user
    // must not thereby hand every external caller the whole store.
    let principal = isolated_principal(Role::Owner, Visibility::Public, Some(TEST_OWNER));
    let (scope, params) = principal.build_sql_scope();
    assert_ne!(scope, "1=1");
    assert_eq!(scope, "1=1 AND owner_id = ?");
    assert_eq!(texts(&params), vec![TEST_OWNER]);
}

#[test]
fn owner_isolated_acl_disabled_principal_is_not_unconditional() {
    let mut principal = isolated_principal(Role::Member, Visibility::Public, Some(TEST_OWNER));
    principal.acl_enforced = false;
    let (scope, params) = principal.build_sql_scope();
    assert_ne!(scope, "1=1");
    assert_eq!(scope, "1=1 AND owner_id = ?");
    assert_eq!(texts(&params), vec![TEST_OWNER]);

    // And with no owner id it admits nothing rather than everything.
    let mut ownerless = isolated_principal(Role::Owner, Visibility::Public, None);
    ownerless.acl_enforced = false;
    let (scope, params) = ownerless.build_sql_scope();
    assert_eq!(scope, "1=1 AND FALSE");
    assert!(params.is_empty(), "expected an empty value");
}

#[test]
fn owner_isolated_scope_keeps_public_and_secret_semantics() {
    // The two deliberate contracts the owner suffix must never touch:
    // `public` rows stay shared, and `secret` rows stay excluded.
    for principal in [
        isolated_principal(Role::Anonymous, Visibility::Private, Some(TEST_OWNER)),
        isolated_principal(Role::Member, Visibility::Project, Some(TEST_OWNER)),
        isolated_principal(Role::Guest, Visibility::Private, None),
    ] {
        let (scope, _) = principal.build_sql_scope();
        assert!(
            scope.contains("visibility = 'public' OR "),
            "the public arm must stay unqualified: {scope}"
        );
        assert!(
            !scope.contains("visibility = 'public' AND owner_id"),
            "the public arm must not gain an owner match: {scope}"
        );
        assert!(
            scope.contains("sensitivity != 'secret'"),
            "the secret exclusion must survive: {scope}"
        );
    }
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
    let mut cases = vec![
        base_principal(Role::Owner, Visibility::Public),
        base_principal(Role::Anonymous, Visibility::Private),
        base_principal(Role::Guest, Visibility::Private),
        base_principal(Role::Member, Visibility::User),
        base_principal(Role::Member, Visibility::Group),
        base_principal(Role::Member, Visibility::Project),
    ];
    // C03 / V02-R5: the owner suffix adds one marker and exactly one bound
    // param per non-public arm when it carries an owner id, and adds
    // neither when it fails closed. Both must hold on every branch.
    for role in [Role::Owner, Role::Anonymous, Role::Guest, Role::Member] {
        for ceiling in [
            Visibility::Private,
            Visibility::User,
            Visibility::Group,
            Visibility::Project,
            Visibility::Public,
        ] {
            for owner in [None, Some(TEST_OWNER), Some("   ")] {
                cases.push(isolated_principal(role.clone(), ceiling.clone(), owner));
            }
        }
    }
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
    assert!(owner.params.is_empty(), "expected an empty value");

    let mut member = base_principal(Role::Member, Visibility::Public);
    member.acl_enforced = false;
    let disabled = member.build_scope_predicate();
    assert_eq!(disabled.template, "1=1");
    assert!(disabled.params.is_empty(), "expected an empty value");
}

// ----------------------------------------------------------------------
// D11 A2: Postgres `$N` renderer. Numbering must start at start_index,
// increment by one per bind marker, leave no markers behind, and keep the
// param list identical (same order) to the dialect-agnostic predicate.
// ----------------------------------------------------------------------

#[test]
fn build_sql_scope_pg_owner_is_unconditional() {
    let principal = base_principal(Role::Owner, Visibility::Public);
    let (sql, params) = principal.build_sql_scope_pg(4);
    assert_eq!(sql, "1=1");
    assert!(params.is_empty(), "expected an empty value");
}

#[test]
fn build_sql_scope_pg_recall_start_index_four() {
    // recall_with_context binds query=$1, sid=$2, limit=$3, so the scope
    // predicate starts at $4. Anonymous => three triple params at $4,$5,$6.
    let principal = base_principal(Role::Anonymous, Visibility::Private);
    let (sql, params) = principal.build_sql_scope_pg(4);
    assert_eq!(
        sql,
        "(visibility = 'public' OR (channel = $4 AND chat_id = $5 AND raw_sender = $6)) \
         AND sensitivity != 'secret'"
    );
    assert_eq!(
        params,
        vec![
            ScopeParam::Text("telegram".to_string()),
            ScopeParam::Text("chat-1".to_string()),
            ScopeParam::Text("sender-1".to_string()),
        ]
    );
    assert!(!sql.contains(SCOPE_BIND_MARKER));
}

#[test]
fn build_sql_scope_pg_forget_start_index_two() {
    // forget_with_context binds key=$1, so the scope predicate starts at $2.
    let principal = base_principal(Role::Anonymous, Visibility::Private);
    let (sql, params) = principal.build_sql_scope_pg(2);
    assert_eq!(
        sql,
        "(visibility = 'public' OR (channel = $2 AND chat_id = $3 AND raw_sender = $4)) \
         AND sensitivity != 'secret'"
    );
    assert_eq!(params.len(), 3);
}

#[test]
fn build_sql_scope_pg_project_ceiling_numbers_all_markers_sequentially() {
    // The most marker-dense branch: 8 binds. Starting at $4 they must run
    // $4..$11 with no gaps, no reuse, no leftover markers.
    let principal = base_principal(Role::Member, Visibility::Project);
    let (sql, params) = principal.build_sql_scope_pg(4);
    assert_eq!(
        sql,
        "(visibility = 'public' OR \
         (visibility = 'private' AND chat_type = 'dm' AND channel = $4 AND chat_id = $5) OR \
         (visibility = 'user' AND sender_id = $6) OR \
         (visibility = 'group' AND chat_type = 'group' AND channel = $7 AND chat_id = $8) OR \
         (visibility = 'project' AND topic_id IN (\
SELECT t.id FROM topics t \
INNER JOIN topic_participants tp ON tp.topic_id = t.id \
WHERE t.project IN ($9,$10) \
AND tp.user_id = $11\
))) \
         AND sensitivity != 'secret'"
    );
    assert_eq!(params.len(), 8);
    assert!(!sql.contains(SCOPE_BIND_MARKER));
    // The highest placeholder is start_index + count - 1.
    assert!(sql.contains("$11"));
    assert!(!sql.contains("$12"));
}

#[test]
fn build_sql_scope_pg_params_match_sqlite_param_values_and_order() {
    // The PG renderer and the SQLite renderer must surface the exact same
    // bound values in the same order; only the placeholder syntax differs.
    for principal in [
        base_principal(Role::Anonymous, Visibility::Private),
        base_principal(Role::Member, Visibility::Group),
        base_principal(Role::Member, Visibility::Project),
    ] {
        let (_sqlite_sql, sqlite_params) = principal.build_sql_scope();
        let (_pg_sql, pg_params) = principal.build_sql_scope_pg(4);
        let sqlite_texts: Vec<&str> = sqlite_params.iter().map(text_of).collect();
        let pg_texts: Vec<&str> = pg_params
            .iter()
            .map(|p| match p {
                ScopeParam::Text(s) => s.as_str(),
            })
            .collect();
        assert_eq!(
            sqlite_texts, pg_texts,
            "param value/order drift for {:?}",
            principal.role
        );
    }
}

// ----------------------------------------------------------------------
// D11 A5: cross-backend scope parity truth table. For the same
// (role, visibility_ceiling) the SQLite renderer and the Postgres renderer
// must encode the SAME allow decision: identical predicate structure
// (modulo placeholder syntax) and identical bound parameter values/order.
// This is the regression lock that the two backends no longer drift.
// ----------------------------------------------------------------------

/// Re-normalize a rendered SQLite SQL string back to the neutral marker so
/// it can be compared structurally against the PG rendering.
fn sqlite_to_marker(sql: &str) -> String {
    sql.replace('?', SCOPE_BIND_MARKER)
}

/// Re-normalize a rendered Postgres SQL string back to the neutral marker.
/// Replaces `$<n>` runs with the marker. Works for any start index because
/// every `$N` maps back to exactly one bind point.
fn pg_to_marker(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '$' && matches!(chars.peek(), Some((_, d)) if d.is_ascii_digit()) {
            // consume the digit run
            while matches!(chars.peek(), Some((_, d)) if d.is_ascii_digit()) {
                chars.next();
            }
            out.push_str(SCOPE_BIND_MARKER);
        } else {
            out.push(ch);
        }
    }
    out
}

fn param_texts(params: &[ScopeParam]) -> Vec<&str> {
    params
        .iter()
        .map(|p| match p {
            ScopeParam::Text(s) => s.as_str(),
        })
        .collect()
}

#[test]
fn cross_backend_scope_parity_truth_table() {
    // Cover every canonical role x visibility_ceiling combination that the
    // truth table branches on. visibility_ceiling only affects Member/Guest
    // (Owner is unconditional; Anonymous ignores the ceiling), so we sweep
    // the full Visibility ladder for Member/Guest and spot-check the others.
    let ceilings = [
        Visibility::System,
        Visibility::Owner,
        Visibility::Private,
        Visibility::User,
        Visibility::Group,
        Visibility::Project,
        Visibility::Public,
    ];
    let roles = [Role::Owner, Role::Anonymous, Role::Member, Role::Guest];
    // C03 / V02-R5: sweep the isolation bit and the owner id as well, so
    // the SQLite `?` and Postgres `$N` renderings of the new arm cannot
    // drift apart either.
    let isolations: [(bool, Option<&str>); 3] = [(false, None), (true, None), (true, Some(TEST_OWNER))];

    for role in roles {
        for ceiling in &ceilings {
            for (require_owner_match, owner_id) in isolations {
                let mut principal = base_principal(role.clone(), ceiling.clone());
                principal.require_owner_match = require_owner_match;
                principal.owner_id = owner_id.map(str::to_string);

                let (sqlite_sql, sqlite_params) = principal.build_sql_scope();
                // Use a non-trivial start index to prove the parity holds
                // independent of the caller's parameter offset.
                let (pg_sql, pg_params) = principal.build_sql_scope_pg(4);

                assert_eq!(
                    sqlite_to_marker(&sqlite_sql),
                    pg_to_marker(&pg_sql),
                    "scope predicate structure drift for role {role:?} ceiling {ceiling:?}"
                );

                let sqlite_texts: Vec<&str> = sqlite_params.iter().map(text_of).collect();
                let pg_texts = param_texts(&pg_params);
                assert_eq!(
                    sqlite_texts, pg_texts,
                    "scope parameter value/order drift for role {role:?} ceiling {ceiling:?}"
                );
            }
        }
    }
}

#[test]
fn cross_backend_parity_no_residual_placeholders_either_backend() {
    // Defense in depth: after normalization neither rendering may contain a
    // raw `?` or `$N` left over (which would mean a marker leaked or a
    // placeholder was double-rendered).
    let principal = base_principal(Role::Member, Visibility::Project);
    let (sqlite_sql, _) = principal.build_sql_scope();
    let (pg_sql, _) = principal.build_sql_scope_pg(7);
    assert!(!sqlite_sql.contains(SCOPE_BIND_MARKER));
    assert!(!pg_sql.contains(SCOPE_BIND_MARKER));
    // The marker round-trip must be self-consistent.
    assert_eq!(sqlite_to_marker(&sqlite_sql), pg_to_marker(&pg_sql));
}

#[test]
fn cross_backend_parity_anonymous_excludes_workspace_and_null_allow() {
    // Regression lock for the D11 tightening: neither rendering may contain
    // the over-permissive Postgres-only allow paths that were removed.
    for ceiling in [Visibility::Private, Visibility::Public] {
        let principal = base_principal(Role::Anonymous, ceiling);
        let (pg_sql, _) = principal.build_sql_scope_pg(2);
        assert!(
            !pg_sql.contains("visibility IS NULL"),
            "tightened scope must not re-introduce the NULL-visibility allow"
        );
        assert!(
            !pg_sql.contains("'workspace'"),
            "tightened scope must not re-introduce the workspace allow"
        );
        // It must still exclude secrets and gate on the authored triple.
        assert!(pg_sql.contains("sensitivity != 'secret'"));
        assert!(pg_sql.contains("raw_sender ="));
    }
}

#[test]
fn post_filter_applies_nfkc_and_regex() {
    let mut principal = base_principal(Role::Guest, Visibility::Public);
    principal.blocked_patterns = vec![Regex::new("api[_-]?key").unwrap()];

    let inputs = vec![
        "normal text".to_string(),
        "\u{24b6}\u{24c5}\u{24be}\u{24c0}\u{24ba}\u{24ce} leaked".to_string(),
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

#[test]
fn principal_hardcoded_patterns_compile() {
    assert!(
        sensitive_patterns().is_ok(),
        "hardcoded sensitive_patterns must compile"
    );
    assert!(email_re().is_ok(), "hardcoded email_re must compile");
    assert!(ipv4_re().is_ok(), "hardcoded ipv4_re must compile");
}

#[test]
fn sensitive_pattern_error_fails_closed() {
    let error = regex::Error::Syntax("test: injected pattern failure".to_string());
    assert!(
        sensitive_patterns_or_closed("ordinary prose", Err(&error)),
        "missing sensitive patterns must classify all content as sensitive"
    );
    assert!(
        contains_pii("ordinary prose", Err(&error), ipv4_re()),
        "missing email pattern must classify all content as PII"
    );
    assert!(
        contains_pii("ordinary prose", email_re(), Err(&error)),
        "missing IP pattern must classify all content as PII"
    );
}
#[test]
fn classification_pattern_errors_have_a_distinct_reason() {
    let error = regex::Error::Syntax("test: injected failure".to_string());
    let patterns = sensitive_patterns().map(Vec::as_slice);
    for signals in [
        classification_risk_signals("ordinary prose", Err(&error), email_re(), ipv4_re()),
        classification_risk_signals("ordinary prose", patterns, Err(&error), ipv4_re()),
        classification_risk_signals("ordinary prose", patterns, email_re(), Err(&error)),
    ] {
        assert_eq!(
            signals,
            ["pattern_unavailable"],
            "pattern failures must not invent sensitive-keyword or PII matches"
        );
    }
}
