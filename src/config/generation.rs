//! Process-level configuration generation ownership.
//!
//! `ConfigGenerationManager` is the only runtime publisher of effective
//! configuration. Readers pin an immutable [`ConfigGeneration`] (or its
//! `Arc<Config>` payload) for the lifetime of one operation.

include!("generation/section_1.rs");
include!("generation/section_2.rs");
