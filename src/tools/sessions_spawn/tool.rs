use super::delegation;
use super::{AnnounceOrigin, DEFAULT_HISTORY_ENTRY_MAX_CHARS, SessionsSpawnTool, parse_spawn_scope};
use crate::channels::traits::Channel;
use crate::security::policy::ApprovalGrant;
use crate::tools::sessions_read_model;
use crate::tools::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

#[async_trait]
impl Tool for SessionsSpawnTool {
    fn name(&self) -> &str {
        "sessions_spawn"
    }

    fn description(&self) -> &str {
        "Hand work to an async sub-agent or to a live `prx chat` session, and manage what is running. \
         Actions and their parameters: 'spawn' (default) task -> run_id; 'spawn_batch' tasks -> batch_id; \
         'join' batch_id (waits for every member, returns all results); 'list'; 'kill' run_id; \
         'history' run_id; 'steer' run_id + message; 'chat_sessions'; 'chat_assign' session_id + task. \
         Work text always goes in `task`, never `message`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let available_agents = self
            .agents
            .iter()
            .filter(|(_, cfg)| cfg.spawn_enabled.unwrap_or(true))
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        crate::tools::schema::with_default_action_requirements(
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["spawn", "spawn_batch", "join", "list", "kill", "history", "steer", "chat_sessions", "chat_assign"],
                        "default": "spawn",
                        "description": "Action to perform, with the parameter it reads: 'spawn' task; \
                         'spawn_batch' tasks; 'join' batch_id; 'list' none; 'kill' run_id; 'history' run_id; \
                         'steer' run_id + message; 'chat_sessions' none; 'chat_assign' session_id + task."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "For 'chat_assign': the live `prx chat` session that gets the task, as listed by 'chat_sessions'. A chat session id, not a sub-agent run_id."
                    },
                    "disposition": {
                        "type": "string",
                        "enum": ["queue", "steer", "interrupt"],
                        "default": "queue",
                        "description": "For 'chat_assign': how to handle a turn that session is already running. 'queue' (default) waits, 'steer' feeds the task to the running turn, 'interrupt' stops it first. Prefer 'queue'."
                    },
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "description": "For 'spawn_batch': the tasks to launch together, unlimited in number. Each entry is a task string or an object {task, agent?, model?, provider?, allow_delegation?, mode?, recipient?}; any other key is rejected. Top-level parameters are per-entry defaults that an entry key overrides. Members never announce their own results: call 'join' with the returned batch_id and report them yourself.",
                        "items": {
                            "anyOf": [
                                {"type": "string", "minLength": 1},
                                Self::batch_member_entry_schema()
                            ]
                        }
                    },
                    "batch_id": {
                        "type": "string",
                        "description": "For 'join': the batch identifier returned by 'spawn_batch'. 'join' blocks until every member has finished, however long that takes."
                    },
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The work to be done, as instructions. Required by 'spawn' and by 'chat_assign' \
                                        ('chat_assign' has no `message`, `prompt` or `text` parameter). Use `tasks` for \
                                        'spawn_batch' and `message` for 'steer'."
                    },
                    "run_id": {
                        "type": "string",
                        "description": "Sub-agent run id returned by 'spawn' and listed by 'list'. Required by 'kill', 'history' and 'steer'. Not a chat session id."
                    },
                    "message": {
                        "type": "string",
                        "description": "For 'steer': text injected into an already-running sub-agent to redirect it. \
                                        Not how work is handed out — 'spawn' and 'chat_assign' take their instructions in `task`."
                    },
                    "model": {
                        "type": "string",
                        "description": "For 'spawn' and 'spawn_batch': model override. Defaults to the gateway model."
                    },
                    "provider": {
                        "type": "string",
                        "description": "For 'spawn' and 'spawn_batch': provider override. Must be a provider the operator configured (the gateway provider, a named agent's provider, a fallback provider or a model route); any other provider, including an unconfigured 'custom:<url>', is refused. Defaults to the agent config provider, then the gateway provider."
                    },
                    "allow_delegation": delegation::request_schema(),
                    "agent": {
                        "type": "string",
                        "description": format!(
                            "For 'spawn' and 'spawn_batch': identity agent name. Available: {}",
                            if available_agents.is_empty() {
                                "(none configured)".to_string()
                            } else {
                                available_agents.join(", ")
                            }
                        )
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["task", "process"],
                        "default": "task",
                        "description": "For 'spawn' and 'spawn_batch': 'task' runs in-process (default), 'process' launches an isolated OS process."
                    },
                    "recipient": {
                        "type": "string",
                        "description": "For 'spawn': recipient (phone number, group ID, …) of the result announcement and any \
                                        kill notice; defaults to the current sender. Under 'spawn_batch' it routes only the \
                                        kill notice. 'chat_assign' always replies into the assigning conversation."
                    },
                    "announce": {
                        "type": "boolean",
                        "description": "For 'spawn', default true: whether the sub-agent posts its own result to the channel. \
                                        A 'spawn_batch' member never announces; collect a batch with 'join' and report it once."
                    },
                    "last_n": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 200,
                        "description": "For 'history': return the last N entries (default 20, keeping final proof markers visible)."
                    },
                    "max_chars_per_entry": {
                        "type": "integer",
                        "minimum": 80,
                        "maximum": 4000,
                        "description": "For 'history': maximum characters per returned entry (default 800)."
                    }
                },
                "required": ["action"]
            }),
            "action",
            "spawn",
            &[
                crate::tools::schema::ActionRequirement {
                    action: "spawn",
                    required: &["task"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "spawn_batch",
                    required: &["tasks"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "kill",
                    required: &["run_id"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "history",
                    required: &["run_id"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "steer",
                    required: &["run_id", "message"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "chat_assign",
                    required: &["session_id", "task"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "join",
                    required: &["batch_id"],
                },
            ],
        )
    }

    async fn set_active_recipient(&self, recipient: &str) {
        *self.default_recipient.write().await = Some(recipient.to_string());
    }

    /// Route sub-agent result announcements back on the channel the triggering
    /// message arrived on (wacli/Signal/Telegram/…). The channel/gateway loop
    /// calls this before each turn; each subsequent spawn snapshots the active
    /// channel, fixing the bug where results were always announced over the
    /// construction-time default channel (Signal) regardless of origin.
    async fn set_active_channel(&self, channel: Arc<dyn Channel>) {
        *self.channel.write().await = channel;
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        crate::tools::execution::verify_current_child_call("sessions_spawn")?;
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("spawn");
        if action == "spawn_batch" {
            anyhow::ensure!(
                !self
                    .hint_config
                    .as_ref()
                    .is_some_and(|config| config.causal_tree.enabled)
                    || crate::runtime::child_source::outgoing_grant().is_some(),
                "decision-enabled spawning requires an issued child grant"
            );
        }
        // OWNER-VIS: every run lookup below is confined to the runs this
        // caller's owner started; the local-process operator sees them all. A
        // run outside the scope answers exactly like a missing one.
        let owner_scope = sessions_read_model::RunOwnerScope::from_args(&args);

        match action {
            "list" => return self.execute_list(&owner_scope).await,
            "kill" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for kill action"))?;
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                // Identity for the kill receipt's outbound gate, taken from the
                // killing turn's own trusted scope. Unlike a completion
                // announcement — which fires long after its launching turn ended
                // and therefore has to rely on an identity captured at spawn
                // time — a kill happens *inside* a live turn, so the authority
                // for the message it emits is present rather than remembered.
                // An absent or untrusted scope degrades to the same "unknown"
                // identity `AnnounceOrigin` documents, never a privileged one.
                let notice_origin = AnnounceOrigin::from_scope(parse_spawn_scope(&args).as_ref());
                return self
                    .execute_kill(
                        run_id,
                        approval_grant.as_ref(),
                        notice_origin.as_ref(),
                        &owner_scope,
                        &args,
                    )
                    .await;
            }
            "history" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for history action"))?;
                let last_n = args
                    .get("last_n")
                    .and_then(|v| v.as_u64())
                    .map(|value| value.clamp(1, 200) as usize);
                let max_chars_per_entry = args
                    .get("max_chars_per_entry")
                    .and_then(|v| v.as_u64())
                    .map(|value| value.clamp(80, 4000) as usize)
                    .unwrap_or(DEFAULT_HISTORY_ENTRY_MAX_CHARS);
                return self
                    .execute_history(run_id, last_n, max_chars_per_entry, &owner_scope, &args)
                    .await;
            }
            "steer" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for steer action"))?;
                let message = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter for steer action"))?;
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                return self
                    .execute_steer(run_id, message, approval_grant.as_ref(), &owner_scope)
                    .await;
            }
            "chat_sessions" => return self.execute_chat_sessions(&args),
            "chat_assign" => return self.execute_chat_assign(&args).await,
            "spawn_batch" => return self.execute_spawn_batch(&args).await,
            "join" => {
                let batch_id = args
                    .get("batch_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'batch_id' parameter for join action"))?;
                return self.execute_join(batch_id, &owner_scope).await;
            }
            _ => {} // fall through to spawn
        }

        Ok(self.execute_spawn(&args, None).await?.result)
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }
}
