//! Rendering of an MCP `tools/call` result into the text the model receives.
//!
//! Every content block the server returns is kept: text is passed through,
//! resource links and embedded resources are named by URI, and image, audio
//! and blob content becomes a placeholder that states its type and size. No
//! base64 payload is ever copied into the rendered text or into a log line.

#[cfg(test)]
use crate::tools::mcp::McpTool;
use anyhow::bail;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock, ResourceContents};
#[cfg(test)]
use serde::Deserialize;

/// Placeholder text for a result with neither content nor structured content.
const EMPTY_RESULT: &str = "(empty result)";

/// Render a completed tool call as `(success, text)`.
///
/// Blocks are joined with `\n` in the order the server sent them. Structured
/// content is appended unless a text block already carries the same JSON
/// value. `isError: true` makes the call a failure but keeps its text.
pub(super) fn render_call_result(result: &CallToolResult) -> (bool, String) {
    let mut parts: Vec<String> = result.content.iter().map(render_block).collect();

    if let Some(structured) = result.structured_content.as_ref()
        && !text_carries_value(&result.content, structured)
    {
        parts.push(format!("[structured content]\n{structured}"));
    }

    let output = if parts.is_empty() {
        EMPTY_RESULT.to_string()
    } else {
        parts.join("\n")
    };
    (!result.is_error.unwrap_or(false), output)
}

/// Render the answer to one `tools/call` request.
///
/// `InputRequired` (MRTR) and `Task` answers are refused with a clear error:
/// PRX cannot yet answer elicitation, sampling or roots requests, nor poll
/// tasks. The error does not close the session the call ran on.
pub(super) fn render_call_response(server_name: &str, response: CallToolResponse) -> anyhow::Result<(bool, String)> {
    match response {
        CallToolResponse::Complete(result) => Ok(render_call_result(&result)),
        CallToolResponse::InputRequired(_) => bail!(
            "MCP server '{server_name}' asked for client input (elicitation/sampling/roots); PRX does not support MCP input requests yet"
        ),
        CallToolResponse::Task(_) => bail!("MCP server returned a task handle; PRX does not support MCP tasks yet"),
        _ => bail!("MCP server returned a response type PRX does not understand"),
    }
}

#[cfg(test)]
impl McpTool {
    /// Value form of [`render_call_result`], for a result already held as JSON.
    /// Only the tests use it: the call paths render the typed response.
    ///
    /// A value that is not a call result is reported as a failure with the
    /// parse error, never as a success with its raw JSON.
    pub(super) fn extract_call_success_and_output(value: &serde_json::Value) -> (bool, String) {
        match CallToolResult::deserialize(value) {
            Ok(result) => render_call_result(&result),
            Err(error) => (false, format!("MCP tool result could not be read: {error}")),
        }
    }
}

fn render_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::ResourceLink(link) => match link.mime_type.as_deref() {
            Some(mime) => format!("[resource link] {} {} ({mime})", link.name, link.uri),
            None => format!("[resource link] {} {}", link.name, link.uri),
        },
        ContentBlock::Resource(embedded) => render_resource_contents(&embedded.resource),
        ContentBlock::Image(image) => format!(
            "[image: {}, {} bytes base64]",
            image.mime_type,
            image.data.len()
        ),
        ContentBlock::Audio(audio) => format!(
            "[audio: {}, {} bytes base64]",
            audio.mime_type,
            audio.data.len()
        ),
        _ => "[unsupported content block]".to_string(),
    }
}

/// Render one resource content: text in full, a blob as a placeholder with its
/// size and no base64 payload. Shared by embedded resources and `resources/read`.
pub(super) fn render_resource_contents(contents: &ResourceContents) -> String {
    match contents {
        ResourceContents::TextResourceContents { uri, text, .. } => format!("[resource {uri}]\n{text}"),
        ResourceContents::BlobResourceContents { uri, mime_type, blob, .. } => format!(
            "[resource {uri}: {}, {} bytes base64]",
            mime_type.as_deref().unwrap_or("unknown"),
            blob.len()
        ),
        _ => "[unsupported embedded resource]".to_string(),
    }
}

/// Whether some text block already holds `structured` as JSON, so the
/// structured content would only repeat it.
fn text_carries_value(content: &[ContentBlock], structured: &serde_json::Value) -> bool {
    content.iter().any(|block| match block {
        ContentBlock::Text(text) => {
            serde_json::from_str::<serde_json::Value>(&text.text).is_ok_and(|parsed| &parsed == structured)
        }
        _ => false,
    })
}
