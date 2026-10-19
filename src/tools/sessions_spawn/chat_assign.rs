use super::{SessionsSpawnTool, parse_spawn_scope};
use crate::runtime::chat_sessions::{AssignPrincipal, Disposition};
use crate::security::SecurityPolicy;
use crate::tools::chat_assignment;
use crate::tools::chat_assignment::ReturnTrip;
use crate::tools::traits::ToolResult;
use serde_json::json;
use std::sync::Arc;

impl SessionsSpawnTool {
    /// The identity behind a chat-assignment call, or the refusal that replaces
    /// it.
    ///
    /// The only source is the `_zc_*` block the runtime writes immediately
    /// before a tool executes, and an absent or untrusted one is a hard refusal
    /// rather than a fallback: an anonymous principal is something a later rule
    /// could match, and the operator plane is the *most* privileged principal
    /// there is. Degrading to either would turn "no identity" into a
    /// permission. The refusal names nobody, because there is nobody to name.
    fn assignment_principal(args: &serde_json::Value) -> Result<AssignPrincipal, ToolResult> {
        AssignPrincipal::from_trusted_scope(args).ok_or_else(|| ToolResult {
            success: false,
            output: String::new(),
            error: Some(
                "This call carries no trusted caller scope, so there is no identity to authorize a \
                 chat-session assignment against. Assignment is refused."
                    .to_string(),
            ),
        })
    }

