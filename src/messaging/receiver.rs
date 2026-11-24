//! Per-session execution serialization and durable backend inbox drivers.
use super::{
    SessionAddr,
    runtime::{MessageInput, MessagingRuntime},
};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock, Weak},
};
type Callback = Arc<dyn Fn(MessageInput) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;
pub(crate) fn gate(address: &SessionAddr) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<parking_lot::Mutex<HashMap<SessionAddr, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES.get_or_init(Default::default).lock();
    gates.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = gates.get(address).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(address.clone(), Arc::downgrade(&gate));
    gate
}
pub(crate) fn register(runtime: Arc<MessagingRuntime>, address: SessionAddr, callback: Callback) -> anyhow::Result<()> {
    register_idle(runtime, address, callback, Arc::new(|| true))
}
pub(crate) fn register_idle(
    runtime: Arc<MessagingRuntime>,
    address: SessionAddr,
    callback: Callback,
    idle: Arc<dyn Fn() -> bool + Send + Sync>,
) -> anyhow::Result<()> {
    // Registration and each poll own their SQLite work on the blocking pool.
    tokio::spawn(async move {
        let backend = Arc::clone(&runtime);
        let Ok(result) = crate::runtime::blocking::spawn_blocking(move || backend.register_backend(address)).await
        else {
            return;
        };
        let mut inbox = match result {
            Ok(Some(inbox)) => inbox,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(error=%crate::agent::diagnostics::error_class(&error), "session mailbox registration refused");
                return;
            }
        };
        // Adopted but not yet woken before a restart: queue them for the first poll.
        let Ok((returned, recovered)) = crate::runtime::blocking::spawn_blocking(move || {
            let recovered = inbox.recover();
            (inbox, recovered)
        })
        .await
        else {
            return;
        };
        inbox = returned;
        match recovered {
            Ok(0) => {}
            Ok(count) => tracing::info!(count, "session mailbox recovered adopted messages awaiting a wake"),
            Err(error) => {
                tracing::warn!(error=%crate::agent::diagnostics::error_class(&error), "session mailbox wake recovery failed");
            }
        }
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            let idle = Arc::clone(&idle);
            let Ok((returned, result)) = crate::runtime::blocking::spawn_blocking(move || {
                let result = if !inbox.live() {
                    None
                } else if !idle() {
                    Some(Ok(Vec::new()))
                } else {
                    Some(inbox.poll())
                };
                (inbox, result)
            })
            .await
            else {
                break;
            };
            inbox = returned;
            let Some(result) = result else {
                break;
            };
            match result {
                Ok(inputs) => {
                    for input in inputs {
                        let retained = input.clone();
                        if input.auto_start()
                            && let Err(error) = callback(input).await
                        {
                            if error.is::<crate::runtime::mailbox_child::ChildBusy>() {
                                let runtime = Arc::clone(&runtime);
                                let _ =
                                    crate::runtime::blocking::spawn_blocking(move || runtime.requeue(retained)).await;
                                continue;
                            }
                            tracing::warn!(error=%crate::agent::diagnostics::error_class(&error),"session mailbox turn refused");
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    Ok(())
}
