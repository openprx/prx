#![cfg(test)]

use super::*;
use crate::runtime::envelope::RuntimeSource;

/// Mirrors exactly what the terminal UI stamps on a turn: see
/// `chat_tool_execution_context` (`src/chat/dispatcher.rs:2072-2090`) and
/// the legacy `ScopeContext` at `src/chat/mod.rs:7142`.
fn local_operator_context() -> ToolExecutionContext {
    ToolExecutionContext::new(
        RuntimeEnvelope::chat_terminal(
            "workspace-a",
            "chat:redux:draft-1",
            crate::memory::MemoryVisibility::Workspace,
        )
        .with_sender("user")
        .with_channel("terminal"),
        "private",
    )
    .with_chat_id("terminal:user")
}

/// Every remote ingress flattens into a `chat_terminal` envelope via
/// `agent_tool_execution_context` (`src/agent/execution_policy.rs`), so these
/// differ from the local operator only in the scope strings — which is
/// precisely why `is_local_operator` checks the whole tuple.
fn remote_context(channel: &str, sender: &str, chat_type: &str, chat_id: &str) -> ToolExecutionContext {
    ToolExecutionContext::new(
        RuntimeEnvelope::chat_terminal("workspace-a", "session-a", crate::memory::MemoryVisibility::Workspace)
            .with_sender(sender)
            .with_channel(channel),
        chat_type,
    )
    .with_chat_id(chat_id)
}

