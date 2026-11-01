//! Per-turn channel approval, keyed by the turn run id.
//!
//! A rich channel installs its resolver for the run. Tool execution reads it
//! by the same id, including from tasks that do not inherit task-locals.

use super::turn_contracts::{ApprovalDecision, ApprovalResolver};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

pub(crate) struct Installed {
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
pub fn install(run_id: &str, resolver: Arc<dyn ApprovalResolver>, force_ask: bool, effective_full: bool) {
    registry().lock().insert(
        run_id.to_string(),
        Installed {
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

/// Build the installed record the turn scopes and the registry share.
pub fn installed(resolver: Arc<dyn ApprovalResolver>, force_ask: bool, effective_full: bool) -> Arc<Installed> {
    Arc::new(Installed {
        resolver,
        force_ask,
        effective_full,
        parent_run_id: None,
    })
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
