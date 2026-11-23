//! Dedicated pinned mTLS messaging; independent of gateway compatibility APIs.
pub mod cli;
mod clients;
mod connections;
mod dispatch;
pub mod identity;
mod leader;
mod listener;
mod outbound;
pub mod peers;
mod resources;
mod tls;
mod trust;
pub use listener::start;
pub(crate) use outbound::RemoteTransport;
