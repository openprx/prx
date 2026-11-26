use crate::security::redaction::redact_secrets;
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use std::path::{Path, PathBuf};

pub const PERSISTED_APPROVAL_GRANT_TTL_SECS: i64 = 24 * 60 * 60;

use super::command_paths::{
    contains_active_shell_substitution, forbidden_path_argument, forbidden_path_argument_inner,
};

/// How much autonomy the agent has
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AutonomyLevel {
    /// Read-only: can observe but not act
    #[serde(alias = "read_only")]
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
    "capability_search",
    // Filesystem / memory reads.
    "file_read",
    "memory_recall",
    "memory_search",
    "memory_get",
    "transcript_history_lookup",
    // Document retrieval (RAG) reads.
    "document_search",
    "document_get_chunk",
    "skills_list",
    "lifecycle_status",
    "lifecycle_list",
    "lifecycle_inspect",
    "skill_read",
    "wasm_plugins_status",
    "hooks_status",
    "mcp_status",
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
        grant.refuse_decision_binding().map_err(|error| error.to_string())?;
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

    /// Spends the one use of a signed v2 grant in the process-level replay
    /// ledger. `true` when this call took the use; `false` for a replay and for
    /// a grant without a signed use budget (a v1 grant), which cannot be
    /// consumed. For callers that decide on a grant themselves, where the gate
    /// does not consume it (the gate only does so under `supervised`).
    #[must_use]
    pub(crate) fn consume_single_use(&self) -> bool {
        self.v2_single_use_key()
            .is_some_and(|(grant_id, max_uses)| try_consume_v2_grant(grant_id, max_uses))
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
        self.authorize_command_execution_at(None, tool_name, command, grant)
    }

    /// Authorize a command against its actual execution root.
    pub fn authorize_command_execution_at(
        self,
        execution_root: Option<&Path>,
        tool_name: &str,
        command: &str,
        grant: Option<&ApprovalGrant>,
    ) -> Result<CommandRiskLevel, String> {
        self.authorize_command_execution_for(None, execution_root, tool_name, command, grant)
    }

    /// [`Self::authorize_command_execution_at`] for a call whose caller
    /// passes its effective `level`: at Full the path-argument checks
    /// (forbidden, dynamic and substituted paths) are skipped; a NUL byte is
    /// still refused.
    pub fn authorize_command_execution_at_level(
        self,
        level: AutonomyLevel,
        execution_root: Option<&Path>,
        tool_name: &str,
        command: &str,
        grant: Option<&ApprovalGrant>,
    ) -> Result<CommandRiskLevel, String> {
        self.authorize_command_execution_for(Some(level), execution_root, tool_name, command, grant)
    }

    fn authorize_command_execution_for(
        self,
        level: Option<AutonomyLevel>,
        execution_root: Option<&Path>,
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
        let mut result =
            self.policy
                .validate_command_execution_at(level, execution_root, command, runtime_approval_granted);
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

/// Security policy enforced on all tool executions
#[derive(Debug, Clone)]
pub struct SecurityPolicy {
    /// Preserve evaluator path confinement even when its tools run at Full.
    pub evaluation_sandbox: bool,
    pub autonomy: AutonomyLevel,
    pub workspace_dir: PathBuf,
    pub workspace_only: bool,
    pub forbidden_paths: Vec<String>,
    pub max_cost_per_day_cents: u32,
    /// Scope-based per-user/channel/chat_type tool access rules.
    pub scope_rules: Vec<crate::config::ScopeRule>,
    /// Default action when no scope rule matches: true = allow, false = deny.
    pub scope_default_allow: bool,
    /// `{channel}:{sender}` origins that own this daemon and may hand work to
    /// any registered `prx chat` session. See
    /// [`is_assignment_allowed`](Self::is_assignment_allowed).
    pub assign_owners: Vec<String>,
    /// FIX-P1-31: audit configuration governing the side-effect gate's audit
    /// trail. When `enabled=false` the gate's best-effort audit hook writes
    /// nothing and performs no `fsync`, so a user who disables `security.audit`
    /// actually pays no audit cost. Defaults to [`AuditConfig::default`]
    /// (`enabled=true`) to preserve the historical always-on behaviour.
    pub audit_config: crate::config::AuditConfig,
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        // The programmatic fallback stays confined (fail closed). A running
        // install builds its policy with `from_config`, whose default is open.
        Self::from_config(&crate::config::AutonomyConfig::confined(), Path::new("."))
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
pub(super) enum QuoteState {
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
pub(super) fn fold_shell_line_continuations(command: &str) -> String {
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
pub(super) fn split_unquoted_segments(command: &str) -> Vec<String> {
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
                    '&' if chars.next_if_eq(&'&').is_none() => {
                        return true;
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
        self.validate_command_execution_at(None, None, command, runtime_approval_granted)
    }

    /// Validate against the shell's selected directory; None means workspace.
    /// `level` is the caller's effective level, never this policy's own: at
    /// Full the path-argument checks are skipped and only a NUL byte is
    /// refused. Without a level every check applies.
    pub fn validate_command_execution_at(
        &self,
        level: Option<AutonomyLevel>,
        execution_root: Option<&Path>,
        command: &str,
        runtime_approval_granted: bool,
    ) -> Result<CommandRiskLevel, String> {
        if level == Some(AutonomyLevel::Full) && !self.evaluation_sandbox {
            if command.contains('\0') {
                return Err(
                    crate::security::PathRefusal::shell(crate::security::PathRefusalReason::InvalidPath, "")
                        .to_string(),
                );
            }
        } else if let Some(violation) = forbidden_path_argument(
            self,
            crate::security::path_policy::effective_root(execution_root, self),
            command,
        ) {
            return Err(violation.into_refusal().to_string());
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

        // Full autonomy bypasses approval risk grading here. Native shell
        // dispatch separately selects its effective autonomy and git policy.
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

    /// Check static shell path arguments against the process directory.
    /// The orchestration validator retains its dynamic-expression restrictions;
    /// this backend check preserves generic shell expansion support.
    pub(crate) fn validate_command_paths_at(
        &self,
        root: &Path,
        command: &str,
    ) -> Result<(), crate::security::PathRefusal> {
        forbidden_path_argument_inner(self, root, command, 0, false)
            .map_or(Ok(()), |violation| Err(violation.into_refusal()))
    }

    /// Check if autonomy level permits any action at all
    pub fn can_act(&self) -> bool {
        self.autonomy != AutonomyLevel::ReadOnly
    }

    // ── Tool Operation Gating ──────────────────────────────────────────────
    // Read operations bypass the autonomy check because they have no side
    // effects. Act operations must pass the autonomy gate (not read-only).
    // There is no action budget: this runtime does not ration how much work an
    // agent may do per unit of time.

    /// Enforce policy for a tool operation.
    ///
    /// Read operations are always allowed. Act operations require non-readonly
    /// autonomy.
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

                Ok(())
            }
        }
    }

    /// Build from config sections
    pub fn from_config(autonomy_config: &crate::config::AutonomyConfig, workspace_dir: &Path) -> Self {
        Self {
            evaluation_sandbox: autonomy_config.evaluation_sandbox,
            autonomy: autonomy_config.level,
            workspace_dir: workspace_dir.to_path_buf(),
            workspace_only: autonomy_config.workspace_only,
            forbidden_paths: autonomy_config.forbidden_paths.clone(),
            max_cost_per_day_cents: autonomy_config.effective_max_cost_per_day_cents(),
            scope_rules: autonomy_config.scopes.rules.clone(),
            scope_default_allow: autonomy_config.scopes.default.allows_unmatched(),
            assign_owners: autonomy_config.scopes.assign_owners.clone(),
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

    /// Outbound recipient authorization — the outbound counterpart of
    /// [`is_tool_allowed`](Self::is_tool_allowed).
    ///
    /// `src_channel` is the channel the turn is anchored to, i.e. the channel a
    /// reply would implicitly go out on. `dst_channel` / `dst_recipient` are the
    /// destination actually being addressed.
    ///
    /// Evaluation order. Unlike `is_tool_allowed`, this is deliberately **not**
    /// first-match-wins: a rule that carries no `send_allow`/`send_deny` entry
    /// expresses nothing about outbound messaging and must not be able to
    /// shadow a later rule that does (see the module note on rule shadowing).
    /// 1. Skip every rule with both `send_allow` and `send_deny` empty — a
    ///    `tools_deny`-only rule is not an outbound rule.
    /// 2. Walk the remaining matching rules **in full**; if any one of them
    ///    denies the destination, the answer is deny. `send_deny` can therefore
    ///    never be silently shadowed by an earlier rule.
    /// 3. The first matching rule with a non-empty `send_allow` acts as the
    ///    whitelist (`ScopeConfig::validate` rejects a later rule whose
    ///    `send_allow` that first one would shadow, so this cannot silently
    ///    discard an operator's intent either).
    /// 4. No matching outbound rule with a whitelist falls back to the channel
    ///    default below.
    ///
    /// The channel default is what keeps this a zero-regression addition:
    /// sending to the channel the turn already owns stays allowed (exactly
    /// today's behaviour), while reaching a *different* channel is denied
    /// unless a `send_allow` entry opts into it. Note that
    /// `autonomy.scopes.default` is intentionally not consulted here: it
    /// governs tool access, and a `deny` default already blocks `message_send`
    /// itself before any recipient is resolved.
    ///
    /// `level` is the effective level of the run that sends, passed by the
    /// caller: at Full the `send_allow` default-deny (the whitelist and the
    /// channel default) does not apply and only `send_deny` refuses.
    #[must_use]
    pub fn is_outbound_allowed(
        &self,
        level: AutonomyLevel,
        sender: &str,
        src_channel: &str,
        chat_type: &str,
        dst_channel: &str,
        dst_recipient: &str,
    ) -> bool {
        self.is_outbound_allowed_for_turn(
            level,
            sender,
            src_channel,
            src_channel,
            chat_type,
            dst_channel,
            dst_recipient,
        )
    }

    /// [`is_outbound_allowed`](Self::is_outbound_allowed) with the turn's
    /// *origin* and its *implicit reply channel* told apart.
    ///
    /// They coincide for every channel-driven turn, which is why the five-argument
    /// form above exists and is what most callers want. They differ on surfaces
    /// that run an agent turn without owning a messaging channel — the gateway
    /// webhook path, whose turn originates on `webhook`/`gateway` yet still
    /// replies through whichever channel handle the daemon registered. Matching
    /// scope rules against the *origin* is what lets an operator write a rule for
    /// that surface at all; keeping the same-channel default measured against the
    /// *reply channel* is what keeps the fix from silently muting those turns.
    #[must_use]
    pub fn is_outbound_allowed_for_turn(
        &self,
        level: AutonomyLevel,
        sender: &str,
        src_channel: &str,
        reply_channel: &str,
        chat_type: &str,
        dst_channel: &str,
        dst_recipient: &str,
    ) -> bool {
        let mut whitelist: Option<&crate::config::ScopeRule> = None;

        for rule in &self.scope_rules {
            // A rule with no outbound entries says nothing about recipients.
            // Skipping it here is what stops a `tools_deny`-only rule from
            // consuming the decision and silently disabling every later
            // `send_deny` that matches the same scope.
            if rule.send_allow.is_empty() && rule.send_deny.is_empty() {
                continue;
            }
            if !rule_matches(rule, sender, src_channel, chat_type) {
                continue;
            }
            // Deny sweeps every matching rule, not just the first one, so a
            // later `send_deny` is always reachable.
            if rule
                .send_deny
                .iter()
                .any(|entry| outbound_deny_entry_matches(entry, dst_channel, dst_recipient))
            {
                return false;
            }
            if whitelist.is_none() && !rule.send_allow.is_empty() {
                whitelist = Some(rule);
            }
        }
        // Full: `send_deny` (swept above) is the only outbound limit.
        if level == AutonomyLevel::Full {
            return true;
        }

        whitelist.map_or(
            // No rule constrains this turn's recipients: the channel the turn
            // already replies on stays reachable, anything else needs an opt-in.
            dst_channel == reply_channel,
            // Matched exactly, with none of `send_deny`'s alias folding: folding
            // is a guess about who two spellings refer to, and a wrong guess must
            // never be what grants reach to a recipient.
            |rule| {
                rule.send_allow
                    .iter()
                    .any(|entry| outbound_entry_matches(entry, dst_channel, dst_recipient))
            },
        )
    }

    /// Whether `sender` may hand work to a registered `prx chat` session.
    ///
    /// This is the assignment counterpart of
    /// [`is_outbound_allowed`](Self::is_outbound_allowed), and it differs from
    /// it in the one way that matters: **it is default-deny.** An outbound send
    /// with no rules configured still reaches the channel the turn already
    /// owns, because refusing it would break every existing deployment. An
    /// assignment has no such prior: it starts work on a *different* entry
    /// point, on this machine, on behalf of whoever spoke into a chat app. With
    /// nothing configured, the answer is no.
    ///
    /// `target_label` / `target_session_id` describe the chat session being
    /// addressed; entries are matched as `{label}:{session_id}` pairs.
    ///
    /// Evaluation order:
    /// 1. `assign_deny` sweeps **every** matching rule that carries assignment
    ///    entries, so a later deny is always reachable and can never be
    ///    shadowed by an earlier rule. A deny outranks everything below,
    ///    including ownership.
    /// 2. An `autonomy.scopes.assign_owners` entry matching this origin allows.
    /// 3. Otherwise the **first** matching rule with a non-empty `assign_allow`
    ///    decides, exactly as `send_allow` does (`ScopeConfig::validate`
    ///    rejects a whitelist an earlier one would shadow, so this cannot
    ///    silently discard an operator's intent).
    /// 4. Nothing matched — deny.
    ///
    /// Rules with neither assignment list are skipped before their criteria are
    /// even considered, so a rule written for some unrelated reason cannot
    /// consume the decision. That is the same shape as the outbound evaluator
    /// and deliberately *not* the first-match-wins shape `tools_allow` /
    /// `tools_deny` keep.
    ///
    /// The operator plane does not come through here at all: a request that
    /// authenticated against the gateway is already an operator, and the caller
    /// is what decides whether to consult this predicate. See
    /// `crate::runtime::chat_sessions::AssignPrincipal`.
    #[must_use]
    pub fn is_assignment_allowed(
        &self,
        sender: &str,
        channel: &str,
        chat_type: &str,
        target_label: &str,
        target_session_id: &str,
    ) -> bool {
        let mut whitelist: Option<&crate::config::ScopeRule> = None;

        for rule in &self.scope_rules {
            if rule.assign_allow.is_empty() && rule.assign_deny.is_empty() {
                continue;
            }
            if !rule_matches(rule, sender, channel, chat_type) {
                continue;
            }
            if rule
                .assign_deny
                .iter()
                .any(|entry| acl_pair_matches(entry, target_label, target_session_id))
            {
                return false;
            }
            if whitelist.is_none() && !rule.assign_allow.is_empty() {
                whitelist = Some(rule);
            }
        }

        if self
            .assign_owners
            .iter()
            .any(|entry| acl_pair_matches(entry, channel, sender))
        {
            return true;
        }

        whitelist.is_some_and(|rule| {
            rule.assign_allow
                .iter()
                .any(|entry| acl_pair_matches(entry, target_label, target_session_id))
        })
    }

    /// Unified tool-authorization decision point (permission-model Phase 1).
    ///
    /// This is the single entry point that replaces the former scattered
    /// authorization logic. Evaluation order:
    ///
    /// 1. **Identity scope ACL** — [`is_tool_allowed`](Self::is_tool_allowed)
    ///    (per user / channel / chat_type). A scope denial is `Deny` except
    ///    in `full`, which has no permission limits.
    /// 2. **Autonomy level**:
    ///    * `full`        → `Allow` (allow everything; no prompt, no grant).
    ///    * `read_only`   → read-only tools `Allow`, everything else `Deny`.
    ///    * `supervised`  → read-only tools `Allow`, everything else `Ask`
    ///      (routes through `ApprovalManager` + `ApprovalGrantV2`).
    ///
    /// The non-CLI-channel fail-closed behaviour (an `Ask` with no approval
    /// resolver becomes a `Deny`) is enforced at the call site in the tool-call
    /// loop, not here.
    #[must_use]
    pub fn decide(&self, tool_name: &str, sender: &str, channel: &str, chat_type: &str) -> ToolDecision {
        // 1. Identity scope ACL — Full autonomy has no permission limits.
        if self.autonomy != AutonomyLevel::Full && !self.is_tool_allowed(tool_name, sender, channel, chat_type) {
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
/// Match one `{channel}:{recipient}` outbound ACL entry against a destination.
///
/// Named separately from [`acl_pair_matches`] so the outbound call sites read
/// in the vocabulary of their own decision; the matching itself is shared, so
/// every ACL list in this file agrees on what an entry means.
fn outbound_entry_matches(entry: &str, dst_channel: &str, dst_recipient: &str) -> bool {
    acl_pair_matches(entry, dst_channel, dst_recipient)
}

/// Match one `{left}:{right}` ACL entry against a pair of literals.
///
/// Shared by every two-segment list in this file — outbound recipients,
/// assignable chat sessions, assignment owners — so all of them agree on what
/// `"*"` means and where the entry splits. `"*"` alone matches everything;
/// otherwise the entry splits at its **first** colon, which keeps a right-hand
/// side that itself contains colons (a JID, a forum topic) intact. Either
/// segment may be `"*"`. An entry with no colon matches nothing;
/// `ScopeConfig::validate` rejects that shape at load time so a deny-list typo
/// cannot silently fail open.
fn acl_pair_matches(entry: &str, left: &str, right: &str) -> bool {
    let entry = entry.trim();
    if entry == "*" {
        return true;
    }
    let Some((entry_left, entry_right)) = entry.split_once(':') else {
        return false;
    };
    let entry_left = entry_left.trim();
    if entry_left != "*" && entry_left != left {
        return false;
    }
    let entry_right = entry_right.trim();
    entry_right == "*" || entry_right == right
}

/// Match one `send_deny` entry against a destination, folding the recipient
/// alias forms that are derivable from the identifier itself.
///
/// Denial is the one direction where widening a match is safe: treating two
/// spellings as the same person can only refuse more, never reach further. The
/// whitelist deliberately does *not* get this treatment — see
/// [`SecurityPolicy::is_outbound_allowed_for_turn`].
///
/// The channel half of the entry is still matched exactly; only the recipient
/// half is folded.
fn outbound_deny_entry_matches(entry: &str, dst_channel: &str, dst_recipient: &str) -> bool {
    if outbound_entry_matches(entry, dst_channel, dst_recipient) {
        return true;
    }
    let Some((channel, recipient)) = entry.trim().split_once(':') else {
        return false;
    };
    let channel = channel.trim();
    if channel != "*" && channel != dst_channel {
        return false;
    }
    let recipient = recipient.trim();
    // `"*"` already matched above; folding it would be meaningless.
    recipient != "*" && fold_recipient(recipient) == fold_recipient(dst_recipient)
}

/// Fold a recipient identifier into the form `send_deny` compares on.
///
/// Only spellings that are recoverable from the string are folded:
///
/// * a JID's `:device` suffix, in either position wacli emits it
///   (`1234:2@s.whatsapp.net` and `1234@lid:2`), and the server's letter case;
/// * one leading `+`, which the WhatsApp Cloud API strips before sending
///   (`+15550001111` and `15550001111` reach the same account);
/// * a UUID's letter case and hyphenation, which Signal accepts either way.
///
/// A colon in a recipient that is *not* a JID is left alone: Telegram addresses
/// a forum topic as `{chat_id}:{thread_id}` and Mattermost a thread as
/// `{channel_id}:{root_id}`, so folding there would deny the parent chat along
/// with the thread the operator named.
///
/// What this cannot fold is the alias forms that are not derivable: a WhatsApp
/// LID (`<id>@lid`) is a different number from the same contact's phone JID,
/// and a Signal UUID is unrelated to that account's E.164 number. Both need a
/// mapping table (whatsmeow's `whatsmeow_lid_map`, or the Signal server) that
/// this layer does not and should not hold — a security predicate must not
/// depend on an external program's database that may simply be absent, because
/// "absent" would read as "not the same person" and fail open. `ScopeRule`'s
/// `send_deny` documentation states the consequence: list every form, or deny
/// the channel outright with `"{channel}:*"`.
fn fold_recipient(recipient: &str) -> String {
    let trimmed = recipient.trim();
    match trimmed.split_once('@') {
        Some((user, server)) => {
            let server = strip_device_suffix(server).to_ascii_lowercase();
            format!("{}@{server}", fold_recipient_user(strip_device_suffix(user)))
        }
        None => fold_recipient_user(trimmed),
    }
}

/// Fold the user half of a recipient: one leading `+`, and UUID spelling.
fn fold_recipient_user(user: &str) -> String {
    let user = user.strip_prefix('+').unwrap_or(user);
    uuid::Uuid::parse_str(user).map_or_else(|_| user.to_string(), |id| id.as_hyphenated().to_string())
}

/// Drop a `:device` suffix from one half of a JID.
fn strip_device_suffix(part: &str) -> &str {
    part.split_once(':').map_or(part, |(base, _)| base)
}

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
mod tests;
