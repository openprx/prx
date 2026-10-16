use super::{
    AnnounceOrigin, DEFAULT_HISTORY_LAST_N, KILLED_BY_USER_REASON, OutboundNotice, ProcessFinalization,
    ProcessTerminationRequestResult, SessionsSpawnTool, SubAgentRun, SubAgentStatus, TaskFinalization,
    deliver_or_withhold_notice, status_label,
};
use crate::memory::{MemoryFabric, MessageEventScope};
use crate::security::SideEffectGate;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::tools::sessions_list::format_run_usage;
use crate::tools::sessions_read_model;
use crate::tools::traits::{Tool, ToolResult};
use chrono::Utc;
use serde_json::json;

impl SessionsSpawnTool {
    pub(super) fn memory_fabric(&self) -> Option<MemoryFabric> {
        self.source_fabric.clone().or_else(|| {
            self.memory.as_ref().map(|memory| {
                MemoryFabric::new(memory.clone(), self.workspace_dir.to_string_lossy())
                    .with_event_recording(self.event_recording)
            })
        })
    }

    async fn record_active_run_task_event(&self, run: &SubAgentRun, event_type: &str, payload: serde_json::Value) {
        let Some(fabric) = self.memory_fabric() else {
            return;
        };
        let mut scope = MessageEventScope::new("sessions_spawn", crate::memory::MemoryVisibility::Workspace)
            .with_session_key(run.session_scope_key.clone())
            .with_run_id(run.id.clone());
        if let Some(owner_id) = run.owner_id.as_deref() {
            scope = scope.with_owner_id(owner_id);
        }
        if let Some(parent_run_id) = run.parent_run_id.as_deref() {
            scope = scope.with_parent_run_id(parent_run_id);
        }
        let payload = json!({
            "task": run.task,
            "status": status_label(&run.status),
            "owner_id": run.owner_id,
            "topic_id": run.topic_id,
            "parent_task_id": run.parent_run_id,
            "source_message_event_id": run.source_message_event_id,
            "detail": payload
        });
        if let Err(error) = fabric
            .record_task_event(scope, run.id.clone(), event_type.to_string(), Some(payload.to_string()))
            .await
        {
            tracing::warn!(run_id = %run.id, event_type, "failed to record sessions_spawn task event: {error}");
        }
    }

