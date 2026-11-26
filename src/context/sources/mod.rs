//! One `Source` per system-prompt segment. Each file holds the text of one
//! slot and nothing else.
mod context_handoff;
mod custom;
mod datetime;
mod delegated_role;
mod host_identity;
mod identity;
mod memory_index;
mod orchestration;
mod role_identity;
mod runtime;
mod safety;
mod skills;
mod task;
mod tool_evidence;
mod workspace;

pub use context_handoff::ContextHandoffSection;
pub(crate) use custom::CustomSection;
pub use datetime::DateTimeSection;
pub(crate) use delegated_role::DelegatedRoleSection;
pub use host_identity::set_share_host_identity;
pub use identity::{BOOTSTRAP_MAX_CHARS, IdentitySection, build_identity_prompt};
pub use orchestration::OrchestrationSection;
pub(crate) use role_identity::RoleIdentitySection;
pub use runtime::RuntimeSection;
pub use safety::{FULL_AUTONOMY_SAFETY, SafetySection};
pub use skills::SkillsSection;
pub use task::TaskSection;
pub use tool_evidence::ToolEvidenceSection;
pub use workspace::WorkspaceSection;
