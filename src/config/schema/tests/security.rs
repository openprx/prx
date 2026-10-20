#![cfg(test)]

use super::*;
use tokio::test;

#[test]
async fn autonomy_config_default() {
    let a = AutonomyConfig::default();
    assert_eq!(a.level, AutonomyLevel::Full);
    assert!(a.workspace_only);
    assert!(a.forbidden_paths.contains(&"/etc".to_string()));
    assert_eq!(a.max_cost_per_day_cents, DEFAULT_AUTONOMY_MAX_COST_PER_DAY_CENTS);
}

#[test]
async fn autonomy_config_ignores_legacy_sandbox_table() {
    let parsed: AutonomyConfig = toml::from_str(
        r#"
level = "full"
workspace_only = false
forbidden_paths = []
max_cost_per_day_cents = 500

[sandbox]
enabled = false
backend = "auto"
"#,
    )
    .expect("legacy sandbox settings remain forward-compatible no-ops");

    assert_eq!(parsed.level, AutonomyLevel::Full);
    assert!(!parsed.workspace_only);
}

#[test]
async fn webhook_config_with_secret() {
    let json = r#"{"port":8080,"secret":"my-secret-key"}"#;
    let parsed: WebhookConfig = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.secret.as_deref(), Some("my-secret-key"));
}

#[test]
async fn webhook_config_without_secret() {
    let json = r#"{"port":8080}"#;
    let parsed: WebhookConfig = serde_json::from_str(json).unwrap();
    assert!(parsed.secret.is_none());
    assert_eq!(parsed.port, 8080);
}

/// A zero audit size would rotate the trail on every record (D-052).
#[tokio::test]
async fn audit_max_size_zero_is_rejected() {
    let mut config = Config::default();
    config.security.audit.max_size_mb = 0;
    let err = config.validate().expect_err("test: a zero audit size must be rejected");
    assert!(
        err.to_string()
            .contains("security.audit.max_size_mb must be at least 1"),
        "test: unexpected error: {err}"
    );

    config.security.audit.max_size_mb = 1;
    assert!(config.validate().is_ok(), "test: the smallest non-zero size loads");
}

#[test]
async fn memory_webhook_config_signing_secret_roundtrip() {
    let config: MemoryWebhookConfig = toml::from_str(
        r#"
bind = "127.0.0.1:16899"
token = "receiver-token"
signing_secret = "hmac-secret"
"#,
    )
    .unwrap();
    assert!(config.configured());
    assert_eq!(config.token.as_deref(), Some("receiver-token"));
    assert_eq!(config.signing_secret.as_deref(), Some("hmac-secret"));

    let legacy: MemoryWebhookConfig = toml::from_str(
        r#"
bind = "127.0.0.1:16899"
"#,
    )
    .unwrap();
    assert!(legacy.signing_secret.is_none());
}

#[test]
async fn checklist_autonomy_default_is_workspace_scoped() {
    let a = AutonomyConfig::default();
    assert!(a.workspace_only, "Default autonomy must be workspace_only");
    assert!(a.forbidden_paths.contains(&"/etc".to_string()), "Must block /etc");
    assert!(a.forbidden_paths.contains(&"/proc".to_string()), "Must block /proc");
    assert!(a.forbidden_paths.contains(&"~/.ssh".to_string()), "Must block ~/.ssh");
}

// ══════════════════════════════════════════════════════════
// SECRETS CONFIG TESTS
// ══════════════════════════════════════════════════════════

#[test]
async fn secrets_config_default_encrypts() {
    let s = SecretsConfig::default();
    assert!(s.encrypt, "Encryption must be enabled by default");
}

#[test]
async fn secrets_config_serde_roundtrip() {
    let s = SecretsConfig { encrypt: false };
    let toml_str = toml::to_string(&s).unwrap();
    let parsed: SecretsConfig = toml::from_str(&toml_str).unwrap();
    assert!(!parsed.encrypt);
}

#[test]
async fn secrets_config_backward_compat_missing_section() {
    let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
    let parsed: Config = toml::from_str(minimal).unwrap();
    assert!(parsed.secrets.encrypt, "Missing [secrets] must default to encrypt=true");
}

#[test]
async fn config_default_has_composio_and_secrets() {
    let c = Config::default();
    assert!(!c.composio.configured());
    assert!(c.auth.codex_auth_json_auto_import);
    assert!(c.composio.api_key.is_none());
    assert!(c.secrets.encrypt);
    assert!(c.browser.allowed_domains.is_empty(), "expected an empty value");
}

#[test]
async fn wacli_config_requires_secret_when_enabled() {
    let cfg = WacliConfig {
        webhook_secret: None,
        ..WacliConfig::default()
    };
    assert!(cfg.validate().is_err(), "enabled without secret must fail");
}

