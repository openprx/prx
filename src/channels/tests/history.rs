#![cfg(test)]
use super::*;

#[test]
fn normalize_cached_channel_turns_merges_consecutive_user_turns() {
    let turns = vec![
        ChatMessage::user("forwarded content"),
        ChatMessage::user("summarize this"),
    ];

    let normalized = normalize_cached_channel_turns(turns);
    assert_eq!(normalized.len(), 1);
    assert_eq!(normalized[0].role, "user");
    assert!(normalized[0].content.contains("forwarded content"));
    assert!(normalized[0].content.contains("summarize this"));
}

#[test]
fn normalize_cached_channel_turns_merges_consecutive_assistant_turns() {
    let turns = vec![
        ChatMessage::user("first user"),
        ChatMessage::assistant("assistant part 1"),
        ChatMessage::assistant("assistant part 2"),
        ChatMessage::user("next user"),
    ];

    let normalized = normalize_cached_channel_turns(turns);
    assert_eq!(normalized.len(), 3);
    assert_eq!(normalized[0].role, "user");
    assert_eq!(normalized[1].role, "assistant");
    assert_eq!(normalized[2].role, "user");
    assert!(normalized[1].content.contains("assistant part 1"));
    assert!(normalized[1].content.contains("assistant part 2"));
}

#[test]
fn clear_sender_history_clears_canonical_only_preserving_shared_legacy() {
    // FIX-P1-25b bug 2: clear (e.g. /model, /models) clears the CURRENT
    // canonical session only. The legacy pre-history is immutable and shared
    // across recipients, so it must survive — both for the cleared recipient
    // (as historical pre-context) and for the sender's other recipients.
    let legacy = "telegram_alice".to_string();
    let key_a = ConversationKey {
        canonical: "channel:telegram:alice:chat-a".to_string(),
        legacy: legacy.clone(),
    };
    let key_b = ConversationKey {
        canonical: "channel:telegram:alice:chat-b".to_string(),
        legacy: legacy.clone(),
    };

    let mut histories = HashMap::new();
    histories.insert(legacy.clone(), vec![ChatMessage::user("legacy-prehistory")]);
    histories.insert(key_a.canonical.clone(), vec![ChatMessage::assistant("a-turn")]);
    histories.insert(key_b.canonical.clone(), vec![ChatMessage::assistant("b-turn")]);
    let ctx = ctx_with_histories(histories);

    clear_sender_history(&ctx, &key_a);

    let histories = ctx.conversation_histories.lock();
    // Canonical A is cleared; legacy and canonical B are untouched.
    assert!(
        histories.get(&key_a.canonical).is_none(),
        "cleared recipient's canonical gone"
    );
    assert_eq!(
        histories
            .get(&legacy)
            .map(|t| t.iter().map(|m| m.content.as_str()).collect::<Vec<_>>()),
        Some(vec!["legacy-prehistory"]),
        "clear must not delete the shared legacy entry"
    );
    assert!(
        histories.get(&key_b.canonical).is_some(),
        "other recipient's canonical untouched"
    );
    drop(histories);

    // Cleared recipient A: canonical empty but legacy pre-history still visible.
    let merged_a = merged_history(&ctx.conversation_histories.lock(), &key_a);
    assert_eq!(
        merged_a.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
        vec!["legacy-prehistory"],
        "cleared recipient still sees immutable legacy pre-history"
    );
    // Other recipient B: legacy pre-history + its own canonical turn.
    let merged_b = merged_history(&ctx.conversation_histories.lock(), &key_b);
    assert_eq!(
        merged_b.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
        vec!["legacy-prehistory", "b-turn"],
        "other recipient retains legacy pre-history and own canonical turn"
    );
}

