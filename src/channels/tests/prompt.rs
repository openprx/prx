#![cfg(test)]
use super::*;

#[derive(Default)]
struct HistoryCaptureProvider {
    calls: parking_lot::Mutex<Vec<Vec<(String, String)>>>,
}

#[async_trait::async_trait]
impl Provider for HistoryCaptureProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("fallback".to_string())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let snapshot = messages
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect::<Vec<_>>();
        let mut calls = self.calls.lock();
        calls.push(snapshot);
        Ok(format!("response-{}", calls.len()))
    }
}

#[derive(Default)]
struct RecallMemory {
    recalls: AtomicU64,
}

#[async_trait::async_trait]
impl Memory for RecallMemory {
    fn name(&self) -> &str {
        "recall-memory"
    }

    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: crate::memory::MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        self.recalls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![crate::memory::MemoryEntry {
            id: "entry-1".to_string(),
            key: "memory_key_1".to_string(),
            content: "Age is 45".to_string(),
            category: crate::memory::MemoryCategory::Conversation,
            timestamp: "2026-02-20T00:00:00Z".to_string(),
            session_id: None,
            score: Some(0.9),
            tags: None,
            access_count: None,
            useful_count: None,
            source: None,
            source_confidence: None,
            verification_status: None,
            lifecycle_state: None,
            compressed_from: None,
        }])
    }

    async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&crate::memory::MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(1)
    }

    async fn health_check(&self) -> bool {
        true
    }
}

#[test]
fn prompt_contains_all_sections() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "test-model", &[], None, None);

    // Section headers
    assert!(
        !prompt.contains("## Tools"),
        "tool inventory must be compiled from the request-local ToolSpec snapshot"
    );
    assert!(prompt.contains("## Safety"), "missing Safety section");
    assert!(prompt.contains("## Workspace"), "missing Workspace section");
    assert!(prompt.contains("## Project Context"), "missing Project Context");
    assert!(prompt.contains("## Current Date & Time"), "missing Date/Time");
    assert!(prompt.contains("## Runtime"), "missing Runtime section");
}

#[test]
fn base_prompt_does_not_advertise_entrypoint_tool_hints() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "gpt-4o", &[], None, None);

    assert!(!prompt.contains("**shell**"));
    assert!(!prompt.contains("Run commands"));
    assert!(!prompt.contains("**memory_recall**"));
}

#[test]
fn base_prompt_excludes_request_local_tool_protocol() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "gpt-4o", &[], None, None);

    assert!(
        !prompt.contains("## Tool Use Protocol"),
        "the shared tool loop must add the protocol from its request-local ToolSpec snapshot"
    );
}

#[test]
fn prompt_injects_safety() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], None, None);

    assert!(prompt.contains("Do not exfiltrate private data"));
    assert!(prompt.contains("Do not run destructive commands"));
    assert!(prompt.contains("Prefer `trash` over `rm`"));
}

#[test]
fn prompt_injects_workspace_files() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], None, None);

    assert!(prompt.contains("### SOUL.md"), "missing SOUL.md header");
    assert!(prompt.contains("Be helpful"), "missing SOUL content");
    assert!(prompt.contains("### IDENTITY.md"), "missing IDENTITY.md");
    assert!(prompt.contains("Name: OpenPRX"), "missing IDENTITY content");
    assert!(prompt.contains("### USER.md"), "missing USER.md");
    assert!(prompt.contains("### AGENTS.md"), "missing AGENTS.md");
    assert!(prompt.contains("### TOOLS.md"), "missing TOOLS.md");
    // HEARTBEAT.md is intentionally excluded from channel prompts — it's only
    // relevant to the heartbeat worker and causes LLMs to emit spurious
    // "HEARTBEAT_OK" acknowledgments in channel conversations.
    assert!(
        !prompt.contains("### HEARTBEAT.md"),
        "HEARTBEAT.md should not be in channel prompt"
    );
    assert!(prompt.contains("### MEMORY.md"), "missing MEMORY.md");
    assert!(prompt.contains("User likes Rust"), "missing MEMORY content");
}

#[test]
fn prompt_missing_identity_files_are_skipped() {
    let tmp = TempDir::new().unwrap();
    // Empty workspace — no files at all
    let prompt = build_system_prompt(tmp.path(), "model", &[], None, None);

    assert!(!prompt.contains("### SOUL.md"));
    assert!(!prompt.contains("### AGENTS.md"));
    assert!(!prompt.contains("### IDENTITY.md"));
}

