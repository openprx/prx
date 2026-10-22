use super::{AutonomyLevel, Deserialize, JsonSchema, Result, Serialize};

// ── Autonomy / Security ──────────────────────────────────────────

/// A single scope rule for tool access control.
///
/// A rule matches when ALL specified criteria match (logical AND).
/// Deny always takes priority over allow within a rule.
///
/// The two halves of a rule are evaluated differently, on purpose:
/// * `tools_allow` / `tools_deny` are **first-match-wins** — the first matching
///   rule decides tool access and later rules are not consulted.
/// * `send_allow` / `send_deny` are evaluated over **every** matching rule that
///   carries at least one outbound entry, so a rule written for some unrelated
///   reason (a `tools_deny`-only rule, say) can never silently swallow a later
///   rule's `send_deny`. See `SecurityPolicy::is_outbound_allowed`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ScopeRule {
    /// Match by sender identity: UUID (e.g. "uuid:xxx"), phone number, or "*" for any sender.
    pub user: Option<String>,
    /// Match by channel name: "signal", "telegram", "discord", etc.
    pub channel: Option<String>,
    /// Match by chat type: "direct", "group", or "*" for any type.
    pub chat_type: Option<String>,
    /// Tool whitelist: only these tools are allowed (empty = all tools permitted).
    #[serde(default)]
    pub tools_allow: Vec<String>,
    /// Tool blacklist: these tools are denied regardless of allow list.
    #[serde(default)]
    pub tools_deny: Vec<String>,
    /// Outbound recipient whitelist for `message_send`.
    ///
    /// Entries are `{channel}:{recipient}` (for example `"telegram:*"` or
    /// `"wacli:1234@s.whatsapp.net"`); a bare `"*"` matches every destination.
    /// Empty (the default) means this rule imposes no whitelist, so the
    /// same-channel / cross-channel default decides.
    ///
    /// The whitelist is matched on the recipient **exactly as written**: unlike
    /// `send_deny`, no alias folding widens it, because widening a whitelist
    /// would grant reach on a guess. The first matching rule with a non-empty
    /// `send_allow` is the one that applies; `validate` rejects a rule whose
    /// `send_allow` an earlier rule would shadow, so this never fails silently.
    #[serde(default)]
    pub send_allow: Vec<String>,
    /// Outbound recipient blacklist, in the same `{channel}:{recipient}` form.
    ///
    /// Evaluated before `send_allow`, mirroring how `tools_deny` outranks
    /// `tools_allow`, and evaluated across *every* matching rule so it can
    /// never be shadowed by an earlier one.
    ///
    /// The recipient is matched after folding the alias forms that are
    /// derivable from the string itself: a JID's `:device` suffix and server
    /// case (`1234@s.whatsapp.net`, `1234:2@s.whatsapp.net` and
    /// `1234@S.WhatsApp.net` are one person), a leading `+` (`+15550001111`
    /// and `15550001111`), and a UUID's letter case and hyphenation.
    ///
    /// **Aliases that are NOT derivable are not covered.** A WhatsApp LID
    /// (`<id>@lid`) carries a different number from the same contact's phone
    /// JID (`<number>@s.whatsapp.net`), and a Signal UUID is unrelated to that
    /// account's E.164 number; correlating either pair needs a mapping table
    /// this layer does not hold. Write one entry per identifier form the
    /// contact can appear under, or deny the whole channel with
    /// `"{channel}:*"`, which is the only form that is alias-proof.
    #[serde(default)]
    pub send_deny: Vec<String>,
    /// Assignment whitelist: which `prx chat` sessions this scope may hand work
    /// to through `POST /api/chat-sessions/{id}/assign`.
    ///
    /// Entries are `{label}:{session_id}` — the same two-segment shape as
    /// `send_allow`, with the live chat session in the place of the recipient.
    /// Either segment may be `"*"`, and a bare `"*"` matches every session. The
    /// label is the human name the chat process registered under, so
    /// `"workstation:*"` follows that chat across restarts while
    /// `"*:{session_id}"` names exactly one run of it.
    ///
    /// **The label is self-declared by whoever registered the session.** It is a
    /// convenience for writing rules, not an identity assertion: anything that
    /// can reach the register endpoint can claim any label. Where that matters,
    /// name the session id, which the daemon generates.
    ///
    /// Assignment is **default-deny**: with no `assign_owners` and no
    /// `assign_allow` anywhere, no channel correspondent can assign anything.
    /// That is the opposite of `send_allow`, whose empty default still permits
    /// the turn's own channel, and it is deliberate — an assignment starts work
    /// on another entry point, so silence must mean "no".
    #[serde(default)]
    pub assign_allow: Vec<String>,
    /// Assignment blacklist, in the same `{label}:{session_id}` form.
    ///
    /// Evaluated across *every* matching rule, so a later `assign_deny` can
    /// never be shadowed by an earlier rule, and it outranks both
    /// `assign_allow` and `autonomy.scopes.assign_owners` — an owner named here
    /// is refused, exactly as `tools_deny` outranks `tools_allow`.
    ///
    /// Unlike `send_deny` there is no alias folding: both segments are compared
    /// literally. A chat session id is generated by this daemon and has exactly
    /// one spelling, so there is no alias to fold and no guess to make.
    #[serde(default)]
    pub assign_deny: Vec<String>,
}