#[test]
fn normalize_cached_channel_turns_strips_fabricated_tool_markers_from_assistant_turns() {
    // The replay copy the model sees must not carry `[Used tools: ...]`
    // lines it invented in an earlier turn — that is what taught it to keep
    // claiming tool runs it never made.
    let turns = vec![
        ChatMessage::user("what is the disk usage?"),
        ChatMessage::assistant("Root is at 41%.\n[Used tools: shell, session_status]"),
        ChatMessage::user("and the logs? you wrote [Used tools: shell] last time"),
        ChatMessage::assistant("[Used tools: shell]\nNothing in the logs."),
    ];

    let normalized = normalize_cached_channel_turns(turns);

    assert_eq!(normalized.len(), 4);
    assert_eq!(normalized[1].content, "Root is at 41%.");
    assert_eq!(normalized[3].content, "Nothing in the logs.");
    assert_eq!(
        normalized[2].content, "and the logs? you wrote [Used tools: shell] last time",
        "a user turn is real conversation content and must never be rewritten"
    );
}

/// The same replay seam, one grain finer: a marker the model wrote *inside*
/// a sentence teaches it the same lesson as one on its own line, and the
/// line-level strip could not see it. A user turn quoting the marker in the
/// same grain must still come back byte-for-byte.
///
/// MUTATION GUARD: point `strip_fabricated_tool_markers_from_assistant_turns`
/// back at the line-level `strip_fabricated_tool_markers` and the two
/// assistant assertions fail with the marker still in the replay copy.
#[test]
fn normalize_cached_channel_turns_strips_inline_fabricated_tool_markers() {
    let turns = vec![
        ChatMessage::user("what is the disk usage?"),
        ChatMessage::assistant("Fixture checked. [Used tools: shell] Root is at 41%."),
        ChatMessage::user("you wrote `Fixture checked. [Used tools: shell]` last time"),
        ChatMessage::assistant("Nothing in the logs. [Used tools: shell, session_status]"),
    ];

    let normalized = normalize_cached_channel_turns(turns);

    assert_eq!(normalized.len(), 4);
    assert_eq!(normalized[1].content, "Fixture checked. Root is at 41%.");
    assert_eq!(normalized[3].content, "Nothing in the logs.");
    assert_eq!(
        normalized[2].content, "you wrote `Fixture checked. [Used tools: shell]` last time",
        "a user turn is real conversation content and must never be rewritten"
    );
}

/// The same replay seam, on the two CommonMark fence shapes a single
/// `in_fenced_block` boolean gets wrong (2026-09-19 cross-audit probe).
///
/// A prose marker after a `~~~` block whose body holds a ```` ```example ````
/// line is a fabricated claim and must not be replayed; a marker inside a
/// ```` ````text ```` block whose body holds a bare ```` ``` ```` line is the
/// model's own code example and must be replayed byte-for-byte, because
/// rewriting it is how the replay copy stops matching what the user saw.
///
/// MUTATION GUARD: accept any fence line as a closer again and the first
/// assistant turn keeps its marker while the second loses part of its code.
#[test]
fn normalize_cached_channel_turns_walks_fences_the_way_commonmark_does() {
    const CODE_EXAMPLE: &str = "````text\n```\nDocumented [Used tools: shell]\n````";
    let turns = vec![
        ChatMessage::user("what is the disk usage?"),
        ChatMessage::assistant("~~~text\n```example\n~~~\nFixture checked. [Used tools: shell]"),
        ChatMessage::user("show me the marker syntax"),
        ChatMessage::assistant(CODE_EXAMPLE),
    ];

    let normalized = normalize_cached_channel_turns(turns);

    assert_eq!(normalized.len(), 4);
    assert_eq!(
        normalized[1].content, "~~~text\n```example\n~~~\nFixture checked.",
        "prose after a mixed-fence block is not code: the fabricated marker must be stripped"
    );
    assert_eq!(
        normalized[3].content, CODE_EXAMPLE,
        "a marker inside a genuine long-fence block is the model's own example and must be replayed verbatim"
    );
}

