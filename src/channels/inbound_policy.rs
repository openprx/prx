//! Inbound allow-list, mention and group-reply policy plus the inbound security gate.

use super::{
    ChannelRuntimeContext, ChatKind, Config, HashMap, HashSet, InboundGate, InboundPolicyConfig, Result,
    RuntimeEnvelope, identity, traits,
};

pub(super) fn conversation_memory_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.sender, msg.id)
}

pub(super) fn normalize_allowlist(values: &[String]) -> HashSet<String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn legacy_chat_kind_from_message(msg: &traits::ChannelMessage) -> ChatKind {
    if msg.reply_target.starts_with("group:") || msg.reply_target.ends_with("@g.us") || msg.sender.ends_with("@g.us") {
        ChatKind::Group
    } else {
        ChatKind::Dm
    }
}

pub(super) fn chat_kind_from_message(msg: &traits::ChannelMessage) -> ChatKind {
    if msg.chat_kind == ChatKind::Dm {
        legacy_chat_kind_from_message(msg)
    } else {
        msg.chat_kind
    }
}

pub(super) fn infer_chat_type_from_message(msg: &traits::ChannelMessage) -> &'static str {
    chat_kind_from_message(msg).scope_chat_type()
}

pub(super) fn extract_group_identifier(msg: &traits::ChannelMessage) -> Option<String> {
    if let Some(group_id) = msg.reply_target.strip_prefix("group:") {
        return Some(group_id.to_string());
    }
    if msg.reply_target.ends_with("@g.us") {
        return Some(msg.reply_target.clone());
    }
    if msg.sender.ends_with("@g.us") {
        return Some(msg.sender.clone());
    }
    None
}

fn allowlist_matches(allowlist: &HashSet<String>, value: &str) -> bool {
    allowlist.contains("*") || allowlist.contains(value)
}

pub(super) fn normalize_dm_policy(channel_name: &str, dm_policy: crate::config::DmPolicy) -> crate::config::DmPolicy {
    match dm_policy {
        crate::config::DmPolicy::Pairing => {
            tracing::warn!(
                channel = channel_name,
                "dm_policy=pairing is not implemented for inbound channel policy; falling back to dm_policy=allowlist"
            );
            crate::config::DmPolicy::Allowlist
        }
        other => other,
    }
}

pub(super) fn evaluate_inbound_policy(policy: &InboundPolicyConfig, msg: &traits::ChannelMessage) -> bool {
    match infer_chat_type_from_message(msg) {
        "group" => match policy.group_policy {
            crate::config::GroupPolicy::Disabled => {
                tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Ignoring group message due to disabled group_policy");
                false
            }
            crate::config::GroupPolicy::Open => true,
            crate::config::GroupPolicy::Allowlist => {
                let Some(group_id) = extract_group_identifier(msg) else {
                    tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Dropping group message: missing group identifier for allowlist check");
                    return false;
                };
                if allowlist_matches(&policy.group_allow_from, &group_id) {
                    true
                } else {
                    tracing::warn!(channel = %msg.channel, sender = %msg.sender, group_id = %group_id, "Dropping group message: group not in allowlist");
                    false
                }
            }
        },
        _ => match policy.dm_policy {
            crate::config::DmPolicy::Disabled => {
                tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Ignoring direct message due to disabled dm_policy");
                false
            }
            crate::config::DmPolicy::Open => true,
            crate::config::DmPolicy::Allowlist => {
                if allowlist_matches(&policy.allowed_from, &msg.sender) {
                    true
                } else {
                    tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Dropping direct message: sender not in allowlist");
                    false
                }
            }
            crate::config::DmPolicy::Pairing => {
                tracing::warn!(
                    channel = %msg.channel,
                    sender = %msg.sender,
                    "dm_policy=pairing is not implemented yet; evaluating as dm_policy=allowlist"
                );
                if allowlist_matches(&policy.allowed_from, &msg.sender) {
                    true
                } else {
                    tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Dropping direct message: sender not in allowlist");
                    false
                }
            }
        },
    }
}

pub(super) fn should_process_inbound_message(ctx: &ChannelRuntimeContext, msg: &traits::ChannelMessage) -> bool {
    let policy = match msg.channel.as_str() {
        "signal" => ctx.signal_inbound_policy.as_ref(),
        "whatsapp" => ctx.whatsapp_inbound_policy.as_ref(),
        _ => None,
    };
    let Some(policy) = policy else {
        return true;
    };

    evaluate_inbound_policy(policy, msg)
}

