//! Shared execution-state colors. Text and glyphs continue to carry meaning.
use super::ToolStatus;
use crate::chat::action::ProviderWorkerRowState;
use ratatui::style::Color;

pub(super) const ACTIVE: Color = Color::Cyan;
pub(super) const SUCCESS: Color = Color::Green;
pub(super) const FAILURE: Color = Color::Red;
pub(super) const ATTENTION: Color = Color::Yellow;
pub(super) const CONTEXT: Color = Color::DarkGray;
pub(super) const INPUT: Color = Color::Blue;

pub(super) const fn tool_color(status: ToolStatus) -> Color {
    match status {
        ToolStatus::Running => ACTIVE,
        ToolStatus::Done => SUCCESS,
        ToolStatus::Error => FAILURE,
        ToolStatus::Cancelled => ATTENTION,
    }
}

/// Labels from the session/worker projection, never inferred from tool output.
pub(super) fn session_color(status: &str) -> Color {
    match status {
        "running" => ACTIVE,
        "completed" | "committed" => SUCCESS,
        "failed" => FAILURE,
        "needs-input" | "cancelling" | "cancelled" | "awaiting-commit" | "awaiting_commit" => ATTENTION,
        _ => CONTEXT,
    }
}

pub(super) const fn worker_color(state: ProviderWorkerRowState) -> Color {
    match state {
        ProviderWorkerRowState::Running => ACTIVE,
        ProviderWorkerRowState::Committed => SUCCESS,
        ProviderWorkerRowState::Failed => FAILURE,
        ProviderWorkerRowState::Cancelling
        | ProviderWorkerRowState::Cancelled
        | ProviderWorkerRowState::AwaitingCommit => ATTENTION,
    }
}
