//! Per-turn channel approval, keyed by the turn run id.
//!
//! A rich channel installs its resolver for the run. Tool execution reads it
//! by the same id, including from tasks that do not inherit task-locals.

use super::turn_contracts::{ApprovalDecision, ApprovalResolver};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnContext {
    pub conversation_id: String,
    pub turn_id: String,
}

pub(crate) struct Installed {
    turn_context: Option<TurnContext>,
    resolver: Arc<dyn ApprovalResolver>,
    force_ask: bool,
    effective_full: bool,
    parent_run_id: Option<String>,
}

fn registry() -> &'static Mutex<HashMap<String, Installed>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Installed>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

tokio::task_local! {
    static ACTIVE: Arc<Installed>;
}

/// Remember that `run_id` must ask `resolver` before a tool runs.
#[cfg(test)]
pub fn install(run_id: &str, resolver: Arc<dyn ApprovalResolver>, force_ask: bool, effective_full: bool) {
    registry().lock().insert(
        run_id.to_string(),
        Installed {
            turn_context: None,
            resolver,
            force_ask,
            effective_full,
            parent_run_id: None,
        },
    );
}

pub fn current_installed() -> Option<Arc<Installed>> {
    ACTIVE.try_with(Arc::clone).ok()
}

/// Keep `installed` visible to tool calls polled on this task.
///
/// The envelope run id and the turn id are the same value when admission
/// preserves it. A spawned call that only has the task scope still resolves
/// against that turn, not against whichever id the executor copied.
pub async fn scope_installed<F>(installed: Arc<Installed>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    ACTIVE.scope(installed, fut).await
}

/// Install explicit context before entering any spawned delegation tasks.
pub fn install_for_turn(
    run_id: &str,
    conversation_id: &str,
    resolver: Arc<dyn ApprovalResolver>,
    force_ask: bool,
    effective_full: bool,
) -> Arc<Installed> {
    let make = || Installed {
        resolver: Arc::clone(&resolver),
        force_ask,
        effective_full,
        parent_run_id: None,
        turn_context: Some(TurnContext {
            conversation_id: conversation_id.to_owned(),
            turn_id: run_id.to_owned(),
        }),
    };
    registry().lock().insert(run_id.to_owned(), make());
    Arc::new(make())
}

pub fn active_turn() -> Option<TurnContext> {
    ACTIVE
        .try_with(|installed| installed.turn_context.clone())
        .ok()
        .flatten()
}

/// Drop the resolver when the turn ends.
pub fn clear(run_id: &str) {
    registry().lock().remove(run_id);
}

/// Resolver installed for `run_id`, if the turn is still active.
pub struct ApprovalContext {
    pub resolver: Arc<dyn ApprovalResolver>,
    pub force_ask: bool,
    pub effective_full: bool,
}

pub fn lookup(run_id: &str) -> Option<ApprovalContext> {
    registry().lock().get(run_id).map(context_of)
}

fn context_of(installed: &Installed) -> ApprovalContext {
    ApprovalContext {
        resolver: Arc::clone(&installed.resolver),
        force_ask: installed.force_ask,
        effective_full: installed.effective_full,
    }
}

/// Registry hit for `run_id`, its parent run, or the resolver scoped to this task.
pub fn lookup_for_call(run_id: &str, parent_run_id: Option<&str>) -> Option<ApprovalContext> {
    if let Some(found) = lookup(run_id) {
        return Some(found);
    }
    if let Some(parent) = parent_run_id.and_then(lookup) {
        return Some(parent);
    }
    ACTIVE
        .try_with(|installed| {
            if let Some(parent) = installed.parent_run_id.as_deref().and_then(lookup) {
                return parent;
            }
            context_of(installed)
        })
        .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::agent::turn_contracts::{ApprovalDecision, ApprovalResolver};
    use crate::approval::ApprovalRequest;

    struct RecordingResolver;

    #[async_trait::async_trait]
    impl ApprovalResolver for RecordingResolver {
        async fn resolve(&self, _request: &ApprovalRequest, _channel: &str) -> ApprovalDecision {
            ApprovalDecision::Deny
        }
    }

    #[test]
    fn lookup_for_call_uses_the_parent_run() {
        let parent = "parent-run";
        let child = "child-run";
        install(parent, Arc::new(RecordingResolver), true, false);
        let found = lookup_for_call(child, Some(parent)).expect("test: parent gate");
        assert!(found.force_ask);
        assert!(!found.effective_full);
        clear(parent);
        assert!(lookup_for_call(child, Some(parent)).is_none());
    }
}

/// Resolve one tool call. `Grant` is the only approval.
pub async fn resolve_tool(
    run_id: &str,
    tool_name: &str,
    arguments: &serde_json::Value,
    channel: &str,
) -> Option<ApprovalDecision> {
    let resolver = lookup_for_call(run_id, None)?.resolver;
    let request = crate::approval::ApprovalRequest::new(tool_name, arguments.clone());
    Some(resolver.resolve(&request, channel).await)
}

/// Capture runtime-owned capabilities before crossing a task boundary.
/// Boxing also keeps nested delegation state off the worker's poll stack.
pub(crate) fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let installed = current_installed();
    let memory = crate::memory::active_view::current();
    let future = Box::pin(future);
    tokio::spawn(async move {
        let work = Box::pin(async move {
            if let Some(installed) = installed {
                scope_installed(installed, future).await
            } else {
                future.await
            }
        });
        if let Some(memory) = memory {
            crate::memory::active_view::scope(memory, work).await
        } else {
            work.await
        }
    })
}
