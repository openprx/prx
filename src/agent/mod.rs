#[allow(clippy::module_inception)]
pub mod agent;
pub(crate) mod approval_collection;
pub mod classifier;
pub(crate) mod compaction;
mod completion_review;
pub(crate) mod context_budget;
pub(crate) mod context_paging;
pub(crate) mod context_preflight;
pub(crate) mod context_recall;
pub(crate) mod diagnostics;
pub mod dispatcher;
pub(crate) mod document_ingest;
pub(crate) mod embedded_source;
pub(crate) mod entry;
pub(crate) mod execution_policy;
mod execution_requirement;
pub(crate) mod history;
pub mod idle;
pub mod memory_loader;
pub mod prompt;
pub(crate) mod provider_round;
pub(crate) mod response_guard;
pub(crate) mod runtime_prompt;
pub mod sanitize;
pub(crate) mod scheduled_source;
pub mod stream_buffer;
pub mod terminal;
pub(crate) mod tool_call_parser;
pub(crate) mod tool_result_recording;
pub(crate) mod tool_scheduler;
pub(crate) mod turn_context;
pub(crate) mod turn_contracts;
pub(crate) mod turn_engine;
pub(crate) mod turn_request;
mod turn_tests;

#[cfg(test)]
pub(crate) use turn_tests::admission as test_admission;

#[cfg(test)]
mod prefix_stability_tests;
#[cfg(test)]
mod tests;

pub use entry::run;
pub(crate) use entry::run_with_runtime_envelope;
