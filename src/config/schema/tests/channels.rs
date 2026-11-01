#![cfg(test)]

use super::*;
use tokio::test;

/// MUTATION GUARD: drop the `always_include` skip in `for_channel_surface`
/// and the re-enabled tool lands in `always_exclude`, where the router gives
/// exclusion the higher priority — silently un-doing the operator override.
#[test]
async fn channel_tiering_folds_defaults_in_without_overriding_the_operator() {
    let mut config = ToolTieringConfig::default();
    assert!(
        config.channel_exclude.contains(&"wasm_plugins_manage".to_string()),
        "the operations surface must be excluded from channels by default"
    );
    assert!(
        config.always_exclude.is_empty(),
        "the default must not touch the operator surface"
    );

    config.always_include.push("git_operations".to_string());
    config.always_exclude.push("shell".to_string());
    let channel = config.for_channel_surface();

    assert!(
        channel.always_exclude.contains(&"shell".to_string()),
        "existing entries stay"
    );
    assert!(
        channel.always_exclude.contains(&"nodes".to_string()),
        "channel_exclude entries are folded in"
    );
    assert!(
        !channel.always_exclude.contains(&"git_operations".to_string()),
        "a tool the operator re-enabled must not be folded into the exclusion"
    );
    assert_eq!(
        channel.always_include, config.always_include,
        "the include list is carried through unchanged"
    );
}

#[test]
async fn channel_tiering_is_identity_when_the_default_is_emptied() {
    let config = ToolTieringConfig {
        channel_exclude: Vec::new(),
        always_exclude: vec!["shell".to_string()],
        ..ToolTieringConfig::default()
    };
    let channel = config.for_channel_surface();
    assert_eq!(channel.always_exclude, vec!["shell".to_string()]);
}

#[test]
async fn channel_tiering_does_not_duplicate_an_already_excluded_tool() {
    let config = ToolTieringConfig {
        always_exclude: vec!["nodes".to_string()],
        ..ToolTieringConfig::default()
    };
    let channel = config.for_channel_surface();
    assert_eq!(
        channel
            .always_exclude
            .iter()
            .filter(|name| name.as_str() == "nodes")
            .count(),
        1
    );
}

#[test]
async fn channels_config_default() {
    let c = ChannelsConfig::default();
    assert!(c.cli);
    assert!(c.telegram.is_none());
    assert!(c.discord.is_none());
}

// ── Telegram / Discord config ────────────────────────────

#[test]
async fn telegram_config_serde() {
    let tc = TelegramConfig {
        bot_token: "123:XYZ".into(),
        allowed_users: vec!["alice".into(), "bob".into()],
        stream_mode: StreamMode::Partial,
        draft_update_interval_ms: 500,
        interrupt_on_new_message: true,
        mention_only: false,
        group_reply_mode: None,
    };
    let json = serde_json::to_string(&tc).unwrap();
    let parsed: TelegramConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.bot_token, "123:XYZ");
    assert_eq!(parsed.allowed_users.len(), 2);
    assert_eq!(parsed.stream_mode, StreamMode::Partial);
    assert_eq!(parsed.draft_update_interval_ms, 500);
    assert!(parsed.interrupt_on_new_message);
}

#[test]
async fn telegram_config_defaults_stream_off() {
    let json = r#"{"bot_token":"tok","allowed_users":[]}"#;
    let parsed: TelegramConfig = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.stream_mode, StreamMode::Off);
    assert_eq!(parsed.draft_update_interval_ms, 1000);
    assert!(!parsed.interrupt_on_new_message);
}

#[test]
async fn discord_config_serde() {
    let dc = DiscordConfig {
        bot_token: "discord-token".into(),
        guild_id: Some("12345".into()),
        allowed_users: vec![],
        listen_to_bots: false,
        mention_only: false,
        group_reply_mode: None,
    };
    let json = serde_json::to_string(&dc).unwrap();
    let parsed: DiscordConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.bot_token, "discord-token");
    assert_eq!(parsed.guild_id.as_deref(), Some("12345"));
}

#[test]
async fn discord_config_optional_guild() {
    let dc = DiscordConfig {
        bot_token: "tok".into(),
        guild_id: None,
        allowed_users: vec![],
        listen_to_bots: false,
        mention_only: false,
        group_reply_mode: None,
    };
    let json = serde_json::to_string(&dc).unwrap();
    let parsed: DiscordConfig = serde_json::from_str(&json).unwrap();
    assert!(parsed.guild_id.is_none());
}

