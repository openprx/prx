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
