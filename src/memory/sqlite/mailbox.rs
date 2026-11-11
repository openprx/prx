//! Dormant mailbox store. No route, tool factory or production writer registers
//! this explicit constructor until ACT-1. Every mutation uses the memory writer.

mod accept;
mod adopt;
pub(super) mod child_projection;
mod deliver;
mod model;
mod outbox;
pub(super) mod pin;
mod publish;
mod receipt;
mod stream;
pub use outbox::MailboxOutbound;
mod erase;
mod gc;
pub use deliver::{MailboxDelivery, MailboxExecution};
pub use erase::MailboxErasure;
pub use gc::MailboxGc;
pub(super) mod runtime;

use std::sync::Arc;

use super::SqliteMemory;
use crate::messaging::mailbox::{MailboxClock, MailboxError, MailboxResult};

/// Explicitly constructed runtime handle. Secret material is never Debug.
#[derive(Clone)]
pub struct SqliteMailbox {
    memory: SqliteMemory,
    local_instance: String,
    secret: Arc<[u8; 32]>,
    clock: Arc<dyn MailboxClock>,
    limits: crate::config::schema::RemoteTreeLimits,
}

impl SqliteMailbox {
    pub fn validate_scope(
        &self,
        scope: &crate::messaging::mailbox::MailboxScope,
        address: &crate::messaging::SessionAddr,
    ) -> MailboxResult<()> {
        if address.instance != self.local_instance {
            return Err(MailboxError::Unauthorized);
        }
        let writer = self.memory.pool.write();
        pin::check_scope(&writer, scope, address, None, true)
    }

    pub fn new(
        memory: SqliteMemory,
        local_instance: String,
        secret: [u8; 32],
        clock: Arc<dyn MailboxClock>,
        limits: crate::config::schema::RemoteTreeLimits,
    ) -> MailboxResult<Self> {
        crate::messaging::mailbox::bounded(&local_instance, 512)?;
        if clock.now() < 0 {
            return Err(MailboxError::InvalidMetadata);
        }
        Ok(Self {
            memory,
            local_instance,
            secret: Arc::new(secret),
            clock,
            limits,
        })
    }
}

pub(super) fn storage_error(error: impl std::fmt::Display) -> MailboxError {
    tracing::warn!(%error,"mailbox SQLite operation failed");
    MailboxError::Storage
}

/// Production builds have no barrier or environment-variable inspection.
#[cfg(not(feature = "test-mock"))]
const fn crash_barrier(_stage: &str) -> MailboxResult<()> {
    Ok(())
}

/// Test-only process crash seam, located in the receiving parent process.
#[cfg(feature = "test-mock")]
fn crash_barrier(stage: &str) -> MailboxResult<()> {
    if let Ok(config) = std::env::var("OPENPRX_TEST_MAILBOX_BARRIER")
        && let Some((selected, path)) = config.split_once('=')
        && selected == stage
    {
        let path = std::path::Path::new(path);
        std::fs::write(format!("{}.started", path.display()), b"started").map_err(storage_error)?;
        while path.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Ok(())
}

mod sender;
pub use sender::SqliteMailboxSender;

#[cfg(feature = "test-mock")]
pub mod harness;
