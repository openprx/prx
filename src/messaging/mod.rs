//! Dormant PRX messaging types (FOUNDATION).
//!
//! Envelope, session address, causal-chain, and outbound-stamp types land here
//! with no production writer, route, or send UX. MSG-CORE and ACT-1 wire them.

mod chain;
mod digest;
pub mod mailbox;
mod types;

pub use chain::{CausalChain, InheritedProvenance, InputProvenance, RemotePeerAttr, TaskInput, stamp_outbound};
pub use digest::request_digest;
pub use types::{
    DeliveryState, ExecRef, IdempotencyKey, MAX_BODY_BYTES, MessageAuth, MessageBody, MessageKind, MessageOrigin,
    PrxMessage, RecipientPin, SessionAddr,
};

pub mod send;
