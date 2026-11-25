//! The compaction entry point of the agent loop. The code lives in
//! `crate::context::reduce` (one file per responsibility); this module keeps
//! the paths its callers use.

pub(crate) use crate::context::reduce::audit::*;
pub(crate) use crate::context::reduce::handoff_note::*;
pub(crate) use crate::context::reduce::patch::*;
pub(crate) use crate::context::reduce::provenance::*;
pub(crate) use crate::context::reduce::roundtrips::*;
pub(crate) use crate::context::reduce::summary::*;

#[cfg(test)]
use crate::agent::turn_contracts::CompactionPatch;
#[cfg(test)]
use std::collections::HashSet;

#[path = "turn_tests/compaction.rs"]
mod tests;
