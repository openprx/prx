//! Read-only status of one published runtime surface (R25): the recovery and
//! journal attachment owned here, plus the engine's backend fields taken from
//! the engine status owner.

use crate::decision_engine::prediction::factory::BackendPlaneStatus;
use crate::runtime::decision_journal::store::StoreMode;
use crate::tools::capability::contract::CatalogGeneration;

/// Read-only status of the exact generation attached to a runtime surface.
/// The factory's configured slots are distinct from the catalog and store
/// attachment. `backend.enabled` reports configuration, not remote admission;
/// callers must also inspect exclusions and recordability. None of these
/// fields confers execution authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecisionPlaneStatus {
    pub(crate) backend: BackendPlaneStatus,
    pub(crate) catalog_generation: CatalogGeneration,
    pub(crate) store_mode: StoreMode,
    pub(crate) durable_key: bool,
    pub(crate) recordable: bool,
}

/// The recovery and journal attachment of one publication.
pub(crate) struct JournalAttachment {
    pub(crate) catalog_generation: CatalogGeneration,
    pub(crate) store_mode: StoreMode,
    pub(crate) durable_key: bool,
    pub(crate) recordable: bool,
}

/// Inspect the attached generation without probing a backend or opening a
/// new store. Admin and control-ladder readers can use this snapshot.
pub(crate) const fn plane_status(backend: BackendPlaneStatus, journal: JournalAttachment) -> DecisionPlaneStatus {
    DecisionPlaneStatus {
        backend,
        catalog_generation: journal.catalog_generation,
        store_mode: journal.store_mode,
        durable_key: journal.durable_key,
        recordable: journal.recordable,
    }
}