pub(super) fn collect_bot_names(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = vec!["prx".to_string()];

    if let Ok(Some(aieos_identity)) = identity::load_aieos_identity(&config.identity, &config.workspace_dir) {
        if let Some(identity) = aieos_identity.identity {
            if let Some(agent_names) = identity.names {
                if let Some(first) = agent_names.first {
                    names.push(first);
                }
                if let Some(last) = agent_names.last {
                    names.push(last);
                }
                if let Some(nickname) = agent_names.nickname {
                    names.push(nickname);
                }
                if let Some(full) = agent_names.full {
                    names.push(full);
                }
            }
        }
    }

    for binding in &config.identity_bindings {
        if let Some(display_name) = binding.display_name.as_deref() {
            names.push(display_name.to_string());
        }
    }

    if let Some(signal) = config.channels_config.signal.as_ref() {
        names.push(signal.account.clone());
    }

    if let Some(whatsapp) = config.channels_config.whatsapp.as_ref() {
        if let Some(pair_phone) = whatsapp.pair_phone.as_deref() {
            names.push(pair_phone.to_string());
            if !pair_phone.starts_with('+') {
                names.push(format!("+{pair_phone}"));
            }
        }
        if let Some(phone_number_id) = whatsapp.phone_number_id.as_deref() {
            names.push(phone_number_id.to_string());
        }
    }

    if let Some(irc) = config.channels_config.irc.as_ref() {
        names.push(irc.nickname.clone());
    }

    let mut seen = HashSet::new();
    names
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .filter(|name| seen.insert(name.to_ascii_lowercase()))
        .collect()
}

pub(super) fn collect_bot_uuids(config: &Config) -> Vec<String> {
    let mut uuids: Vec<String> = Vec::new();
    if let Some(signal) = config.channels_config.signal.as_ref() {
        let account = signal.account.trim().to_string();
        if !account.is_empty() {
            uuids.push(account);
        }
    }
    uuids
}

pub(super) fn collect_mention_only_by_channel(config: &Config) -> HashMap<String, bool> {
    let mut mention_only = HashMap::new();

    if let Some(telegram) = config.channels_config.telegram.as_ref() {
        mention_only.insert("telegram".to_string(), telegram.mention_only);
    }
    if let Some(discord) = config.channels_config.discord.as_ref() {
        mention_only.insert("discord".to_string(), discord.mention_only);
    }
    if let Some(slack) = config.channels_config.slack.as_ref() {
        mention_only.insert("slack".to_string(), slack.mention_only);
    }
    if let Some(mattermost) = config.channels_config.mattermost.as_ref() {
        mention_only.insert("mattermost".to_string(), mattermost.mention_only.unwrap_or(false));
    }
    if let Some(imessage) = config.channels_config.imessage.as_ref() {
        mention_only.insert("imessage".to_string(), imessage.mention_only);
    }
    if let Some(matrix) = config.channels_config.matrix.as_ref() {
        mention_only.insert("matrix".to_string(), matrix.mention_only);
    }
    if let Some(signal) = config.channels_config.signal.as_ref() {
        mention_only.insert("signal".to_string(), signal.mention_only);
    }
    if let Some(whatsapp) = config.channels_config.whatsapp.as_ref() {
        mention_only.insert("whatsapp".to_string(), whatsapp.mention_only);
    }
    if let Some(wacli) = config.channels_config.wacli.as_ref() {
        mention_only.insert("wacli".to_string(), wacli.mention_only);
    }
    if let Some(linq) = config.channels_config.linq.as_ref() {
        mention_only.insert("linq".to_string(), linq.mention_only);
    }
    if let Some(nextcloud_talk) = config.channels_config.nextcloud_talk.as_ref() {
        mention_only.insert("nextcloud_talk".to_string(), nextcloud_talk.mention_only);
    }
    if let Some(irc) = config.channels_config.irc.as_ref() {
        mention_only.insert("irc".to_string(), irc.mention_only);
    }
    if let Some(lark) = config.channels_config.lark.as_ref() {
        mention_only.insert("lark".to_string(), lark.mention_only);
    }
    if let Some(dingtalk) = config.channels_config.dingtalk.as_ref() {
        mention_only.insert("dingtalk".to_string(), dingtalk.mention_only);
    }
    if let Some(qq) = config.channels_config.qq.as_ref() {
        mention_only.insert("qq".to_string(), qq.mention_only);
    }

    mention_only
}