/// R2 visibility: only the first matching `send_allow` is ever consulted, so
/// a whitelist an earlier rule already covers is dead configuration. Say so
/// at load time rather than letting the operator find out by a refused send.
#[test]
async fn scope_config_rejects_a_shadowed_send_allow() {
    let cfg = ScopeConfig {
        default: ScopeDefault::Allow,
        rules: vec![
            scope_rule_with(None, None, &["telegram:*"]),
            scope_rule_with(Some("wacli"), None, &["wacli:1234@s.whatsapp.net"]),
        ],
        assign_owners: Vec::new(),
    };
    let error = cfg.validate().expect_err("a shadowed send_allow must be rejected");
    let error = error.to_string();
    assert!(error.contains("rules[1].send_allow"), "{error}");
    assert!(error.contains("rules[0]"), "{error}");
}

/// The shadow test is conservative: a rule that only *sometimes* matches
/// what a later rule matches shadows nothing.
#[test]
async fn scope_config_accepts_a_narrower_rule_that_is_not_shadowed() {
    let ordered = ScopeConfig {
        default: ScopeDefault::Allow,
        rules: vec![
            scope_rule_with(Some("wacli"), None, &["wacli:*"]),
            scope_rule_with(Some("telegram"), None, &["telegram:*"]),
        ],
        assign_owners: Vec::new(),
    };
    ordered.validate().expect("disjoint criteria never shadow");

    let narrowing = ScopeConfig {
        default: ScopeDefault::Allow,
        rules: vec![
            scope_rule_with(Some("wacli"), Some("group"), &["wacli:*"]),
            scope_rule_with(Some("wacli"), None, &["wacli:1234@s.whatsapp.net"]),
        ],
        assign_owners: Vec::new(),
    };
    narrowing
        .validate()
        .expect("a chat_type-scoped rule does not cover the unscoped one below it");
}

/// A rule carrying no `send_allow` shadows nothing — it is not an outbound
/// rule, and `is_outbound_allowed` skips it for exactly that reason.
#[test]
async fn scope_config_accepts_a_send_allow_below_a_tools_only_rule() {
    let cfg = ScopeConfig {
        default: ScopeDefault::Allow,
        rules: vec![
            ScopeRule {
                tools_deny: vec!["shell".into()],
                ..ScopeRule::default()
            },
            scope_rule_with(None, None, &["telegram:*"]),
        ],
        assign_owners: Vec::new(),
    };
    cfg.validate().expect("a tools-only rule does not shadow a send_allow");
}

#[test]
async fn scope_config_accepts_well_formed_outbound_entries() {
    assert!(
        scope_config_with(&["telegram:*", "*"], &["wacli:1234@s.whatsapp.net", "*:+15550001111"])
            .validate()
            .is_ok()
    );
    assert!(ScopeConfig::default().validate().is_ok());
}

#[test]
async fn scope_config_rejects_outbound_entry_without_channel_segment() {
    // A colon-less entry can never match, which for send_deny would be a
    // silent fail-open.
    let err = scope_config_with(&[], &["+15550001111"]).validate().unwrap_err();
    assert!(err.to_string().contains("send_deny"), "{err}");
}

#[test]
async fn scope_config_rejects_empty_outbound_segments() {
    assert!(scope_config_with(&[], &[":+1555"]).validate().is_err());
    assert!(scope_config_with(&[], &["telegram:"]).validate().is_err());
    assert!(scope_config_with(&["  "], &[]).validate().is_err());
}

/// The same malformed-entry check the outbound lists get, on the assignment
/// lists. A `{label}:{session}` entry that cannot match is a silent
/// fail-open on `assign_deny`, so it is refused at load time.
///
/// MUTATION GUARD: stop validating the assignment lists in
/// `ScopeConfig::validate` and this test fails.
#[test]
async fn scope_config_rejects_malformed_assignment_entries() {
    let colonless = ScopeConfig {
        rules: vec![assign_rule(None, &[], &["workstation"])],
        ..ScopeConfig::default()
    };
    let error = colonless
        .validate()
        .expect_err("a colon-less assign_deny entry can never match");
    let error = error.to_string();
    assert!(error.contains("assign_deny"), "{error}");
    assert!(error.contains("{label}:{session}"), "{error}");

    let empty_half = ScopeConfig {
        rules: vec![assign_rule(None, &["workstation:"], &[])],
        ..ScopeConfig::default()
    };
    assert!(
        empty_half.validate().is_err(),
        "an empty session segment matches nothing"
    );

    let owners = ScopeConfig {
        assign_owners: vec!["wacli".to_string()],
        ..ScopeConfig::default()
    };
    let error = owners.validate().expect_err("an owner entry needs both segments");
    let error = error.to_string();
    assert!(error.contains("assign_owners"), "{error}");
    assert!(error.contains("{channel}:{sender}"), "{error}");
}