// ── iMessage / Matrix config ────────────────────────────

#[test]
async fn imessage_config_serde() {
    let ic = IMessageConfig {
        allowed_contacts: vec!["+1234567890".into(), "user@icloud.com".into()],
        mention_only: false,
    };
    let json = serde_json::to_string(&ic).unwrap();
    let parsed: IMessageConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.allowed_contacts.len(), 2);
    assert_eq!(parsed.allowed_contacts[0], "+1234567890");
}

#[test]
async fn imessage_config_empty_contacts() {
    let ic = IMessageConfig {
        allowed_contacts: vec![],
        mention_only: false,
    };
    let json = serde_json::to_string(&ic).unwrap();
    let parsed: IMessageConfig = serde_json::from_str(&json).unwrap();
    assert!(parsed.allowed_contacts.is_empty(), "expected an empty value");
}

#[test]
async fn imessage_config_wildcard() {
    let ic = IMessageConfig {
        allowed_contacts: vec!["*".into()],
        mention_only: false,
    };
    let toml_str = toml::to_string(&ic).unwrap();
    let parsed: IMessageConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.allowed_contacts, vec!["*"]);
}

#[test]
async fn matrix_config_serde() {
    let mc = MatrixConfig {
        homeserver: "https://matrix.org".into(),
        access_token: "syt_token_abc".into(),
        user_id: Some("@bot:matrix.org".into()),
        device_id: Some("DEVICE123".into()),
        room_id: "!room123:matrix.org".into(),
        allowed_users: vec!["@user:matrix.org".into()],
        mention_only: false,
    };
    let json = serde_json::to_string(&mc).unwrap();
    let parsed: MatrixConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.homeserver, "https://matrix.org");
    assert_eq!(parsed.access_token, "syt_token_abc");
    assert_eq!(parsed.user_id.as_deref(), Some("@bot:matrix.org"));
    assert_eq!(parsed.device_id.as_deref(), Some("DEVICE123"));
    assert_eq!(parsed.room_id, "!room123:matrix.org");
    assert_eq!(parsed.allowed_users.len(), 1);
}

#[test]
async fn matrix_config_toml_roundtrip() {
    let mc = MatrixConfig {
        homeserver: "https://synapse.local:8448".into(),
        access_token: "tok".into(),
        user_id: None,
        device_id: None,
        room_id: "!abc:synapse.local".into(),
        allowed_users: vec!["@admin:synapse.local".into(), "*".into()],
        mention_only: false,
    };
    let toml_str = toml::to_string(&mc).unwrap();
    let parsed: MatrixConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.homeserver, "https://synapse.local:8448");
    assert_eq!(parsed.allowed_users.len(), 2);
}

#[test]
async fn matrix_config_backward_compatible_without_session_hints() {
    let toml = r#"
homeserver = "https://matrix.org"
access_token = "tok"
room_id = "!ops:matrix.org"
allowed_users = ["@ops:matrix.org"]
"#;

    let parsed: MatrixConfig = toml::from_str(toml).unwrap();
    assert_eq!(parsed.homeserver, "https://matrix.org");
    assert!(parsed.user_id.is_none());
    assert!(parsed.device_id.is_none());
}

#[test]
async fn signal_config_serde() {
    let sc = SignalConfig {
        http_url: "http://127.0.0.1:16866".into(),
        account: "+1234567890".into(),
        group_id: Some("group123".into()),
        allowed_from: vec!["+1111111111".into()],
        ignore_attachments: true,
        ignore_stories: false,
        storm_protection: SignalStormProtectionConfig {
            dedupe_ttl_secs: 90,
            min_reply_interval_secs: 3,
            abnormal_threshold: 12,
            abnormal_window_secs: 45,
            breaker_duration_secs: 120,
        },
        ..Default::default()
    };
    let json = serde_json::to_string(&sc).unwrap();
    let parsed: SignalConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.http_url, "http://127.0.0.1:16866");
    assert_eq!(parsed.account, "+1234567890");
    assert_eq!(parsed.group_id.as_deref(), Some("group123"));
    assert_eq!(parsed.allowed_from.len(), 1);
    assert!(parsed.ignore_attachments);
    assert!(!parsed.ignore_stories);
    assert_eq!(parsed.storm_protection.dedupe_ttl_secs, 90);
    assert_eq!(parsed.storm_protection.min_reply_interval_secs, 3);
    assert_eq!(parsed.storm_protection.abnormal_threshold, 12);
    assert_eq!(parsed.storm_protection.abnormal_window_secs, 45);
    assert_eq!(parsed.storm_protection.breaker_duration_secs, 120);
}