#[test]
fn build_identity_prompt_loads_existing_files_only() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("SOUL.md"), "Soul").unwrap();
    std::fs::write(tmp.path().join("TOOLS.md"), "Tools").unwrap();

    let prompt = build_identity_prompt(tmp.path());
    assert!(prompt.contains("### SOUL.md"));
    assert!(prompt.contains("Soul"));
    assert!(prompt.contains("### TOOLS.md"));
    assert!(!prompt.contains("### AGENTS.md"));
    assert!(!prompt.contains("File not found"));
}

#[test]
fn prompt_bootstrap_only_if_exists() {
    let ws = make_workspace();
    // No BOOTSTRAP.md — should not appear
    let prompt = build_system_prompt(ws.path(), "model", &[], None, None);
    assert!(
        !prompt.contains("### BOOTSTRAP.md"),
        "BOOTSTRAP.md should not appear when missing"
    );

    // Create BOOTSTRAP.md — should appear
    std::fs::write(ws.path().join("BOOTSTRAP.md"), "# Bootstrap\nFirst run.").unwrap();
    let prompt2 = build_system_prompt(ws.path(), "model", &[], None, None);
    assert!(
        prompt2.contains("### BOOTSTRAP.md"),
        "BOOTSTRAP.md should appear when present"
    );
    assert!(prompt2.contains("First run"));
}

#[test]
fn prompt_no_daily_memory_injection() {
    let ws = make_workspace();
    let memory_dir = ws.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    std::fs::write(memory_dir.join(format!("{today}.md")), "# Daily\nSome note.").unwrap();

    let prompt = build_system_prompt(ws.path(), "model", &[], None, None);

    // Daily notes should NOT be in the system prompt (on-demand via tools)
    assert!(
        !prompt.contains("Daily Notes"),
        "daily notes should not be auto-injected"
    );
    assert!(!prompt.contains("Some note"), "daily content should not be in prompt");
}

#[test]
fn prompt_runtime_metadata() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "claude-sonnet-4", &[], None, None);

    assert!(prompt.contains("Model: claude-sonnet-4"));
    assert!(prompt.contains(&format!("OS: {}", std::env::consts::OS)));
    assert!(prompt.contains("Host:"));
}

#[test]
fn prompt_skills_are_lazy_and_include_declared_tools() {
    let ws = make_workspace();
    let skills = vec![crate::skills::Skill {
        name: "code-review".into(),
        description: "Review code for bugs".into(),
        version: "1.0.0".into(),
        author: None,
        tags: vec![],
        tools: vec![crate::skills::SkillTool {
            name: "lint".into(),
            description: "Run static checks".into(),
            kind: "shell".into(),
            command: "cargo clippy".into(),
            args: HashMap::new(),
        }],
        prompts: vec!["Always run cargo test before final response.".into()],
        location: None,
        embedding: None,
    }];

    let prompt = build_system_prompt(ws.path(), "model", &skills, None, None);

    assert!(prompt.contains("<available_skills>"), "missing skills XML");
    assert!(prompt.contains("<name>code-review</name>"));
    assert!(prompt.contains("<description>Review code for bugs</description>"));
    assert!(prompt.contains("SKILL.md</location>"));
    assert!(prompt.contains("call `skill_read`"));
    assert!(!prompt.contains("<instructions>"));
    assert!(!prompt.contains("Always run cargo test before final response."));
    assert!(prompt.contains("<tools>"));
    assert!(prompt.contains("<name>lint</name>"));
    assert!(prompt.contains("<kind>shell</kind>"));
    assert!(prompt.contains("lazy-loaded"));
}

#[test]
fn prompt_skills_escape_reserved_xml_chars() {
    let ws = make_workspace();
    let skills = vec![crate::skills::Skill {
        name: "code<review>&".into(),
        description: "Review \"unsafe\" and 'risky' bits".into(),
        version: "1.0.0".into(),
        author: None,
        tags: vec![],
        tools: vec![crate::skills::SkillTool {
            name: "run\"linter\"".into(),
            description: "Run <lint> & report".into(),
            kind: "shell&exec".into(),
            command: "cargo clippy".into(),
            args: HashMap::new(),
        }],
        prompts: vec!["Use <tool_call> and & keep output \"safe\"".into()],
        location: None,
        embedding: None,
    }];

    let prompt = build_system_prompt(ws.path(), "model", &skills, None, None);

    assert!(prompt.contains("<name>code&lt;review&gt;&amp;</name>"));
    assert!(prompt.contains("<description>Review &quot;unsafe&quot; and &apos;risky&apos; bits</description>"));
    assert!(prompt.contains("<name>run&quot;linter&quot;</name>"));
    assert!(prompt.contains("<description>Run &lt;lint&gt; &amp; report</description>"));
    assert!(prompt.contains("<kind>shell&amp;exec</kind>"));
    assert!(!prompt.contains("Use &lt;tool_call&gt;"));
}