/// What `autonomy.scopes` does with a tool call no rule matches.
///
/// Parsed once, at load: `allow` or `deny` after trimming and ignoring case,
/// and nothing else (D-050). The runtime (`SecurityPolicy::from_config`) and
/// the configuration plan (`config::manage::plan`) read this one value, so a
/// spelling such as `"deny "` can never mean deny to the plan and allow at
/// runtime. An unknown value is refused when the configuration loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScopeDefault {
    /// Unmatched tool calls are allowed (the default).
    #[default]
    Allow,
    /// Unmatched tool calls are refused.
    Deny,
}

impl ScopeDefault {
    /// The canonical reading of a configured value, or `None` when it is
    /// neither `allow` nor `deny`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    /// Whether a tool call no rule matches is allowed.
    #[must_use]
    pub const fn allows_unmatched(self) -> bool {
        matches!(self, Self::Allow)
    }
}

impl<'de> Deserialize<'de> for ScopeDefault {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw)
            .ok_or_else(|| serde::de::Error::custom("autonomy.scopes.default must be \"allow\" or \"deny\""))
    }
}

/// Scope-based tool access control configuration (`[autonomy.scopes]`).
///
/// Controls which tools are available per-user, per-channel, and per-chat-type.
///
/// Example config:
/// ```toml
/// [autonomy.scopes]
/// default = "allow"
///
/// [[autonomy.scopes.rules]]
/// channel = "signal"
/// chat_type = "group"
/// tools_deny = ["shell", "file_write"]
///
/// [[autonomy.scopes.rules]]
/// user = "uuid:untrusted-user-uuid"
/// tools_allow = ["memory_recall"]   # whitelist-only
///
/// [[autonomy.scopes.rules]]
/// channel = "wacli"
/// send_deny = ["*:+15550001111"]    # never message this recipient
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScopeConfig {
    /// Default action when no rule matches: "allow" (default) or "deny".
    #[serde(default)]
    pub default: ScopeDefault,
    /// Ordered list of scope rules evaluated top-to-bottom.
    #[serde(default)]
    pub rules: Vec<ScopeRule>,
    /// Correspondents who own this daemon and may hand work to any `prx chat`
    /// session it knows about.
    ///
    /// Entries are `{channel}:{sender}` — the same two-segment shape
    /// `send_allow` uses for a destination, read here as an *origin*. Either
    /// segment may be `"*"`; a bare `"*"` makes every correspondent an owner
    /// and is almost never what an operator wants.
    ///
    /// The identity compared here is the runtime-injected `_zc_scope`, not
    /// anything a model or an HTTP body claimed, so an entry names a real
    /// platform account. An owner still loses to a matching `assign_deny`.
    ///
    /// Empty (the default) means no correspondent owns this daemon, so the only
    /// way to assign work is an authenticated operator-plane request.
    #[serde(default)]
    pub assign_owners: Vec<String>,
}