#[test]
async fn signal_config_toml_roundtrip() {
    let sc = SignalConfig {
        http_url: "http://localhost:8080".into(),
        account: "+9876543210".into(),
        group_id: None,
        allowed_from: vec!["*".into()],
        ignore_attachments: false,
        ignore_stories: true,
        ..Default::default()
    };
    let toml_str = toml::to_string(&sc).unwrap();
    let parsed: SignalConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.http_url, "http://localhost:8080");
    assert_eq!(parsed.account, "+9876543210");
    assert!(parsed.group_id.is_none());
    assert!(parsed.ignore_stories);
}

#[test]
async fn signal_config_defaults() {
    let json = r#"{"http_url":"http://127.0.0.1:16866","account":"+1234567890"}"#;
    let parsed: SignalConfig = serde_json::from_str(json).unwrap();
    assert!(parsed.group_id.is_none());
    assert!(parsed.allowed_from.is_empty(), "expected an empty value");
    assert!(!parsed.ignore_attachments);
    assert!(!parsed.ignore_stories);
    assert_eq!(parsed.storm_protection.dedupe_ttl_secs, 60);
    assert_eq!(parsed.storm_protection.min_reply_interval_secs, 2);
    assert_eq!(parsed.storm_protection.abnormal_threshold, 10);
    assert_eq!(parsed.storm_protection.abnormal_window_secs, 60);
    assert_eq!(parsed.storm_protection.breaker_duration_secs, 300);
}

#[test]
async fn channels_config_with_imessage_and_matrix() {
    let c = ChannelsConfig {
        cli: true,
        telegram: None,
        discord: None,
        slack: None,
        mattermost: None,
        webhook: None,
        imessage: Some(IMessageConfig {
            allowed_contacts: vec!["+1".into()],
            mention_only: false,
        }),
        matrix: Some(MatrixConfig {
            homeserver: "https://m.org".into(),
            access_token: "tok".into(),
            user_id: None,
            device_id: None,
            room_id: "!r:m".into(),
            allowed_users: vec!["@u:m".into()],
            mention_only: false,
        }),
        signal: None,
        whatsapp: None,
        wacli: None,
        linq: None,
        nextcloud_talk: None,
        email: None,
        irc: None,
        lark: None,
        dingtalk: None,
        qq: None,
        ramflux: None,
    };
    let toml_str = toml::to_string_pretty(&c).unwrap();
    let parsed: ChannelsConfig = toml::from_str(&toml_str).unwrap();
    assert!(parsed.imessage.is_some());
    assert!(parsed.matrix.is_some());
    assert_eq!(parsed.imessage.unwrap().allowed_contacts, vec!["+1"]);
    assert_eq!(parsed.matrix.unwrap().homeserver, "https://m.org");
}

#[test]
async fn channels_config_default_has_no_imessage_matrix() {
    let c = ChannelsConfig::default();
    assert!(c.imessage.is_none());
    assert!(c.matrix.is_none());
}

// ── Edge cases: serde(default) for allowed_users ─────────

#[test]
async fn discord_config_deserializes_without_allowed_users() {
    // Old configs won't have allowed_users — serde(default) should fill vec![]
    let json = r#"{"bot_token":"tok","guild_id":"123"}"#;
    let parsed: DiscordConfig = serde_json::from_str(json).unwrap();
    assert!(parsed.allowed_users.is_empty(), "expected an empty value");
}

#[test]
async fn discord_config_deserializes_with_allowed_users() {
    let json = r#"{"bot_token":"tok","guild_id":"123","allowed_users":["111","222"]}"#;
    let parsed: DiscordConfig = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.allowed_users, vec!["111", "222"]);
}

#[test]
async fn slack_config_deserializes_without_allowed_users() {
    let json = r#"{"bot_token":"xoxb-tok"}"#;
    let parsed: SlackConfig = serde_json::from_str(json).unwrap();
    assert!(parsed.allowed_users.is_empty(), "expected an empty value");
}

#[test]
async fn slack_config_deserializes_with_allowed_users() {
    let json = r#"{"bot_token":"xoxb-tok","allowed_users":["U111"]}"#;
    let parsed: SlackConfig = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.allowed_users, vec!["U111"]);
}

