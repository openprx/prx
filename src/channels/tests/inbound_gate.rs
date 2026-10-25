#![cfg(test)]
use super::*;

/// Recording channel registered under the `signal` name, so the Signal
/// vision-fallback path (which resolves `target_channel` by `msg.channel`)
/// can observe whether the fallback reply was actually sent. Mirrors
/// `RecordingChannel` but reports `name() == "signal"`.
#[derive(Default)]
struct SignalRecordingChannel {
    sent_messages: tokio::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Channel for SignalRecordingChannel {
    fn name(&self) -> &str {
        "signal"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent_messages
            .lock()
            .await
            .push(format!("{}:{}", message.recipient, message.content));
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// FIX-P0-10/11/12: under autonomy=ReadOnly the inbound SideEffectGate must
/// abort `process_channel_message` before the first conversation-turn
/// persistence — so `append_conversation_turn` is never called and no reply
/// is sent. This is the "mock-gate-rejects-all" check.
#[tokio::test]
async fn process_channel_message_inbound_gate_blocks_under_readonly() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let memory = Arc::new(CountingMemory::default());
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let ctx = gate_test_ctx(
        &workspace,
        crate::security::AutonomyLevel::ReadOnly,
        Arc::clone(&memory),
        channel,
    );

    run_owned_channel_probe(ctx, gate_test_message(), CancellationToken::new()).await;

    assert_eq!(
        memory.append_calls.load(Ordering::SeqCst),
        0,
        "ReadOnly autonomy must block the inbound conversation-turn persistence"
    );
    assert!(
        channel_impl.sent_messages.lock().await.is_empty(),
        "ReadOnly autonomy must not send any reply"
    );
}

/// FIX-P0-10/11/12: under explicit Supervised autonomy the inbound gate
/// must let normal traffic through — `append_conversation_turn` runs for the
/// inbound user turn (and at least one reply is appended), proving the gate
/// does not falsely reject low-risk inbound messages.
#[tokio::test]
async fn process_channel_message_inbound_gate_allows_under_supervised() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let memory = Arc::new(CountingMemory::default());
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let ctx = gate_test_ctx(
        &workspace,
        crate::security::AutonomyLevel::Supervised,
        Arc::clone(&memory),
        channel,
    );

    run_owned_channel_probe(ctx, gate_test_message(), CancellationToken::new()).await;

    assert!(
        memory.append_calls.load(Ordering::SeqCst) >= 1,
        "Supervised autonomy must allow the inbound conversation-turn persistence (normal flow not broken)"
    );
}

/// A DM message whose content is >= 30 chars and free of noise patterns, so
/// `should_autosave_content` returns true and the autosave branch is reached
/// (otherwise the gate result would be moot and the autosave assertions would
/// pass vacuously). `reply_target` has no group marker, so it infers as a DM.
fn gate_test_dm_message() -> traits::ChannelMessage {
    traits::ChannelMessage {
        id: "gate-msg-autosave".to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-gate".to_string(),
        content: "this is a sufficiently long direct message worth autosaving".to_string(),
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
    }
}

fn gate_test_telegram_group_message() -> traits::ChannelMessage {
    traits::ChannelMessage {
        id: "gate-msg-telegram-group".to_string(),
        sender: "alice".to_string(),
        reply_target: "-100200300".to_string(),
        content: "[Telegram Group] Alice: this is a sufficiently long group message that must not autosave".to_string(),
        channel: "telegram".to_string(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: ChatKind::Group,
        chat_title: Some("Build Room".to_string()),
        sender_display: Some("Alice".to_string()),
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: true,
        sender_is_bot: false,
    }
}

/// Test seam: deny only the `:autosave` operation, allowing inbound + outbound.
/// The real `SecurityPolicy` cannot express this (Act gating is autonomy + rate
/// only), so the autosave-deny end-to-end control flow is driven via this mock.
struct AutosaveOnlyDeny;

impl crate::security::inbound_gate::InboundAuthorizer for AutosaveOnlyDeny {
    fn authorize(&self, _tool_name: &str, operation_name: &str, _risk: ResourceRiskLevel) -> Result<(), String> {
        if operation_name.contains(":autosave") {
            Err(format!("test deny: {operation_name}"))
        } else {
            Ok(())
        }
    }
}

/// Test seam: deny only the `:outbound` operation, allowing inbound + autosave.
/// Used to drive the Signal vision-fallback outbound-deny path independently of
/// the action budget, so the fallback gate's deny semantics are asserted in
/// isolation (the real policy cannot express a per-operation deny).
struct OutboundOnlyDeny;

impl crate::security::inbound_gate::InboundAuthorizer for OutboundOnlyDeny {
    fn authorize(&self, _tool_name: &str, operation_name: &str, _risk: ResourceRiskLevel) -> Result<(), String> {
        if operation_name.contains(":outbound") {
            Err(format!("test deny: {operation_name}"))
        } else {
            Ok(())
        }
    }
}

/// A Signal image message that forces the vision preflight into the
/// `Fallback` branch: `channel == "signal"` plus a `vision_required=true`
/// marker makes `is_signal_image_message` true, and the test `DummyProvider`
/// reports `supports_vision() == false`, so the preflight short-circuits to
/// `Fallback` before any network call.
fn signal_vision_fallback_message() -> traits::ChannelMessage {
    traits::ChannelMessage {
        id: "gate-msg-vision".to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-gate".to_string(),
        content: "[signal-meta vision_required=true]".to_string(),
        channel: "signal".to_string(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}

/// Build a gate test context wired to a `SignalRecordingChannel` (registered
/// under the `signal` name) instead of the default `RecordingChannel`, so the
/// Signal vision-fallback path can resolve its `target_channel`.
fn signal_gate_ctx_with(
    workspace: &TempDir,
    opts: GateTestOpts,
    memory: Arc<CountingMemory>,
    channel: Arc<SignalRecordingChannel>,
) -> Arc<ChannelRuntimeContext> {
    let channel_dyn: Arc<dyn Channel> = channel;
    gate_test_ctx_with(workspace, opts, memory, channel_dyn)
}

/// DEV-05 (vision-fallback outbound deny): the Signal vision fallback emits an
/// assistant reply, so it must pass the same outbound gate as the normal path.
/// With outbound denied (op-selective seam), the fallback must NOT send a reply
/// — yet the inbound user turn (gated + persisted before the fallback branch)
/// must remain. This closes the matrix gap Codex flagged: previously the
/// fallback short-circuited a reply ahead of the outbound gate.
#[tokio::test]
async fn process_channel_message_vision_fallback_outbound_denied() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let memory = Arc::new(CountingMemory::default());
    let channel_impl = Arc::new(SignalRecordingChannel::default());
    let mut opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
    opts.test_inbound_authorizer = Some(Arc::new(OutboundOnlyDeny));
    let ctx = signal_gate_ctx_with(&workspace, opts, Arc::clone(&memory), Arc::clone(&channel_impl));

    run_owned_channel_probe(ctx, signal_vision_fallback_message(), CancellationToken::new()).await;

    assert!(
        memory.append_calls.load(Ordering::SeqCst) >= 1,
        "outbound deny must not abort the turn: the inbound user turn still persists"
    );
    assert!(
        channel_impl.sent_messages.lock().await.is_empty(),
        "outbound deny must suppress the Signal vision fallback reply"
    );
}

/// DEV-05 (positive control): with outbound allowed (Supervised, no op-selective
/// deny) the Signal vision fallback DOES send its reply. This guards the deny
/// test above from passing vacuously — proving the fallback send path is
/// actually reachable for this message and only the gate suppresses it.
#[tokio::test]
async fn process_channel_message_vision_fallback_outbound_allowed_sends() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let memory = Arc::new(CountingMemory::default());
    let channel_impl = Arc::new(SignalRecordingChannel::default());
    let opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
    let ctx = signal_gate_ctx_with(&workspace, opts, Arc::clone(&memory), Arc::clone(&channel_impl));

    run_owned_channel_probe(ctx, signal_vision_fallback_message(), CancellationToken::new()).await;

    let sent = channel_impl.sent_messages.lock().await;
    let responses = response_messages(&sent);
    assert_eq!(
        responses.len(),
        1,
        "Signal vision fallback must send exactly one reply when outbound is allowed"
    );
    assert!(
        responses.first().unwrap().contains(SIGNAL_IMAGE_UNCERTAINTY_FALLBACK),
        "the sent reply must be the vision uncertainty fallback text"
    );
}

/// D6-3 (autosave deny, mock seam): inject an authorizer that denies only
/// `:autosave`. The inbound user turn must still persist and the reply must
/// still be sent, but the autosave memory write must be skipped (store_calls
/// == 0). This isolates the autosave skip-only deny semantics from inbound /
/// outbound, which the real policy cannot do per-operation.
#[tokio::test]
async fn process_channel_message_autosave_gate_denied_by_mock() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let memory = Arc::new(CountingMemory::default());
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
    opts.auto_save_memory = true;
    opts.test_inbound_authorizer = Some(Arc::new(AutosaveOnlyDeny));
    let ctx = gate_test_ctx_with(&workspace, opts, Arc::clone(&memory), channel);

    run_owned_channel_probe(ctx, gate_test_dm_message(), CancellationToken::new()).await;

    assert!(
        memory.append_calls.load(Ordering::SeqCst) >= 1,
        "autosave deny must not abort the turn: the inbound user turn still persists"
    );
    assert_eq!(
        memory.store_calls.load(Ordering::SeqCst),
        0,
        "autosave deny must skip the autosave memory write"
    );
    assert!(
        !channel_impl.sent_messages.lock().await.is_empty(),
        "autosave deny must not suppress the reply (skip-only, not abort)"
    );
}

/// D6-3 (autosave allow, positive control): with autosave permitted (no
/// op-selective deny) the autosave memory write runs (store_calls > 0). This
/// guards the autosave-deny test above from passing vacuously — proving the
/// autosave branch is actually reachable for this message under allow.
#[tokio::test]
async fn process_channel_message_autosave_gate_allowed_writes_memory() {
    let workspace = TempDir::new().expect("channel recording fixture workspace");
    let memory = Arc::new(CountingMemory::default());
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
    opts.auto_save_memory = true;
    let mut ctx = gate_test_ctx_with(&workspace, opts, Arc::clone(&memory), channel);

    let recorded = super::recording_fixture::attach(
        Arc::get_mut(&mut ctx).expect("unique channel fixture context"),
        workspace.path(),
    );

    run_owned_channel_probe(ctx, gate_test_dm_message(), CancellationToken::new()).await;

    assert!(
        recorded.owned_readbacks() > 0,
        "channel input must be durably reread by owner"
    );

    assert!(
        memory.store_calls.load(Ordering::SeqCst) >= 1,
        "autosave allow must perform the autosave memory write"
    );
}

#[tokio::test]
async fn process_channel_message_telegram_group_does_not_autosave() {
    let workspace = TempDir::new().expect("test: private channel workspace");
    let memory = Arc::new(CountingMemory::default());
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
    opts.auto_save_memory = true;
    let ctx = gate_test_ctx_with(&workspace, opts, Arc::clone(&memory), channel);

    run_owned_channel_probe(ctx, gate_test_telegram_group_message(), CancellationToken::new()).await;

    assert!(
        memory.append_calls.load(Ordering::SeqCst) >= 1,
        "group message inbound turn should still persist"
    );
    assert_eq!(
        memory.store_calls.load(Ordering::SeqCst),
        0,
        "Telegram group messages must not take the DM autosave path"
    );
}

/// D8-1 / Step 7.3 (E2E): a full channel turn stamps the same per-turn
/// run_id on inbound, assistant, and shared terminal events. The finalizer
/// emits exactly one assistant projection and one terminal marker. No
/// parent_run_id is set (no spawn lineage in this turn).
#[tokio::test]
async fn process_channel_message_stamps_per_turn_run_id_on_message_events() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let mut runtime_config = Config::default();
    runtime_config.autonomy.level = crate::security::AutonomyLevel::Supervised;
    runtime_config.default_provider = Some("test-provider".to_string());
    runtime_config.default_model = Some("test-model".to_string());
    let config_manager = crate::config::new_shared(runtime_config);
    let ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: Arc::clone(&config_manager),
        config_generation: config_manager.pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::clone(&memory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(HookManager::new(tmp.path().to_path_buf())),
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
        workspace_dir: Arc::new(tmp.path().to_path_buf()),

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
            security: Arc::new(crate::security::SecurityPolicy {
                autonomy: crate::security::AutonomyLevel::Supervised,
                ..crate::security::SecurityPolicy::default()
            }),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(ctx, gate_test_message(), CancellationToken::new()).await;

    let events = memory
        .list_message_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: tmp.path().to_string_lossy().to_string(),
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: Some("test-channel".to_string()),
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            20,
        )
        .await
        .unwrap();

