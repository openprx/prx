use std::time::Duration;
use parking_lot::RwLock;
use std::path::PathBuf;

use super::config::RuntimeState;
use super::hook_core::{HOOK_EVENT_DELIVERY_BOUND, HookCore, HookPolicy};
use super::event::{HookEvent, redacted_hook_payload};
use super::HOOKS_JSON_FILE;

/// Hook events the queue holds before further events are dropped and counted.
pub const HOOK_QUEUE_CAPACITY: usize = 1024;

/// Upper bound on the wait for queued events at shutdown.
pub const HOOK_DRAIN_BOUND: Duration = Duration::from_secs(3);

pub(super) enum DispatchMessage {
    Event {
        event: HookEvent,
        payload: serde_json::Value,
    },
    Flush(tokio::sync::oneshot::Sender<()>),
}

#[derive(Default)]
pub(super) struct DispatchStats {
    queued: std::sync::atomic::AtomicU64,
    delivered: std::sync::atomic::AtomicU64,
    dropped: std::sync::atomic::AtomicU64,
    /// Woken after every finished delivery (test waits).
    #[cfg(test)]
    delivery_signal: tokio::sync::Notify,
}

/// Counters of one manager's background dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookDispatchStats {
    /// Events accepted into the queue.
    pub queued: u64,
    /// Events whose command, WASM and event-bus delivery finished.
    pub delivered: u64,
    /// Events dropped because the queue was full or closed.
    pub dropped: u64,
}

/// Observe-only hook manager. `emit` queues the event and returns at once; a
/// single background worker per manager delivers events in emit order, so
/// order is kept for every session and turn, and a slow hook never delays a
/// turn.
pub struct HookManager {
    pub(super) core: std::sync::Arc<HookCore>,
    pub(super) queue: std::sync::Arc<std::sync::OnceLock<tokio::sync::mpsc::Sender<DispatchMessage>>>,
    pub(super) capacity: usize,
    pub(super) stats: std::sync::Arc<DispatchStats>,
}

/// A second handle on the same manager: the same configuration, queue and
/// counters, so events emitted through either are delivered in one order by
/// one worker.
impl Clone for HookManager {
    fn clone(&self) -> Self {
        Self {
            core: std::sync::Arc::clone(&self.core),
            queue: std::sync::Arc::clone(&self.queue),
            capacity: self.capacity,
            stats: std::sync::Arc::clone(&self.stats),
        }
    }
}

impl std::ops::Deref for HookManager {
    type Target = HookCore;

    fn deref(&self) -> &HookCore {
        &self.core
    }
}

impl HookManager {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self::with_queue_capacity(workspace_dir, HOOK_QUEUE_CAPACITY)
    }

    pub(super) fn with_queue_capacity(workspace_dir: PathBuf, capacity: usize) -> Self {
        let media_artifacts = crate::media::MediaArtifactOwner::for_workspace(&workspace_dir);
        Self {
            core: std::sync::Arc::new(HookCore {
                hooks_json_path: workspace_dir.join(HOOKS_JSON_FILE),
                workspace_dir,
                policy: RwLock::new(HookPolicy::Fixed {
                    allowed: false,
                    launch_root: None,
                }),
                media_artifacts,
                state: RwLock::new(RuntimeState::default()),
                delivery_bound: HOOK_EVENT_DELIVERY_BOUND,
                #[cfg(feature = "wasm-plugins")]
                plugin_runtime: tokio::sync::RwLock::new(None),
            }),
            queue: std::sync::Arc::new(std::sync::OnceLock::new()),
            capacity: capacity.max(1),
            stats: std::sync::Arc::new(DispatchStats::default()),
        }
    }

    /// Read whether workspace command hooks run
    /// (`security.workspace_hooks`) from the live process configuration at
    /// every event, and start them in `hooks/` beside `config.toml`
    /// (HOOKS-ENV). Without this, workspace command hooks do not run.
    #[must_use]
    pub fn with_config(self, shared: crate::config::SharedConfig) -> Self {
        *self.core.policy.write() = HookPolicy::Live(shared);
        self
    }

    /// A manager for a process that holds a configuration snapshot but no
    /// live configuration handle (the scheduler): whether workspace command
    /// hooks run, and the directory they start in, are taken from `config`
    /// now.
    #[must_use]
    pub fn for_config_snapshot(config: &crate::config::Config) -> Self {
        let manager = Self::new(config.workspace_dir.clone());
        *manager.core.policy.write() = HookPolicy::Fixed {
            allowed: config.security.workspace_hooks == crate::config::WorkspaceHooks::On,
            launch_root: config
                .config_path
                .parent()
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(|dir| dir.join("hooks")),
        };
        manager
    }

    /// Shorten the per-event delivery bound (tests).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_delivery_bound(mut self, bound: Duration) -> Self {
        if let Some(core) = std::sync::Arc::get_mut(&mut self.core) {
            core.delivery_bound = bound;
        }
        self
    }

    /// Workspace hooks run, starting under `launch_root` (tests).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn allowing_workspace_hooks(self, launch_root: PathBuf) -> Self {
        *self.core.policy.write() = HookPolicy::Fixed {
            allowed: true,
            launch_root: Some(launch_root),
        };
        self
    }
}

impl HookManager {
    /// Queue a test event and wait (bounded) until it was delivered, so the
    /// operator's explicit test reports a finished delivery.
    pub async fn test_event(&self, event: HookEvent, payload: serde_json::Value) {
        self.emit(event, payload).await;
        self.drain(HOOK_DRAIN_BOUND).await;
    }

