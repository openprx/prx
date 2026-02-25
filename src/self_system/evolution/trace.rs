use std::future::Future;
use uuid::Uuid;

tokio::task_local! {
    static TRACE_CONTEXT: TraceContext;
}

/// Per-request trace identifiers propagated through async call chains.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TraceContext {
    pub trace_id: String,
    pub experiment_id: String,
}

impl TraceContext {
    /// Create a new context with generated UUIDv7 IDs.
    pub fn new() -> Self {
        Self {
            trace_id: generate_trace_id(),
            experiment_id: generate_experiment_id(),
        }
    }
}

impl Default for TraceContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a sortable UUIDv7 trace ID.
pub fn generate_trace_id() -> String {
    Uuid::now_v7().to_string()
}

/// Generate a sortable UUIDv7 experiment ID.
pub fn generate_experiment_id() -> String {
    Uuid::now_v7().to_string()
}

/// Run an async block within a trace context.
pub async fn with_trace<F, Fut, T>(context: TraceContext, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    TRACE_CONTEXT.scope(context, f()).await
}

/// Get the current active trace context if present.
pub fn current_trace() -> Option<TraceContext> {
    match TRACE_CONTEXT.try_with(Clone::clone) {
        Ok(ctx) => Some(ctx),
        Err(err) => {
            tracing::debug!(error = %err, "trace context is not available in current task");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_distinct_v7_ids() {
        let a = generate_trace_id();
        let b = generate_trace_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(b.len(), 36);
    }

    #[tokio::test]
    async fn with_trace_propagates_context() {
        let ctx = TraceContext::new();
        let trace_id = ctx.trace_id.clone();
        let experiment_id = ctx.experiment_id.clone();

        let observed = with_trace(ctx, || async { current_trace().unwrap() }).await;
        assert_eq!(observed.trace_id, trace_id);
        assert_eq!(observed.experiment_id, experiment_id);
    }
}