    /// `action='chat_sessions'` — the live `prx chat` sessions *this caller*
    /// may hand work to.
    ///
    /// Filtered rather than complete on purpose. `chat_sessions::assign`
    /// authorizes before it reports a session unknown, precisely so a
    /// correspondent who may not assign anywhere cannot use it to learn which
    /// sessions exist; an unfiltered listing on the same principal's tool
    /// surface would hand back exactly that. A caller with no grants sees an
    /// empty list, which is the same thing it would learn by trying.
    pub(super) fn execute_chat_sessions(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let principal = match Self::assignment_principal(args) {
            Ok(principal) => principal,
            Err(refusal) => return Ok(refusal),
        };
        let sessions = crate::runtime::chat_sessions::list()
            .into_iter()
            .filter(|session| {
                chat_assignment::may_assign(&self.security, &principal, &session.label, &session.session_id)
            })
            .map(|session| {
                json!({
                    "session_id": session.session_id,
                    "label": session.label,
                    "liveness": session.liveness.as_str(),
                    "last_poll_age_secs": session.last_poll_age_secs,
                    "queued": session.queued,
                    "delivered": session.delivered,
                    "accepted": session.accepted,
                    "registered_at_unix_ms": session.registered_at_unix_ms,
                })
            })
            .collect::<Vec<_>>();
        let note = if sessions.is_empty() {
            "No chat session is both registered and assignable by you. Assignment is default-deny: \
             an operator has to name the origin in `autonomy.scopes.assign_owners` or give it an \
             `assign_allow` entry."
        } else {
            "`liveness` is evidence, not a clock: 'silent' means the session has not pulled its \
             mailbox recently and may be busy or gone. Nothing is evicted for it."
        };
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({"sessions": sessions, "note": note}))?,
            error: None,
        })
    }

    /// A mailbox refusal, plus the one thing that keeps an unknown id from
    /// costing a round trip.
    ///
    /// Only [`ChatSessionError::UnknownSession`] is enriched. A model that gets
    /// "no session with that id" answers by guessing another id — the real-machine
    /// run produced `prx-agent-chat-6914` out of thin air — so the refusal that
    /// says the id is wrong is exactly where the right ones belong.
    ///
    /// **Every other refusal is passed through untouched, and that is a security
    /// decision, not an omission.** `NotAuthorized` is raised for a principal that
    /// may not assign here; attaching a listing to it would turn the one refusal
    /// that is supposed to reveal nothing into an enumeration oracle. The hint
    /// itself is filtered per session by the same [`chat_assignment::may_assign`]
    /// the `chat_sessions` listing uses, and is omitted entirely when the caller
    /// may assign nowhere.
    fn assignment_refusal(
        security: &SecurityPolicy,
        principal: &AssignPrincipal,
        error: &crate::runtime::chat_sessions::ChatSessionError,
    ) -> String {
        let hint = matches!(error, crate::runtime::chat_sessions::ChatSessionError::UnknownSession)
            .then(|| chat_assignment::assignable_sessions_hint(security, principal))
            .flatten();
        hint.map_or_else(
            || error.to_string(),
            |hint| format!("{error}. Sessions you may assign to right now: {hint}"),
        )
    }

    /// `action='chat_assign'` — hand a task to a live `prx chat` session and
    /// arrange for its answer to come back here.
    ///
    /// In-process rather than over the daemon's own HTTP API, and that is the
    /// substance of the action rather than an optimisation: an HTTP request
    /// carries no trustworthy caller identity, so the same call made over the
    /// loopback API would be authorized as the operator plane — the most
    /// privileged principal there is — no matter who typed the message. The
    /// only identity worth anything here is the one the runtime injected, and it
    /// exists only inside this process.
    ///
    /// The call returns as soon as the mailbox accepts the task. It does not
    /// wait: a chat session has no deadline, so waiting here would park the
    /// correspondent's turn for as long as the other end takes. The relay
    /// started below is what closes the loop, and it is a work-registry row of
    /// its own so an operator can end it.
    pub(super) async fn execute_chat_assign(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let principal = match Self::assignment_principal(args) {
            Ok(principal) => principal,
            Err(refusal) => return Ok(refusal),
        };
        let session_id = args
            .get("session_id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' parameter for chat_assign action"))?;
        let task = args
            .get("task")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'task' parameter for chat_assign action"))?;
        let disposition = match args.get("disposition").and_then(|value| value.as_str()) {
            // Unknown values are rejected rather than folded into the default:
            // a caller that asked to interrupt and silently got a queue would
            // have its intent evaporate with nothing said.
            Some(raw) => Disposition::parse(raw)
                .ok_or_else(|| anyhow::anyhow!("Unknown disposition '{raw}': use 'queue', 'steer' or 'interrupt'"))?,
            None => Disposition::Queue,
        };

        let receipt =
            match crate::runtime::chat_sessions::assign(&self.security, &principal, session_id, task, disposition) {
                Ok(receipt) => receipt,
                // Already redacted at the source: the mailbox's refusals name the
                // parties by `op_id` fingerprint, so this is safe to hand to a model
                // that will read it back out loud.
                Err(error) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(Self::assignment_refusal(&self.security, &principal, &error)),
                    });
                }
            };

        // Where the answer goes is settled here, from the assigning turn's own
        // trusted scope, and never from the tool arguments — unlike `spawn`,
        // which accepts an explicit `recipient` and gates it. There is no reason
        // for one correspondent to assign work whose result lands in someone
        // else's conversation, so the capability simply is not offered.
        let scope = parse_spawn_scope(args);
        let recipient = scope.as_ref().map_or_else(
            || match &principal {
                // No conversation on the scope: answer the correspondent
                // directly, which is the only address left that is still theirs.
                AssignPrincipal::Correspondent { sender, .. } => sender.clone(),
                AssignPrincipal::OperatorPlane => crate::runtime::chat_sessions::OPERATOR_PLANE_CHANNEL.to_string(),
            },
            |scope| scope.chat_id.clone(),
        );
        let channel = self.resolve_announce_channel(Some(principal.channel())).await;
        let relay_work_id = chat_assignment::spawn_return_trip(ReturnTrip {
            assignment_id: receipt.assignment_id.clone(),
            session_ref: receipt.session_ref.clone(),
            origin: principal,
            recipient,
            channel,
            security: Arc::clone(&self.security),
        });

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "assignment_id": receipt.assignment_id,
                "session_id": receipt.session_id,
                "session_ref": receipt.session_ref,
                "disposition": receipt.disposition.as_str(),
                "queued_ahead": receipt.queued_ahead,
                "work_id": receipt.work_id.to_string(),
                "relay_work_id": relay_work_id.to_string(),
                "delivery": "Accepted into the session's mailbox. Its result will be relayed to this \
                             conversation on its own when the session reports it — say that it was \
                             handed over and stop; do not wait for it here.",
            }))?,
            error: None,
        })
    }
}
