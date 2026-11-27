//! Process-wide resource subscription hub for `mcp_call` subscribe and
//! unsubscribe. Subscriptions live in memory for the life of the process.
//!
//! A hub is shared by every registry of one workspace directory in the
//! process. Each registry adopts the hub with a weak reference, so a replaced
//! registry is forgotten, and the newest live registry serves reconnects.

use super::McpTool;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

/// The hubs of this process, keyed by the workspace directory of their registries.
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Hub>>> {
    static HUBS: OnceLock<Mutex<HashMap<PathBuf, Arc<Hub>>>> = OnceLock::new();
    HUBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One workspace's subscriptions and the registries that serve them.
#[derive(Default)]
pub(super) struct Hub {
    /// Registries that adopted this hub, oldest first. Dead ones are pruned on adopt.
    owners: Mutex<Vec<Weak<McpTool>>>,
}

impl Hub {
    /// The newest registry still alive, if any.
    pub(super) fn newest_owner(&self) -> Option<Arc<McpTool>> {
        self.owners.lock().iter().rev().find_map(Weak::upgrade)
    }
}

/// Register `tool` with the hub of `workspace_dir`, creating the hub on first use.
pub(crate) fn adopt(tool: &Arc<McpTool>, workspace_dir: &Path) {
    let hub = {
        let mut hubs = registry().lock();
        Arc::clone(hubs.entry(workspace_dir.to_path_buf()).or_default())
    };
    let mut owners = hub.owners.lock();
    owners.retain(|owner| owner.strong_count() > 0);
    owners.push(Arc::downgrade(tool));
}

/// The hub that `tool` adopted, if any. A registry built outside the tool
/// setup (for example a diagnostics snapshot) has none.
pub(super) fn hub_for(tool: &McpTool) -> Option<Arc<Hub>> {
    let hubs = registry().lock();
    hubs.values()
        .find(|hub| {
            hub.owners
                .lock()
                .iter()
                .any(|owner| std::ptr::eq(owner.as_ptr(), tool))
        })
        .cloned()
}
