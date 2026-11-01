//! Validation for `reply.surface` blocks and actions.
//!
//! Surfaces are declarative data. Script, HTML, and non-http(s) URLs are rejected.
//! A rejection becomes `turn.status` `failed` with code `surface_rejected`.

use super::super::rich::{SurfaceOp, SurfaceReply};
use crate::config::RamfluxSurfaceLimits;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchemaError {
    #[error("surface is missing fallback_text")]
    MissingFallback,
    #[error("surface exceeds the block, action, or byte limit")]
    TooLarge,
    #[error("surface block kind {0} is not allowed")]
    BlockKind(String),
    #[error("surface contains script or html")]
    Markup,
    #[error("surface url must be http or https")]
    Url,
}

/// Validate one surface reply against the channel limits.
///
/// # Errors
///
/// Returns [`SchemaError`] when the reply is not safe to hand to the agent-host.
pub fn validate(reply: &SurfaceReply, limits: &RamfluxSurfaceLimits, allowed: &[String]) -> Result<(), SchemaError> {
    let blocks = reply.blocks.as_deref().unwrap_or(&[]);
    let actions = reply.actions.as_deref().unwrap_or(&[]);
    // Text-stream frames carry plain text. Markup and URL rules apply only to
    // blocks and actions, so a long reply or one that mentions `<script` is
    // still delivered as text.
    if matches!(reply.op, SurfaceOp::StreamAppend) || (blocks.is_empty() && actions.is_empty()) {
        return Ok(());
    }
    if reply.fallback_text.as_deref().unwrap_or("").trim().is_empty() {
        return Err(SchemaError::MissingFallback);
    }
    if u32::try_from(blocks.len()).unwrap_or(u32::MAX) > limits.max_blocks
        || u32::try_from(actions.len()).unwrap_or(u32::MAX) > limits.max_actions
    {
        return Err(SchemaError::TooLarge);
    }
    let mut bytes = reply.fallback_text.as_deref().unwrap_or("").len();
    bytes = bytes.saturating_add(reply.text_delta.as_deref().unwrap_or("").len());
    for block in blocks {
        if !allowed.iter().any(|kind| kind == &block.kind) {
            return Err(SchemaError::BlockKind(block.kind.clone()));
        }
        let rendered = block.data.to_string();
        bytes = bytes.saturating_add(block.id.len()).saturating_add(rendered.len());
        reject_markup(&block.id)?;
        reject_markup(&rendered)?;
        reject_urls(&rendered)?;
    }
    for action in actions {
        bytes = bytes.saturating_add(action.id.len()).saturating_add(action.label.len());
        reject_markup(&action.label)?;
        reject_urls(&action.label)?;
    }
    if u32::try_from(bytes).unwrap_or(u32::MAX) > limits.max_bytes {
        return Err(SchemaError::TooLarge);
    }
    if let Some(text) = reply.fallback_text.as_deref() {
        reject_markup(text)?;
    }
    Ok(())
}

fn reject_markup(text: &str) -> Result<(), SchemaError> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("<script")
        || lower.contains("<iframe")
        || lower.contains("<html")
        || lower.contains("javascript:")
        || lower.contains("onerror=")
        || lower.contains("onclick=")
    {
        return Err(SchemaError::Markup);
    }
    Ok(())
}

fn reject_urls(text: &str) -> Result<(), SchemaError> {
    let lower = text.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(index) = rest.find("://") {
        let prefix = rest.get(..index).unwrap_or("");
        let start = prefix
            .rfind(|ch: char| ch.is_whitespace() || ch == '"' || ch == '\'')
            .map_or(0, |pos| pos.saturating_add(1));
        let scheme = prefix.get(start..).unwrap_or("");
        if scheme != "http" && scheme != "https" {
            return Err(SchemaError::Url);
        }
        let after = index.saturating_add(3);
        rest = rest.get(after..).unwrap_or("");
    }
    Ok(())
}