/// `assign_allow` is first-match-wins exactly like `send_allow`, so a
/// whitelist an earlier rule already covers is dead configuration and is
/// rejected rather than silently ignored.
///
/// MUTATION GUARD: drop the `assign_allow` call to
/// `validate_no_shadowed_allow` and this test fails.
#[test]
async fn scope_config_rejects_a_shadowed_assign_allow() {
    let cfg = ScopeConfig {
        rules: vec![
            assign_rule(None, &["*"], &[]),
            assign_rule(Some("wacli"), &["workstation:*"], &[]),
        ],
        ..ScopeConfig::default()
    };
    let error = cfg.validate().expect_err("a shadowed assign_allow must be rejected");
    let error = error.to_string();
    assert!(error.contains("rules[1].assign_allow"), "{error}");
    assert!(error.contains("rules[0]"), "{error}");
}

/// A rule with only an `assign_deny` shadows nothing: deny sweeps every
/// matching rule, so there is nothing for it to swallow.
#[test]
async fn scope_config_accepts_an_assign_allow_below_a_deny_only_rule() {
    let cfg = ScopeConfig {
        rules: vec![
            assign_rule(None, &[], &["*:some-session"]),
            assign_rule(None, &["workstation:*"], &[]),
        ],
        ..ScopeConfig::default()
    };
    cfg.validate()
        .expect("a deny-only rule does not shadow an assign_allow");
}

#[test]
async fn scope_config_accepts_well_formed_assignment_entries() {
    let cfg = ScopeConfig {
        rules: vec![assign_rule(Some("wacli"), &["workstation:*", "*"], &["*:dead-session"])],
        assign_owners: vec!["wacli:+15550001111".to_string(), "*:root".to_string()],
        ..ScopeConfig::default()
    };
    cfg.validate().expect("well-formed assignment entries load");
}

// ── D-050 item 1: one canonical reading of runtime enumerations ──────

/// RED before SECRETS-i (R-SECRETSfg H-1, P20): `autonomy.scopes.default`
/// was a string the plan read trimmed and the runtime read untrimmed, so
/// `"deny "` was deny to the plan and allow at runtime. It is parsed once
/// now: case and padding do not matter, and the runtime policy built
/// from it denies an unmatched tool.
#[test]
async fn scope_default_is_read_once_and_the_runtime_agrees() {
    for raw in ["deny", "deny ", " DENY", "Deny"] {
        let scopes: ScopeConfig =
            toml::from_str(&format!("default = \"{raw}\"\n")).expect("test: scope default parses");
        assert_eq!(scopes.default, ScopeDefault::Deny, "{raw:?}");
        let mut autonomy = AutonomyConfig::default();
        autonomy.scopes = scopes;
        let policy = crate::security::SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));
        assert!(
            !policy.is_tool_allowed("shell", "uuid:someone", "signal", "direct"),
            "{raw:?} must deny an unmatched tool at runtime"
        );
    }
    let scopes: ScopeConfig = toml::from_str("default = \" allow\"\n").expect("test: allow");
    assert_eq!(scopes.default, ScopeDefault::Allow);
    assert_eq!(
        toml::to_string(&scopes).expect("test: render").lines().next(),
        Some("default = \"allow\""),
        "the stored form is canonical"
    );
}

/// D-063 G-1, RED before (B2f F-3 refused the load): a
/// `security.audit.log_path` through a symlinked directory in the
/// workspace must not block configuration load, since the agent can plant
/// such a link. After: the configuration loads with a warning naming the
/// setting and the link, and the audit logger built from it still
/// refuses to write through the link (the record is skipped, nothing is
/// written in the link's target).
#[cfg(unix)]
#[test]
async fn an_audit_log_path_through_a_symlinked_directory_loads_with_a_warning() {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(workspace.join("real")).expect("test: real dir");
    std::os::unix::fs::symlink(workspace.join("real"), workspace.join("alias")).expect("test: link");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "default_temperature = 0.7\n\n[security.audit]\nlog_path = \"alias/audit.log\"\n",
    )
    .expect("test: write config");
    let (loaded, logs) = capture_logs(|| Config::load_from_path(&path, workspace.clone()));
    let loaded = loaded.expect("test: a planted link does not block the load");
    assert!(
        logs.contains("security.audit.log_path runs through a symbolic link") && logs.contains("alias"),
        "the load warns, naming the link: {logs}"
    );

    let logger =
        crate::security::audit::AuditLogger::new(&loaded.security.audit, workspace.clone()).expect("test: logger");
    let event = crate::security::audit::AuditEvent::new(crate::security::audit::AuditEventType::SecurityEvent);
    assert!(logger.log(&event).is_err(), "the logger still refuses the link");
    assert!(
        !workspace.join("real/audit.log").exists(),
        "nothing is written through the link"
    );
}