#[test]
fn prompt_truncation() {
    let ws = make_workspace();
    // Write a file larger than BOOTSTRAP_MAX_CHARS
    let big_content = "x".repeat(BOOTSTRAP_MAX_CHARS + 1000);
    std::fs::write(ws.path().join("AGENTS.md"), &big_content).unwrap();

    let prompt = build_system_prompt(ws.path(), "model", &[], None, None);

    assert!(prompt.contains("truncated at"), "large files should be truncated");
    assert!(!prompt.contains(&big_content), "full content should not appear");
}

#[test]
fn prompt_empty_files_skipped() {
    let ws = make_workspace();
    std::fs::write(ws.path().join("TOOLS.md"), "").unwrap();

    let prompt = build_system_prompt(ws.path(), "model", &[], None, None);

    // Empty file should not produce a header
    assert!(!prompt.contains("### TOOLS.md"), "empty files should be skipped");
}

#[test]
fn channel_log_truncation_is_utf8_safe_for_multibyte_text() {
    let msg = "Hello from OpenPRX 🌍. Current status is healthy, and café-style UTF-8 text stays safe in logs.";

    // Reproduces the production crash path where channel logs truncate at 80 chars.
    let result = std::panic::catch_unwind(|| crate::util::truncate_with_ellipsis(msg, 80));
    assert!(result.is_ok(), "truncate_with_ellipsis should never panic on UTF-8");

    let truncated = result.unwrap();
    assert!(!truncated.is_empty(), "expected a non-empty value");
    assert!(truncated.is_char_boundary(truncated.len()));
}

#[test]
fn runtime_prompt_excludes_channel_delivery_context() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], None, None);

    assert!(!prompt.contains("## Channel Capabilities"));
    assert!(!prompt.contains("running as a messaging bot"));
    assert!(!prompt.contains("automatically sent back"));
}

#[test]
fn channel_prompt_includes_channel_delivery_context() {
    let ws = make_workspace();
    let base_prompt = build_system_prompt(ws.path(), "model", &[], None, None);
    let msg = traits::ChannelMessage {
        channel: "telegram".to_string(),
        reply_target: "chat-1".to_string(),
        chat_kind: ChatKind::Group,
        chat_title: Some("Ops".to_string()),
        ..Default::default()
    };
    let prompt = build_channel_system_prompt(&base_prompt, &msg, None, Some("@bot"));

    assert!(
        prompt.contains("## Channel Capabilities"),
        "missing Channel Capabilities section"
    );
    assert!(prompt.contains("running as a messaging bot"), "missing channel context");
    assert!(
        prompt.contains("NEVER repeat, describe, or echo credentials"),
        "missing channel safety instruction"
    );
    assert!(
        prompt.contains("When responding on Telegram"),
        "missing Telegram delivery guidance"
    );
    assert!(
        prompt.contains("## Current Conversation")
            && prompt.contains("- Platform: telegram | You: @bot")
            && prompt.contains("- Type: group | Chat: \"Ops\" (chat-1)"),
        "missing current conversation context"
    );
    assert!(
        prompt.contains("use the chat_profile_update tool (not memory_store)"),
        "missing chat_profile_update preference hint"
    );
}

fn test_chat_profile(channel: &str, chat_id: &str, chat_kind: &str, purpose: &str) -> ChatProfile {
    ChatProfile {
        id: format!("{channel}-{chat_id}"),
        channel: channel.to_string(),
        chat_id: chat_id.to_string(),
        chat_kind: chat_kind.to_string(),
        title: Some(format!("title-{chat_id}")),
        purpose: Some(purpose.to_string()),
        notes: Some("shared operating notes".to_string()),
        tags: vec!["ops".to_string(), "handoff".to_string()],
        updated_by: "agent".to_string(),
        created_at: "2026-07-06T00:00:00Z".to_string(),
        updated_at: "2026-07-06T00:00:00Z".to_string(),
    }
}