    let user_event = events
        .iter()
        .find(|e| e.role == "user")
        .expect("user message event recorded");
    let outbound_events = memory
        .list_message_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: tmp.path().to_string_lossy().to_string(),
                agent_id: None,
                persona_id: None,
                session_key: user_event.session_key.clone(),
                channel: Some("test-channel".to_string()),
                sender: Some("prx".to_string()),
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            20,
        )
        .await
        .unwrap();
    let assistant_event = outbound_events
        .iter()
        .find(|e| e.role == "assistant")
        .expect("assistant message event recorded");
    let terminal_events = outbound_events
        .iter()
        .filter(|event| event.event_type == "turn.finalized")
        .collect::<Vec<_>>();
    assert_eq!(
        outbound_events.iter().filter(|event| event.role == "assistant").count(),
        1,
        "the shared finalizer must project one assistant event"
    );
    assert_eq!(terminal_events.len(), 1, "the channel turn must close once");

    let user_run_id = user_event.run_id.as_deref().expect("user event must carry a run_id");
    assert!(!user_run_id.is_empty(), "user run_id must be non-empty");
    assert_eq!(
        assistant_event.run_id.as_deref(),
        Some(user_run_id),
        "user and assistant events of one turn must share the same run_id"
    );
    assert_eq!(
        terminal_events.first().and_then(|event| event.run_id.as_deref()),
        Some(user_run_id),
        "the terminal marker must share the channel turn run_id"
    );
    assert!(
        user_event.parent_run_id.is_none() && assistant_event.parent_run_id.is_none(),
        "a turn with no spawn lineage must not set parent_run_id"
    );
    for event in std::iter::once(user_event)
        .chain(std::iter::once(assistant_event))
        .chain(terminal_events.iter().copied())
    {
        assert_eq!(
            event.config_generation_id,
            Some(0),
            "every event in one channel turn must retain its pinned config generation"
        );
        assert!(
            event
                .config_source_revision
                .as_deref()
                .is_some_and(|revision| !revision.is_empty()),
            "every event in one channel turn must retain its config source revision"
        );
    }
}

