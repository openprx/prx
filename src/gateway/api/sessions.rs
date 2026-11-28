mod mailbox;
pub(in crate::gateway) use mailbox::recover_startup;
mod rf_delete;
pub(in crate::gateway) use rf_delete::settle_pending_at_startup;
mod rf_message;
mod rf_replay;
mod target;

include!("sessions/section_1.rs");
include!("sessions/section_2.rs");
include!("sessions/section_3.rs");
