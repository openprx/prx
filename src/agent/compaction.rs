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
use crate::agent::context_budget::estimate_history_tokens;
#[cfg(test)]
use crate::agent::document_ingest::DocumentIngestRuntime;
#[cfg(test)]
use crate::agent::response_guard::strip_assistant_fabricated_tool_markers;
#[cfg(test)]
use crate::agent::turn_contracts::CompactionPatch;
#[cfg(test)]
use crate::agent::turn_contracts::CompactionPatchError;
#[cfg(test)]
use crate::agent::turn_contracts::CompactionPatchGuard;
#[cfg(test)]
use crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER;
#[cfg(test)]
use crate::memory::CompactionRunInput;
#[cfg(test)]
use crate::memory::CompactionSourceEventRange;
#[cfg(test)]
use crate::memory::Memory;
#[cfg(test)]
use crate::memory::MemoryCategory;
#[cfg(test)]
use crate::memory::MemoryPrincipal;
#[cfg(test)]
use crate::memory::MemoryStoreMetadata;
#[cfg(test)]
use crate::memory::MemoryWriteContext;
#[cfg(test)]
use crate::memory::MessageEvent;
#[cfg(test)]
use crate::memory::SessionContextQuery;
#[cfg(test)]
use crate::providers::ChatMessage;
#[cfg(test)]
use crate::providers::Provider;
#[cfg(test)]
use crate::util::truncate_with_ellipsis;
#[cfg(test)]
use anyhow::Result;
#[cfg(test)]
use sha2::Digest;
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
#[cfg(test)]
use std::fmt::Write;
#[cfg(test)]
use uuid::Uuid;
#[path = "turn_tests/compaction.rs"]
mod tests;
