#![cfg(test)]
use super::*;

#[test]
fn channel_configuration_statuses_include_wacli() {
    let mut config = Config::default();
    config.channels_config.wacli = Some(crate::config::WacliConfig::default());

    let statuses = channel_configuration_statuses(&config);

    assert_eq!(
        statuses.iter().find(|(name, _)| *name == "Wacli"),
        Some(&("Wacli", true))
    );
}

fn smart_test_msg(sender: &str, reply_target: &str) -> traits::ChannelMessage {
    traits::ChannelMessage {
        sender: sender.to_string(),
        reply_target: reply_target.to_string(),
        channel: "telegram".to_string(),
        ..Default::default()
    }
}

/// The busiest registry row in the runtime must not be where an address leaks.
///
/// One of these labels is minted per inbound message, and
/// `GET /api/runtime/tasks` publishes it verbatim to any caller the pairing
/// check admits — which, with pairing off, is every process on the box. The
/// send path has always fingerprinted the correspondent; a turn row that
/// spelled the same person out would have made the redaction decorative.
///
/// Compared against `ref_for_channel_recipient`'s own output rather than a
/// literal digest: a second hand-rolled shortening that merely *looked*
/// redacted is exactly how the two sides drifted apart before, so the test
/// asserts they share the function, not that they happen to agree today.
///
/// MUTATION GUARD: interpolate `msg.sender` or `msg.reply_target` into
/// `channel_turn_label` and this fails.
#[test]
fn a_channel_turn_label_fingerprints_the_correspondent() {
    let sender = "+15550001111@s.whatsapp.net";
    let group = "120363000000000000@g.us";
    let msg = traits::ChannelMessage {
        sender: sender.to_string(),
        reply_target: group.to_string(),
        channel: "wacli".to_string(),
        ..Default::default()
    };
    let label = channel_turn_label(&msg);

    assert!(
        !label.contains(sender),
        "the sender must not appear in plaintext: {label}"
    );
    assert!(!label.contains(group), "the chat must not appear in plaintext: {label}");
    // The channel is the part that says *where*, and it is not sensitive.
    assert!(
        label.contains("wacli"),
        "the label must still name the channel: {label}"
    );
    // Same function as the outbound path, so one person reads the same in
    // an inbound turn row and in a send row.
    assert!(
        label.contains(&crate::security::op_id::ref_for_channel_recipient("wacli", sender)),
        "the sender must appear as the shared fingerprint: {label}"
    );
    assert!(
        label.contains(&crate::security::op_id::ref_for_channel_recipient("wacli", group)),
        "the chat must appear as the shared fingerprint: {label}"
    );
    // Two correspondents on one channel still tell apart.
    let other = traits::ChannelMessage {
        sender: "+15550002222@s.whatsapp.net".to_string(),
        reply_target: group.to_string(),
        channel: "wacli".to_string(),
        ..Default::default()
    };
    assert_ne!(label, channel_turn_label(&other));
}

#[test]
fn is_bot_sender_detects_bot_naming() {
    assert!(is_bot_sender(&smart_test_msg("helperbot", "g1")));
    assert!(is_bot_sender(&smart_test_msg("news-bot", "g1")));
    assert!(!is_bot_sender(&smart_test_msg("alice", "g1")));
    assert!(!is_bot_sender(&smart_test_msg("", "g1")));
}

#[test]
fn is_bot_sender_trusts_platform_flag_over_name() {
    // Authoritative platform flag wins even when the name looks human — a bot
    // that does NOT name itself "*bot" is still caught (anti bot-to-bot loop).
    let mut msg = smart_test_msg("alice", "g1");
    msg.sender_is_bot = true;
    assert!(
        is_bot_sender(&msg),
        "sender_is_bot=true must mark a human-named account as a bot"
    );

    // Flag false + human name => not a bot (fallback heuristic does not match).
    let human = smart_test_msg("alice", "g1");
    assert!(!is_bot_sender(&human));
}

#[test]
fn smart_proactive_cooldown_blocks_then_record_extends() {
    let ctx_cooldown: Arc<parking_lot::Mutex<HashMap<String, std::time::Instant>>> =
        Arc::new(parking_lot::Mutex::new(HashMap::new()));
    // Build a minimal ctx-like surface is heavy; instead test the pure key +
    // map semantics that the helpers rely on.
    let msg = smart_test_msg("alice", "group:teamchat");
    let key = smart_cooldown_key(&msg);
    assert_eq!(key, "telegram:teamchat");
    // Empty map => not within cooldown.
    {
        let guard = ctx_cooldown.lock();
        assert!(guard.get(&key).is_none());
    }
    // After inserting "now", a fresh lookup is within the window.
    {
        let mut guard = ctx_cooldown.lock();
        guard.insert(key.clone(), std::time::Instant::now());
    }
    {
        let guard = ctx_cooldown.lock();
        let within = guard.get(&key).is_some_and(|last| {
            std::time::Instant::now().duration_since(*last)
                < std::time::Duration::from_secs(SMART_PROACTIVE_COOLDOWN_SECS)
        });
        assert!(within, "just-recorded proactive reply must be within cooldown");
    }
}