impl Default for ScopeConfig {
    fn default() -> Self {
        Self {
            default: ScopeDefault::default(),
            rules: Vec::new(),
            assign_owners: Vec::new(),
        }
    }
}

impl ScopeConfig {
    /// Validate the outbound recipient ACL entries of every rule.
    ///
    /// Two shapes of silently-ineffective configuration are rejected here:
    ///
    /// 1. An entry that is neither `"*"` nor a well-formed `{channel}:{recipient}`
    ///    pair could never match anything, which for a `send_deny` entry would be
    ///    a silent fail-open.
    /// 2. A `send_allow` whitelist that an earlier rule's whitelist already
    ///    shadows. `send_deny` sweeps every matching rule and so cannot be
    ///    shadowed, but only the first matching `send_allow` applies — an
    ///    operator who adds a narrower rule below a broader one would otherwise
    ///    get no effect and no error. Reorder the rules, or fold the entries
    ///    into the rule that shadows them.
    pub fn validate(&self) -> Result<()> {
        for (index, rule) in self.rules.iter().enumerate() {
            for (field, entries) in [("send_allow", &rule.send_allow), ("send_deny", &rule.send_deny)] {
                for entry in entries {
                    validate_acl_pair_entry(
                        &format!("autonomy.scopes.rules[{index}].{field}"),
                        "channel",
                        "recipient",
                        entry,
                    )?;
                }
            }
            for (field, entries) in [("assign_allow", &rule.assign_allow), ("assign_deny", &rule.assign_deny)] {
                for entry in entries {
                    validate_acl_pair_entry(
                        &format!("autonomy.scopes.rules[{index}].{field}"),
                        "label",
                        "session",
                        entry,
                    )?;
                }
            }
        }
        for entry in &self.assign_owners {
            validate_acl_pair_entry("autonomy.scopes.assign_owners", "channel", "sender", entry)?;
        }
        self.validate_no_shadowed_allow("send_allow", |rule| &rule.send_allow)?;
        self.validate_no_shadowed_allow("assign_allow", |rule| &rule.assign_allow)
    }

    /// Reject a whitelist an earlier rule makes unreachable.
    ///
    /// Both `send_allow` and `assign_allow` are first-match-wins, so a narrower
    /// rule written below a broader one would take effect nowhere and say
    /// nothing about it. Their deny counterparts sweep every matching rule and
    /// therefore cannot be shadowed, which is why only the allow side is
    /// checked here.
    pub(super) fn validate_no_shadowed_allow(
        &self,
        field: &str,
        entries: impl Fn(&ScopeRule) -> &Vec<String>,
    ) -> Result<()> {
        for (index, rule) in self.rules.iter().enumerate() {
            if entries(rule).is_empty() {
                continue;
            }
            let shadow = self
                .rules
                .iter()
                .take(index)
                .position(|earlier| !entries(earlier).is_empty() && scope_criteria_cover(earlier, rule));
            if let Some(shadow) = shadow {
                anyhow::bail!(
                    "autonomy.scopes.rules[{index}].{field} can never apply: rules[{shadow}]                      matches every context rules[{index}] matches and already carries a {field}                      whitelist, so only that earlier whitelist is ever consulted. Move                      rules[{index}] above rules[{shadow}], narrow rules[{shadow}], or merge the                      entries into rules[{shadow}].{field}."
                );
            }
        }
        Ok(())
    }
}