#[test]
async fn discord_config_toml_backward_compat() {
    let toml_str = r#"
bot_token = "tok"
guild_id = "123"
"#;
    let parsed: DiscordConfig = toml::from_str(toml_str).unwrap();
    assert!(parsed.allowed_users.is_empty(), "expected an empty value");
    assert_eq!(parsed.bot_token, "tok");
}

#[test]
async fn slack_config_toml_backward_compat() {
    let toml_str = r#"
bot_token = "xoxb-tok"
channel_id = "C123"
"#;
    let parsed: SlackConfig = toml::from_str(toml_str).unwrap();
    assert!(parsed.allowed_users.is_empty(), "expected an empty value");
    assert_eq!(parsed.channel_id.as_deref(), Some("C123"));
}

// ── WhatsApp config ──────────────────────────────────────

#[test]
async fn whatsapp_config_serde() {
    let wc = WhatsAppConfig {
        access_token: Some("EAABx...".into()),
        phone_number_id: Some("123456789".into()),
        verify_token: Some("my-verify-token".into()),
        app_secret: None,
        session_path: None,
        pair_phone: None,
        pair_code: None,
        allowed_numbers: vec!["+1234567890".into(), "+9876543210".into()],
        dm_policy: DmPolicy::default(),
        group_policy: GroupPolicy::default(),
        group_allow_from: vec![],
        mention_only: false,
        group_reply_mode: None,
        ws_url: None,
        read_pool_size: None,
    };
    let json = serde_json::to_string(&wc).unwrap();
    let parsed: WhatsAppConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.access_token, Some("EAABx...".into()));
    assert_eq!(parsed.phone_number_id, Some("123456789".into()));
    assert_eq!(parsed.verify_token, Some("my-verify-token".into()));
    assert_eq!(parsed.allowed_numbers.len(), 2);
}

#[test]
async fn whatsapp_config_toml_roundtrip() {
    let wc = WhatsAppConfig {
        access_token: Some("tok".into()),
        phone_number_id: Some("12345".into()),
        verify_token: Some("verify".into()),
        app_secret: Some("secret123".into()),
        session_path: None,
        pair_phone: None,
        pair_code: None,
        allowed_numbers: vec!["+1".into()],
        dm_policy: DmPolicy::default(),
        group_policy: GroupPolicy::default(),
        group_allow_from: vec![],
        mention_only: false,
        group_reply_mode: None,
        ws_url: None,
        read_pool_size: None,
    };
    let toml_str = toml::to_string(&wc).unwrap();
    let parsed: WhatsAppConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.phone_number_id, Some("12345".into()));
    assert_eq!(parsed.allowed_numbers, vec!["+1"]);
}

#[test]
async fn whatsapp_config_deserializes_without_allowed_numbers() {
    let json = r#"{"access_token":"tok","phone_number_id":"123","verify_token":"ver"}"#;
    let parsed: WhatsAppConfig = serde_json::from_str(json).unwrap();
    assert!(parsed.allowed_numbers.is_empty(), "expected an empty value");
}

#[test]
async fn whatsapp_config_wildcard_allowed() {
    let wc = WhatsAppConfig {
        access_token: Some("tok".into()),
        phone_number_id: Some("123".into()),
        verify_token: Some("ver".into()),
        app_secret: None,
        session_path: None,
        pair_phone: None,
        pair_code: None,
        allowed_numbers: vec!["*".into()],
        dm_policy: DmPolicy::default(),
        group_policy: GroupPolicy::default(),
        group_allow_from: vec![],
        mention_only: false,
        group_reply_mode: None,
        ws_url: None,
        read_pool_size: None,
    };
    let toml_str = toml::to_string(&wc).unwrap();
    let parsed: WhatsAppConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.allowed_numbers, vec!["*"]);
}

#[test]
async fn whatsapp_config_backend_type_cloud_precedence_when_ambiguous() {
    let wc = WhatsAppConfig {
        access_token: Some("tok".into()),
        phone_number_id: Some("123".into()),
        verify_token: Some("ver".into()),
        app_secret: None,
        session_path: Some("~/.openprx/state/whatsapp-web/session.db".into()),
        pair_phone: None,
        pair_code: None,
        allowed_numbers: vec!["+1".into()],
        dm_policy: DmPolicy::default(),
        group_policy: GroupPolicy::default(),
        group_allow_from: vec![],
        mention_only: false,
        group_reply_mode: None,
        ws_url: None,
        read_pool_size: None,
    };
    assert!(wc.is_ambiguous_config());
    assert_eq!(wc.backend_type(), "cloud");
}

