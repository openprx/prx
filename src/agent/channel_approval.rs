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
}

fn registry() -> &'static Mutex<HashMap<String, Installed>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Installed>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

tokio::task_local! {
    static ACTIVE: Arc<Installed>;
}

/// Remember that `run_id` must ask `resolver` before a tool runs.
pub fn install(run_id: &str, resolver: Arc<dyn ApprovalResolver>, force_ask: bool) {
    registry()
        .lock()
        .insert(run_id.to_string(), Installed { resolver, force_ask });
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
pub fn installed(resolver: Arc<dyn ApprovalResolver>, force_ask: bool) -> Arc<Installed> {
    Arc::new(Installed { resolver, force_ask })
}

/// Drop the resolver when the turn ends.
pub fn clear(run_id: &str) {
    registry().lock().remove(run_id);
}

/// Resolver installed for `run_id`, if the turn is still active.
pub fn lookup(run_id: &str) -> Option<(Arc<dyn ApprovalResolver>, bool)> {
    registry()
        .lock()
        .get(run_id)
        .map(|installed| (Arc::clone(&installed.resolver), installed.force_ask))
}

/// Registry hit for `run_id`, otherwise the resolver scoped to this task.
pub fn lookup_for_call(run_id: &str) -> Option<(Arc<dyn ApprovalResolver>, bool)> {
    lookup(run_id).or_else(active_installed)
}

fn active_installed() -> Option<(Arc<dyn ApprovalResolver>, bool)> {
    ACTIVE
        .try_with(|installed| (Arc::clone(&installed.resolver), installed.force_ask))
        .ok()
}

/// Resolve one tool call. `Grant` is the only approval.
pub async fn resolve_tool(
    run_id: &str,
    tool_name: &str,
    arguments: &serde_json::Value,
    channel: &str,
) -> Option<ApprovalDecision> {
    let (resolver, _) = lookup_for_call(run_id)?;
    let request = crate::approval::ApprovalRequest::new(tool_name, arguments.clone());
    Some(resolver.resolve(&request, channel).await)
}