/// Run the real injection path and read back the marker the tool would see.
fn injected_trust(context: &ToolExecutionContext, caller_args: serde_json::Value) -> bool {
    let normalized =
        normalize_arguments(&caller_args, context, false, None).expect("normalize_arguments accepts a JSON object");
    normalized
        .get("_prx_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .expect("the runtime always authors _prx_scope_trusted")
}

#[test]
fn local_terminal_operator_is_trusted() {
    assert!(
        local_operator_context().is_local_operator(),
        "the terminal UI tuple must be recognised as the local operator, or \
             gateway config.patch/restart is unreachable for everyone"
    );
    assert!(injected_trust(
        &local_operator_context(),
        serde_json::json!({"action": "restart"})
    ));
}

/// OWNER-VIS (D-019 Q-5): the run-view predicate admits the two
/// local-process operator surfaces and nothing else. Driven through the
/// real `normalize_arguments`, so it reads exactly the scope a tool sees.
#[test]
fn only_the_terminal_ui_and_the_agent_cli_are_local_process_operators() {
    let authored = |context: &ToolExecutionContext| {
        normalize_arguments(&serde_json::json!({}), context, false, None)
            .expect("normalize_arguments accepts a JSON object")
    };
    assert!(scope_is_local_process_operator(&authored(&local_operator_context())));
    assert!(scope_is_local_process_operator(&authored(&remote_context(
        "cli",
        "agent",
        "private",
        "agent:local"
    ))));
    for (channel, sender, chat_type, chat_id) in [
        ("telegram", "1234567", "private", "1234567"),
        ("webhook", "webhook-client", "gateway", "gateway:webhook:webhook-client"),
        ("telegram", "1234567", "gateway", "gateway:telegram:1234567"),
        ("console", "console-user", "private", "console-session-1"),
        ("cli", "agent", "private", "agent:elsewhere"),
        ("cli", "user", "private", "agent:local"),
        ("agent", "agent", "private", "agent:local"),
    ] {
        assert!(
            !scope_is_local_process_operator(&authored(&remote_context(channel, sender, chat_type, chat_id))),
            "{channel}/{sender}/{chat_id} is not a local-process operator"
        );
    }
    let mut isolated = remote_context("cli", "agent", "private", "agent:local");
    isolated.envelope.owner_isolated = true;
    assert!(
        !scope_is_local_process_operator(&authored(&isolated)),
        "an owner-isolated scope is never the operator"
    );
    assert!(
        !scope_is_local_process_operator(&serde_json::json!({
            "_prx_scope_trusted": true,
            "_zc_scope": {"channel": "cli", "sender": "agent", "chat_id": "agent:local"}
        })),
        "an untrusted scope is never the operator"
    );
}

#[test]
fn remote_ingress_is_never_trusted() {
    // channel, sender, chat_type, chat_id per the real ScopeContext sites.
    let remote = [
        // src/channels/turn.rs (ScopeContext construction) — IM adapters.
        ("telegram", "1234567", "private", "1234567"),
        ("whatsapp", "995551518602@s.whatsapp.net", "group", "120363@g.us"),
        // src/gateway/mod.rs:2006 — webhook / fabric turns.
        ("webhook", "webhook-client", "gateway", "gateway:webhook:webhook-client"),
        ("telegram", "1234567", "gateway", "gateway:telegram:1234567"),
        // src/gateway/api/sessions.rs:316 — console web UI.
        ("console", "console-user", "private", "console-session-1"),
        // src/agent/execution_policy.rs — `prx agent` CLI with no scope.
        ("cli", "agent", "private", "agent:local"),
    ];
    for (channel, sender, chat_type, chat_id) in remote {
        let context = remote_context(channel, sender, chat_type, chat_id);
        assert!(
            !context.is_local_operator(),
            "{channel}/{sender} must not be treated as the local operator"
        );
        assert!(
            !injected_trust(&context, serde_json::json!({"action": "restart"})),
            "{channel}/{sender} must not receive _prx_scope_trusted=true"
        );
    }
}

/// The allowlist is a conjunction: flipping any single field must deny.
/// This is what keeps the predicate fail-closed against a renamed channel,
/// a new runtime source, or a partially-forged scope.
#[test]
fn every_single_field_deviation_denies() {
    let base = local_operator_context();
    assert!(base.is_local_operator(), "sanity: the unmodified tuple is trusted");

    let mut wrong_channel = local_operator_context();
    wrong_channel.envelope = wrong_channel.envelope.with_channel("terminal-x");
    assert!(!wrong_channel.is_local_operator(), "channel must match exactly");

    let mut wrong_sender = local_operator_context();
    wrong_sender.envelope = wrong_sender.envelope.with_sender("local-user");
    assert!(
        !wrong_sender.is_local_operator(),
        "sender must be the terminal UI's literal 'user', not the envelope default"
    );

    let wrong_chat_id = local_operator_context().with_chat_id("terminal:user:extra");
    assert!(!wrong_chat_id.is_local_operator(), "chat_id must match exactly");

    let mut wrong_chat_type = local_operator_context();
    wrong_chat_type.chat_type = "group".to_string();
    assert!(!wrong_chat_type.is_local_operator(), "chat_type must match exactly");

    let mut wrong_source = local_operator_context();
    wrong_source.envelope.source = RuntimeSource::Gateway;
    assert!(!wrong_source.is_local_operator(), "runtime source must be Chat");
}

/// A missing envelope field falls back to `"unknown"` and must deny rather
/// than accidentally match.
#[test]
fn absent_scope_fields_deny() {
    let mut context = local_operator_context();
    context.envelope.channel = None;
    context.envelope.sender = None;
    assert!(!context.is_local_operator());
    assert!(!injected_trust(&context, serde_json::json!({"action": "restart"})));
}

/// T14 prefix stripping stays in force: the caller's own value is dropped,
/// never merged. Asserted in *both* directions so the test proves runtime
/// authorship rather than mere agreement with the caller.
#[test]
fn caller_supplied_marker_is_ignored_in_both_directions() {
    // A remote caller claiming trust gains nothing.
    let remote = remote_context("telegram", "1234567", "private", "1234567");
    assert!(
        !injected_trust(
            &remote,
            serde_json::json!({"action": "restart", "_prx_scope_trusted": true}),
        ),
        "a forged _prx_scope_trusted=true must not survive stripping"
    );

    // ...and a caller claiming *un*trust cannot suppress the real marker,
    // which shows the value is authored, not merely filtered.
    assert!(
        injected_trust(
            &local_operator_context(),
            serde_json::json!({"action": "restart", "_prx_scope_trusted": false}),
        ),
        "the runtime, not the caller, decides the marker"
    );

    // The whole runtime prefix range stays caller-proof.
    let normalized = normalize_arguments(
        &serde_json::json!({
            "action": "restart",
            "_prx_scope_trusted": true,
            "_prx_anything_else": "forged",
            "_zc_approval_granted": true,
        }),
        &remote,
        false,
        None,
    )
    .expect("normalize_arguments accepts a JSON object");
    assert!(
        normalized.get("_prx_anything_else").is_none(),
        "forged _prx_ key survived"
    );
    assert_eq!(
        normalized
            .get("_zc_approval_granted")
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "forged approval flag must be overwritten by the runtime"
    );
}

// ── CRON-ORIGIN: scheduled jobs carry and run as their creating origin ──

/// One field of the service-authored `_zc_scope` block.
fn scoped<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get("_zc_scope")
        .and_then(|scope| scope.get(key))
        .and_then(serde_json::Value::as_str)
}

