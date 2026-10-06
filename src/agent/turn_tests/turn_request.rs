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
use crate::memory::Memory;
use std::sync::Arc;

/// The ledger is runtime infrastructure, not a document-ingest side benefit:
/// entry points that never resolve an ingest scope (session workers, delegated
/// sub-agents, spawned sessions) must still get one, or every side-effecting
/// tool they try to run is refused.
#[test]
fn tool_loop_memory_carries_a_ledger_even_without_a_document_ingest_scope() {
    let dir = tempfile::TempDir::new().expect("test: tempdir");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: sqlite memory"));
    let runtime = ToolLoopMemory::new(&memory, dir.path(), None);
    assert!(runtime.ingest().is_none(), "this entry point has no ingest scope");
    assert!(
        runtime.ledger.is_some(),
        "the ledger must not depend on whether document ingest is configured"
    );
}

/// The same holds for a memory backend that keeps no event log of its own.
#[test]
fn tool_loop_memory_resolves_a_ledger_for_a_backend_without_an_event_log() {
    let dir = tempfile::TempDir::new().expect("test: tempdir");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::MarkdownMemory::new(dir.path()));
    let runtime = ToolLoopMemory::new(&memory, dir.path(), None);
    let ledger = runtime.ledger.as_ref().expect("test: ledger");
    assert_eq!(ledger.name(), "sqlite");
}