/// Collect the effective group reply mode for channels that support smart
/// group-reply (currently Telegram, Discord, WhatsApp, and wacli). Other channels are
/// absent and fall back to their `mention_only`-derived behavior unchanged.
pub(super) fn collect_group_reply_mode_by_channel(config: &Config) -> HashMap<String, crate::config::GroupReplyMode> {
    let mut modes = HashMap::new();
    if let Some(telegram) = config.channels_config.telegram.as_ref() {
        modes.insert(
            "telegram".to_string(),
            crate::config::GroupReplyMode::resolve(telegram.group_reply_mode, telegram.mention_only),
        );
    }
    if let Some(discord) = config.channels_config.discord.as_ref() {
        modes.insert(
            "discord".to_string(),
            crate::config::GroupReplyMode::resolve(discord.group_reply_mode, discord.mention_only),
        );
    }
    if let Some(whatsapp) = config.channels_config.whatsapp.as_ref() {
        modes.insert(
            "whatsapp".to_string(),
            crate::config::GroupReplyMode::resolve(whatsapp.group_reply_mode, whatsapp.mention_only),
        );
    }
    if let Some(wacli) = config.channels_config.wacli.as_ref() {
        modes.insert(
            "wacli".to_string(),
            crate::config::GroupReplyMode::resolve(wacli.group_reply_mode, wacli.mention_only),
        );
    }
    modes
}

pub(super) fn is_mention_only_enabled(ctx: &ChannelRuntimeContext, channel_name: &str) -> bool {
    ctx.mention_only_by_channel.get(channel_name).copied().unwrap_or(false)
}

/// Effective group reply mode for a channel (smart group-reply). Defaults to
/// `MentionOnly`-equivalent behavior when the channel is not registered as a
/// smart-capable channel.
pub(super) fn group_reply_mode_for(ctx: &ChannelRuntimeContext, channel_name: &str) -> crate::config::GroupReplyMode {
    ctx.group_reply_mode_by_channel
        .get(channel_name)
        .copied()
        .unwrap_or_else(|| crate::config::GroupReplyMode::resolve(None, is_mention_only_enabled(ctx, channel_name)))
}

pub(super) fn is_bot_mentioned(ctx: &ChannelRuntimeContext, msg: &traits::ChannelMessage, content: &str) -> bool {
    // Check UUID-based mentions first (Signal @mentions)
    for uuid in &ctx.bot_uuids {
        if msg.mentioned_uuids.contains(uuid) {
            return true;
        }
    }
    // Fall back to text-based name matching
    let content_lower = content.to_lowercase();
    for name in &ctx.bot_names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if content_lower.contains(&trimmed.to_lowercase()) {
            return true;
        }
    }
    false
}