#[test]
fn current_conversation_prompt_cross_chat_isolation() {
    let group_a = traits::ChannelMessage {
        channel: "telegram".to_string(),
        reply_target: "group-a".to_string(),
        chat_kind: ChatKind::Group,
        chat_title: Some("Group A".to_string()),
        ..Default::default()
    };
    let group_b = traits::ChannelMessage {
        channel: "telegram".to_string(),
        reply_target: "group-b".to_string(),
        chat_kind: ChatKind::Group,
        chat_title: Some("Group B".to_string()),
        ..Default::default()
    };
    let dm = traits::ChannelMessage {
        channel: "telegram".to_string(),
        reply_target: "dm-1".to_string(),
        chat_kind: ChatKind::Dm,
        chat_title: Some("Direct".to_string()),
        ..Default::default()
    };
    let profile_a = test_chat_profile("telegram", "group-a", "group", "A-only release room");

    let prompt_a = build_current_conversation_prompt(&group_a, Some(&profile_a), Some("@bot"));
    let prompt_b = build_current_conversation_prompt(&group_b, None, Some("@bot"));
    let prompt_dm = build_current_conversation_prompt(&dm, None, Some("@bot"));

    assert!(prompt_a.contains("A-only release room"));
    assert!(!prompt_b.contains("A-only release room"));
    assert!(!prompt_dm.contains("A-only release room"));
    assert!(prompt_b.contains("- Type: group | Chat: \"Group B\" (group-b)"));
    assert!(prompt_dm.contains("- Type: dm | Chat: \"Direct\" (dm-1)"));
}

#[test]
fn current_conversation_prompt_snapshots_no_profile_with_profile_and_long_truncation() {
    let msg = traits::ChannelMessage {
        channel: "wacli".to_string(),
        reply_target: "12345@g.us".to_string(),
        chat_kind: ChatKind::Group,
        chat_title: Some("Release War Room".to_string()),
        ..Default::default()
    };
    let no_profile = build_current_conversation_prompt(&msg, None, Some("99550001@s.whatsapp.net"));
    assert!(no_profile.contains("- Platform: wacli | You: 99550001@s.whatsapp.net"));
    assert!(no_profile.contains("- Type: group | Chat: \"Release War Room\" (12345@g.us)"));
    assert!(!no_profile.contains("Purpose (self-maintained):"));

    let mut profile = test_chat_profile("wacli", "12345@g.us", "group", "Coordinate release approvals");
    let with_profile = build_current_conversation_prompt(&msg, Some(&profile), Some("99550001@s.whatsapp.net"));
    assert!(with_profile.contains("- Purpose (self-maintained): Coordinate release approvals"));
    assert!(with_profile.contains("- Notes: shared operating notes | Tags: ops, handoff"));

    profile.purpose = Some("p".repeat(400));
    profile.notes = Some("n".repeat(900));
    let long = build_current_conversation_prompt(&msg, Some(&profile), Some("99550001@s.whatsapp.net"));
    assert!(long.contains(&format!("{}...", "p".repeat(180))));
    assert!(!long.contains(&"p".repeat(300)));
    assert!(long.contains(&format!("{}...", "n".repeat(240))));
    assert!(!long.contains(&"n".repeat(400)));
}

#[test]
fn current_conversation_prompt_renders_thread_self_consistently() {
    let msg = traits::ChannelMessage {
        channel: "slack".to_string(),
        reply_target: "thread-1".to_string(),
        chat_kind: ChatKind::Thread,
        chat_title: Some("Thread".to_string()),
        ..Default::default()
    };
    let prompt = build_current_conversation_prompt(&msg, None, Some("prx"));
    assert!(prompt.contains("- Type: thread | Chat: \"Thread\" (thread-1)"));
}

#[test]
fn prompt_workspace_path() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], None, None);

    assert!(prompt.contains(&format!("Working directory: `{}`", ws.path().display())));
}

