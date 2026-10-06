//! Configuration management transactions.
//!
//! One layer every configuration write surface goes through:
//!
//! - [`plan`]: the redacted semantic plan of a change and its risk class;
//! - [`history`]: the durable record of each transaction with the stored
//!   before-image a rollback restores;
//! - [`transaction`]: validate-then-commit with compensating restore, and
//!   rollback;
//! - `validate`: model validation for every selection a change affects,
//!   the decision backend included;
//! - [`mutation`]: typed changes of one section (the decision plane's);
//! - [`credentials`]: owner-bound credential references in the encrypted
//!   auth-profile store;
//! - [`service`]: typed provider, model, default, route and agent-binding
//!   operations over the transaction, with referential integrity;
//! - [`render`]: the one text rendering of a plan;
//! - [`migrate`]: the legacy-configuration report and migration (PMM-H).

pub(crate) mod boundary;
pub mod credentials;
pub mod history;
pub mod migrate;
pub mod mutation;
pub mod plan;
pub mod render;
pub mod service;
pub mod transaction;
pub(crate) mod validate;

pub use history::{ConfigActor, ConfigHistory, HistoryRecord, RecordStatus};
pub use plan::{ChangeKind, ConfigChangeEntry, ConfigChangeRisk, ConfigChangeSummary, RiskReason};
pub use render::render_plan;
pub use service::{ManageError, ManagedChange, ManagedPlan, ManagementService, RootCredential, SelectionOperation};
pub use transaction::{
    ApplyOptions, ConfigChange, ConfigPlan, ConfigTransactionError, ConfigTransactionOutcome, ConfigTransactions,
    NotAppliedOutcome, RollbackRequest, Settlement, WatcherActivation, WatcherOutcome,
};
