use crate::channels::traits::{ChannelMessage, ChatKind};
use crate::tools::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

const DEFAULT_DELAY_SECONDS: u64 = 60;
const MAX_DELAY_SECONDS: u64 = 24 * 60 * 60;
const MAX_MESSAGE_CHARS: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScheduledInputStatus {
    Pending,
    Delivered,
    Cancelled,
    Failed,
}

impl ScheduledInputStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
struct ScheduledInputJob {
    id: u64,
    due_at: DateTime<Utc>,
    message_preview: String,
    status: ScheduledInputStatus,
    error: Option<String>,
    abort_handle: tokio::task::AbortHandle,
}

#[derive(Debug, Default)]
struct ScheduledInputState {
    jobs: Vec<ScheduledInputJob>,
}

#[derive(Clone, Default)]
pub struct ScheduledInputHandle {
    input_tx: Arc<Mutex<Option<mpsc::Sender<ChannelMessage>>>>,
    state: Arc<Mutex<ScheduledInputState>>,
    next_id: Arc<AtomicU64>,
}

impl ScheduledInputHandle {
    pub fn set_input_sender(&self, input_tx: mpsc::Sender<ChannelMessage>) {
        *self.input_tx.lock() = Some(input_tx);
    }
}

pub struct ScheduledInputTool {
    handle: ScheduledInputHandle,
}

impl ScheduledInputTool {
    #[must_use]
    pub fn new(handle: ScheduledInputHandle) -> Self {
        handle.next_id.fetch_max(1, Ordering::Relaxed);
        Self { handle }
    }

    fn execute_list(&self) -> ToolResult {
        let state = self.handle.state.lock();
        if state.jobs.is_empty() {
            return ToolResult {
                success: true,
                output: "No scheduled chat wake-ups.".to_string(),
                error: None,
            };
        }
        let mut lines = Vec::with_capacity(state.jobs.len() + 1);
        lines.push("Scheduled chat wake-ups:".to_string());
        for job in state.jobs.iter().rev().take(25).rev() {
            let error = job
                .error
                .as_deref()
                .map(|error| format!(" error={error}"))
                .unwrap_or_default();
            lines.push(format!(
                "- id={} status={} due_at={} message={}{}",
                job.id,
                job.status.as_str(),
                job.due_at.to_rfc3339(),
                job.message_preview,
                error
            ));
        }
        ToolResult {
            success: true,
            output: lines.join("\n"),
            error: None,
        }
    }

    fn execute_cancel(&self, id: u64) -> ToolResult {
        let mut state = self.handle.state.lock();
        let Some(job) = state.jobs.iter_mut().find(|job| job.id == id) else {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No scheduled chat wake-up with id={id}.")),
            };
        };
        if job.status != ScheduledInputStatus::Pending {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Scheduled chat wake-up id={} is already {}.",
                    id,
                    job.status.as_str()
                )),
            };
        }
        job.abort_handle.abort();
        job.status = ScheduledInputStatus::Cancelled;
        ToolResult {
            success: true,
            output: format!("Cancelled scheduled chat wake-up id={id}."),
            error: None,
        }
    }

    fn execute_schedule(&self, message: &str, delay_seconds: u64, priority: bool) -> ToolResult {
        let Some(input_tx) = self.handle.input_tx.lock().clone() else {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some("chat_schedule is not ready: main input sender has not been attached.".to_string()),
            };
        };
        let message = truncate_chars(message.trim(), MAX_MESSAGE_CHARS);
        if message.is_empty() {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing non-empty `message` for action=schedule.".to_string()),
            };
        }
        let delay_seconds = delay_seconds.clamp(1, MAX_DELAY_SECONDS);
        let id = self.handle.next_id.fetch_add(1, Ordering::Relaxed);
        let due_at = Utc::now() + chrono::Duration::seconds(delay_seconds as i64);
        let state = Arc::clone(&self.handle.state);
        let content = if priority {
            format!("/now {message}")
        } else {
            message.clone()
        };
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(delay_seconds)).await;
            let msg = scheduled_channel_message(id, content);
            let result = input_tx.send(msg).await;
            let mut state = state.lock();
            if let Some(job) = state.jobs.iter_mut().find(|job| job.id == id) {
                match result {
                    Ok(()) => job.status = ScheduledInputStatus::Delivered,
                    Err(error) => {
                        job.status = ScheduledInputStatus::Failed;
                        job.error = Some(error.to_string());
                    }
                }
            }
        });
        let abort_handle = task.abort_handle();
        {
            let mut state = self.handle.state.lock();
            state.jobs.push(ScheduledInputJob {
                id,
                due_at,
                message_preview: truncate_chars(&message, 160),
                status: ScheduledInputStatus::Pending,
                error: None,
                abort_handle,
            });
            if state.jobs.len() > 100 {
                let excess = state.jobs.len().saturating_sub(100);
                for job in state.jobs.drain(0..excess) {
                    if job.status == ScheduledInputStatus::Pending {
                        job.abort_handle.abort();
                    }
                }
            }
        }
        ToolResult {
            success: true,
            output: format!(
                "Scheduled chat wake-up id={id} delay_seconds={delay_seconds} due_at={} priority={priority}.",
                due_at.to_rfc3339()
            ),
            error: None,
        }
    }
}

#[async_trait]
impl Tool for ScheduledInputTool {
    fn name(&self) -> &str {
        "chat_schedule"
    }