#[test]
async fn whatsapp_config_backend_type_web() {
    let wc = WhatsAppConfig {
        access_token: None,
        phone_number_id: None,
        verify_token: None,
        app_secret: None,
        session_path: Some("~/.openprx/state/whatsapp-web/session.db".into()),
        pair_phone: None,
        pair_code: None,
        allowed_numbers: vec![],
        dm_policy: DmPolicy::default(),
        group_policy: GroupPolicy::default(),
        group_allow_from: vec![],
        mention_only: false,
        group_reply_mode: None,
        ws_url: None,
        read_pool_size: None,
    };
    assert!(!wc.is_ambiguous_config());
    assert_eq!(wc.backend_type(), "web");
}

#[tokio::test]
async fn whatsapp_read_pool_size_zero_is_rejected() {
    let mut config = Config::default();
    assert!(config.validate().is_ok());

    let mut wa = WhatsAppConfig {
        access_token: None,
        phone_number_id: None,
        verify_token: None,
        app_secret: None,
        session_path: Some("~/.openprx/state/whatsapp-web/session.db".into()),
        pair_phone: None,
        pair_code: None,
        allowed_numbers: vec![],
        dm_policy: DmPolicy::default(),
        group_policy: GroupPolicy::default(),
        group_allow_from: vec![],
        mention_only: false,
        group_reply_mode: None,
        ws_url: None,
        read_pool_size: Some(0),
    };
    config.channels_config.whatsapp = Some(wa.clone());
    let err = config.validate().expect_err("test: a zero read pool must be rejected");
    assert!(
        err.to_string().contains("read_pool_size"),
        "test: unexpected error: {err}"
    );

    // Unset means "derive from the machine", which is valid.
    wa.read_pool_size = None;
    config.channels_config.whatsapp = Some(wa.clone());
    assert!(config.validate().is_ok());

    wa.read_pool_size = Some(1);
    config.channels_config.whatsapp = Some(wa);
    assert!(config.validate().is_ok());
}

#[test]
async fn channels_config_with_whatsapp() {
    let c = ChannelsConfig {
        cli: true,
        telegram: None,
        discord: None,
        slack: None,
        mattermost: None,
        webhook: None,
        imessage: None,
        matrix: None,
        signal: None,
        whatsapp: Some(WhatsAppConfig {
            access_token: Some("tok".into()),
            phone_number_id: Some("123".into()),
            verify_token: Some("ver".into()),
            app_secret: None,
            session_path: None,
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["+1".into()],
            dm_policy: DmPolicy::default(),
            group_policy: GroupPolicy::default(),
            group_allow_from: vec![],
            mention_only: false,
            group_reply_mode: None,
            ws_url: None,
            read_pool_size: None,
        }),
        wacli: None,
        linq: None,
        nextcloud_talk: None,
        email: None,
        irc: None,
        lark: None,
        dingtalk: None,
        qq: None,
        ramflux: None,
    };
    let toml_str = toml::to_string_pretty(&c).unwrap();
    let parsed: ChannelsConfig = toml::from_str(&toml_str).unwrap();
    assert!(parsed.whatsapp.is_some());
    let wa = parsed.whatsapp.unwrap();
    assert_eq!(wa.phone_number_id, Some("123".into()));
    assert_eq!(wa.allowed_numbers, vec!["+1"]);
}

#[test]
async fn channels_config_default_has_no_whatsapp() {
    let c = ChannelsConfig::default();
    assert!(c.whatsapp.is_none());
}

#[test]
async fn channels_config_default_has_no_nextcloud_talk() {
    let c = ChannelsConfig::default();
    assert!(c.nextcloud_talk.is_none());
}

