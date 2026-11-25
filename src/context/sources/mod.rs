//! One `Source` per system-prompt segment. Each file holds the text of one
//! slot and nothing else.
mod context_handoff;
mod custom;
mod datetime;
mod identity;
mod orchestration;
mod runtime;
mod safety;
mod skills;
mod task;
mod tool_evidence;
mod workspace;

pub use context_handoff::ContextHandoffSection;
pub(crate) use custom::CustomSection;
pub use datetime::DateTimeSection;
pub use identity::{BOOTSTRAP_MAX_CHARS, IdentitySection, build_identity_prompt};
pub use orchestration::OrchestrationSection;
pub use runtime::RuntimeSection;
pub use safety::{FULL_AUTONOMY_SAFETY, SafetySection};
pub use skills::SkillsSection;
pub use task::TaskSection;
pub use tool_evidence::ToolEvidenceSection;
pub use workspace::WorkspaceSection;