    fn description(&self) -> &str {
        "Schedule a future message back into the current PRX chat main session. Use this for dispatcher self-wake observation loops after the current turn completes."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["schedule", "list", "cancel"],
                    "default": "schedule",
                    "description": "schedule creates a future main-session wake-up; list shows pending/delivered/cancelled wake-ups; cancel aborts a pending wake-up."
                },
                "message": {
                    "type": "string",
                    "description": "Message to inject into the main chat session when the wake-up fires."
                },
                "delay_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_DELAY_SECONDS,
                    "default": DEFAULT_DELAY_SECONDS,
                    "description": "Delay before injecting the message. Defaults to 60 seconds."
                },
                "priority": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, injects the wake-up as a priority /now message."
                },
                "id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Wake-up id for cancel."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(|value| value.as_str())
            .unwrap_or("schedule");
        let result = match action {
            "schedule" => {
                let Some(message) = args.get("message").and_then(|value| value.as_str()) else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Missing `message` for action=schedule.".to_string()),
                    });
                };
                let delay_seconds = args
                    .get("delay_seconds")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(DEFAULT_DELAY_SECONDS);
                let priority = args.get("priority").and_then(|value| value.as_bool()).unwrap_or(false);
                self.execute_schedule(message, delay_seconds, priority)
            }
            "list" => self.execute_list(),
            "cancel" => {
                let Some(id) = args.get("id").and_then(|value| value.as_u64()) else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Missing numeric `id` for action=cancel.".to_string()),
                    });
                };
                self.execute_cancel(id)
            }
            other => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Invalid chat_schedule action `{other}`.")),
            },
        };
        Ok(result)
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Core
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Scheduling, ToolCategory::Automation]
    }
}

fn scheduled_channel_message(id: u64, content: String) -> ChannelMessage {
    ChannelMessage {
        id: format!("chat-schedule-{id}"),
        sender: "prx-scheduler".to_string(),
        reply_target: "terminal".to_string(),
        content,
        channel: "terminal".to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs()),
        chat_kind: ChatKind::Dm,
        chat_title: Some("Scheduled chat wake-up".to_string()),
        sender_display: Some("PRX Scheduler".to_string()),
        thread_ts: None,
        mentioned_uuids: Vec::new(),
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: true,
    }
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = input.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::traits::Tool;

    #[tokio::test]
    async fn schedule_delivers_message_to_input_sender() {
        let handle = ScheduledInputHandle::default();
        let (tx, mut rx) = mpsc::channel(2);
        handle.set_input_sender(tx);
        let tool = ScheduledInputTool::new(handle.clone());

        let result = tool
            .execute(json!({
                "action": "schedule",
                "message": "observe sessions now",
                "delay_seconds": 1
            }))
            .await
            .unwrap();

        assert!(result.success, "{result:?}");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("scheduled message should arrive")
            .expect("input channel remains open");
        assert_eq!(msg.sender, "prx-scheduler");
        assert_eq!(msg.content, "observe sessions now");

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let list = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(list.output.contains("status=delivered"), "{}", list.output);
    }

    #[tokio::test]
    async fn cancel_prevents_pending_delivery() {
        let handle = ScheduledInputHandle::default();
        let (tx, mut rx) = mpsc::channel(2);
        handle.set_input_sender(tx);
        let tool = ScheduledInputTool::new(handle.clone());

        let result = tool
            .execute(json!({
                "action": "schedule",
                "message": "should not arrive",
                "delay_seconds": 60
            }))
            .await
            .unwrap();
        assert!(result.success, "{result:?}");
        let id = result
            .output
            .split("id=")
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .expect("schedule output includes id");

        let cancel = tool.execute(json!({"action": "cancel", "id": id})).await.unwrap();
        assert!(cancel.success, "{cancel:?}");
        assert!(rx.try_recv().is_err());

        let list = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(list.output.contains("status=cancelled"), "{}", list.output);
    }

    #[tokio::test]
    async fn first_schedule_id_matches_schema_minimum() {
        let handle = ScheduledInputHandle::default();
        let (tx, _rx) = mpsc::channel(2);
        handle.set_input_sender(tx);
        let tool = ScheduledInputTool::new(handle);

        let result = tool
            .execute(json!({
                "action": "schedule",
                "message": "schema-visible id",
                "delay_seconds": 60
            }))
            .await
            .unwrap();

        assert!(result.success, "{result:?}");
        assert!(result.output.contains("id=1 "), "{}", result.output);
    }

    #[tokio::test]
    async fn pruned_pending_jobs_are_aborted_before_becoming_invisible() {
        let handle = ScheduledInputHandle::default();
        let (tx, mut rx) = mpsc::channel(200);
        handle.set_input_sender(tx);
        let tool = ScheduledInputTool::new(handle);

        for idx in 0..101 {
            let result = tool
                .execute(json!({
                    "action": "schedule",
                    "message": format!("wake-{idx}"),
                    "delay_seconds": 1
                }))
                .await
                .unwrap();
            assert!(result.success, "{result:?}");
        }

        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let mut delivered = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            delivered.push(msg.content);
        }

        assert_eq!(delivered.len(), 100, "{delivered:?}");
        assert!(
            !delivered.iter().any(|content| content == "wake-0"),
            "pruned wake-up must not be delivered after it is no longer listable/cancellable: {delivered:?}"
        );
        assert!(delivered.iter().any(|content| content == "wake-100"));
    }
}