#[test]
async fn signal_policy_defaults_and_startup_timeout() {
    let parsed: SignalConfig = toml::from_str(r#"account = "+1234567890""#).unwrap();
    assert_eq!(parsed.startup_timeout_ms, 30_000);
    assert_eq!(parsed.dm_policy, DmPolicy::Allowlist);
    assert_eq!(parsed.group_policy, GroupPolicy::Allowlist);
}

#[test]
async fn signal_dm_policy_unknown_variant_fails_deserialization() {
    let parsed = toml::from_str::<SignalConfig>(
        r#"
account = "+1234567890"
dm_policy = "unknown_policy"
"#,
    );
    assert!(parsed.is_err());
}

#[test]
async fn whatsapp_allowed_from_alias_deserializes_into_allowed_numbers() {
    let parsed: WhatsAppConfig = toml::from_str(
        r#"
access_token = "tok"
phone_number_id = "id"
verify_token = "verify"
allowed_from = ["*"]
"#,
    )
    .unwrap();
    assert_eq!(parsed.allowed_numbers, vec!["*"]);
}

#[test]
async fn lark_config_serde() {
    let lc = LarkConfig {
        app_id: "cli_123456".into(),
        app_secret: "secret_abc".into(),
        encrypt_key: Some("encrypt_key".into()),
        verification_token: Some("verify_token".into()),
        allowed_users: vec!["user_123".into(), "user_456".into()],
        use_feishu: true,
        receive_mode: LarkReceiveMode::Websocket,
        port: None,
        mention_only: false,
    };
    let json = serde_json::to_string(&lc).unwrap();
    let parsed: LarkConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.app_id, "cli_123456");
    assert_eq!(parsed.app_secret, "secret_abc");
    assert_eq!(parsed.encrypt_key.as_deref(), Some("encrypt_key"));
    assert_eq!(parsed.verification_token.as_deref(), Some("verify_token"));
    assert_eq!(parsed.allowed_users.len(), 2);
    assert!(parsed.use_feishu);
}

#[test]
async fn lark_config_toml_roundtrip() {
    let lc = LarkConfig {
        app_id: "cli_123456".into(),
        app_secret: "secret_abc".into(),
        encrypt_key: Some("encrypt_key".into()),
        verification_token: Some("verify_token".into()),
        allowed_users: vec!["*".into()],
        use_feishu: false,
        receive_mode: LarkReceiveMode::Webhook,
        port: Some(9898),
        mention_only: false,
    };
    let toml_str = toml::to_string(&lc).unwrap();
    let parsed: LarkConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.app_id, "cli_123456");
    assert_eq!(parsed.app_secret, "secret_abc");
    assert!(!parsed.use_feishu);
}

/// D-060 L-4: a webhook-mode Lark channel without a verification token
/// used to load and then fail at every listener start (retried with
/// backoff forever). It is refused when the configuration loads, naming
/// the setting; WebSocket mode without a token still loads.
#[test]
async fn a_lark_webhook_without_a_verification_token_fails_config_load() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
            &config_path,
            "default_temperature = 0.7\n\n[channels_config]\ncli = true\n\n[channels_config.lark]\napp_id = \"cli_a\"\napp_secret = \"s\"\n\
             receive_mode = \"webhook\"\nport = 9898\n",
        )
        .unwrap();
    let error = Config::load_from_path(&config_path, dir.path().join("workspace"))
        .expect_err("test: webhook mode without a token must not load");
    assert!(
        format!("{error:#}").contains("channels_config.lark.verification_token"),
        "the error names the setting: {error:#}"
    );

    std::fs::write(
            &config_path,
            "default_temperature = 0.7\n\n[channels_config]\ncli = true\n\n[channels_config.lark]\napp_id = \"cli_a\"\napp_secret = \"s\"\n\
             receive_mode = \"websocket\"\n",
        )
        .unwrap();
    let loaded = Config::load_from_path(&config_path, dir.path().join("workspace"))
        .expect("test: websocket mode needs no token");
    assert!(loaded.channels_config.lark.is_some());

    let mut lark = loaded.channels_config.lark.expect("test: lark section");
    lark.receive_mode = LarkReceiveMode::Webhook;
    lark.verification_token = Some("  ".into());
    assert!(lark.validate().is_err(), "a blank token counts as unset");
    lark.verification_token = Some("tok".into());
    assert!(lark.validate().is_ok());
}

#[test]
async fn lark_config_deserializes_without_optional_fields() {
    let json = r#"{"app_id":"cli_123","app_secret":"secret"}"#;
    let parsed: LarkConfig = serde_json::from_str(json).unwrap();
    assert!(parsed.encrypt_key.is_none());
    assert!(parsed.verification_token.is_none());
    assert!(parsed.allowed_users.is_empty(), "expected an empty value");
    assert!(!parsed.use_feishu);
}