/// The service-authored scope a tool sees for `context`.
fn authored_scope(context: &ToolExecutionContext) -> serde_json::Value {
    normalize_arguments(&serde_json::json!({}), context, false, None)
        .expect("normalize_arguments accepts a JSON object")
}

/// Only the two local-process operator surfaces create operator jobs; a
/// network surface is recorded as itself, an isolated one stays isolated,
/// and a scope the runtime did not author has no creator at all.
#[test]
fn a_job_is_attributed_to_the_operator_only_from_an_operator_scope() {
    assert_eq!(
        JobOrigin::from_trusted_args(&authored_scope(&local_operator_context()), None),
        JobOrigin::LocalOperator
    );
    assert_eq!(
        JobOrigin::from_trusted_args(
            &authored_scope(&remote_context("cli", "agent", "private", "agent:local")),
            None
        ),
        JobOrigin::LocalOperator
    );
    let webhook = remote_context("webhook", "client-a", "gateway", "gateway:webhook:client-a");
    assert_eq!(
        JobOrigin::from_trusted_args(&authored_scope(&webhook), Some(" owner-a ")),
        JobOrigin::Principal {
            channel: "webhook".to_string(),
            sender: "client-a".to_string(),
            chat_type: "gateway".to_string(),
            owner_id: Some("owner-a".to_string()),
            owner_isolated: false,
        }
    );
    // An owner-isolated caller with the CLI tuple is still not the operator.
    let mut isolated = remote_context("cli", "agent", "private", "agent:local");
    isolated.envelope.owner_isolated = true;
    let origin = JobOrigin::from_trusted_args(&authored_scope(&isolated), Some("owner-mcp"));
    assert!(!origin.is_local_operator());
    assert!(origin.is_owner_isolated());
    // A forged operator claim without the runtime's trust marker.
    assert_eq!(
        JobOrigin::from_trusted_args(
            &serde_json::json!({
                "_prx_scope_trusted": true,
                "_zc_scope": {"channel": "terminal", "sender": "user", "chat_id": "terminal:user"}
            }),
            None
        ),
        JobOrigin::Unattributed
    );
}

/// A stored origin round-trips, and anything unprovable — a pre-origin row
/// (`NULL`), an empty value, an unreadable one — never becomes the
/// operator.
#[test]
fn a_stored_origin_round_trips_and_fails_closed() {
    for origin in [
        JobOrigin::LocalOperator,
        JobOrigin::Unattributed,
        JobOrigin::Principal {
            channel: "telegram".to_string(),
            sender: "alice".to_string(),
            chat_type: "direct".to_string(),
            owner_id: Some("owner-alice".to_string()),
            owner_isolated: true,
        },
    ] {
        let stored = origin.to_stored().expect("test: encode origin");
        assert_eq!(JobOrigin::from_stored(Some(&stored)), origin);
    }
    for raw in [
        None,
        Some(""),
        Some("  "),
        Some("local_operator"),
        Some("{\"kind\":\"root\"}"),
    ] {
        assert_eq!(JobOrigin::from_stored(raw), JobOrigin::Unattributed, "{raw:?}");
    }
}

