//! Operator exclusions and model allowlists shared by capability consumers.

/// Return whether a core dependency may be exposed under the turn's explicit
/// exclusion and model-allowlist policy. This is used by prompt compilers to
/// avoid advertising capabilities whose required tool cannot be selected.
pub fn core_dependency_is_available(name: &str, model: &str, config: &crate::config::ToolTieringConfig) -> bool {
    !config.always_exclude.iter().any(|excluded| excluded == name)
        && model_allows_tool_name(model, name, &config.model_allowlists)
}

/// Return whether a named prompt/catalog entry is visible to this model.
/// Models without a configured allowlist retain the ordinary tool surface.
pub fn model_allows_tool_name<S: std::hash::BuildHasher>(
    model: &str,
    tool_name: &str,
    model_allowlists: &std::collections::HashMap<String, Vec<String>, S>,
) -> bool {
    model_allowlists
        .get(model)
        .is_none_or(|allowlist| allowlist.iter().any(|allowed| allowed == tool_name))
}

/// The existing Plan scheduler classifier; unknown tools remain intercepted.
pub(crate) fn plan_intercepts_tool(name: &str) -> bool {
    !matches!(
        name,
        // Read tools: safe to run in plan mode.
        "file_read"
            | "grep"
            | "web_search"
            | "web_search_tool"
            | "web_fetch"
            | "memory_recall"
            | "memory_search"
            | "memory_get"
            | "transcript_history_lookup"
            | "document_search"
            | "document_get_chunk"
            | "sessions_list"
            | "sessions_history"
            | "session_status"
            | "agents_list"
            | "image_info"
            | "hardware_board_info"
            | "hardware_memory_map"
            | "hardware_memory_read"
    )
}

/// Resolve the execution identity, rather than classifying an alias or effect label.
pub(crate) fn plan_intercepts_entry(entry: &super::catalog::CapabilityEntry) -> bool {
    use super::catalog::ExecutionTarget;
    match &entry.target {
        ExecutionTarget::Tool(name) => plan_intercepts_tool(name),
        ExecutionTarget::Skill { root, .. } | ExecutionTarget::Node { root, .. } => plan_intercepts_tool(root),
        ExecutionTarget::Hook => true,
    }
}

/// Tools that enforce per-owner isolation for an owner-isolated origin
/// (V02-R9 / OQ-21).
///
/// An owner-isolated context — today an externally authenticated MCP caller —
/// may execute only these. Each rebuilds its principal from the service-authored
/// `_zc_scope` and applies [`crate::tools::execution::scope_requires_owner_match`] (or the run registry's
/// owner scope) to every row it reads or deletes. Every other tool is refused:
/// autonomy approval is not consulted at `AutonomyLevel::Full`, so the operator's
/// `mcp_server.exposed_tools` list would otherwise be the only boundary between
/// an external caller and a mutating tool that does not scope its effect by
/// owner. A tool joins this list only once it is owner-aware.
///
/// `session_status` and `sessions_send` are owner-aware for in-process turns
/// but the gateway does not register them, so they stay off the list until it
/// does: an exposed entry is then refused with 403 before any identity binding
/// is written, instead of passing this gate and answering 404 afterwards.
pub const OWNER_ISOLATION_AWARE_TOOLS: &[&str] = &[
    // Requires the admitted source-bound port; JSON scope cannot authorize it.
    "capability_search",
    "memory_search",
    "memory_recall",
    "memory_get",
    "document_search",
    "document_get_chunk",
    "sessions_list",
    "sessions_history",
    "memory_forget",
    // Confines its write to the caller's own keys (owner_predicate::
    // isolated_store_owner / store_conflict_guard on both adapters).
    "memory_store",
    // Refuses host paths and non-private visibility for an isolated caller and
    // stores under an owner-namespaced document id (document_ingest.rs).
    // `document_sync` always takes a host path, so it is never admitted.
    "document_ingest",
];

/// Both public identity and actual backend must name the audited implementation.
pub(crate) fn admits_owner_isolated_identity(public_name: &str, backend_name: &str) -> bool {
    backend_name == public_name && OWNER_ISOLATION_AWARE_TOOLS.contains(&public_name)
}

#[cfg(test)]
mod owner_isolation_tests {
    #[test]
    fn owner_isolation_requires_actual_audited_backend_identity() {
        assert!(super::admits_owner_isolated_identity("memory_search", "memory_search"));
        assert!(!super::admits_owner_isolated_identity("memory_search", "mcp"));
        assert!(!super::admits_owner_isolated_identity(
            "prx_memory_alias",
            "memory_search"
        ));
        assert!(!super::admits_owner_isolated_identity("file_write", "file_write"));
        assert!(!super::admits_owner_isolated_identity("file_edit", "file_edit"));
    }
}
