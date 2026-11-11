//! Opt-in process mailbox lifetime. The runtime supplies it explicitly; the
//! normal spawn factory has no mailbox binding. Failed joins retain the owner.
use super::{frames::WorkerFrameChannels, protocol::WorkerManifest};
use crate::messaging::SessionAddr;
use anyhow::{Result, ensure};
use std::sync::Arc;

pub struct WorkerMailboxBinding {
    pub address: SessionAddr,
    pub frames: WorkerFrameChannels,
}
/// Runtime authority, never deserialized from a model request.
pub trait WorkerMailboxFactory: Send + Sync {
    fn bind(&self, manifest: &WorkerManifest) -> Result<WorkerMailboxBinding>;
    /// `joined=false` is unresolved ownership, never permission to drop caches.
    fn release(&self, run_id: &str, joined: bool);
}

pub(crate) struct BoundWorkerMailbox {
    pub(crate) frames: Option<WorkerFrameChannels>,
    factory: Arc<dyn WorkerMailboxFactory>,
    run_id: String,
    spawned: bool,
    joined: bool,
}
impl BoundWorkerMailbox {
    pub(crate) fn bind(factory: Arc<dyn WorkerMailboxFactory>, manifest: &mut WorkerManifest) -> Result<Self> {
        let binding = factory.bind(manifest)?;
        let checked = crate::messaging::mailbox::validate_address(&binding.address)
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                ensure!(
                    manifest.owner_id.as_deref() == Some(binding.address.owner.as_str())
                        && manifest.agent_id.as_deref().unwrap_or("main") == binding.address.agent,
                    "mailbox binding differs from worker owner or agent"
                );
                Ok(())
            });
        if let Err(error) = checked {
            factory.release(&manifest.run_id, true);
            return Err(error);
        }
        manifest.session_addr = Some(binding.address);
        Ok(Self {
            frames: Some(binding.frames),
            factory,
            run_id: manifest.run_id.clone(),
            spawned: false,
            joined: false,
        })
    }
    pub(crate) const fn spawned(&mut self) {
        self.spawned = true;
    }
    pub(crate) const fn joined(&mut self) {
        self.joined = true;
    }
}
impl Drop for BoundWorkerMailbox {
    fn drop(&mut self) {
        self.factory.release(&self.run_id, !self.spawned || self.joined);
    }
}
