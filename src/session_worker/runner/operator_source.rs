//! Authenticated worker transport precedes shared snapshot restoration.
use super::{AuthenticatedWorkerTransport, Config, Result, WorkerManifest};
use crate::runtime::operator_spawn::process::OperatorProcessSnapshots;

pub(super) fn restore(
    manifest: &WorkerManifest,
    config: Config,
    transport: &AuthenticatedWorkerTransport,
) -> Result<OperatorProcessSnapshots> {
    transport.verify_manifest(manifest)?;
    let workspace = config.workspace_dir.to_string_lossy();
    let key = crate::decision::key::DecisionKey::from_worker_channel(
        &transport.key,
        &workspace,
        &manifest.run_id,
        &manifest.config_generation,
    )?;
    OperatorProcessSnapshots::restore(manifest, config, key)
}