#[test]
fn conversation_memory_key_uses_message_id() {
    let msg = traits::ChannelMessage {
        id: "msg_abc123".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "hello".into(),
        channel: "slack".into(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };

    assert_eq!(conversation_memory_key(&msg), "slack_U123_msg_abc123");
}

#[test]
fn conversation_memory_key_is_unique_per_message() {
    let msg1 = traits::ChannelMessage {
        id: "msg_1".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "first".into(),
        channel: "slack".into(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    let msg2 = traits::ChannelMessage {
        id: "msg_2".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "second".into(),
        channel: "slack".into(),
        timestamp: 2,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };

    assert_ne!(conversation_memory_key(&msg1), conversation_memory_key(&msg2));
}

#[tokio::test]
async fn autosave_keys_preserve_multiple_conversation_facts() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();

    let msg1 = traits::ChannelMessage {
        id: "msg_1".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "I'm Paul".into(),
        channel: "slack".into(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    let msg2 = traits::ChannelMessage {
        id: "msg_2".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "I'm 45".into(),
        channel: "slack".into(),
        timestamp: 2,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };

    mem.store(
        &conversation_memory_key(&msg1),
        &msg1.content,
        MemoryCategory::Conversation,
        None,
    )
    .await
    .unwrap();
    mem.store(
        &conversation_memory_key(&msg2),
        &msg2.content,
        MemoryCategory::Conversation,
        None,
    )
    .await
    .unwrap();

    assert_eq!(mem.count().await.unwrap(), 2);

    let recalled = mem.recall("45", 5, None).await.unwrap();
    assert!(recalled.iter().any(|entry| entry.content.contains("45")));
}

#[tokio::test]
async fn channel_plain_greeting_and_follow_up_reach_provider_with_default_causal_config() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(HistoryCaptureProvider::default());
    let mut runtime_config = Config::default();
    assert_eq!(
        runtime_config.causal_tree,
        crate::causal_tree::CausalTreeConfig::default()
    );
    runtime_config.default_provider = Some("test-provider".to_string());
    runtime_config.default_model = Some("test-model".to_string());
    let config_manager = crate::config::new_shared(runtime_config);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: Arc::clone(&config_manager),
        config_generation: config_manager.pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: provider_impl.clone(),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        runtime_ctx.clone(),
        traits::ChannelMessage {
            id: "msg-a".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "hello".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    run_owned_channel_probe(
        runtime_ctx,
        traits::ChannelMessage {
            id: "msg-b".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "follow up".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 2,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    let calls = provider_impl.calls.lock();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].len(), 2);
    assert_eq!(calls[0][0].0, "system");
    assert_eq!(calls[0][1].0, "user");
    assert_eq!(calls[1].len(), 4);
    assert_eq!(calls[1][0].0, "system");
    assert_eq!(calls[1][1].0, "user");
    assert_eq!(calls[1][2].0, "assistant");
    assert_eq!(calls[1][3].0, "user");
    assert!(calls[1][1].1.contains("hello"));
    assert!(calls[1][2].1.contains("response-1"));
    assert!(calls[1][3].1.contains("follow up"));
}

#[tokio::test]
async fn process_channel_message_answers_plain_turn_without_retrieval_or_persisted_context() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(HistoryCaptureProvider::default());
    let memory = Arc::new(RecallMemory::default());
    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: provider_impl.clone(),
        default_provider: Arc::new("test-provider".to_string()),
        memory: memory.clone(),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        runtime_ctx.clone(),
        traits::ChannelMessage {
            id: "msg-ctx-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-ctx".to_string(),
            content: "hello".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    let calls = provider_impl.calls.lock();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 2);
    assert_eq!(calls[0][1].0, "user");
    assert!(calls[0][1].1.contains("hello"));
    assert!(!calls[0][1].1.contains("[Memory context]"));
    assert!(!calls[0][1].1.contains("Age is 45"));
    assert_eq!(
        memory.recalls.load(Ordering::SeqCst),
        0,
        "a plain direct-answer turn must not retrieve memory"
    );

    let histories = runtime_ctx.conversation_histories.lock();
    let turns = histories
        .get("channel:test-channel:alice:chat-ctx")
        .expect("history should be stored for sender");
    assert_eq!(turns[0].role, "user");
    assert_eq!(turns[0].content, "hello");
    assert!(!turns[0].content.contains("[Memory context]"));
}

#[tokio::test]
async fn process_channel_message_telegram_keeps_system_instruction_at_top_only() {
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(HistoryCaptureProvider::default());
    let mut histories = HashMap::new();
    histories.insert(
        "telegram_alice".to_string(),
        vec![
            ChatMessage::assistant("stale assistant"),
            ChatMessage::user("earlier user question"),
            ChatMessage::assistant("earlier assistant reply"),
        ],
    );

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: provider_impl.clone(),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(histories)),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        runtime_ctx.clone(),
        traits::ChannelMessage {
            id: "tg-msg-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-telegram".to_string(),
            content: "hello".to_string(),
            channel: "telegram".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        },
        CancellationToken::new(),
    )
    .await;

    let calls = provider_impl.calls.lock();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 4);

    let roles = calls[0].iter().map(|(role, _)| role.as_str()).collect::<Vec<_>>();
    assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
    assert!(
        calls[0][0]
            .1
            .contains("When responding on Telegram, include media markers"),
        "telegram delivery instruction should live in the system prompt"
    );
    assert!(!calls[0].iter().skip(1).any(|(role, _)| role == "system"));
}