#[test]
fn merged_history_strips_fabricated_tool_markers_without_touching_the_cache() {
    // Not every caller runs the normalizer over this union, so the strip has
    // to hold here on its own. The cached map must come out unchanged: the
    // replay copy is sanitized, the stored turn is not.
    let key = ConversationKey {
        canonical: "channel:whatsapp:u1:chat-1".to_string(),
        legacy: "whatsapp_u1".to_string(),
    };
    let mut map: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    map.insert(
        key.legacy.clone(),
        vec![
            ChatMessage::user("old-question"),
            ChatMessage::assistant("old-answer\n[Used tools: gateway]"),
        ],
    );
    map.insert(
        key.canonical.clone(),
        vec![
            ChatMessage::user("new-question"),
            ChatMessage::assistant("[Used tools: shell]\nnew-answer"),
        ],
    );

    let merged = merged_history(&map, &key);
    let rendered: Vec<&str> = merged.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        rendered,
        vec!["old-question", "old-answer", "new-question", "new-answer"],
        "assistant turns replayed to the model must carry no fabricated tool markers"
    );
    assert_eq!(
        map.get(&key.canonical)
            .and_then(|turns| turns.last())
            .map(|turn| turn.content.as_str()),
        Some("[Used tools: shell]\nnew-answer"),
        "the cached turn (and the sqlite row behind it) must keep the original bytes"
    );
}

#[test]
fn merged_history_unions_legacy_prehistory_with_canonical_turns_in_order() {
    // FIX-P1-25b bug 1 (read-merge, not move): a session that was migrated
    // and then ran once stores old turns under the legacy key and new turns
    // under the canonical key. The read path must surface the ordered union
    // (legacy first, then canonical), not just the canonical slice.
    let key = ConversationKey {
        canonical: "channel:telegram:u1:chat-1".to_string(),
        legacy: "telegram_u1".to_string(),
    };
    let mut map: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    map.insert(
        key.legacy.clone(),
        vec![ChatMessage::user("old-question"), ChatMessage::assistant("old-answer")],
    );
    map.insert(
        key.canonical.clone(),
        vec![ChatMessage::user("new-question"), ChatMessage::assistant("new-answer")],
    );

    let merged = merged_history(&map, &key);
    let rendered: Vec<&str> = merged.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        rendered,
        vec!["old-question", "old-answer", "new-question", "new-answer"],
        "merged read must include legacy pre-history then canonical turns, in order"
    );
    // Read-merge must not mutate the underlying map (no move/delete of legacy).
    assert_eq!(map.get(&key.legacy).map(Vec::len), Some(2));
    assert_eq!(map.get(&key.canonical).map(Vec::len), Some(2));
}

#[test]
fn merged_history_preserves_real_repeats_and_truncates_to_window() {
    let key = ConversationKey {
        canonical: "channel:telegram:u1:chat-1".to_string(),
        legacy: "telegram_u1".to_string(),
    };
    // Real, intentional repeats within a conversation (e.g. the user sends the
    // same text twice in a row) MUST be preserved. legacy and canonical keys
    // partition the timeline (each physical turn lives under exactly one key),
    // so there is no cross-store duplication to collapse — and a content-based
    // dedup would wrongly delete these genuine repeats. Pure concatenation.
    let mut map: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    map.insert(
        key.legacy.clone(),
        // Two identical legacy turns + a distinct one.
        vec![
            ChatMessage::user("ping"),
            ChatMessage::user("ping"),
            ChatMessage::assistant("pong"),
        ],
    );
    map.insert(
        key.canonical.clone(),
        // The same "ping" content again post-cutover plus a repeated canonical turn.
        vec![
            ChatMessage::user("ping"),
            ChatMessage::assistant("ok"),
            ChatMessage::assistant("ok"),
        ],
    );
    let merged = merged_history(&map, &key);
    let rendered: Vec<&str> = merged.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        rendered,
        vec!["ping", "ping", "pong", "ping", "ok", "ok"],
        "all turns preserved in order; no dedup of genuine repeats across or within stores"
    );

    // Window truncation keeps the most recent turns across the union.
    let mut big: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    big.insert(
        key.legacy.clone(),
        (0..MAX_CHANNEL_HISTORY)
            .map(|i| ChatMessage::user(format!("legacy-{i}")))
            .collect(),
    );
    big.insert(
        key.canonical.clone(),
        (0..MAX_CHANNEL_HISTORY)
            .map(|i| ChatMessage::user(format!("canonical-{i}")))
            .collect(),
    );
    let merged = merged_history(&big, &key);
    assert_eq!(merged.len(), MAX_CHANNEL_HISTORY);
    // The newest canonical turn survives; the oldest legacy turn is dropped.
    assert_eq!(
        merged.last().map(|m| m.content.as_str()),
        Some(format!("canonical-{}", MAX_CHANNEL_HISTORY - 1).as_str())
    );
    assert!(merged.iter().all(|m| m.content != "legacy-0"));
}

