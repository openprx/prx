//! Source-owned entry authority for controlled instruction experiments.
//! Parsed input is intent; none of these opaque owners is deserializable.
mod authority;
mod case;
mod input;
mod origin;
mod profile;
mod review;
pub(crate) use review::{LocalControlAuthority, LocalOwnerReview, LocalOwnerReviewStamp};

pub use authority::ControlledAdmissionPin;
pub(crate) use authority::{
    EntryAction, EntryCall, EntryInvocation, RunContinuation, ValidatedLifecycleCommand, arguments_digest,
    current_admission_pin, current_invocation, current_run_continuation, retain_run_continuation, scope_invocation,
};
pub(crate) use case::ControlledCaseScope;
pub(crate) use input::{ChatInput, ChatInputSender, TerminalInputSink, poll_original_terminal};
pub use origin::EntryOriginRecord;
pub(crate) use origin::{TerminalInputProducer, TerminalInputProof, VerifiedEntryOrigin};
pub(crate) use profile::{ControlledH2Profile, ControlledReaderLifetime, EntryKind, EntryLimits};
