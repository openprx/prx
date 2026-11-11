//! Dormant sender-only tool. No tool factory registers this constructor.
use super::traits::{Tool, ToolCategory, ToolResult};
use crate::messaging::mailbox::MailboxError;
use crate::messaging::send::{MailboxSender, parse_target, send_result, target_schema};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct PrxMessageTool {
    sender: Arc<dyn MailboxSender>,
}
impl PrxMessageTool {
    pub fn new(sender: Arc<dyn MailboxSender>) -> Self {
        Self { sender }
    }
}
#[async_trait]
impl Tool for PrxMessageTool {
    fn name(&self) -> &str {
        "prx_message"
    }
    fn description(&self) -> &str {
        "Send one Data message to a session mailbox."
    }
    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Communication]
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"properties":{"to":target_schema(),"text":{"type":"string","minLength":1,"maxLength":262144},"mime":{"type":"string","enum":["text/plain","text/markdown"],"default":"text/plain"}},"required":["to","text"]})
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let object = args.as_object().ok_or(MailboxError::InvalidMetadata)?;
        if object.keys().any(|key| !matches!(key.as_str(), "to" | "text" | "mime")) {
            return Err(MailboxError::InvalidMetadata.into());
        }
        let target = parse_target(
            object.get("to").ok_or(MailboxError::InvalidAddress)?,
            self.sender.source(),
        )?;
        let text = object
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or(MailboxError::InvalidMetadata)?;
        let mime = match object.get("mime") {
            None => "text/plain",
            Some(Value::String(mime)) if matches!(mime.as_str(), "text/plain" | "text/markdown") => mime,
            _ => return Err(MailboxError::InvalidMime.into()),
        };
        let result = self.sender.send(target, text.to_owned(), mime.to_owned()).await;
        let success = result.is_ok();
        let error = result.as_ref().err().map(ToString::to_string);
        Ok(ToolResult {
            success,
            output: send_result(result).to_string(),
            error,
        })
    }
}
