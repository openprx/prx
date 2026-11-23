//! Agent/channel/scheduled backends retain a publication, never a parent turn.
use super::{ChildBusy, MailboxChildSource};
use crate::memory::child_sessions::ChildSessionRecord;
use crate::runtime::turn_publication::TurnRuntimeService;
use anyhow::{Context, Result};
use std::sync::{Arc, OnceLock, Weak};
type Key = (String, String, String);
static ROOTS: OnceLock<parking_lot::Mutex<std::collections::HashMap<Key, Weak<Host>>>> = OnceLock::new();
struct Host {
    key: Key,
    parent: parking_lot::RwLock<Arc<TurnRuntimeService>>,
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(roots) = ROOTS.get() {
            roots.lock().remove(&self.key);
        }
    }
}
fn busy(record: &ChildSessionRecord) -> bool {
    record
        .runs
        .last()
        .and_then(|run| crate::runtime::registry::resolve_address(run))
        .is_some()
}
pub(crate) fn watch(parent: Arc<TurnRuntimeService>, owner: String, root: String, agent: String) -> Result<()> {
    use crate::tools::capability::turn_state::TurnSurfaceKind;
    // Those live backends own their UI/rf approval adapters and current host.
    if matches!(
        parent.surface,
        TurnSurfaceKind::Interactive | TurnSurfaceKind::Server | TurnSurfaceKind::Worker
    ) {
        return Ok(());
    }
    if parent.generation.effective.memory.backend != "sqlite" {
        return Ok(());
    }
    let runtime = crate::messaging::runtime::MessagingRuntime::for_config(&parent.generation.effective)?;
    let address = runtime.address(&owner, &root, &agent);
    let scope = runtime.store.session_scope(&runtime.workspace, &address, false)?;
    let key = (runtime.workspace.clone(), owner.clone(), root.clone());
    let host = {
        let mut roots = ROOTS.get_or_init(Default::default).lock();
        roots.retain(|_, host| host.strong_count() > 0);
        if let Some(host) = roots.get(&key).and_then(Weak::upgrade) {
            *host.parent.write() = parent;
            return Ok(());
        }
        let host = Arc::new(Host {
            key: key.clone(),
            parent: parking_lot::RwLock::new(parent),
        });
        roots.insert(key, Arc::downgrade(&host));
        host
    };
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            let reader = Arc::clone(&runtime);
            let captured_scope = scope.clone();
            let target = address.clone();
            let parent = Arc::clone(&host.parent.read());
            let selected_owner = owner.clone();
            let Ok(Ok(Some(records))) = crate::runtime::blocking::spawn_blocking(move || {
                if reader.store.validate_scope(&captured_scope, &target).is_err() {
                    return Ok(None);
                }
                let store = parent.memory.child_session_store().context("child store unavailable")?;
                Ok::<_, anyhow::Error>(Some(store.sessions(&reader.workspace, &selected_owner)?))
            })
            .await
            else {
                break;
            };
            for mut record in records
                .into_iter()
                .filter(|record| record.parent_session == root && !busy(record))
            {
                record.context.clear();
                let address = runtime.address(&owner, &record.session_key, &record.agent);
                let idle_host = Arc::clone(&host);
                let idle_record = record.clone();
                let idle = Arc::new(move || {
                    idle_host
                        .parent
                        .read()
                        .memory
                        .child_session_store()
                        .and_then(|store| store.sessions(&idle_record.workspace, &idle_record.owner).ok())
                        .is_some_and(|records| {
                            records
                                .iter()
                                .any(|record| record.session_key == idle_record.session_key && !busy(record))
                        })
                });
                let host = Arc::clone(&host);
                let result = crate::messaging::receiver::register_idle(
                    Arc::clone(&runtime),
                    address,
                    Arc::new(move |input| {
                        let parent = Arc::clone(&host.parent.read());
                        let record = record.clone();
                        Box::pin(async move {
                            if busy(&record) {
                                return Err(ChildBusy.into());
                            }
                            let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
                            cleanup
                                .scope(async {
                                    MailboxChildSource::admit(
                                        &parent,
                                        record.clone(),
                                        input.clone(),
                                        Some(super::no_surface()),
                                    )
                                    .await?
                                    .continue_turn(parent.registry.as_ref())
                                    .await?;
                                    let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
                                    loop {
                                        interval.tick().await;
                                        let memory = Arc::clone(&parent.memory);
                                        let receiver = record.clone();
                                        let records = crate::runtime::blocking::spawn_blocking(move || {
                                            memory
                                                .child_session_store()
                                                .context("child store unavailable")?
                                                .sessions(&receiver.workspace, &receiver.owner)
                                        })
                                        .await??;
                                        let Some(current) =
                                            records.iter().find(|current| current.session_key == record.session_key)
                                        else {
                                            break;
                                        };
                                        if !busy(current) {
                                            break;
                                        }
                                    }
                                    Ok(())
                                })
                                .await
                        })
                    }),
                    idle,
                );
                if let Err(error) = result {
                    tracing::warn!(error=%crate::agent::diagnostics::error_class(&error),"child receiver registration refused");
                }
            }
        }
    });
    Ok(())
}
