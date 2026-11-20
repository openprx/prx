#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;
use crate::config::Config;
use tempfile::TempDir;

#[test]
fn runtime_base_prompt_excludes_request_local_tool_catalog() {
    let workspace = TempDir::new().unwrap();
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config
        .tool_tiering
        .model_allowlists
        .insert("small-model".to_string(), vec!["shell".to_string()]);
    let prompt = build_runtime_system_prompt(&config, "small-model", &[], true);
    assert!(!prompt.contains("**shell**"));
    assert!(!prompt.contains("**file_read**"));
}

#[test]
fn runtime_prompt_hides_skills_when_skill_read_is_unavailable() {
    let workspace = TempDir::new().unwrap();
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config
        .tool_tiering
        .model_allowlists
        .insert("small-model".to_string(), vec!["shell".to_string()]);
    let skills = vec![crate::skills::Skill {
        name: "release".to_string(),
        description: "Release the project".to_string(),
        version: "1.0.0".to_string(),
        author: None,
        tags: Vec::new(),
        tools: Vec::new(),
        prompts: Vec::new(),
        location: None,
        embedding: None,
    }];

    let prompt = build_runtime_system_prompt(&config, "small-model", &skills, true);

    assert!(!prompt.contains("<available_skills>"));
    assert!(!prompt.contains("<name>release</name>"));
}

/// The chat prompt of a build whose turns replay handoffs compactly states the
/// handoff rule exactly once, right after the tool-evidence rule, and the
/// paragraph is part of the system message every budget estimate counts.
#[test]
fn chat_prompt_states_the_handoff_rule_once() {
    let workspace = TempDir::new().unwrap();
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    let rule = crate::agent::compaction::CONTEXT_HANDOFF_PROMPT_RULE;

    let chat = build_chat_system_prompt(&config, "model", &[], true);
    let runtime = build_runtime_system_prompt(&config, "model", &[], true);

    let expected = usize::from(cfg!(feature = "terminal-tui"));
    assert_eq!(
        chat.matches(rule).count(),
        expected,
        "the chat prompt states the rule once"
    );
    assert!(!runtime.contains(rule), "a prompt outside chat does not state it");
    if expected == 1 {
        let evidence = chat
            .find(crate::agent::tool_evidence::TOOL_EVIDENCE_PROMPT_RULE)
            .expect("the evidence rule");
        let handoff = chat.find(rule).expect("the handoff rule");
        assert!(evidence < handoff, "the handoff rule follows the evidence rule");
        assert_eq!(
            build_chat_system_prompt(&config, "model", &[], true),
            chat,
            "the prompt is byte-stable between turns"
        );
    }
}
