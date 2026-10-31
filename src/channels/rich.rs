//! Rich channel extension types.
//!
//! These types carry identity, surfaces, tasks, and tool progress that do not
//! fit in [`ChannelMessage`]. They do not perform IO. Only the ramflux channel
//! implements [`RichChannel`]; every method has a default except the two that
//! define the bridge.

use super::traits::{Channel, ChannelMessage, ChatKind};
use crate::agent::turn_contracts::ApprovalResolver;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Who sent an inbound ramflux event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sender {
    pub principal_id: String,
    pub device_id: String,
    pub kind: SenderKind,
    #[serde(default)]
    pub display_name: Option<String>,
    pub trust: Trust,
    pub is_self: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    Human,
    HeadlessAi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Unverified,
    Verified,
    Stale,
}

/// Conversation the event belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chat {
    pub conversation_id: String,
    pub kind: ChatRoomKind,
    #[serde(default)]
    pub title: Option<String>,
    pub member_count: u32,
    pub mentioned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRoomKind {
    Dm,
    Group,
}

/// Encrypted object reference. Bytes are fetched with `attachment.fetch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRef {
    pub object_id: String,
    pub mime: String,
    pub size: u64,
    #[serde(default)]
    pub name: Option<String>,
    pub sha256: String,
}

/// One declarative surface block. `data` is kind-specific and untrusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceBlock {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// A button or other declarative action. No code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceAction {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub risk: Option<String>,
    #[serde(default)]
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceOp {
    Create,
    Patch,
    StreamAppend,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEventKind {
    Started,
    Progress,
    Finished,
    Failed,
}

/// Tool progress projected from the agent loop. Summaries are already redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEvent {
    pub kind: ToolEventKind,
    pub tool_call_id: String,
    pub tool_name: String,
    pub iteration: u64,
    pub duration_ms: Option<u64>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Started,
    Thinking,
    ToolRunning,
    AwaitingApproval,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnError {
    pub code: String,
    pub message: String,
}

/// Typed turn lifecycle notice. `message` is an operator code, never user content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatus {
    pub conversation_id: String,
    pub turn_id: String,
    pub state: TurnState,
    pub error: Option<TurnError>,
}

/// Inbound user message mapped onto the existing channel pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichMessage {
    pub conversation_id: String,
    pub message_id: String,
    pub sender: Sender,
    pub chat: Chat,
    pub text: String,
    pub attachments: Vec<AttachmentRef>,
    pub reply_to: Option<String>,
    pub timestamp_ms: u64,
}

impl RichMessage {
    /// Project this message onto [`ChannelMessage`].
    ///
    /// `sender` is the principal id, `reply_target` is the conversation id,
    /// and `chat_kind` follows [`Chat::kind`].
    #[must_use]
    pub fn to_channel_message(&self) -> ChannelMessage {
        let chat_kind = match self.chat.kind {
            ChatRoomKind::Dm => ChatKind::Dm,
            ChatRoomKind::Group => ChatKind::Group,
        };
        ChannelMessage {
            id: self.message_id.clone(),
            sender: self.sender.principal_id.clone(),
            reply_target: self.conversation_id.clone(),
            content: self.text.clone(),
            channel: "ramflux".to_string(),
            timestamp: self.timestamp_ms,
            chat_kind,
            chat_title: self.chat.title.clone(),
            sender_display: self.sender.display_name.clone(),
            thread_ts: None,
            mentioned_uuids: Vec::new(),
            mentioned: self.chat.mentioned,
            is_group_hint: chat_kind == ChatKind::Group,
            sender_is_bot: self.sender.kind == SenderKind::HeadlessAi,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceActionInbound {
    pub conversation_id: String,
    pub sender: Sender,
    pub surface_id: String,
    pub version: u64,
    pub surface_hash: String,
    pub action_id: String,
    pub values: serde_json::Value,
    pub args_hash: String,
    pub message_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEventInbound {
    pub conversation_id: String,
    pub sender: Sender,
    pub task_id: String,
    pub version: u64,
    pub kind: String,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    AgentAdded,
    AgentRemoved,
    AgentRevoked,
    MemberJoined,
    MemberLeft,
    ConversationRenamed,
    AgentOffline,
    AgentOnline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlInbound {
    pub conversation_id: String,
    pub kind: ControlKind,
    pub actor: Option<Sender>,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDeletedInbound {
    pub conversation_id: String,
    pub message_id: String,
    pub deleted_by: Sender,
    pub timestamp_ms: u64,
}

/// Events the agent-host delivers to prx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RichInbound {
    Message(RichMessage),
    SurfaceAction(SurfaceActionInbound),
    TaskEvent(TaskEventInbound),
    Control(ControlInbound),
    MessageDeleted(MessageDeletedInbound),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextReply {
    pub conversation_id: String,
    pub turn_id: String,
    pub text: String,
    pub is_final: bool,
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceReply {
    pub conversation_id: String,
    pub turn_id: String,
    pub op: SurfaceOp,
    pub surface_id: String,
    pub version: u64,
    pub base_hash: Option<String>,
    pub blocks: Option<Vec<SurfaceBlock>>,
    pub actions: Option<Vec<SurfaceAction>>,
    pub ops: Option<serde_json::Value>,
    pub block_id: Option<String>,
    pub text_delta: Option<String>,
    pub is_final: Option<bool>,
    pub fallback_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStep {
    pub id: String,
    pub title: String,
    pub state: String,
    pub needs_me: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskUpdateReply {
    pub conversation_id: String,
    pub turn_id: String,
    pub task_id: String,
    pub version: u64,
    pub state: String,
    pub steps: Vec<TaskStep>,
    pub result_ref: Option<String>,
    pub result_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskProposeReply {
    pub conversation_id: String,
    pub turn_id: String,
    pub task_id: String,
    pub title: String,
    pub steps: Vec<TaskStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentReply {
    pub conversation_id: String,
    pub turn_id: String,
    pub bytes_b64: Option<String>,
    pub stream_id: Option<String>,
    pub mime: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypingReply {
    pub conversation_id: String,
    pub turn_id: String,
    pub on: bool,
}

/// Outbound rich reply. Every variant carries the turn that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RichReply {
    Text(TextReply),
    Surface(SurfaceReply),
    TaskUpdate(TaskUpdateReply),
    TaskPropose(TaskProposeReply),
    Attachment(AttachmentReply),
    Typing(TypingReply),
    Status(TurnStatus),
}

/// Extension implemented by channels that speak structured turns.
///
/// `Arc<dyn Channel>` reaches this through [`Channel::as_rich`]. Channels that
/// do not implement it keep every default.
#[async_trait]
pub trait RichChannel: Channel {
    /// Receive structured inbound events until the channel stops.
    async fn listen_rich(&self, tx: mpsc::Sender<RichInbound>) -> anyhow::Result<()>;

    /// Send one structured reply on the bridge.
    async fn send_rich(&self, reply: &RichReply) -> anyhow::Result<()>;

    /// Resolver for supervised tool calls. `None` keeps the caller's default.
    fn approval_resolver(&self) -> Option<Arc<dyn ApprovalResolver>> {
        None
    }

    /// Forward one tool-loop event. The default ignores it.
    async fn on_tool_event(&self, _event: &ToolEvent) {}

    /// Register the cancellation token for a live turn.
    fn bind_turn(&self, _conversation_id: &str, _turn_id: &str, _token: tokio_util::sync::CancellationToken) {}

    /// Drop a turn registration. Cancelling is separate.
    fn unbind_turn(&self, _conversation_id: &str, _turn_id: &str) {}
}
