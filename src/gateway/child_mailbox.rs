//! Backend-owned wake drivers for durable children, independent of parent turns.
mod approvals;
use super::AppState;
use crate::memory::child_sessions::ChildSessionRecord;
use crate::messaging::runtime::{MessageInput, MessagingRuntime};
use anyhow::{Context, Result};
use std::sync::{Arc, OnceLock, Weak};
static ROOTS: OnceLock<parking_lot::Mutex<std::collections::HashMap<crate::messaging::SessionAddr, Weak<()>>>> =
    OnceLock::new();
struct Registration {
    root: crate::messaging::SessionAddr,
    token: Arc<()>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(roots) = ROOTS.get() {
            let mut roots = roots.lock();
            if roots
                .get(&self.root)
                .is_some_and(|entry| Weak::ptr_eq(entry, &Arc::downgrade(&self.token)))
            {
                roots.remove(&self.root);
            }
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
pub(super) fn register(state: &AppState, envelope: &crate::runtime::envelope::RuntimeEnvelope) -> Result<()> {
    let config = state.config.load_full();
    if config.memory.backend != "sqlite" {
        return Ok(());
    }
    let runtime = MessagingRuntime::for_config(&config)?;
    let root = runtime.address(
        &envelope.resolved_owner_id(),
        &envelope.session_key,
        envelope.agent_id.as_deref().unwrap_or("main"),
    );
    let root_scope = runtime.store.session_scope(&runtime.workspace, &root, false)?;
    let lifetime = Arc::new(());
    {
        let mut roots = ROOTS.get_or_init(Default::default).lock();
        roots.retain(|_, root| root.strong_count() > 0);
        if roots.get(&root).and_then(Weak::upgrade).is_some() {
            return Ok(());
        }
        roots.insert(root.clone(), Arc::downgrade(&lifetime));
    }
    let console = envelope.source == crate::runtime::envelope::RuntimeSource::Console;
    let state = state.clone();
    tokio::spawn(async move {
        let _lifetime = Registration {
            root: root.clone(),
            token: lifetime,
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            let reader = Arc::clone(&runtime);
            let scope = root_scope.clone();
            let address = root.clone();
            let memory = Arc::clone(&state.mem);
            let Ok(Ok(Some(records))) = crate::runtime::blocking::spawn_blocking(move || {
                if reader.store.validate_scope(&scope, &address).is_err() {
                    return Ok(None);
                }
                let store = memory.child_session_store().context("child store unavailable")?;
                Ok::<_, anyhow::Error>(Some(store.sessions(&reader.workspace, &address.owner)?))
            })
            .await
            else {
                break;
            };
            for mut record in records
                .into_iter()
                .filter(|record| record.parent_session == root.session && !busy(record))
            {
                record.context.clear();
                let address = runtime.address(&record.owner, &record.session_key, &record.agent);
                let idle_state = state.clone();
                let idle_record = record.clone();
                let idle = Arc::new(move || {
                    idle_state
                        .mem
                        .child_session_store()
                        .and_then(|store| store.sessions(&idle_record.workspace, &idle_record.owner).ok())
                        .is_some_and(|records| {
                            records
                                .iter()
                                .any(|record| record.session_key == idle_record.session_key && !busy(record))
                        })
                });
                let state = state.clone();
                let result = crate::messaging::receiver::register_idle(
                    Arc::clone(&runtime),
                    address,
                    Arc::new(move |input| {
                        let state = state.clone();
                        let record = record.clone();
                        Box::pin(run(state, record, input, console))
                    }),
                    idle,
                );
                if let Err(error) = result {
                    tracing::warn!(error=%crate::agent::diagnostics::error_class(&error),"child mailbox registration refused");
                }
            }
        }
    });
    Ok(())
}
async fn run(state: AppState, record: ChildSessionRecord, input: MessageInput, console: bool) -> Result<()> {
    let turn = state.admit_turn_runtime()?;
    let cleanup = Arc::clone(&turn.publication_slot.cleanup);
    cleanup
        .scope(async {
            let publication = turn.publication_slot.publish(&turn, &state).await?;
            let configured = Arc::clone(&turn.config_generation.effective);
            let incoming = input.clone();
            let runtime = crate::runtime::blocking::spawn_blocking(move || {
                let runtime = MessagingRuntime::for_config(&configured)?;
                incoming.verify(&runtime)?;
                Ok::<_, anyhow::Error>(runtime)
            })
            .await??;
            let wait_secs = turn
                .config_generation
                .effective
                .agents
                .get(&record.agent)
                .map_or(turn.config_generation.effective.agent.approval_wait_secs, |agent| {
                    agent.approval_wait_secs
                });
            let live = approvals::live(&state, &record, &input, console, wait_secs).await?;
            crate::runtime::mailbox_child::MailboxChildSource::admit(
                &publication,
                record.clone(),
                input.clone(),
                Some(live),
            )
            .await?
            .continue_turn(turn.tools_registry.as_ref())
            .await?;
            // Keep this inbox serial until the independently owned task/process
            // settles. The next message cannot become a second concurrent turn.
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                interval.tick().await;
                let reader = Arc::clone(&runtime);
                let incoming = input.clone();
                let memory = Arc::clone(&state.mem);
                let receiver = record.clone();
                let records = crate::runtime::blocking::spawn_blocking(move || {
                    incoming.verify(&reader)?;
                    memory
                        .child_session_store()
                        .context("child store unavailable")?
                        .sessions(&receiver.workspace, &receiver.owner)
                })
                .await??;
                let Some(current) = records.iter().find(|current| current.session_key == record.session_key) else {
                    break;
                };
                if !busy(current) {
                    break;
                }
            }
            Ok(())
        })
        .await
}