#[test]
fn merged_history_shares_legacy_prehistory_across_recipients() {
    // FIX-P1-25b bug 2 (recipient dimension): the legacy key is sender-scoped
    // (no recipient), so the same sender's two canonical conversations (one per
    // recipient) both read the legacy pre-history, while their new canonical
    // turns stay separate (no cross-talk).
    let mut map: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    let legacy = "telegram_alice".to_string();
    map.insert(legacy.clone(), vec![ChatMessage::user("legacy-shared-by-alice")]);
    let key_a = ConversationKey {
        canonical: "channel:telegram:alice:chat-a".to_string(),
        legacy: legacy.clone(),
    };
    let key_b = ConversationKey {
        canonical: "channel:telegram:alice:chat-b".to_string(),
        legacy,
    };
    map.insert(key_a.canonical.clone(), vec![ChatMessage::assistant("reply-in-a")]);
    map.insert(key_b.canonical.clone(), vec![ChatMessage::assistant("reply-in-b")]);

    let merged_a = merged_history(&map, &key_a);
    let merged_b = merged_history(&map, &key_b);
    let rendered_a: Vec<&str> = merged_a.iter().map(|m| m.content.as_str()).collect();
    let rendered_b: Vec<&str> = merged_b.iter().map(|m| m.content.as_str()).collect();

    assert_eq!(rendered_a, vec!["legacy-shared-by-alice", "reply-in-a"]);
    assert_eq!(rendered_b, vec!["legacy-shared-by-alice", "reply-in-b"]);
    // No cross-talk: recipient A never sees recipient B's canonical turn.
    assert!(!rendered_a.contains(&"reply-in-b"));
    assert!(!rendered_b.contains(&"reply-in-a"));
}

#[tokio::test]
async fn load_persisted_histories_rehydrates_legacy_and_canonical_for_merge() {
    // FIX-P1-25b bug 1 end-to-end: persist old turns under the legacy key and
    // new turns under the canonical key (the "migrated then ran once" state),
    // then restart-hydrate. The hydrated map must keep both keys so a merged
    // read returns the full ordered union — proving no read window is lost
    // across a restart.
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

    let key = ConversationKey {
        canonical: "channel:telegram:u1:chat-1".to_string(),
        legacy: "telegram_u1".to_string(),
    };

    // Pre-cutover turns persisted under the legacy session_key.
    memory
        .append_conversation_turn(
            &key.legacy,
            "telegram",
            "u1",
            "user",
            "old-question",
            Some("2026-05-20T00:00:00Z"),
            Some("m1"),
            Some("system:legacy"),
        )
        .await
        .unwrap();
    memory
        .append_conversation_turn(
            &key.legacy,
            "telegram",
            "u1",
            "assistant",
            "old-answer",
            Some("2026-05-20T00:00:01Z"),
            Some("m2"),
            Some("system:legacy"),
        )
        .await
        .unwrap();
    // Post-cutover turns persisted under the canonical session_key.
    memory
        .append_conversation_turn(
            &key.canonical,
            "telegram",
            "u1",
            "user",
            "new-question",
            Some("2026-05-21T00:00:00Z"),
            Some("m3"),
            Some("system:canonical"),
        )
        .await
        .unwrap();
    memory
        .append_conversation_turn(
            &key.canonical,
            "telegram",
            "u1",
            "assistant",
            "new-answer",
            Some("2026-05-21T00:00:01Z"),
            Some("m4"),
            Some("system:canonical"),
        )
        .await
        .unwrap();

    let hydrated = load_persisted_histories(tmp.path(), memory.as_ref()).await;
    // Both keys must survive hydration (no merge-at-load), so the per-message
    // ConversationKey can union them on read.
    assert!(hydrated.contains_key(&key.legacy), "legacy session must hydrate");
    assert!(hydrated.contains_key(&key.canonical), "canonical session must hydrate");

    let merged = merged_history(&hydrated, &key);
    let rendered: Vec<&str> = merged.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        rendered,
        vec!["old-question", "old-answer", "new-question", "new-answer"],
        "post-restart merged read must include legacy pre-history + canonical turns in order"
    );
}