/// Whatever channel and sender a creator had — even the literal operator
/// strings — a non-operator job's run satisfies neither operator
/// predicate: it keeps the `cron` source and its `cron:{id}` chat id. It
/// acts as its creator's owner, so its run view is confined to that owner.
///
/// MUTATION GUARD: build the scheduled context on a `Chat` envelope with
/// the CLI chat id and the `("cli", "agent")` rows go red.
#[test]
fn a_non_operator_scheduled_run_is_never_an_operator_and_acts_as_its_creator() {
    let run_envelope = || RuntimeEnvelope::cron("workspace-a", "job-1", "attempt-1").with_owner_id("owner-stored");
    assert!(
        JobOrigin::LocalOperator
            .scheduled_tool_context(run_envelope())
            .is_none()
    );

    for (channel, sender, chat_type) in [
        ("webhook", "client-a", "gateway"),
        ("telegram", "1234567", "private"),
        ("cli", "agent", "private"),
        ("terminal", "user", "private"),
    ] {
        let origin = JobOrigin::Principal {
            channel: channel.to_string(),
            sender: sender.to_string(),
            chat_type: chat_type.to_string(),
            owner_id: Some("owner-creator".to_string()),
            owner_isolated: false,
        };
        let context = origin
            .scheduled_tool_context(run_envelope())
            .expect("test: a principal origin gets its own context");
        assert!(!context.is_local_operator(), "{channel}/{sender}");
        let scope = authored_scope(&context);
        assert!(!scope_is_local_process_operator(&scope), "{channel}/{sender}");
        assert_eq!(scoped(&scope, "runtime_source"), Some("cron"));
        assert_eq!(scoped(&scope, "channel"), Some(channel));
        assert_eq!(scoped(&scope, "sender"), Some(sender));
        assert_eq!(scoped(&scope, "chat_id"), Some("cron:job-1"));
        assert_eq!(scoped(&scope, "owner_id"), Some("owner-creator"));
        assert_eq!(
            crate::tools::sessions_read_model::RunOwnerScope::from_args(&scope),
            crate::tools::sessions_read_model::RunOwnerScope::Owner(Some("owner-creator".to_string())),
            "{channel}/{sender}: the run view is the creator's own"
        );
    }

    let unattributed = JobOrigin::Unattributed
        .scheduled_tool_context(run_envelope())
        .expect("test: an unattributed job gets its own context");
    let scope = authored_scope(&unattributed);
    assert!(!unattributed.is_local_operator());
    assert!(!scope_is_local_process_operator(&scope));
    assert_eq!(scoped(&scope, "owner_id"), Some("owner-stored"));
    assert_eq!(
        crate::tools::sessions_read_model::RunOwnerScope::from_args(&scope),
        crate::tools::sessions_read_model::RunOwnerScope::Owner(Some("owner-stored".to_string()))
    );

    let isolated = JobOrigin::Principal {
        channel: "mcp".to_string(),
        sender: "subject".to_string(),
        chat_type: "private".to_string(),
        owner_id: Some("owner-mcp".to_string()),
        owner_isolated: true,
    }
    .scheduled_tool_context(run_envelope())
    .expect("test: an isolated origin gets its own context");
    assert!(
        isolated.envelope.owner_isolated,
        "an isolated creator's run stays isolated"
    );
    assert!(scope_requires_owner_match(&authored_scope(&isolated)));
}

/// R-CRON F-4: a non-operator job with no stored owner (a legacy row, or a
/// job created in-process) used to act as the one owner every such job
/// derives from `cron`/`cron` (or `xin`/`xin`), so all of them shared a
/// memory, run-view and job-management bucket. Each now acts as an owner
/// of its own, derived from its job id; a stored owner still wins.
///
/// MUTATION GUARD: drop the per-job owner fallback in
/// `scheduled_tool_context` and this goes red.
#[test]
fn an_ownerless_scheduled_job_acts_as_an_owner_of_its_own() {
    let run_owner = |origin: &JobOrigin, envelope: RuntimeEnvelope| {
        let context = origin
            .scheduled_tool_context(envelope)
            .expect("test: a non-operator job gets its own context");
        crate::tools::sessions_read_model::RunOwnerScope::from_args(&authored_scope(&context))
    };
    let ownerless_principal = JobOrigin::Principal {
        channel: "unknown".to_string(),
        sender: "unknown".to_string(),
        chat_type: "unknown".to_string(),
        owner_id: None,
        owner_isolated: false,
    };
    for origin in [JobOrigin::Unattributed, ownerless_principal] {
        let first = run_owner(&origin, RuntimeEnvelope::cron("ws", "job-1", "a"));
        let second = run_owner(&origin, RuntimeEnvelope::cron("ws", "job-2", "a"));
        let xin = run_owner(&origin, RuntimeEnvelope::xin("ws", "job-1", 1));
        assert!(
            matches!(&first, crate::tools::sessions_read_model::RunOwnerScope::Owner(Some(_))),
            "{origin:?}: {first:?}"
        );
        assert_ne!(first, second, "{origin:?}: two ownerless jobs must not share an owner");
        assert_ne!(
            first, xin,
            "{origin:?}: a cron job and a xin task must not share an owner"
        );
        assert_eq!(
            first,
            run_owner(&origin, RuntimeEnvelope::cron("ws", "job-1", "b")),
            "stable per job"
        );
    }
    assert_eq!(
        run_owner(
            &JobOrigin::Unattributed,
            RuntimeEnvelope::cron("ws", "job-1", "a").with_owner_id("owner-stored")
        ),
        crate::tools::sessions_read_model::RunOwnerScope::Owner(Some("owner-stored".to_string())),
        "a stored owner wins"
    );
}

