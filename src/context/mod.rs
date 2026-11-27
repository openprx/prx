//! The context module: what a turn sends to the model, how a session's log
//! is kept, and the values computed once per turn for every consumer.

pub(crate) mod api;
pub(crate) mod budget;
pub(crate) mod call;
pub(crate) mod layout;
pub(crate) mod reduce;
pub(crate) mod request_dump;
pub(crate) mod request_provenance;
pub(crate) mod sources;
pub(crate) mod transcript;