#[test]
fn collect_group_reply_mode_resolves_smart_capable_channels() {
    // The collector only registers smart-capable channels (telegram/discord/
    // whatsapp) and resolves their effective mode from explicit + mention_only.
    let mut config = Config::default();
    config.channels_config.telegram = Some(crate::config::TelegramConfig {
        bot_token: "t".into(),
        allowed_users: vec!["*".into()],
        stream_mode: crate::config::StreamMode::default(),
        draft_update_interval_ms: 1000,
        interrupt_on_new_message: false,
        mention_only: true,
        group_reply_mode: Some(crate::config::GroupReplyMode::Smart),
    });
    config.channels_config.discord = Some(crate::config::DiscordConfig {
        bot_token: "d".into(),
        guild_id: None,
        allowed_users: vec!["*".into()],
        listen_to_bots: false,
        mention_only: true,
        group_reply_mode: None, // derive => MentionOnly
    });
    config.channels_config.whatsapp = Some(crate::config::WhatsAppConfig {
        allowed_numbers: vec!["*".into()],
        group_reply_mode: Some(crate::config::GroupReplyMode::Smart),
        ..Default::default()
    });
    config.channels_config.wacli = Some(crate::config::WacliConfig {
        group_reply_mode: Some(crate::config::GroupReplyMode::Smart),
        bot_jid: Some("123@s.whatsapp.net".into()),
        ..Default::default()
    });
    let modes = collect_group_reply_mode_by_channel(&config);
    assert_eq!(modes.get("telegram"), Some(&crate::config::GroupReplyMode::Smart));
    assert_eq!(modes.get("discord"), Some(&crate::config::GroupReplyMode::MentionOnly));
    assert_eq!(modes.get("whatsapp"), Some(&crate::config::GroupReplyMode::Smart));
    assert_eq!(modes.get("wacli"), Some(&crate::config::GroupReplyMode::Smart));
    assert!(
        !modes.contains_key("slack"),
        "non-smart-capable channels are not registered"
    );
}

/// WhatsApp without an explicit `group_reply_mode` derives the legacy
/// `mention_only`-based mode, so existing configs are byte-for-byte unchanged.
#[test]
fn collect_group_reply_mode_whatsapp_derives_from_mention_only() {
    let mut config = Config::default();
    config.channels_config.whatsapp = Some(crate::config::WhatsAppConfig {
        allowed_numbers: vec!["*".into()],
        mention_only: true,
        group_reply_mode: None,
        ..Default::default()
    });
    let modes = collect_group_reply_mode_by_channel(&config);
    assert_eq!(
        modes.get("whatsapp"),
        Some(&crate::config::GroupReplyMode::MentionOnly),
        "mention_only=true + None => MentionOnly (legacy behavior preserved)"
    );

    config.channels_config.whatsapp = Some(crate::config::WhatsAppConfig {
        allowed_numbers: vec!["*".into()],
        mention_only: false,
        group_reply_mode: None,
        ..Default::default()
    });
    let modes = collect_group_reply_mode_by_channel(&config);
    assert_eq!(
        modes.get("whatsapp"),
        Some(&crate::config::GroupReplyMode::Off),
        "mention_only=false + None => Off (legacy behavior preserved)"
    );
}

/// Mirror of the central smart-gate decision for a WhatsApp `@g.us` group
/// message: with whatsapp registered as Smart, `group_reply_mode_for` yields
/// Smart and the `@g.us` reply_target makes it a group, so `smart_group` is
/// true and the bot reads every message (model decides via stay_silent).
#[test]
fn whatsapp_group_message_triggers_smart_gate() {
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.group_reply_mode_by_channel
        .insert("whatsapp".to_string(), crate::config::GroupReplyMode::Smart);

    let msg = traits::ChannelMessage {
        channel: "whatsapp".to_string(),
        sender: "+123".to_string(),
        reply_target: "120363000000000000@g.us".to_string(),
        content: "anyone around?".to_string(),
        ..Default::default()
    };

    let mode = group_reply_mode_for(&ctx, &msg.channel);
    assert!(mode.is_smart(), "whatsapp resolves to Smart mode");

    // Central group detection: `@g.us` reply_target => group.
    let is_group = msg.reply_target.starts_with("group:") || msg.reply_target.ends_with("@g.us");
    assert!(is_group, "@g.us reply_target is recognized as a group");

    let in_group_for_smart = is_group || msg.is_group_hint;
    let smart_group = mode.is_smart() && in_group_for_smart && !is_system_message(&msg);
    assert!(smart_group, "smart gate is active for whatsapp @g.us group");
}