/// The seam `agent_tool_execution_context` reads: installed for the
/// duration of the scheduled future only, absent everywhere else.
#[tokio::test]
async fn the_scheduled_tool_context_is_visible_only_inside_its_run() {
    assert!(scheduled_job_tool_scope().is_none());
    let config = crate::config::Config::default();
    let scope = JobOrigin::Unattributed
        .scheduled_tool_scope(RuntimeEnvelope::cron("ws", "job-1", "attempt-1"), &config)
        .expect("test: an unattributed job gets a scope");
    let seen = with_scheduled_job_tool_scope(Some(scope.clone()), async { scheduled_job_tool_scope() }).await;
    assert_eq!(seen.map(|seen| seen.context), Some(scope.context));
    assert!(scheduled_job_tool_scope().is_none());
    let operator = with_scheduled_job_tool_scope(None, async { scheduled_job_tool_scope() }).await;
    assert!(operator.is_none());
}

/// R-CRON F-1: the policy a non-operator scheduled run decides with is the
/// configured one — scope rules and level — except that an unattended run
/// refuses what would ask. The operator's job gets no scheduled scope and
/// keeps the operator's policy.
#[test]
fn a_scheduled_scope_carries_the_configured_policy_for_its_creator() {
    let mut config = crate::config::Config::default();
    config.autonomy.scopes.rules = vec![crate::config::ScopeRule {
        user: None,
        channel: Some("webhook".to_string()),
        chat_type: None,
        tools_allow: vec![],
        tools_deny: vec!["shell".to_string()],
        send_allow: vec![],
        send_deny: vec![],
        assign_allow: vec![],
        assign_deny: vec![],
    }];
    let webhook = JobOrigin::Principal {
        channel: "webhook".to_string(),
        sender: "client-a".to_string(),
        chat_type: "gateway".to_string(),
        owner_id: Some("owner-a".to_string()),
        owner_isolated: false,
    };
    let envelope = || RuntimeEnvelope::cron("ws", "job-1", "attempt-1");
    assert!(
        JobOrigin::LocalOperator
            .scheduled_tool_scope(envelope(), &config)
            .is_none()
    );

    let scope = webhook
        .scheduled_tool_scope(envelope(), &config)
        .expect("test: a webhook job gets a scope");
    let policy = SecurityEffectPolicy::new(Arc::clone(&scope.policy));
    let descriptor = |name: &str| ToolDescriptor {
        public_name: name.to_string(),
        backend_name: name.to_string(),
        description: String::new(),
        parameters: serde_json::json!({"type": "object"}),
        execution_parameters: None,
        tier: ToolTier::Standard,
        categories: Vec::new(),
        effect: ToolEffect::Act,
        adapter: ToolAdapterKind::Native,
        process_worker_compatible: true,
        availability: CapabilityAvailability::ready("test"),
    };
    assert_eq!(
        policy.decide(&descriptor("shell"), &scope.context),
        ToolExecutionDecision::Deny
    );
    assert_eq!(
        policy.decide(&descriptor("file_write"), &scope.context),
        ToolExecutionDecision::Allow
    );

    config.autonomy.level = crate::security::policy::AutonomyLevel::Supervised;
    // CRON-g: the scope is built by `build_security_policy`, so the
    // configured audit block reaches it — also when the level is rewritten.
    config.security.audit.log_path = "cron-g-audit.log".to_string();
    config.security.audit.max_size_mb = 7;
    let supervised = webhook
        .scheduled_tool_scope(envelope(), &config)
        .expect("test: a webhook job gets a scope");
    assert_eq!(
        SecurityEffectPolicy::new(Arc::clone(&supervised.policy))
            .decide(&descriptor("file_write"), &supervised.context),
        ToolExecutionDecision::Deny,
        "an unattended run refuses what would ask"
    );
    assert_eq!(supervised.policy.audit_config.log_path, "cron-g-audit.log");
    assert_eq!(supervised.policy.audit_config.max_size_mb, 7);
}
