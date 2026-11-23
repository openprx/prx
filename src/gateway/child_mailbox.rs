//! Backend-owned wake drivers for durable children, independent of parent turns.
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
    let state = state.clone();
    tokio::spawn(async move {
        let _lifetime = Registration {
            root: root.clone(),
            token: lifetime,
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            if runtime.store.validate_scope(&root_scope, &root).is_err() {
                break;
            }
            let Some(store) = state.mem.child_session_store() else {
                break;
            };
            let Ok(records) = store.sessions(&runtime.workspace, &root.owner) else {
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
                        Box::pin(run(state, record, input))
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
async fn run(state: AppState, record: ChildSessionRecord, input: MessageInput) -> Result<()> {
    let turn = state.admit_turn_runtime()?;
    let cleanup = Arc::clone(&turn.publication_slot.cleanup);
    cleanup
        .scope(async {
            let publication = turn.publication_slot.publish(&turn, &state).await?;
            let runtime = MessagingRuntime::for_config(&turn.config_generation.effective)?;
            input.verify(&runtime)?;
            let mut live = crate::runtime::mailbox_child::no_surface();
            if state.rf_streams.connected(&record.owner)
                && let Some(store) = state.mem.sync_store()
            {
                let event = input
                    .receipt
                    .adopted_message_event_id
                    .context("adopted child event missing")?
                    .to_string();
                if let Some(scope) = store.live_scope(&record.owner, &event)? {
                    live =
                        crate::agent::live_frames::LiveTurnRef(Arc::new(super::run_approvals::RunApprovalResolver {
                            scope,
                            streams: Arc::clone(&state.rf_streams),
                            approvals: Arc::clone(&state.rf_approvals),
                            cancellation: tokio_util::sync::CancellationToken::new(),
                            wait_secs: turn
                                .config_generation
                                .effective
                                .agents
                                .get(&record.agent)
                                .map_or(turn.config_generation.effective.agent.approval_wait_secs, |agent| {
                                    agent.approval_wait_secs
                                }),
                        }));
                }
            }
            crate::runtime::mailbox_child::MailboxChildSource::admit(
                publication,
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
                input.verify(&runtime)?;
                let records = state
                    .mem
                    .child_session_store()
                    .context("child store unavailable")?
                    .sessions(&record.workspace, &record.owner)?;
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