// ── D2-2: channels security hot-reload ──────────────────────────────────

/// Exercise the exact gate calls the four channel authorization points make
/// (inbound/autosave/outbound/scope all funnel through
/// `SideEffectGate::new(snapshot.as_ref()).authorize_resource_operation`).
fn channel_gate_allows(policy: &crate::security::SecurityPolicy) -> bool {
    SideEffectGate::new(policy)
        .authorize_resource_operation("channel", "channel:test:inbound", ResourceRiskLevel::Low, None)
        .is_ok()
        && SideEffectGate::new(policy)
            .authorize_resource_operation("channel", "channel:test:outbound", ResourceRiskLevel::Low, None)
            .is_ok()
}

/// Build a coherent `SecurityGen` from an explicit autonomy level,
/// mirroring exactly what the stamp-change branch constructs.
fn make_gen(autonomy: crate::security::AutonomyLevel) -> Arc<SecurityGen> {
    Arc::new(SecurityGen {
        security: Arc::new(crate::security::SecurityPolicy {
            autonomy,
            ..crate::security::SecurityPolicy::default()
        }),
    })
}

/// T2-a: the channel's own gate observes a security hot-swap. Start
/// Supervised (gate allows), then store a ReadOnly generation into the same
/// `ArcSwap` and confirm the snapshot read at the authorization point now
/// denies — without rebuilding the context.
#[test]
fn t2a_channels_gate_hot_updates_on_security_store() {
    let security: Arc<arc_swap::ArcSwap<SecurityGen>> = Arc::new(arc_swap::ArcSwap::new(make_gen(
        crate::security::AutonomyLevel::Supervised,
    )));

    // Before reload: Supervised → low-risk inbound/outbound allowed.
    let before = security.load_full();
    assert!(
        channel_gate_allows(before.security.as_ref()),
        "Supervised snapshot must allow low-risk channel gates"
    );

    // Hot-swap to ReadOnly (what the stamp-change branch does via store()).
    security.store(make_gen(crate::security::AutonomyLevel::ReadOnly));

    // After reload: the snapshot read at the gate now denies.
    let after = security.load_full();
    assert!(
        !channel_gate_allows(after.security.as_ref()),
        "after storing a ReadOnly generation the channel gate snapshot must deny (hot update took effect)"
    );
}

