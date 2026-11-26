//! The named reduction points of a turn, and the turn finalizer.
//!
//! A turn reduces its request at three points: before the loop opens
//! ([`pre_turn`]), at the start of each provider round ([`next_round`]), and
//! after a round's tool results are appended ([`after_tools`]). Each point runs
//! a fixed list of reduction steps through [`ladder`]. This module only names
//! those calls; it has no behaviour of its own.

use anyhow::Result;

use crate::context::reduce::clear::ClearOldResults;
use crate::context::reduce::compact::{PreTurnCompact, RoundCompact};
use crate::context::reduce::engine::{AfterTools, PreTurn, Round};
use crate::context::reduce::trim::{MidTurnTrim, RoundTrim};
use crate::context::reduce::{Report, ladder};

/// Reduce the request before the turn's loop opens.
pub(crate) async fn pre_turn(state: &mut PreTurn<'_>) -> Result<Report> {
    ladder(&[&ClearOldResults, &PreTurnCompact], state).await
}

/// Reduce the request at the start of a provider round, after the tool surface
/// is measured.
pub(crate) async fn next_round(state: &mut Round<'_>) -> Result<Report> {
    ladder(&[&ClearOldResults, &RoundCompact, &RoundTrim], state).await
}

/// Reduce the request after a round's tool results were appended.
pub(crate) async fn after_tools(state: &mut AfterTools<'_>) -> Result<Report> {
    ladder(&[&MidTurnTrim], state).await
}
