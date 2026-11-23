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
    // Registration is idempotent for the currently running backend owner.
    let Some(mut inbox) = runtime.register_backend(address)? else {
        return Ok(());
    };
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            if !inbox.live() {
                break;
            }
            if !idle() {
                continue;
            }
            match inbox.poll() {
                Ok(inputs) => {
                    for input in inputs {
                        let retained = input.clone();
                        if input.auto_start()
                            && let Err(error) = callback(input).await
                        {
                            if error.is::<crate::runtime::mailbox_child::ChildBusy>() {
                                let _ = runtime.requeue(retained);
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
