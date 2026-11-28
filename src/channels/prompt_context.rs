//! System prompt and conversation prompt assembly for channel turns.

use super::inbound_policy::chat_kind_from_message;
use super::{ChatKind, ChatProfile, MemoryVisibility, Write, traits};

pub(super) fn channel_message_visibility(msg: &traits::ChannelMessage) -> MemoryVisibility {
    if chat_kind_from_message(msg).is_group_like() {
        MemoryVisibility::Session
    } else {
        MemoryVisibility::Workspace
    }
}

pub(super) fn to_rfc3339_timestamp(raw_timestamp: u64) -> Option<String> {
    if raw_timestamp == 0 {
        return None;
    }

    let raw_timestamp = i64::try_from(raw_timestamp).ok()?;
    let timestamp = if raw_timestamp > 10_000_000_000 {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(raw_timestamp)
    } else {
        chrono::DateTime::<chrono::Utc>::from_timestamp(raw_timestamp, 0)
    }?;
    Some(timestamp.to_rfc3339())
}

pub(super) fn interruption_scope_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.reply_target, msg.sender)
}

fn channel_delivery_instructions(channel_name: &str) -> Option<&'static str> {
    match channel_name {
        "telegram" => Some(
            "When responding on Telegram, include media markers for files or URLs that should be sent as attachments. Use one marker per attachment with this exact syntax: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], [VIDEO:<path-or-url>], [AUDIO:<path-or-url>], or [VOICE:<path-or-url>]. Keep normal user-facing text outside markers and never wrap markers in code fences.",
        ),
        _ => None,
    }
}

fn channel_runtime_instructions(channel_name: &str) -> String {
    let mut instructions = String::from(
        "## Channel Capabilities\n\n\
         - You are running as a messaging bot. Your response is automatically sent back to the user's channel.\n\
         - You do NOT need to ask permission to respond; just respond directly.\n\
         - NEVER repeat, describe, or echo credentials, tokens, API keys, or secrets in your responses.\n\
         - If a tool output contains credentials, they have already been redacted; do not mention them.\n",
    );
    if let Some(delivery) = channel_delivery_instructions(channel_name) {
        instructions.push('\n');
        instructions.push_str(delivery);
    }
    instructions
}

/// System-prompt addendum for smart group-reply turns. Overrides the default
/// "respond directly" guidance: tells the model it is one participant in a group
/// and that staying silent (via `stay_silent`) is often the correct choice.
///
/// When the bot was explicitly @-mentioned, it should normally answer; otherwise
/// it should only speak when it can clearly add value.
pub(super) const fn smart_group_prompt_addendum(mentioned: bool) -> &'static str {
    if mentioned {
        "\n## Group Chat Mode (smart)\n\n\
         - You are ONE participant in a group conversation. This message addresses you directly.\n\
         - Reply normally — but stay concise and on-topic; do not dominate the conversation.\n\
         - If a reply truly is not warranted, you may call the `stay_silent` tool with a short reason \
           instead of sending a message.\n"
    } else {
        "\n## Group Chat Mode (smart)\n\n\
         - You are ONE participant in a group conversation. This message was NOT addressed to you.\n\
         - Only speak when you can clearly add value (answer a question aimed at the group, correct a \
           critical error, or provide help you are uniquely able to give).\n\
         - For small talk, chatter between other people, or anything not relevant to you, call the \
           `stay_silent` tool with a short reason. Staying silent is the DEFAULT and correct choice for \
           most messages — do not feel obliged to reply.\n\
         - Never reply just to acknowledge; silence is better than noise.\n"
    }
}

fn prompt_trim(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut truncated = false;
    for (idx, ch) in value.trim().chars().enumerate() {
        if idx >= max_chars {
            truncated = true;
            break;
        }
        out.push(ch);
    }
    if truncated {
        out.push_str("...");
    }
    out
}

const fn display_chat_kind(kind: ChatKind) -> &'static str {
    match kind {
        ChatKind::Dm => "dm",
        ChatKind::Group => "group",
        ChatKind::Thread => "thread",
    }
}