/// Strip channel metadata from message content, returning only the user-authored text.
/// This prevents group names or sender IDs embedded in metadata from
/// triggering false-positive mention detection.
///
/// Strips:
/// - Entire lines matching `[xxx-meta ...]` (e.g. `[signal-meta sender=... group=...]`)
/// - Inline prefixes like `[Signal Group: groupname] sender:` at the start of lines
pub(super) fn strip_channel_metadata(content: &str) -> String {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            // Skip entire lines that are metadata tags (e.g. [signal-meta ...])
            if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.contains("-meta ") {
                return None;
            }
            // Strip inline group prefixes like "[Signal Group: name] sender: "
            // These contain group names that could false-positive on bot name matching.
            let cleaned = trimmed.strip_prefix('[').map_or_else(
                || line.to_string(),
                |rest| {
                    rest.find(']').map_or_else(
                        || line.to_string(),
                        |bracket_end| {
                            let tag = &rest[..bracket_end];
                            if tag.contains("Group") || tag.contains("group") {
                                // Skip past the "] sender: " part
                                let after_bracket = &rest[bracket_end + 1..];
                                // Skip "sender_id: " prefix after the bracket
                                after_bracket.find(": ").map_or_else(
                                    || after_bracket.trim_start().to_string(),
                                    |colon_pos| after_bracket[colon_pos + 2..].to_string(),
                                )
                            } else {
                                line.to_string()
                            }
                        },
                    )
                },
            );
            Some(cleaned)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn warn_open_policy_allowlist_alignment(
    channel_name: &str,
    dm_policy: crate::config::DmPolicy,
    group_policy: crate::config::GroupPolicy,
    allowed_from: &HashSet<String>,
    group_allow_from: &HashSet<String>,
) {
    if dm_policy == crate::config::DmPolicy::Open && !allowed_from.contains("*") {
        tracing::warn!(
            channel = channel_name,
            "dm_policy=open configured without '*' in allowed_from; this weakens static safety intent"
        );
    }
    if group_policy == crate::config::GroupPolicy::Open && !group_allow_from.contains("*") {
        tracing::warn!(
            channel = channel_name,
            "group_policy=open configured without '*' in group_allow_from; this weakens static safety intent"
        );
    }
}

/// Per-sender conversation key, carrying both the unified canonical key and the
/// legacy `{channel}_{sender}` key for backward-compatible storage continuity.
///
/// FIX-P1-25b / FIX-P1-02: the canonical key is derived through
/// [`RuntimeEnvelope::canonical_session_key`], the single source-agnostic
/// derivation shared with chat/agent/gateway, so all ingress paths key on one
/// deterministic format instead of each mode inventing its own.
///
/// ## Read-merge (union) over the legacy key, never move
///
/// The channel ingress historically persisted conversation turns (and hydrated
/// the in-memory history map on startup) under the `legacy` key
/// (`{channel}_{sender}`). The canonical key adds a `recipient` component
/// (`channel:{channel}:{sender}:{recipient}`), so a single legacy key can map to
/// *several* canonical keys — one per recipient the sender talked to.
///
/// Two consequences forbid a "move the legacy entry to the canonical key"
/// strategy:
///
/// 1. **History read window.** Once any new turn is written under the canonical
///    key, a move only ever fires when the canonical entry is absent; after the
///    first canonical write the legacy turns would be silently skipped on read,
///    dropping all pre-cutover history.
/// 2. **Recipient dimension mismatch.** The legacy key is inherently
///    sender-scoped (it has no recipient). Moving it into one canonical
///    (recipient-specific) key both misattributes the data to a single
///    recipient and orphans it for every other recipient of the same sender.
///
/// Instead, writes go to the canonical key only, and every *read* path takes the
/// **union of the canonical and legacy histories** (see [`merged_history`]):
/// legacy data is treated as read-only pre-history that is visible to *every*
/// canonical conversation of the same sender. Because the legacy data genuinely
/// predates the recipient distinction, surfacing it to each recipient is the
/// faithful behaviour — no turn is lost, none is misrouted, and the legacy key
/// is never written, moved, or deleted from durable storage.
pub(super) struct ConversationKey {
    pub(super) canonical: String,
    pub(super) legacy: String,
}

impl ConversationKey {
    pub(super) fn from_message(msg: &traits::ChannelMessage) -> Self {
        // The recipient component mirrors RuntimeEnvelope::channel's recipient
        // (the chat_id / reply_target), so a channel turn and its envelope agree
        // on the canonical key.
        let canonical = RuntimeEnvelope::channel(
            String::new(),
            msg.channel.clone(),
            msg.sender.clone(),
            Some(msg.reply_target.clone()),
        )
        .canonical_session_key();
        Self {
            canonical,
            legacy: format!("{}_{}", msg.channel, msg.sender),
        }
    }
}

/// Resolve the inbound gate for this message. Production always uses the
/// static-dispatch `InboundGate::for_policy` over the real `SideEffectGate`. In
/// test builds, an injected `test_inbound_authorizer` (when present) takes
/// precedence so tests can express an operation-selective deny the real policy
/// cannot. The three `authorize_channel_*` helpers below funnel through this so
/// the operation-naming convention lives only in `InboundGate`.
#[cfg(not(test))]
pub(super) fn authorize_channel_inbound(
    _ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
    sender: &str,
) -> Result<(), String> {
    InboundGate::for_policy(security).authorize_inbound(channel, sender)
}

#[cfg(test)]
pub(super) fn authorize_channel_inbound(
    ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
    sender: &str,
) -> Result<(), String> {
    ctx.test_inbound_authorizer.as_ref().map_or_else(
        || InboundGate::for_policy(security).authorize_inbound(channel, sender),
        |authorizer| InboundGate::new(authorizer.as_ref()).authorize_inbound(channel, sender),
    )
}

#[cfg(not(test))]
pub(super) fn authorize_channel_autosave(
    _ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
) -> Result<(), String> {
    InboundGate::for_policy(security).authorize_autosave(channel)
}

#[cfg(test)]
pub(super) fn authorize_channel_autosave(
    ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
) -> Result<(), String> {
    ctx.test_inbound_authorizer.as_ref().map_or_else(
        || InboundGate::for_policy(security).authorize_autosave(channel),
        |authorizer| InboundGate::new(authorizer.as_ref()).authorize_autosave(channel),
    )
}

#[cfg(not(test))]
pub(super) fn authorize_channel_outbound(
    _ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
) -> Result<(), String> {
    InboundGate::for_policy(security).authorize_outbound(channel)
}

#[cfg(test)]
pub(super) fn authorize_channel_outbound(
    ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
) -> Result<(), String> {
    ctx.test_inbound_authorizer.as_ref().map_or_else(
        || InboundGate::for_policy(security).authorize_outbound(channel),
        |authorizer| InboundGate::new(authorizer.as_ref()).authorize_outbound(channel),
    )
}
