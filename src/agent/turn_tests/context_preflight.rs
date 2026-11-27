#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;
#[cfg(test)]
use crate::agent::compaction::post_compaction_refresh_aside;
use crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER;
use crate::agent::turn_tests::{SystemSummaryProvider, first_orphan_tool_index, tool_roundtrip_history_fixture};
use crate::providers::ChatMessage;

/// The refresh aside compaction writes is recognised by the one marker
/// both sides share, so the producer and the matcher cannot drift apart.
#[test]
fn the_refresh_aside_compaction_writes_is_loop_written() {
    let aside = post_compaction_refresh_aside();
    assert!(aside.content.starts_with(POST_COMPACTION_REFRESH_MARKER));
    assert!(is_loop_written_user_note(&aside));
    assert!(!is_loop_written_user_note(&ChatMessage::user(
        "[Post-compaction notes] which should I keep?"
    )));
}
