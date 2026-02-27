use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Manifest passed from parent `sessions_spawn` to `session-worker`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerManifest {
    pub run_id: String,
    pub task: String,
    pub provider_name: String,
    pub model: String,
    pub api_key: Option<String>,
    pub temperature: f64,
    pub workspace_dir: PathBuf,
    pub memory_db_path: PathBuf,
    pub allowed_tools: Vec<String>,
    pub timeout_seconds: u64,
    pub max_iterations: usize,
    pub system_prompt: Option<String>,
    pub identity_dir: Option<String>,
    pub scope_sender: Option<String>,
    pub scope_channel: Option<String>,
    pub scope_chat_type: Option<String>,
    pub scope_chat_id: Option<String>,
    #[serde(default)]
    pub spawn_depth: usize,
    #[serde(default)]
    pub session_scope_key: String,
    #[serde(default)]
    pub parent_run_id: Option<String>,
    #[serde(default)]
    pub compaction_config: Option<crate::config::AgentCompactionConfig>,
}

/// Result returned by `session-worker` on stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_manifest_roundtrip_json() {
        let manifest = WorkerManifest {
            run_id: "run-1".into(),
            task: "analyze".into(),
            provider_name: "openrouter".into(),
            model: "anthropic/claude-sonnet-4.6".into(),
            api_key: None,
            temperature: 0.7,
            workspace_dir: PathBuf::from("/tmp/ws"),
            memory_db_path: PathBuf::from("/tmp/ws/brain.db"),
            allowed_tools: vec!["shell".into(), "file_read".into()],
            timeout_seconds: 120,
            max_iterations: 24,
            system_prompt: None,
            identity_dir: Some("identity/worker".into()),
            scope_sender: Some("openprx_user".into()),
            scope_channel: Some("telegram".into()),
            scope_chat_type: Some("direct".into()),
            scope_chat_id: Some("chat-1".into()),
            spawn_depth: 1,
            session_scope_key: "telegram:chat-1:openprx_user".into(),
            parent_run_id: Some("run-0".into()),
            compaction_config: Some(crate::config::AgentCompactionConfig::default()),
        };

        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        let parsed: WorkerManifest = serde_json::from_str(&json).expect("deserialize manifest");

        assert_eq!(parsed.run_id, "run-1");
        assert_eq!(parsed.allowed_tools.len(), 2);
        assert_eq!(parsed.identity_dir.as_deref(), Some("identity/worker"));
        assert_eq!(parsed.scope_sender.as_deref(), Some("openprx_user"));
        assert_eq!(parsed.spawn_depth, 1);
        assert_eq!(
            parsed.session_scope_key,
            "telegram:chat-1:openprx_user".to_string()
        );
        assert_eq!(parsed.parent_run_id.as_deref(), Some("run-0"));
        assert!(parsed.compaction_config.is_some());
    }

    #[test]
    fn worker_result_roundtrip_json() {
        let result = WorkerResult {
            success: false,
            output: String::new(),
            error: Some("timeout".into()),
        };

        let json = serde_json::to_string(&result).expect("serialize result");
        let parsed: WorkerResult = serde_json::from_str(&json).expect("deserialize result");

        assert!(!parsed.success);
        assert_eq!(parsed.error.as_deref(), Some("timeout"));
    }
}