    /// Queue `event` for background delivery and return at once; the turn
    /// never waits on a hook. The payload is redacted here, once, for every
    /// consumer (command hooks, WASM lifecycle hooks, the plugin event bus),
    /// so no secret waits in the queue. When the queue is full the event is
    /// dropped and counted (see [`Self::dispatch_stats`]).
    pub fn emit(&self, event: HookEvent, payload: serde_json::Value) -> impl std::future::Future<Output = ()> + use<> {
        self.emit_as(event, payload, super::identity::current().as_ref())
    }

    /// [`Self::emit`] for an event that belongs to `identity` rather than to
    /// the turn the calling task runs (a session opened, a scheduled run
    /// claimed).
    pub fn emit_for(
        &self,
        identity: &super::identity::HookIdentity,
        event: HookEvent,
        payload: serde_json::Value,
    ) -> impl std::future::Future<Output = ()> + use<> {
        self.emit_as(event, payload, Some(identity))
    }

    pub(super) fn emit_as(
        &self,
        event: HookEvent,
        data: serde_json::Value,
        identity: Option<&super::identity::HookIdentity>,
    ) -> impl std::future::Future<Output = ()> + use<> {
        let owned = match redacted_hook_payload(&data) {
            std::borrow::Cow::Owned(redacted) => Some(redacted),
            std::borrow::Cow::Borrowed(_) => None,
        };
        let data = owned.unwrap_or(data);
        let payload = super::envelope::build(event, &self.workspace_dir, identity, data);
        let sent = self.sender().map_or(Err("no async runtime"), |sender| {
            sender
                .try_send(DispatchMessage::Event { event, payload })
                .map_err(|error| match error {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => "queue full",
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => "dispatcher stopped",
                })
        });
        match sent {
            Ok(()) => {
                self.stats.queued.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(reason) => {
                let dropped = self.stats.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                // Logged on the first drop and then every 100th, so a stalled
                // hook cannot also flood the log.
                if dropped == 1 || dropped.is_multiple_of(100) {
                    tracing::warn!(
                        event = event.as_str(),
                        reason,
                        dropped_total = dropped,
                        capacity = self.capacity,
                        "hook event dropped"
                    );
                }
            }
        }
        std::future::ready(())
    }

    /// Counters of the background dispatcher.
    #[must_use]
    pub fn dispatch_stats(&self) -> HookDispatchStats {
        HookDispatchStats {
            queued: self.stats.queued.load(std::sync::atomic::Ordering::Relaxed),
            delivered: self.stats.delivered.load(std::sync::atomic::Ordering::Relaxed),
            dropped: self.stats.dropped.load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Wait until every event queued before this call was delivered, but at
    /// most `bound`. Returns whether the queue was fully drained in time;
    /// events still queued at the bound keep being delivered while the
    /// runtime lives. The shutdown path calls this with [`HOOK_DRAIN_BOUND`].
    pub async fn drain(&self, bound: Duration) -> bool {
        if self.queue.get().is_none() {
            return true;
        }
        let (done, delivered) = tokio::sync::oneshot::channel();
        let wait = async {
            let Some(sender) = self.sender() else {
                return false;
            };
            if sender.send(DispatchMessage::Flush(done)).await.is_err() {
                return false;
            }
            delivered.await.is_ok()
        };
        tokio::time::timeout(bound, wait).await.unwrap_or(false)
    }

    /// Drain at process or component shutdown: wait at most
    /// [`HOOK_DRAIN_BOUND`] and log what is abandoned. Every owner of a live
    /// manager calls this on its exit path.
    pub async fn drain_for_shutdown(&self) {
        if !self.drain(HOOK_DRAIN_BOUND).await {
            tracing::warn!(
                bound_secs = HOOK_DRAIN_BOUND.as_secs(),
                "hook events were still queued at shutdown; the rest are abandoned"
            );
        }
    }

    /// Wait until `count` events were delivered in total (tests whose
    /// producer enqueues from a spawned task, where `drain` could run first).
    #[cfg(test)]
    pub(crate) async fn wait_delivered(&self, count: u64) {
        loop {
            let signal = self.stats.delivery_signal.notified();
            tokio::pin!(signal);
            signal.as_mut().enable();
            if self.stats.delivered.load(std::sync::atomic::Ordering::Relaxed) >= count {
                return;
            }
            signal.await;
        }
    }

    /// The queue's sender; the single worker starts with the first event.
    /// `None` outside a tokio runtime, where no worker can start (the event is
    /// dropped and counted rather than panicking the caller).
    fn sender(&self) -> Option<&tokio::sync::mpsc::Sender<DispatchMessage>> {
        if let Some(sender) = self.queue.get() {
            return Some(sender);
        }
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        Some(self.queue.get_or_init(|| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<DispatchMessage>(self.capacity);
            let core = std::sync::Arc::clone(&self.core);
            let stats = std::sync::Arc::clone(&self.stats);
            runtime.spawn(async move {
                while let Some(message) = rx.recv().await {
                    match message {
                        DispatchMessage::Event { event, payload } => {
                            core.deliver_bounded(event, &payload).await;
                            stats.delivered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            #[cfg(test)]
                            stats.delivery_signal.notify_waiters();
                        }
                        DispatchMessage::Flush(done) => {
                            // The waiter may have given up at its bound.
                            let _ = done.send(());
                        }
                    }
                }
            });
            tx
        }))
    }
}