#[test]
async fn lark_config_defaults_to_lark_endpoint() {
    let json = r#"{"app_id":"cli_123","app_secret":"secret"}"#;
    let parsed: LarkConfig = serde_json::from_str(json).unwrap();
    assert!(!parsed.use_feishu, "use_feishu should default to false (Lark)");
}

#[test]
async fn lark_config_with_wildcard_allowed_users() {
    let json = r#"{"app_id":"cli_123","app_secret":"secret","allowed_users":["*"]}"#;
    let parsed: LarkConfig = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.allowed_users, vec!["*"]);
}

#[test]
async fn nextcloud_talk_config_serde() {
    let nc = NextcloudTalkConfig {
        base_url: "https://cloud.example.com".into(),
        app_token: "app-token".into(),
        webhook_secret: Some("webhook-secret".into()),
        allowed_users: vec!["user_a".into(), "*".into()],
        mention_only: false,
    };

    let json = serde_json::to_string(&nc).unwrap();
    let parsed: NextcloudTalkConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.base_url, "https://cloud.example.com");
    assert_eq!(parsed.app_token, "app-token");
    assert_eq!(parsed.webhook_secret.as_deref(), Some("webhook-secret"));
    assert_eq!(parsed.allowed_users, vec!["user_a", "*"]);
}

#[test]
async fn nextcloud_talk_config_defaults_optional_fields() {
    let json = r#"{"base_url":"https://cloud.example.com","app_token":"app-token"}"#;
    let parsed: NextcloudTalkConfig = serde_json::from_str(json).unwrap();
    assert!(parsed.webhook_secret.is_none());
    assert!(parsed.allowed_users.is_empty(), "expected an empty value");
}

#[test]
async fn group_reply_mode_resolve_explicit_wins() {
    assert_eq!(
        GroupReplyMode::resolve(Some(GroupReplyMode::Smart), false),
        GroupReplyMode::Smart
    );
    assert_eq!(
        GroupReplyMode::resolve(Some(GroupReplyMode::Off), true),
        GroupReplyMode::Off
    );
}

#[test]
async fn group_reply_mode_resolve_derives_from_mention_only() {
    // None => derive from legacy mention_only, preserving prior behavior.
    assert_eq!(GroupReplyMode::resolve(None, true), GroupReplyMode::MentionOnly);
    assert_eq!(GroupReplyMode::resolve(None, false), GroupReplyMode::Off);
}

#[test]
async fn group_reply_mode_smart_flags() {
    assert!(GroupReplyMode::Smart.is_smart());
    assert!(!GroupReplyMode::MentionOnly.is_smart());
    assert!(!GroupReplyMode::Off.is_smart());
    // Only smart passes unmentioned messages through the channel layer.
    assert!(!GroupReplyMode::Smart.drops_unmentioned_at_channel_layer());
    assert!(GroupReplyMode::MentionOnly.drops_unmentioned_at_channel_layer());
    assert!(GroupReplyMode::Off.drops_unmentioned_at_channel_layer());
}

#[test]
async fn telegram_config_group_reply_mode_defaults_none() {
    let toml = r#"
            bot_token = "x"
            allowed_users = ["*"]
        "#;
    let cfg: TelegramConfig = toml::from_str(toml).expect("parse telegram config");
    assert_eq!(cfg.group_reply_mode, None);
    assert!(!cfg.mention_only);
}

#[test]
async fn discord_config_group_reply_mode_smart_parses() {
    let toml = r#"
            bot_token = "x"
            group_reply_mode = "smart"
        "#;
    let cfg: DiscordConfig = toml::from_str(toml).expect("parse discord config");
    assert_eq!(cfg.group_reply_mode, Some(GroupReplyMode::Smart));
}

#[test]
async fn wacli_config_defaults_webhook() {
    let cfg = WacliConfig::default();
    assert_eq!(cfg.webhook_listen, "127.0.0.1:16868");
    assert_eq!(cfg.webhook_path, "/wacli");
    assert!(cfg.webhook_secret.is_none());
    assert!(!cfg.allow_unsigned_loopback);
    assert!(!cfg.self_chat_mode);
    assert!(cfg.host.is_none() && cfg.port.is_none());
    assert!(cfg.validate().is_err(), "configured wacli requires authentication");
}