/// T2-b: rebuild happens only on a `store` (i.e. only in the stamp-change
/// branch). When no store occurs, repeated `load_full()` returns the *same*
/// `Arc` (pointer-identical), proving per-message reads never rebuild; after
/// a `store` the pointer changes and carries the new generation.
#[test]
fn t2b_load_full_is_stable_until_store() {
    let security: Arc<arc_swap::ArcSwap<SecurityGen>> = Arc::new(arc_swap::ArcSwap::new(make_gen(
        crate::security::AutonomyLevel::Supervised,
    )));

    let a = security.load_full();
    let b = security.load_full();
    assert!(
        Arc::ptr_eq(&a, &b),
        "without a store, successive load_full() snapshots must be the same Arc (no per-read rebuild)"
    );

    // The stamp-change branch path: store a freshly built generation.
    security.store(make_gen(crate::security::AutonomyLevel::ReadOnly));

    let c = security.load_full();
    assert!(
        !Arc::ptr_eq(&a, &c),
        "after store(), load_full() must observe the new Arc (the rebuilt generation)"
    );
    assert_eq!(
        c.security.autonomy,
        crate::security::AutonomyLevel::ReadOnly,
        "the stored snapshot must carry the rebuilt ReadOnly policy"
    );
}

/// T2-scope-deny: storing a generation built from a config with
/// `[autonomy.scopes] default = "deny"` flips scope authorization
/// (`is_tool_allowed`, what `scope_or_pipeline_denial` consults) to deny.
#[test]
fn t2_scope_authorization_denies_after_store() {
    // Build a real SecurityPolicy from a deny-by-default scope config so the
    // scope path used by the tool-call loop reflects the new generation.
    let mut autonomy = crate::config::AutonomyConfig::default();
    autonomy.scopes.default = crate::config::ScopeDefault::Deny;
    let denying = crate::security::SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

    let security: Arc<arc_swap::ArcSwap<SecurityGen>> = Arc::new(arc_swap::ArcSwap::new(make_gen(
        crate::security::AutonomyLevel::Supervised,
    )));
    // Before: the explicit Supervised generation allows the tool in scope.
    assert!(
        security
            .load_full()
            .security
            .is_tool_allowed("memory_recall", "alice", "telegram", "direct"),
        "default scope must allow the tool before the deny generation is stored"
    );

    security.store(Arc::new(SecurityGen {
        security: Arc::new(denying),
    }));

    assert!(
        !security
            .load_full()
            .security
            .is_tool_allowed("memory_recall", "alice", "telegram", "direct"),
        "after storing a deny-by-default scope generation, scope authorization must deny"
    );
}