/// "@ always answers": an explicit @-mention in a WhatsApp group is detected
/// by the central mention path (here via the channel-layer `mentioned` hint
/// that WhatsApp Web sets from `context_info.mentioned_jid`), bypassing the
/// proactive pre-gate/cooldown.
#[test]
fn whatsapp_group_at_mention_is_detected() {
    let ctx = ctx_with_histories(HashMap::new());
    let msg = traits::ChannelMessage {
        channel: "whatsapp".to_string(),
        sender: "+123".to_string(),
        reply_target: "120363000000000000@g.us".to_string(),
        content: "@bot please summarize".to_string(),
        mentioned: true, // set by whatsapp_web from mentioned_jid
        is_group_hint: true,
        ..Default::default()
    };
    let user_text = strip_channel_metadata(&msg.content);
    let mentioned = msg.mentioned || is_bot_mentioned(&ctx, &msg, &user_text);
    assert!(mentioned, "channel-layer @-mention hint marks the bot as addressed");

    // Text-based fallback also works when the body names the bot.
    let msg2 = traits::ChannelMessage {
        channel: "whatsapp".to_string(),
        reply_target: "120363000000000000@g.us".to_string(),
        content: "hey prx what's up".to_string(),
        ..Default::default()
    };
    let user_text2 = strip_channel_metadata(&msg2.content);
    assert!(
        msg2.mentioned || is_bot_mentioned(&ctx, &msg2, &user_text2),
        "text bot-name fallback still detects mention (bot_names = [\"prx\"])"
    );
}

/// Non-smart WhatsApp behavior is unchanged: when whatsapp is absent from the
/// smart map (or mention_only-derived), `group_reply_mode_for` is NOT smart,
/// so `smart_group` is false and the legacy mention_only drop path governs.
#[test]
fn whatsapp_non_smart_behavior_unchanged() {
    // No whatsapp entry at all => fall back to mention_only-derived mode.
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.mention_only_by_channel.insert("whatsapp".to_string(), true);
    let mode = group_reply_mode_for(&ctx, "whatsapp");
    assert_eq!(
        mode,
        crate::config::GroupReplyMode::MentionOnly,
        "absent + mention_only=true => MentionOnly, not Smart"
    );
    assert!(!mode.is_smart(), "non-smart whatsapp never enters the smart gate");

    // Explicit MentionOnly registration is likewise not smart.
    ctx.group_reply_mode_by_channel
        .insert("whatsapp".to_string(), crate::config::GroupReplyMode::MentionOnly);
    assert!(!group_reply_mode_for(&ctx, "whatsapp").is_smart());
}

/// A WhatsApp DM (no `@g.us`, no group hint) can never be smart, so the bot
/// is never silenced and `stay_silent` is never exposed — even when whatsapp
/// is configured Smart.
#[test]
fn whatsapp_dm_is_never_smart_silent() {
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.group_reply_mode_by_channel
        .insert("whatsapp".to_string(), crate::config::GroupReplyMode::Smart);
    let msg = traits::ChannelMessage {
        channel: "whatsapp".to_string(),
        sender: "+123".to_string(),
        reply_target: "+123".to_string(), // DM: a bare number, not @g.us
        content: "hi".to_string(),
        ..Default::default()
    };
    let is_group = msg.reply_target.starts_with("group:") || msg.reply_target.ends_with("@g.us");
    let in_group_for_smart = is_group || msg.is_group_hint;
    let mode = group_reply_mode_for(&ctx, &msg.channel);
    let smart_group = mode.is_smart() && in_group_for_smart && !is_system_message(&msg);
    assert!(!smart_group, "DMs are never smart => never silenced");
}

/// Signal (also `@g.us`-agnostic but using its own paths) is unaffected by
/// extending smart to whatsapp: registering whatsapp Smart leaves signal's
/// effective mode entirely mention_only-derived.
#[test]
fn signal_unaffected_by_whatsapp_smart() {
    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.group_reply_mode_by_channel
        .insert("whatsapp".to_string(), crate::config::GroupReplyMode::Smart);
    ctx.mention_only_by_channel.insert("signal".to_string(), true);
    // signal has no smart registration => derived from mention_only.
    let signal_mode = group_reply_mode_for(&ctx, "signal");
    assert_eq!(
        signal_mode,
        crate::config::GroupReplyMode::MentionOnly,
        "signal stays mention_only-derived; whatsapp smart does not bleed over"
    );
    assert!(!signal_mode.is_smart(), "signal is never smart here");
}