#[test]
async fn wacli_config_parses_official_webhook_toml() {
    let toml = r#"
            webhook_listen = "127.0.0.1:16868"
            webhook_path = "/wacli"
            webhook_secret = "s3cr3t"
            store_dir = "/prx_workspace/.wacli"
            bot_jid = "123@s.whatsapp.net"
            self_chat_mode = true
            group_reply_mode = "smart"
        "#;
    let cfg: WacliConfig = toml::from_str(toml).expect("parse wacli config");
    assert_eq!(cfg.webhook_secret.as_deref(), Some("s3cr3t"));
    assert_eq!(cfg.group_reply_mode, Some(GroupReplyMode::Smart));
    assert!(cfg.self_chat_mode);
    cfg.validate().expect("valid wacli config");
}

#[test]
async fn wacli_self_chat_requires_owner_identity() {
    let cfg = WacliConfig {
        webhook_secret: Some("s3cr3t".into()),
        self_chat_mode: true,
        ..WacliConfig::default()
    };
    let err = cfg.validate().expect_err("self-chat without bot identity must fail");
    assert!(
        err.to_string()
            .contains("self_chat_mode requires bot_jid or bot_number")
    );
}

#[test]
async fn wacli_config_outbound_media_path_policy_defaults_to_unrestricted() {
    assert!(
        !WacliConfig::default().outbound_media_workspace_only,
        "outbound media paths are unrestricted unless the operator opts in"
    );
    let toml = r#"
            webhook_secret = "s3cr3t"
            outbound_media_workspace_only = true
        "#;
    let cfg: WacliConfig = toml::from_str(toml).expect("parse wacli config");
    assert!(cfg.outbound_media_workspace_only);
    cfg.validate().expect("the confinement switch is a valid setting");
}

#[test]
async fn wacli_config_unsigned_loopback_ok() {
    let cfg = WacliConfig {
        webhook_secret: None,
        allow_unsigned_loopback: true,
        webhook_listen: "127.0.0.1:16868".into(),
        ..WacliConfig::default()
    };
    cfg.validate().expect("unsigned loopback is allowed on 127.0.0.1");
}

#[test]
async fn wacli_config_unsigned_non_loopback_rejected() {
    let cfg = WacliConfig {
        webhook_secret: None,
        allow_unsigned_loopback: true,
        webhook_listen: "0.0.0.0:16868".into(),
        ..WacliConfig::default()
    };
    assert!(cfg.validate().is_err(), "unsigned non-loopback must fail");
}

#[test]
async fn wacli_config_rejects_legacy_jsonrpc_fields() {
    // Stale daemon config (host/port) must produce a clear migration error.
    let toml = r#"
            host = "127.0.0.1"
            port = 16867
            webhook_secret = "s"
        "#;
    let cfg: WacliConfig = toml::from_str(toml).expect("parse legacy wacli config");
    let err = cfg.validate().expect_err("legacy host/port must be rejected");
    assert!(
        err.to_string().contains("no longer supported"),
        "error should guide migration: {err}"
    );
}

#[test]
async fn wacli_webhook_path_must_start_with_slash() {
    let cfg = WacliConfig {
        webhook_secret: Some("s3cr3t".into()),
        webhook_path: "wacli".into(), // missing leading slash
        ..WacliConfig::default()
    };
    assert!(cfg.validate().is_err(), "path without leading slash must fail");
}

#[test]
async fn wacli_webhook_path_must_not_be_empty() {
    let cfg = WacliConfig {
        webhook_secret: Some("s3cr3t".into()),
        webhook_path: "".into(),
        ..WacliConfig::default()
    };
    assert!(cfg.validate().is_err(), "empty path must fail");
}

#[test]
async fn wacli_webhook_path_must_not_contain_axum_capture_syntax() {
    for bad_path in &["/wacli/{id}", "/wacli/*rest", "/:foo"] {
        let cfg = WacliConfig {
            webhook_secret: Some("s3cr3t".into()),
            webhook_path: bad_path.to_string(),
            ..WacliConfig::default()
        };
        assert!(cfg.validate().is_err(), "path with axum syntax must fail: {bad_path}");
    }
}

#[test]
async fn wacli_webhook_path_valid_paths_pass() {
    for good_path in &["/wacli", "/wacli/inbound", "/webhook/v2"] {
        let cfg = WacliConfig {
            webhook_secret: Some("s3cr3t".into()),
            webhook_path: good_path.to_string(),
            ..WacliConfig::default()
        };
        cfg.validate()
            .unwrap_or_else(|e| panic!("valid path {good_path} must pass: {e}"));
    }
}
