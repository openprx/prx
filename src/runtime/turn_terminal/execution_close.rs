//! Closing a turn's execution at its terminal (R25 terminal row).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::tools::capability::execution_trace::{ExecutionTrace, ExecutionTraceSnapshot};
use crate::tools::capability::search_tool::DiscoveryState;

/// Close execution, clear discovery offers (waiting for an admission that
/// entered before closure, so terminal evidence sees its final metadata),
/// and seal the execution trace.
pub(crate) fn terminal_execution_trace(
    closed: &AtomicBool,
    discovery: &DiscoveryState,
    trace: &OnceLock<ExecutionTrace>,
) -> ExecutionTraceSnapshot {
    closed.store(true, Ordering::Release);
    discovery.close();
    trace.get_or_init(Default::default).seal()
}