pub(super) fn build_current_conversation_prompt(
    msg: &traits::ChannelMessage,
    profile: Option<&ChatProfile>,
    bot_identity: Option<&str>,
) -> String {
    let kind = profile
        .map(|profile| profile.chat_kind.as_str())
        .map(|value| match value {
            "group" => "group",
            "thread" => "thread",
            _ => "dm",
        })
        .unwrap_or_else(|| display_chat_kind(chat_kind_from_message(msg)));
    let title = msg
        .chat_title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| profile.and_then(|profile| profile.title.as_deref()))
        .unwrap_or("untitled");
    let you = bot_identity.unwrap_or(msg.channel.as_str());
    let mut block = format!(
        "## Current Conversation\n- Platform: {} | You: {}\n- Type: {} | Chat: \"{}\" ({})",
        msg.channel,
        prompt_trim(you, 48),
        kind,
        prompt_trim(title, 80),
        prompt_trim(&msg.reply_target, 96)
    );
    if let Some(profile) = profile {
        if let Some(purpose) = profile.purpose.as_deref().filter(|value| !value.trim().is_empty()) {
            let _ = write!(block, "\n- Purpose (self-maintained): {}", prompt_trim(purpose, 180));
        }
        let notes = profile.notes.as_deref().filter(|value| !value.trim().is_empty());
        let tags = if profile.tags.is_empty() {
            None
        } else {
            Some(profile.tags.join(", "))
        };
        if notes.is_some() || tags.is_some() {
            let _ = write!(
                block,
                "\n- Notes: {} | Tags: {}",
                notes
                    .map(|value| prompt_trim(value, 240))
                    .unwrap_or_else(|| "none".to_string()),
                tags.as_deref()
                    .map(|value| prompt_trim(value, 120))
                    .unwrap_or_else(|| "none".to_string())
            );
        }
    }
    block.push_str("\n- When you learn what this chat is for, use the chat_profile_update tool (not memory_store).");
    block
}

/// The channel's instructions and the current-conversation block, compiled by
/// the prompt layout as one custom section after the standard sections.
struct ChannelInstructionsSection {
    text: String,
}

impl crate::agent::prompt::PromptSection for ChannelInstructionsSection {
    fn name(&self) -> &str {
        "channel_instructions"
    }

    fn build(&self, _ctx: &crate::agent::prompt::PromptContext<'_>) -> anyhow::Result<String> {
        Ok(self.text.clone())
    }
}

/// The builder of a channel turn: the standard sections, then the channel
/// instructions and the current conversation of `msg`.
pub(super) fn channel_prompt_builder(
    msg: &traits::ChannelMessage,
    profile: Option<&ChatProfile>,
    bot_identity: Option<&str>,
) -> crate::agent::prompt::SystemPromptBuilder {
    let instructions = channel_runtime_instructions(&msg.channel);
    let current_conversation = build_current_conversation_prompt(msg, profile, bot_identity);
    crate::agent::prompt::SystemPromptBuilder::with_defaults().add_section(Box::new(ChannelInstructionsSection {
        text: format!("{instructions}\n\n{current_conversation}"),
    }))
}

/// Load workspace identity files and build a system prompt.
///
/// Follows the `OpenClaw` framework structure by default. The sections come in
/// the order of `crate::context::layout::Layout::standard`:
/// 1. Task-mode guidance
/// 2. Context handoff — how to look up earlier turns moved out of the window
/// 3. Safety — guardrail reminder
/// 4. Orchestration
/// 5. Skills — lazy catalog metadata; instructions are read with `skill_read`
/// 6. Workspace — working directory
/// 7. Identity — workspace files SOUL, AGENTS, IDENTITY, USER, TOOLS, MEMORY,
///    THINKING, then BOOTSTRAP while it is pending
/// 8. Date & Time — timezone for cache stability
/// 9. Runtime — host, OS, model
///
/// When `identity_config` is set to AIEOS format, the bootstrap files section
/// is replaced with the AIEOS identity data loaded from file or inline JSON.
///
/// Daily memory files (`memory/*.md`) are NOT injected — they are accessed
/// on-demand via `memory_recall` / `memory_search` / `memory_get` tools.
pub fn build_system_prompt(
    workspace_dir: &std::path::Path,
    model_name: &str,
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
    autonomy: crate::security::AutonomyLevel,
) -> String {
    build_system_prompt_with_mode(
        workspace_dir,
        model_name,
        skills,
        identity_config,
        bootstrap_max_chars,
        false,
        autonomy,
    )
}

pub fn build_system_prompt_with_mode(
    workspace_dir: &std::path::Path,
    model_name: &str,
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
    native_tools: bool,
    autonomy: crate::security::AutonomyLevel,
) -> String {
    build_system_prompt_with_builder(
        crate::agent::prompt::SystemPromptBuilder::with_defaults(),
        workspace_dir,
        model_name,
        skills,
        identity_config,
        bootstrap_max_chars,
        native_tools,
        autonomy,
    )
}

/// [`build_system_prompt_with_mode`] compiled by `builder`: the canonical
/// sections, plus any section a caller's session adds to them.
pub(crate) fn build_system_prompt_with_builder(
    builder: crate::agent::prompt::SystemPromptBuilder,
    workspace_dir: &std::path::Path,
    model_name: &str,
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
    native_tools: bool,
    autonomy: crate::security::AutonomyLevel,
) -> String {
    let context = crate::agent::prompt::PromptContext {
        workspace_dir,
        model_name,
        skills,
        identity_config,
        bootstrap_max_chars,
        native_tools,
        autonomy,
    };
    builder.build(&context).unwrap_or_else(|error| {
        tracing::error!(%error, "failed to build canonical system prompt");
        "You are OpenPRX, a fast and efficient AI assistant built in Rust. Be helpful, concise, and direct.".to_string()
    })
}