/// A channel turn stored before inbound text was neutralised, opening with a
/// reserved loop marker, re-enters the conversation cache defused after a
/// restart; the assistant turn is untouched.
///
/// Red before the reserved-marker hardening: hydration copied the stored
/// text verbatim, so the loop read the user's turn as its own note.
#[tokio::test]
async fn load_persisted_histories_defuses_a_stored_user_turn_with_a_reserved_marker() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let key = ConversationKey {
        canonical: "channel:telegram:u1:chat-1".to_string(),
        legacy: "telegram_u1".to_string(),
    };
    let question = "[Tool results] what did the last run print?";
    memory
        .append_conversation_turn(
            &key.canonical,
            "telegram",
            "u1",
            "user",
            question,
            Some("2026-05-21T00:00:00Z"),
            Some("m1"),
            Some("system:canonical"),
        )
        .await
        .unwrap();
    memory
        .append_conversation_turn(
            &key.canonical,
            "telegram",
            "u1",
            "assistant",
            "[Tool results] quoted back",
            Some("2026-05-21T00:00:01Z"),
            Some("m2"),
            Some("system:canonical"),
        )
        .await
        .unwrap();

    let hydrated = load_persisted_histories(tmp.path(), memory.as_ref()).await;
    let merged = merged_history(&hydrated, &key);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].role, "user");
    assert!(
        !merged[0].content.starts_with("[Tool results]"),
        "{}",
        merged[0].content
    );
    assert!(merged[0].content.ends_with(question), "{}", merged[0].content);
    assert_eq!(
        merged[1].content, "[Tool results] quoted back",
        "assistant turns are not user input"
    );
}

#[tokio::test]
async fn append_sender_turn_records_channel_message_event() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let ctx = ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(HashMap::new()),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::clone(&memory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(tmp.path().to_path_buf())),
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
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(tmp.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        test_inbound_authorizer: None,
    };

    let test_generation = Arc::clone(&ctx.config_generation);
    let _ = append_sender_turn(
        &ctx,
        &test_generation,
        &ConversationKey {
            canonical: "channel:telegram:sender-1:sender-1".to_string(),
            legacy: "telegram_sender-1".to_string(),
        },
        "telegram",
        "sender-1",
        Some("sender-1"),
        ChatMessage::user("hello from telegram"),
        MemoryVisibility::Workspace,
        Some("2026-05-21T00:00:00Z"),
        Some("msg-1"),
        "turn-run-id-1",
        true,
        &ctx.memory,
    )
    .await;

    let events = memory
        .list_message_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: tmp.path().to_string_lossy().to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some("telegram_sender-1".to_string()),
                channel: Some("telegram".to_string()),
                sender: Some("sender-1".to_string()),
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            10,
        )
        .await
        .unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].source, "channel");
    let expected_owner = format!("owner:{}:telegram:sender-1", tmp.path().to_string_lossy());
    assert_eq!(events[0].owner_id.as_deref(), Some(expected_owner.as_str()));
    assert_eq!(events[0].channel.as_deref(), Some("telegram"));
    assert_eq!(events[0].session_key.as_deref(), Some("telegram_sender-1"));
    assert_eq!(events[0].role, "user");
    assert_eq!(events[0].content, "hello from telegram");
    // D8-1: the per-turn run_id threaded through append_sender_turn lands on
    // the recorded channel message event (non-empty, exact value).
    assert_eq!(events[0].run_id.as_deref(), Some("turn-run-id-1"));
}