    /// List all tracked sub-agent runs.
    pub(super) async fn execute_list(
        &self,
        owner_scope: &sessions_read_model::RunOwnerScope,
    ) -> anyhow::Result<ToolResult> {
        let registry = self.active_runs.read().await;
        let runs = registry
            .iter()
            .filter(|run| owner_scope.admits(run.owner_id.as_deref()))
            .collect::<Vec<_>>();
        if runs.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No sub-agent runs tracked.".into(),
                error: None,
            });
        }

        let lines: Vec<String> = runs
            .iter()
            .map(|r| {
                let status = match &r.status {
                    SubAgentStatus::Running => "🔄 running".to_string(),
                    SubAgentStatus::AwaitingInput { prompt } => {
                        format!("❓ awaiting approval: {prompt}")
                    }
                    SubAgentStatus::Completed(msg) => {
                        let preview = msg.chars().take(60).collect::<String>();
                        let ellipsis = if msg.len() > 60 { "…" } else { "" };
                        format!("✅ completed: {preview}{ellipsis}")
                    }
                    SubAgentStatus::Failed(e) => format!("❌ failed: {e}"),
                };
                let age = (Utc::now() - r.started_at).num_seconds();
                let parent = r.parent_run_id.as_deref().unwrap_or("root");
                let usage = format_run_usage(&r.token_usage_records);
                format!(
                    "• `{}` [source=runtime, manageable=true, usage={usage}, {age}s ago] {status}\n  task: {}\n  depth: {} | parent: {}",
                    r.id, r.task, r.spawn_depth, parent
                )
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: format!("Sub-agent runs ({} total):\n\n{}", runs.len(), lines.join("\n\n")),
            error: None,
        })
    }

    /// Kill a running sub-agent by its run ID.
    pub(super) async fn execute_kill(
        &self,
        run_id: &str,
        approval_grant: Option<&ApprovalGrant>,
        notice_origin: Option<&AnnounceOrigin>,
        owner_scope: &sessions_read_model::RunOwnerScope,
        args: &serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        let (process_control, task_kill_target) = {
            let mut runs = self.active_runs.write().await;
            match runs
                .iter_mut()
                .find(|r| r.id == run_id && owner_scope.admits(r.owner_id.as_deref()))
            {
                Some(run) => {
                    match &run.status {
                        // A live run (executing) or one suspended awaiting approval
                        // (NeedsInput) is killable: aborting the task tears down the
                        // suspended approval resolver's pending await along with it.
                        SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => {
                            let operation_name = format!("sessions_spawn:kill:{run_id}");
                            if let Err(error) = SideEffectGate::new(self.security.as_ref())
                                .authorize_resource_operation(
                                    self.name(),
                                    &operation_name,
                                    ResourceRiskLevel::Medium,
                                    approval_grant,
                                )
                            {
                                return Ok(ToolResult {
                                    success: false,
                                    output: String::new(),
                                    error: Some(error),
                                });
                            }
                            if let Some(control) = run.process_control.clone() {
                                (Some(control), None)
                            } else {
                                let recipient = run.recipient.clone();
                                // Per-turn channel bound at spawn time — kill-notify routes
                                // to the same channel + recipient as the launching message,
                                // not the shared active channel (avoids cross-channel leak).
                                let channel_name = run.channel_name.clone();
                                let rid = run.id.clone();
                                (None, Some((recipient, channel_name, rid, run.clone())))
                            }
                        }
                        SubAgentStatus::Completed(_) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Run `{run_id}` already completed.")),
                            });
                        }
                        SubAgentStatus::Failed(e) => {
                            if run
                                .task_control
                                .as_ref()
                                .is_some_and(|control| control.finalization() == Some(TaskFinalization::Terminated))
                                || run.process_control.as_ref().is_some_and(|control| {
                                    control.finalization() == Some(ProcessFinalization::Terminated)
                                })
                            {
                                return Ok(ToolResult {
                                    success: true,
                                    output: format!("Sub-agent `{run_id}` has been killed."),
                                    error: None,
                                });
                            }
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Run `{run_id}` already failed: {e}")),
                            });
                        }
                    }
                }
                None => (None, None),
            }
        };

        if let Some(control) = process_control {
            return match control.request_termination(KILLED_BY_USER_REASON).await {
                ProcessTerminationRequestResult::Finalized(ProcessFinalization::Terminated) => Ok(ToolResult {
                    success: true,
                    output: format!("Sub-agent `{run_id}` has been killed."),
                    error: None,
                }),
                ProcessTerminationRequestResult::Finalized(ProcessFinalization::Natural) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` finalized before the termination request won.")),
                }),
                ProcessTerminationRequestResult::Finalized(ProcessFinalization::TerminationFailed) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` could not be terminated by its process owner.")),
                }),
                ProcessTerminationRequestResult::Finalized(ProcessFinalization::Unconfirmed) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` cleanup and settlement remain unconfirmed.")),
                }),
                ProcessTerminationRequestResult::Pending => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Termination was requested for run `{run_id}`, but its process owner is still pending reap; the run remains active."
                    )),
                }),
            };
        }

        let Some((recipient_opt, channel_name_opt, rid, killed_run)) = task_kill_target else {
            return Ok(self.no_runtime_run_result(run_id, args).await);
        };

        let Some(control) = killed_run.task_control.as_ref() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Task termination owner is unavailable; completion is unconfirmed".to_owned()),
            });
        };
        match control.request_termination(KILLED_BY_USER_REASON).await {
            Some(TaskFinalization::Terminated) => {}
            Some(TaskFinalization::Natural) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` finalized before termination was confirmed.")),
                });
            }
            Some(TaskFinalization::Unconfirmed) | None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Termination for run `{run_id}` remains unconfirmed; cleanup has not been acknowledged."
                    )),
                });
            }
        }
        let killed_run = self
            .active_runs
            .read()
            .await
            .iter()
            .find(|run| run.id == rid)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("terminated task row is unavailable"))?;

        self.record_active_run_task_event(&killed_run, "task.killed", json!({"reason": "killed by user"}))
            .await;

        if let Some(target) = recipient_opt {
            let channel = self.resolve_announce_channel(channel_name_opt.as_deref()).await;
            // The receipt is addressed to the run's `recipient`, which the model
            // wrote on the *spawn* call — the identical bypass surface the
            // completion announcement had, and the one it kept after that was
            // closed: name any address, kill the run, and the notice went out
            // with no authorization at all.
            //
            // This does not re-open the deliberate call that a kill receipt
            // ignores `announce`. That switch answers "does a run report itself?"
            // and a kill is the operator's own action, not the run's; the ACL
            // answers "may this process address that recipient?". Two questions,
            // two mechanisms — folding them into one switch would mean either
            // silencing operator receipts or leaving them unauthorized.
            //
            // MUTATION GUARD: send directly on `channel` here instead and
            // `a_kill_notice_to_a_denied_recipient_is_withheld_and_recorded_on_the_run`
            // plus
            // `a_model_supplied_recipient_cannot_bypass_send_deny_through_the_kill_notice`
            // go red.
            deliver_or_withhold_notice(
                self.security.as_ref(),
                notice_origin,
                channel.as_ref(),
                &killed_run.history,
                &rid,
                &target,
                &format!("🤖 Sub-agent `{rid}` was killed by user."),
                OutboundNotice::KillNotice,
            )
            .await;
        } else {
            tracing::warn!(
                run_id = %rid,
                "Sub-agent was killed but no recipient configured for announcement"
            );
        }

        Ok(ToolResult {
            success: true,
            output: format!("Sub-agent `{run_id}` has been killed."),
            error: None,
        })
    }

    /// Return the conversation history of a sub-agent run.
    pub(super) async fn execute_history(
        &self,
        run_id: &str,
        last_n: Option<usize>,
        max_chars_per_entry: usize,
        owner_scope: &sessions_read_model::RunOwnerScope,
        args: &serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        let runs = self.active_runs.read().await;
        let Some(run) = runs
            .iter()
            .find(|r| r.id == run_id && owner_scope.admits(r.owner_id.as_deref()))
        else {
            return Ok(self.no_runtime_run_result(run_id, args).await);
        };

        let entries = run.history.read().await;
        let usage = format_run_usage(&run.token_usage_records);
        if entries.is_empty() {
            let status = match &run.status {
                SubAgentStatus::Running => "still running, no history captured yet",
                SubAgentStatus::AwaitingInput { .. } => "awaiting approval, no history captured yet",
                SubAgentStatus::Completed(_) => "completed but history is empty",
                SubAgentStatus::Failed(_) => "failed, history may be incomplete",
            };
            return Ok(ToolResult {
                success: true,
                output: format!(
                    "No history entries for run `{run_id}` ({status}).\nmetadata: source=runtime, manageable=true, usage={usage}"
                ),
                error: None,
            });
        }

        let retained = last_n.unwrap_or(DEFAULT_HISTORY_LAST_N).min(entries.len());
        let skipped = entries.len().saturating_sub(retained);
        let lines: Vec<String> = entries
            .iter()
            .skip(skipped)
            .map(|e| {
                let ts = e.timestamp.format("%H:%M:%S").to_string();
                let preview: String = e.content.chars().take(max_chars_per_entry).collect();
                let ellipsis = if e.content.chars().count() > max_chars_per_entry {
                    "\n[entry truncated]"
                } else {
                    ""
                };
                format!("[{ts}] **{}**: {}{}", e.role, preview, ellipsis)
            })
            .collect();
        let omitted = if skipped > 0 {
            format!("\n\n[history omitted: {skipped} older entries; use last_n/max_chars_per_entry to adjust]")
        } else {
            String::new()
        };

        Ok(ToolResult {
            success: true,
            output: format!(
                "Conversation history for sub-agent `{run_id}` ({} entries, showing last {retained}):\nmetadata: source=runtime, manageable=true, usage={usage}\n\n{}{}",
                entries.len(),
                lines.join("\n\n"),
                omitted
            ),
            error: None,
        })
    }

    async fn no_runtime_run_result(&self, run_id: &str, args: &serde_json::Value) -> ToolResult {
        if self.recovered_run_exists(run_id, args).await {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Run `{run_id}` exists only as a memory-backed projected session (source=memory, manageable=false). It is not present in the current runtime registry, so sessions_spawn cannot kill, steer, or read live history for it. Use sessions_list to inspect it; treat it as stale/interrupted unless a new runtime process reattaches it."
                )),
            };
        }
        ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("No runtime run found with ID `{run_id}`.")),
        }
    }

    /// Whether the caller could recover `run_id` from memory. Asked with the
    /// caller's own scope, so another owner's recorded run is not confirmed to
    /// exist.
    async fn recovered_run_exists(&self, run_id: &str, args: &serde_json::Value) -> bool {
        let Some(fabric) = self.memory_fabric() else {
            return false;
        };
        sessions_read_model::recover_task_runs(Some(&fabric.memory()), fabric.workspace_id(), args, 100)
            .await
            .is_ok_and(|runs| runs.iter().any(|run| run.run_id == run_id))
    }

    /// Inject a steering message into a running sub-agent.
    pub(super) async fn execute_steer(
        &self,
        run_id: &str,
        message: &str,
        approval_grant: Option<&ApprovalGrant>,
        owner_scope: &sessions_read_model::RunOwnerScope,
    ) -> anyhow::Result<ToolResult> {
        // The steer sender is cloned out of the registry and the guard is
        // dropped *before* the send. The channel is bounded, so a send can now
        // park until the sub-agent drains it; parking while still holding a
        // read guard would deadlock, because the receiving loop takes a write
        // guard (`restore_running`) as its very next step after `recv`.
        let (steer_tx, run_snapshot) = {
            let runs = self.active_runs.read().await;
            let Some(run) = runs
                .iter()
                .find(|r| r.id == run_id && owner_scope.admits(r.owner_id.as_deref()))
            else {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("No run found with ID `{run_id}`.")),
                });
            };

            match &run.status {
                // A suspended (AwaitingInput) run still owns a live steer channel;
                // a plain steer cancels the inner loop (tearing down the pending
                // approval await) and re-injects the operator's message as a new
                // turn, exactly as for a running session. Structured approval
                // decisions go through `/approve` / `/deny` instead.
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => {
                    let Some(ref tx) = run.steer_tx else {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "Run `{run_id}` is running but has no steer channel (legacy run)."
                            )),
                        });
                    };
                    let operation_name = format!("sessions_spawn:steer:{run_id}");
                    if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
                        self.name(),
                        &operation_name,
                        ResourceRiskLevel::Low,
                        approval_grant,
                    ) {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(error),
                        });
                    }
                    (tx.clone(), run.clone())
                }
                SubAgentStatus::Completed(_) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Run `{run_id}` already completed; cannot steer.")),
                    });
                }
                SubAgentStatus::Failed(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Run `{run_id}` already failed ({e}); cannot steer.")),
                    });
                }
            }
        };

        // Backpressure, not loss: if the sub-agent has not drained its queue,
        // this await slows the steering caller down instead of growing the
        // queue without bound. `send` only fails once the receiver is gone.
        steer_tx
            .send(message.to_string())
            .await
            .map_err(|_| anyhow::anyhow!("Sub-agent steer channel closed unexpectedly"))?;

        self.record_active_run_task_event(
            &run_snapshot,
            "task.steered",
            json!({"message_preview": message.chars().take(500).collect::<String>()}),
        )
        .await;
        Ok(ToolResult {
            success: true,
            output: format!(
                "Steering message sent to sub-agent `{run_id}`. \
                 The agent will incorporate it at the next opportunity."
            ),
            error: None,
        })
    }
}

/// Queue depth for a sub-agent's steering channel.
///
/// This is a backpressure valve, not a rate limit: it is sized so that no
/// realistic operator or model ever reaches it, because a steering message is
/// an explicit, deliberate redirection of a running agent — a session with a
/// thousand of them pending has a runaway producer, not a busy one. When the
/// queue does fill, `sessions_spawn:steer` parks on `send().await` so the
/// producer is slowed rather than having messages dropped or the process
/// growing without bound. Each queued entry is one operator string, so the
/// worst-case memory held here is negligible next to the run's own history.
pub(crate) const STEER_CHANNEL_CAPACITY: usize = 1024;

/// Render a steer message as the user turn that gets injected into a sub-agent's
/// conversation.
///
/// Shared by both modes on purpose: task mode injects it in-process and process
/// mode injects it inside the worker, so a steered run reads identically in
/// `sessions_history` whichever way it was spawned.
pub(crate) fn steering_instruction(message: &str) -> String {
    format!("[Steering instruction from operator] {message}")
}
