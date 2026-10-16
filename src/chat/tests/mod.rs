#![cfg(test)]

use super::*;

#[cfg(test)]
mod session_cleanup_tests;

#[cfg(all(test, feature = "terminal-tui"))]
mod transcript_selection_tests;

#[cfg(test)]
mod fallback_chat_output_tests;

#[cfg(test)]
mod runtime_display_tests;

#[cfg(test)]
mod compact_command_tests;

#[cfg(test)]
mod provider_connection_tests;

#[cfg(test)]
mod session_load_error_semantics_tests;

#[cfg(test)]
mod session_runtime_binding_tests;

#[cfg(test)]
mod chat_message_event_projection_tests;

#[cfg(test)]
mod legacy_chat_compaction_audit_tests;

#[cfg(test)]
mod file_mention_tests;

#[cfg(test)]
mod finalize_draft_fallback_tests;

#[cfg(all(test, feature = "terminal-tui"))]
mod reasoning_extraction_tests;

#[cfg(feature = "terminal-tui")]
#[cfg(test)]
mod terminal_guard_tests;

#[cfg(test)]
mod tracing_redirect_tests;

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod s4_a_4;

#[cfg(test)]
mod wave8_routing_failure_trace_tests;

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod p2_iss005_tests;

#[cfg(test)]
mod p6c2_diff_tests;

#[cfg(all(test, feature = "terminal-tui"))]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod iss_019_diff_apply_tests;

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod p6b2_external_editor_tests;

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
#[allow(clippy::indexing_slicing)]
mod p3_directional_switch_tests;

#[cfg(test)]
mod p7a_resume_tests;

#[cfg(test)]
mod p7b_branch_rewind_tests;

#[cfg(all(test, feature = "terminal-tui"))]
mod regfix_approval_switch_tests;

#[cfg(all(test, feature = "terminal-tui"))]
mod p6b1_transcript_tests;

#[cfg(test)]
mod v4_reload_recap_tests;

#[cfg(test)]
mod userinfo_display_tests;

#[test]
fn leading_system_prompt_preserves_skill_rag_resumed_turns() {
    let mut history = vec![
        ChatMessage::user("resumed user"),
        ChatMessage::assistant("resumed assistant"),
    ];

    upsert_leading_system_prompt(&mut history, "runtime system".to_string());

    assert_eq!(
        history
            .iter()
            .map(|message| (message.role.as_str(), message.content.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("system", "runtime system"),
            ("user", "resumed user"),
            ("assistant", "resumed assistant"),
        ]
    );
}