/// Whether `outer` matches every request context `inner` matches.
///
/// Conservative on purpose: a criterion covers another only when it is absent
/// (or `"*"`, which is the same thing) or literally equal, so this never claims
/// a shadow that is not really there.
pub(super) fn scope_criteria_cover(outer: &ScopeRule, inner: &ScopeRule) -> bool {
    pub(super) fn covers(outer: Option<&String>, inner: Option<&String>) -> bool {
        match outer.map(String::as_str) {
            None | Some("*") => true,
            Some(criterion) => inner.map(String::as_str) == Some(criterion),
        }
    }
    covers(outer.user.as_ref(), inner.user.as_ref())
        && covers(outer.channel.as_ref(), inner.channel.as_ref())
        && covers(outer.chat_type.as_ref(), inner.chat_type.as_ref())
}

/// Reject a two-segment ACL entry that could never match anything.
///
/// One validator for every `{left}:{right}` list — outbound recipients,
/// assignable chat sessions, assignment owners — because an entry that cannot
/// match is a silent fail-open on a deny list, and catching it in one place is
/// what stops a new list from quietly skipping the check.
pub(super) fn validate_acl_pair_entry(path: &str, left_name: &str, right_name: &str, entry: &str) -> Result<()> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{path} must not contain an empty entry");
    }
    if trimmed == "*" {
        return Ok(());
    }
    let Some((left, right)) = trimmed.split_once(':') else {
        anyhow::bail!("{path} entry {trimmed:?} must be \"*\" or \"{{{left_name}}}:{{{right_name}}}\"");
    };
    if left.trim().is_empty() {
        anyhow::bail!("{path} entry {trimmed:?} has an empty {left_name} segment");
    }
    if right.trim().is_empty() {
        anyhow::bail!("{path} entry {trimmed:?} has an empty {right_name} segment");
    }
    Ok(())
}

/// Autonomy and security policy configuration (`[autonomy]` section).
///
/// Controls what the agent is allowed to do: shell commands, filesystem access,
/// risk approval gates, and per-policy budgets.
///
/// Permission-model Phase 1: a single `[autonomy]` section is the sole
/// permission knob. `level` (readonly / supervised / full) drives the unified
/// `SecurityPolicy::decide` decision point. The legacy `[security.tool_policy]`
/// pipeline and the per-tool `auto_approve` / `always_ask` / `allowed_commands`
/// lists have been removed; coarse risk presets are governed entirely by
/// `level`. Fine-grained per-risk classification is deferred to Phase 2.
pub const DEFAULT_AUTONOMY_MAX_COST_PER_DAY_CENTS: u32 = 500;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutonomyConfig {
    /// Autonomy level: `readonly` (alias `read_only`), `supervised`, or `full` (default).
    pub level: AutonomyLevel,
    /// Restrict file writes and command paths to the workspace directory. Default: `true`.
    pub workspace_only: bool,
    /// Explicit path denylist. Default includes system-critical paths.
    pub forbidden_paths: Vec<String>,
    /// Maximum cost per day in cents per policy. Default: `500`.
    pub max_cost_per_day_cents: u32,

    /// Operator acknowledgement for an intentionally unrestricted profile.
    /// This suppresses doctor noise only; it does not alter policy behavior.
    #[serde(default)]
    pub acknowledge_unrestricted_profile: bool,

    /// Scope-based tool access control: per-user/channel/chat-type allow/deny rules.
    #[serde(default)]
    pub scopes: ScopeConfig,
}

impl Default for AutonomyConfig {
    fn default() -> Self {
        Self {
            level: AutonomyLevel::Full,
            workspace_only: true,
            forbidden_paths: vec![
                "/etc".into(),
                "/root".into(),
                "/home".into(),
                "/usr".into(),
                "/bin".into(),
                "/sbin".into(),
                "/lib".into(),
                "/opt".into(),
                "/boot".into(),
                "/dev".into(),
                "/proc".into(),
                "/sys".into(),
                "/var".into(),
                "/tmp".into(),
                "~/.ssh".into(),
                "~/.gnupg".into(),
                "~/.aws".into(),
                "~/.config".into(),
            ],
            max_cost_per_day_cents: DEFAULT_AUTONOMY_MAX_COST_PER_DAY_CENTS,
            acknowledge_unrestricted_profile: false,
            scopes: ScopeConfig::default(),
        }
    }
}