#[test]
fn inbound_policy_allowlist_rejects_unknown_direct_sender() {
    let policy = InboundPolicyConfig {
        dm_policy: crate::config::DmPolicy::Allowlist,
        group_policy: crate::config::GroupPolicy::Allowlist,
        allowed_from: normalize_allowlist(&["+1111111111".to_string()]),
        group_allow_from: HashSet::new(),
    };
    let msg = traits::ChannelMessage {
        id: "1".to_string(),
        sender: "+2222222222".to_string(),
        reply_target: "+2222222222".to_string(),
        content: "hello".to_string(),
        channel: "signal".to_string(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    assert!(!evaluate_inbound_policy(&policy, &msg));
}

#[test]
fn inbound_policy_open_with_wildcard_allowlist_allows_direct_sender() {
    let policy = InboundPolicyConfig {
        dm_policy: crate::config::DmPolicy::Open,
        group_policy: crate::config::GroupPolicy::Allowlist,
        allowed_from: normalize_allowlist(&["*".to_string()]),
        group_allow_from: HashSet::new(),
    };
    let msg = traits::ChannelMessage {
        id: "dm-open".to_string(),
        sender: "+19999999999".to_string(),
        reply_target: "+19999999999".to_string(),
        content: "hello".to_string(),
        channel: "signal".to_string(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    assert!(evaluate_inbound_policy(&policy, &msg));
}

#[test]
fn structured_chat_kind_drives_group_scope_for_bare_telegram_targets() {
    let msg = traits::ChannelMessage {
        id: "tg-group".to_string(),
        sender: "alice".to_string(),
        reply_target: "-100200300".to_string(),
        content: "[Telegram Group] Alice: group update".to_string(),
        channel: "telegram".to_string(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: ChatKind::Group,
        chat_title: Some("Build Room".to_string()),
        sender_display: Some("Alice".to_string()),
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: true,
        sender_is_bot: false,
    };

    assert_eq!(infer_chat_type_from_message(&msg), "group");
    assert_eq!(channel_message_visibility(&msg), MemoryVisibility::Session);
}

#[test]
fn inbound_policy_group_disabled_rejects_group_message() {
    let policy = InboundPolicyConfig {
        dm_policy: crate::config::DmPolicy::Open,
        group_policy: crate::config::GroupPolicy::Disabled,
        allowed_from: normalize_allowlist(&["*".to_string()]),
        group_allow_from: normalize_allowlist(&["group-1".to_string()]),
    };
    let msg = traits::ChannelMessage {
        id: "grp-disabled".to_string(),
        sender: "+1111111111".to_string(),
        reply_target: "group:group-1".to_string(),
        content: "group hello".to_string(),
        channel: "signal".to_string(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    assert!(!evaluate_inbound_policy(&policy, &msg));
}

#[test]
fn inbound_policy_pairing_follows_allowlist_behavior() {
    let policy = InboundPolicyConfig {
        dm_policy: crate::config::DmPolicy::Pairing,
        group_policy: crate::config::GroupPolicy::Allowlist,
        allowed_from: normalize_allowlist(&["+10000000000".to_string()]),
        group_allow_from: HashSet::new(),
    };
    let msg = traits::ChannelMessage {
        id: "dm-pairing".to_string(),
        sender: "+10000000000".to_string(),
        reply_target: "+10000000000".to_string(),
        content: "hello".to_string(),
        channel: "signal".to_string(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    assert!(evaluate_inbound_policy(&policy, &msg));

    let blocked = traits::ChannelMessage {
        sender: "+20000000000".to_string(),
        ..msg
    };
    assert!(!evaluate_inbound_policy(&policy, &blocked));
}

#[test]
fn normalize_dm_policy_falls_back_pairing_to_allowlist() {
    assert_eq!(
        normalize_dm_policy("signal", crate::config::DmPolicy::Pairing),
        crate::config::DmPolicy::Allowlist
    );
    assert_eq!(
        normalize_dm_policy("signal", crate::config::DmPolicy::Open),
        crate::config::DmPolicy::Open
    );
}

#[test]
fn inbound_policy_group_allowlist_accepts_allowed_group() {
    let policy = InboundPolicyConfig {
        dm_policy: crate::config::DmPolicy::Allowlist,
        group_policy: crate::config::GroupPolicy::Allowlist,
        allowed_from: HashSet::new(),
        group_allow_from: normalize_allowlist(&["group-1".to_string()]),
    };
    let msg = traits::ChannelMessage {
        id: "2".to_string(),
        sender: "+1111111111".to_string(),
        reply_target: "group:group-1".to_string(),
        content: "group hello".to_string(),
        channel: "signal".to_string(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    assert!(evaluate_inbound_policy(&policy, &msg));
}
