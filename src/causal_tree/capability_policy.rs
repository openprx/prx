//! Pure capability admission policy owned by the causal decision domain.

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

/// Tools that enforce per-owner isolation for an owner-isolated origin
/// (V02-R9 / OQ-21).
///
/// An owner-isolated context — today an externally authenticated MCP caller —
/// may execute only these. Stateful tools rebuild their principal from the service-authored
/// `_zc_scope` and applies [`crate::tools::execution::scope_requires_owner_match`] (or the run registry's
/// owner scope) to every row it reads or deletes. Every other tool is refused:
/// autonomy approval is not consulted at `AutonomyLevel::Full`, so the operator's
/// `mcp_server.exposed_tools` list would otherwise be the only boundary between
/// an external caller and a mutating tool that does not scope its effect by
/// owner. A tool joins this list only once it is owner-aware or reads no private
/// runtime state. Web search is stateless; its provider key and quota are shared.
///
/// `session_status` and `sessions_send` are owner-aware for in-process turns
/// but the gateway does not register them, so they stay off the list until it
/// does: an exposed entry is then refused with 403 before any identity binding
/// is written, instead of passing this gate and answering 404 afterwards.
pub const OWNER_ISOLATION_AWARE_TOOLS: &[&str] = &[
    // Stateless public web reads; no access to another owner's local data.
    "web_search_tool",
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
    // The tool restricts isolated callers to owner-bound proposal reads/cancellation.
    // Shared projection, executable import and legacy workspace actions are refused.
    "skills_manage",
];

/// Both public identity and actual backend must name the audited implementation.
pub(crate) fn admits_owner_isolated_identity(public_name: &str, backend_name: &str) -> bool {
    (backend_name == public_name && OWNER_ISOLATION_AWARE_TOOLS.contains(&public_name))
        || (backend_name == "skills_manage"
            && matches!(public_name, "lifecycle_status" | "lifecycle_list" | "lifecycle_inspect"))
}

#[cfg(test)]
mod owner_isolation_tests {
    #[test]
    fn owner_isolation_requires_actual_audited_backend_identity() {
        assert!(super::admits_owner_isolated_identity(
            "web_search_tool",
            "web_search_tool"
        ));
        assert!(!super::admits_owner_isolated_identity("web_search_tool", "mcp"));
        assert!(!super::admits_owner_isolated_identity("web_search", "web_search_tool"));
        assert!(super::admits_owner_isolated_identity("skills_manage", "skills_manage"));
        assert!(!super::admits_owner_isolated_identity("skills_manage", "mcp"));
        assert!(!super::admits_owner_isolated_identity("shell", "shell"));
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