/// T2-decide-deny: storing a generation whose `SecurityPolicy` has a
/// deny-by-default scope ACL flips the unified `SecurityPolicy::decide`
/// decision point to `Deny` — and the policy rides in the SAME `SecurityGen`,
/// so the decision can never lag a generation.
#[test]
fn t2_decide_denies_after_store() {
    use crate::security::policy::ToolDecision;

    // Build a deny-by-default SecurityPolicy via the real config path.
    let mut autonomy = crate::config::AutonomyConfig::default();
    autonomy.level = crate::security::AutonomyLevel::Supervised;
    autonomy.scopes.default = crate::config::ScopeDefault::Deny;
    let denying = crate::security::SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

    let security: Arc<arc_swap::ArcSwap<SecurityGen>> = Arc::new(arc_swap::ArcSwap::new(make_gen(
        crate::security::AutonomyLevel::Supervised,
    )));

    // Before: the explicit Supervised generation's unified decision allows
    // (scope allows; Supervised would Ask for a side-effecting tool, but a
    // read-only tool is Allow) — assert the read-only tool is allowed.
    assert_eq!(
        security
            .load_full()
            .security
            .decide("memory_recall", "alice", "telegram", "direct"),
        ToolDecision::Allow,
        "default generation must allow the read-only tool before the deny generation is stored"
    );

    // Store a generation whose policy denies via the scope ACL default.
    security.store(Arc::new(SecurityGen {
        security: Arc::new(denying),
    }));

    let live = security.load_full();
    assert_eq!(
        live.security.decide("memory_recall", "alice", "telegram", "direct"),
        ToolDecision::Deny,
        "after storing a deny-by-default scope generation, the unified decision must be Deny"
    );
    // The autonomy half came from the SAME stored generation (coherent gen):
    // The configured policy keeps Supervised, proving policy and scope ACL
    // travel together in one SecurityGen.
    assert_eq!(
        live.security.autonomy,
        crate::security::AutonomyLevel::Supervised,
        "policy fields travel together in one SecurityGen"
    );
}
