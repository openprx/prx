//! Restart settlement and file-publication repair before gateway serving.
use super::AppState;
use crate::memory::sync::SyncError;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

/// Complete restart settlement and config repair before the listener serves.
pub(in crate::gateway) async fn startup(state: &AppState, shutdown: &CancellationToken) -> anyhow::Result<()> {
    if let Err(error) = startup_inner(state, shutdown).await {
        tracing::warn!(error_kind=%crate::agent::diagnostics::error_class(&error),"sync startup refused");
        return Err(SyncError::Storage.into());
    }
    Ok(())
}
async fn startup_inner(state: &AppState, shutdown: &CancellationToken) -> anyhow::Result<()> {
    let Some(store) = state.mem.sync_store() else {
        return Ok(());
    };
    let owner = state
        .pairing
        .install_owner()
        .ok_or_else(|| anyhow::anyhow!("sync startup has no install owner"))?
        .as_str()
        .to_owned();
    let generation = state.config.pin();
    let config = Arc::clone(&generation.effective);
    let revision = generation.source_revision.fingerprint_sha256.clone();
    let workspace = config.workspace_dir.to_string_lossy().into_owned();
    let sync = Arc::clone(&store);
    let who = owner.clone();
    crate::runtime::blocking::spawn_blocking(move || sync.configure(&workspace, &who)).await??;
    let mut after = String::new();
    loop {
        let sync = Arc::clone(&store);
        let cursor = after.clone();
        let candidates = crate::runtime::blocking::spawn_blocking(move || sync.restart_candidates(&cursor)).await??;
        if candidates.is_empty() {
            break;
        }
        for candidate in candidates {
            after = candidate.run_id.clone();
            let Some(lifetime) = candidate.lifetime else {
                continue;
            };
            let gone = match state
                .mem
                .claim_ingress_recovery(
                    &candidate.workspace,
                    &lifetime,
                    std::time::Instant::now() + Duration::from_secs(4),
                )
                .await
            {
                Ok(claim) => claim.is_some(),
                Err(error) if error.is::<crate::memory::ingress_recovery::IngressLockMissing>() => false,
                Err(error) => return Err(error),
            };
            if gone {
                state
                    .mem
                    .interrupt_rf_run(&candidate.workspace, &candidate.owner, &candidate.run_id, true)
                    .await?;
            }
        }
    }
    // A message-woken run that died with the previous process is never replayed:
    // close the approvals it left open before any wake or request is served. A
    // failure leaves them listed but undecidable (no live waiter holds them).
    let sync = Arc::clone(&store);
    if !matches!(
        crate::runtime::blocking::spawn_blocking(move || sync.cancel_orphaned_woken_approvals()).await,
        Ok(Ok(_))
    ) {
        tracing::warn!("startup sweep of message-woken approvals failed");
    }
    let sync = Arc::clone(&store);
    crate::runtime::blocking::spawn_blocking(move || sync.refresh_config(&config, &revision)).await??;
    let sync = Arc::clone(&store);
    let who = owner.clone();
    crate::runtime::blocking::spawn_blocking(move || sync.journal(&who, 0, 0)).await??;
    let stop = shutdown.clone();
    let drain = store.shutdown_signal();
    crate::memory::message_retention::spawn_hourly(Arc::clone(&store), state.config.clone(), shutdown.clone());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.tick().await;
        loop {
            tokio::select! {
                ()=stop.cancelled()=>{drain.cancel();break;}
                _=tick.tick()=>{
                    let sync=Arc::clone(&store);let who=owner.clone();
                    let result=crate::runtime::blocking::spawn_blocking(move||sync.journal(&who,0,0)).await;
                    if !matches!(result,Ok(Ok(_))){tracing::warn!("sync journal maintenance failed");}
                }
            }
        }
    });
    Ok(())
}
