//! Host correlation acceptance on the real listener/dispatcher/turn path.
use super::*;
use crate::channels::rich::*;
use std::collections::BTreeSet;

fn message(id: i64, conversation: &str, input: &str, host: &str) -> Value {
    let mut frame = inbound_message(id, conversation, input, "correlation request", 1);
    frame["params"]["turn_id"] = host.into();
    frame
}

fn action(id: i64, host: &str, conversation: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"inbound.surface_action","params":{
        "conversation_id":conversation,"sender":sender(),"surface_id":"card","version":1,
        "surface_hash":"abc","action_id":"choose","values":{"choice":"open"},"args_hash":"def",
        "message_id":"source","turn_id":host
    }})
}

async fn finish(host: &mut Host, expected: &str) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        let frame = host.recv().await;
        if frame.get("method").is_some() {
            assert_eq!(frame["params"]["turn_id"], expected, "{frame}");
            ack(host, &frame).await;
        }
        let done =
            frame["method"] == "turn.status" && matches!(frame["params"]["state"].as_str(), Some("done" | "failed"));
        frames.push(frame);
        if done {
            return frames;
        }
    }
}

async fn released(channel: &RamfluxChannel, conversation: &str, host: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while channel.turnid_live(conversation, host).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("test: guard release");
}

#[derive(Default)]
struct IdentityProvider {
    runs: Mutex<Vec<String>>,
}
impl IdentityProvider {
    fn reply(&self) -> String {
        let turn = crate::channels::rich_turn::active_turn().expect("test: active turn");
        assert!(
            uuid::Uuid::parse_str(&turn.turn_id).is_ok(),
            "internal ID must remain a UUID"
        );
        assert!(crate::runtime::registry::resolve_address(&turn.turn_id).is_some());
        assert!(crate::agent::channel_approval::lookup(&turn.turn_id).is_some());
        self.runs.lock().push(turn.turn_id);
        "correlated reply".into()
    }
}
#[async_trait::async_trait]
impl Provider for IdentityProvider {
    async fn chat_traced(
        &self,
        request: crate::providers::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        let started_at = chrono::Utc::now();
        let response = self.chat(request, model, temperature).await?;
        Ok(crate::providers::traits::ChatTrace {
            response,
            attempts: vec![crate::llm::route_decision::ProviderAttempt {
                seq: 1,
                provider: "default".to_owned(),
                model: model.to_owned(),
                started_at,
                finished_at: chrono::Utc::now(),
                status: crate::llm::route_decision::AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "default".to_owned(),
            final_model: model.to_owned(),
            served_member: None,
            tokens_used: crate::llm::route_decision::TokenUsage::reported(Some(5), Some(3), Some(8)),
        })
    }
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        Ok(self.reply())
    }
    async fn chat_with_history(&self, _: &[ChatMessage], _: &str, _: f64) -> anyhow::Result<String> {
        Ok(self.reply())
    }
}

async fn identity_boot() -> (
    tempfile::TempDir,
    Arc<RamfluxChannel>,
    Arc<crate::memory::SqliteMemory>,
    Host,
    Arc<IdentityProvider>,
) {
    let provider = Arc::new(IdentityProvider::default());
    let (dir, channel, memory, host) = boot(
        |_| RamfluxConfig::default(),
        provider.clone(),
        vec![],
        Default::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    (dir, channel, memory, host, provider)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_supplied_and_all_outbound_serializers_keep_internal_identity() {
    let outbound_structs: BTreeSet<_> = include_str!("../../ramflux/protocol.rs")
        .split("pub struct ")
        .filter(|section| section.split('}').next().unwrap_or("").contains("turn_id: &'a str"))
        .map(|section| section.split('<').next().unwrap())
        .collect();
    assert_eq!(
        outbound_structs,
        BTreeSet::from([
            "ReplyTextParams",
            "ReplySurfaceParams",
            "ReplyTaskUpdateParams",
            "ReplyTaskProposeParams",
            "ReplyAttachmentParams",
            "ReplyTypingParams",
            "ToolEventParams",
            "ApprovalRequestParams",
            "AttachmentFetchParams",
            "TurnStatusParams",
        ])
    );
    let (dir, channel, memory, mut host, provider) = identity_boot().await;
    host.send(&message(101, "conv-1", "first", "wire-host")).await;
    let frames = finish(&mut host, "wire-host").await;
    released(&channel, "conv-1", "wire-host").await;
    let internal = provider.runs.lock()[0].clone();
    assert_ne!(internal, "wire-host");
    assert!(frames.iter().any(|f| f["params"]["surface_id"] == "final-wire-host"));
    // Exercise every protocol outbound struct through its production serializer,
    // in a second live turn held at the provider boundary.
    let (hold, hits, _, release, mut entered) = HoldProvider::new();
    channel.shutdown();
    drop(host);
    let channel = Arc::new(
        RamfluxChannel::new(
            RamfluxConfig {
                socket_path: Some(dir.path().join("all.sock")),
                ..Default::default()
            },
            dir.path(),
        )
        .unwrap(),
    );
    let ctx = runtime(dir.path(), channel.clone(), hold, vec![], memory, Default::default());
    spawn_turns(channel.clone(), ctx);
    let mut host = Host::connect(channel.socket_path()).await;
    hello(&mut host, "supervised").await;
    host.send(&message(102, "conv-1", "all", "wire-host")).await;
    wait_hits(&mut host, &hits, 1, &mut entered).await;
    let internal = channel.turnid_live("conv-1", "wire-host").unwrap();
    let sending = channel.clone();
    let run = internal.clone();
    let task = tokio::spawn(async move {
        let conv = "conv-1".to_owned();
        for reply in [
            RichReply::Typing(TypingReply {
                conversation_id: conv.clone(),
                turn_id: run.clone(),
                on: true,
            }),
            RichReply::Text(TextReply {
                conversation_id: conv.clone(),
                turn_id: run.clone(),
                text: "delta".into(),
                is_final: false,
                reply_target: None,
            }),
            RichReply::Surface(SurfaceReply {
                conversation_id: conv.clone(),
                turn_id: run.clone(),
                op: SurfaceOp::StreamAppend,
                surface_id: "stream".into(),
                version: 1,
                base_hash: None,
                blocks: None,
                actions: None,
                ops: None,
                block_id: Some("body".into()),
                text_delta: Some("delta".into()),
                is_final: Some(false),
                fallback_text: None,
            }),
            RichReply::TaskPropose(TaskProposeReply {
                conversation_id: conv.clone(),
                turn_id: run.clone(),
                task_id: "task".into(),
                title: "task".into(),
                steps: vec![],
            }),
            RichReply::TaskUpdate(TaskUpdateReply {
                conversation_id: conv.clone(),
                turn_id: run.clone(),
                task_id: "task".into(),
                version: 1,
                state: "running".into(),
                steps: vec![],
                result_ref: None,
                result_summary: None,
            }),
            RichReply::Attachment(AttachmentReply {
                conversation_id: conv.clone(),
                turn_id: run.clone(),
                bytes_b64: Some("YQ==".into()),
                stream_id: None,
                mime: "text/plain".into(),
                name: None,
            }),
        ] {
            sending.send_rich(&reply).await.unwrap();
        }
        crate::channels::rich_turn::scope_if_rich(Some(&(sending.clone() as Arc<dyn Channel>)), &conv, &run, async {
            sending
                .on_tool_event(&ToolEvent {
                    kind: ToolEventKind::Started,
                    tool_call_id: "call".into(),
                    tool_name: "probe".into(),
                    iteration: 1,
                    duration_ms: None,
                    summary: None,
                })
                .await;
            sending.fetch_attachment("object", 10).await.unwrap();
            let context = crate::agent::channel_approval::lookup(&run).unwrap();
            let _ = context
                .resolver
                .resolve(&crate::approval::ApprovalRequest::new("probe", json!({})), "ramflux")
                .await;
        })
        .await;
    });
    let mut methods = BTreeSet::new();
    loop {
        let frame = host.recv().await;
        let method = frame["method"].as_str().unwrap_or("");
        if method.is_empty() {
            continue;
        }
        assert_eq!(frame["params"]["turn_id"], "wire-host", "{frame}");
        methods.insert(method.to_owned());
        if method == "attachment.fetch" {
            host.send(&json!({"jsonrpc":"2.0","id":frame["id"],"result":{"bytes_b64":"YQ==","mime":"text/plain"}}))
                .await;
        } else {
            ack(&mut host, &frame).await;
        }
        if method == "approval.request" {
            host.send(&json!({"jsonrpc":"2.0","id":103,"method":"approval.decision","params":{"conversation_id":"conv-1","approval_id":frame["params"]["approval_id"],"decision":"approved","decided_by":sender(),"decided_at_ms":3}})).await;
            break;
        }
    }
    task.await.unwrap();
    release.send(true).unwrap();
    let frames = finish(&mut host, "wire-host").await;
    methods.extend(frames.iter().filter_map(|f| f["method"].as_str().map(str::to_owned)));
    assert_eq!(
        methods,
        BTreeSet::from(
            [
                "turn.status",
                "reply.text",
                "reply.typing",
                "reply.surface",
                "reply.task_propose",
                "reply.task_update",
                "reply.attachment",
                "tool.event",
                "attachment.fetch",
                "approval.request"
            ]
            .map(str::to_owned)
        )
    );
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_absent_preserves_uuid_wire_identity() {
    let (_dir, channel, _memory, mut host, provider) = identity_boot().await;
    host.send(&inbound_message(110, "conv-1", "legacy", "legacy input", 1))
        .await;
    let mut wire = None;
    loop {
        let f = host.recv().await;
        if f.get("method").is_some() {
            let id = f["params"]["turn_id"].as_str().unwrap().to_owned();
            assert!(uuid::Uuid::parse_str(&id).is_ok());
            if let Some(previous) = &wire {
                assert_eq!(previous, &id);
            } else {
                wire = Some(id);
            }
            ack(&mut host, &f).await;
        }
        if f["params"]["state"] == "done" {
            break;
        }
    }
    assert_eq!(provider.runs.lock().as_slice(), &[wire.unwrap()]);
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_invalid_before_admission_and_replay_mismatch() {
    let (_dir, channel, _memory, mut host, provider) = identity_boot().await;
    for (n, host_id) in [
        "".to_owned(),
        "x".repeat(129),
        "bad/character".to_owned(),
        "é".to_owned(),
    ]
    .iter()
    .enumerate()
    {
        let id = 120 + n as i64;
        host.send(&message(id, "conv-1", "invalid", host_id)).await;
        assert_eq!(recv_id(&mut host, id, false).await["error"]["code"], -32602);
    }
    assert!(provider.runs.lock().is_empty());
    host.send(&message(125, "conv-1", "invalid", "valid")).await;
    finish(&mut host, "valid").await;
    host.send(&message(126, "conv-1", "invalid", "other")).await;
    assert_eq!(recv_id(&mut host, 126, true).await["error"]["code"], -32602);
    host.send(&message(127, "conv-1", "invalid", "valid")).await;
    assert_eq!(recv_id(&mut host, 127, true).await["result"]["ok"], true);
    assert_eq!(provider.runs.lock().len(), 1);
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_duplicate_race_is_scoped_to_conversation() {
    let (provider, hits, _, release, mut entered) = HoldProvider::new();
    let (_dir, channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        vec![],
        Default::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    // Separate socket writers cannot both own the channel; use concurrent frame
    // dispatch after a barrier through the same production on_frame entry.
    let results = channel
        .turnid_race(
            message(130, "conv-1", "race-a", "same"),
            message(131, "conv-1", "race-b", "same"),
        )
        .await;
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .any(|error| error.contains("duplicate turn_id same"))
    );
    wait_hits(&mut host, &hits, 1, &mut entered).await;
    host.send(&message(132, "conv-2", "race-c", "same")).await;
    wait_hits(&mut host, &hits, 2, &mut entered).await;
    let a = channel.turnid_live("conv-1", "same").unwrap();
    let b = channel.turnid_live("conv-2", "same").unwrap();
    assert_ne!(a, b);
    release.send(true).unwrap();
    let mut completed = 0;
    while completed < 2 {
        let f = host.recv().await;
        if f.get("method").is_some() {
            assert_eq!(f["params"]["turn_id"], "same");
            ack(&mut host, &f).await;
        }
        if f["params"]["state"] == "done" {
            completed += 1;
        }
    }
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_reuse_has_separate_durable_records_and_deletion_fence() {
    let (dir, channel, memory, mut host, provider) = identity_boot().await;
    for (id, input) in [(140, "one"), (141, "two")] {
        host.send(&message(id, "conv-1", input, "reuse")).await;
        finish(&mut host, "reuse").await;
        released(&channel, "conv-1", "reuse").await;
    }
    let runs = provider.runs.lock().clone();
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[0], runs[1]);
    let scoped = channel.memory_for_turn(memory.clone(), "conv-1");
    for run in &runs {
        for suffix in ["final", "usage"] {
            let key = format!("turn:{run}:{suffix}");
            let event = scoped
                .find_message_event_by_idempotency_key(dir.path().to_str().unwrap(), &key)
                .await
                .unwrap()
                .expect("test: distinct durable terminal/usage record");
            assert_eq!(event.run_id.as_deref(), Some(run.as_str()));
        }
    }
    let histories = scoped
        .load_recent_conversation_histories(&memory_principal(), 20, 10)
        .await
        .unwrap();
    let rows: Vec<_> = histories.values().flatten().collect();
    assert_eq!(rows.iter().filter(|r| r.role == "assistant").count(), 2);
    host.send(&json!({"jsonrpc":"2.0","id":142,"method":"inbound.message_deleted","params":{"conversation_id":"conv-1","message_id":"one","deleted_by":sender(),"timestamp_ms":3}})).await;
    assert_eq!(recv_id(&mut host, 142, true).await["result"]["erased"], true);
    host.send(&message(143, "conv-1", "three", "reuse")).await;
    finish(&mut host, "reuse").await;
    assert!(
        scoped
            .find_message_event_by_idempotency_key(dir.path().to_str().unwrap(), &format!("turn:{}:final", runs[0]))
            .await
            .unwrap()
            .is_none()
    );
    assert!(dir.path().exists());
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_queue_interrupt_and_displacement_release() {
    let (provider, hits, _, release, mut entered) = HoldProvider::new();
    let (_dir, channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        vec![],
        Default::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&message(150, "conv-1", "running", "run-host")).await;
    wait_hits(&mut host, &hits, 1, &mut entered).await;
    host.send(&message(151, "conv-1", "queued", "queue-host")).await;
    assert_eq!(recv_id(&mut host, 151, true).await["result"]["ok"], true);
    assert!(channel.turnid_live("conv-1", "queue-host").is_none());
    host.send(&json!({"jsonrpc":"2.0","id":152,"method":"turn.cancel","params":{"reason":"host request","conversation_id":"conv-1","turn_id":"queue-host"}})).await;
    assert_eq!(recv_id(&mut host, 152, true).await["result"]["ok"], false);
    release.send(true).unwrap();
    finish(&mut host, "run-host").await;
    finish(&mut host, "queue-host").await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    channel.shutdown();

    let (provider, hits, _, release, mut entered) = HoldProvider::new();
    let (_dir, channel, _memory, mut host) = boot(
        |_| RamfluxConfig {
            on_new_message: crate::config::RamfluxOnNewMessage::Interrupt,
            ..Default::default()
        },
        provider,
        vec![],
        Default::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&message(153, "conv-1", "old", "old-host")).await;
    wait_hits(&mut host, &hits, 1, &mut entered).await;
    let old = channel.turnid_live("conv-1", "old-host").unwrap();
    channel.turnid_queue_barrier(true);
    host.send(&message(155, "conv-1", "displaced", "displaced-host")).await;
    assert_eq!(recv_id(&mut host, 155, true).await["result"]["ok"], true);
    channel.turnid_queue_barrier(false);
    host.send(&message(154, "conv-1", "new", "new-host")).await;
    // The old mapping remains until guard cleanup, including its failure frame.
    let mut old_finished = false;
    while !old_finished {
        let f = host.recv().await;
        if f["method"] == "turn.status" && f["params"]["state"] == "failed" && f["params"]["turn_id"] == "old-host" {
            old_finished = true;
        }
        if f.get("method").is_some() {
            ack(&mut host, &f).await;
        }
    }
    assert!(uuid::Uuid::parse_str(&old).is_ok());
    wait_hits(&mut host, &hits, 2, &mut entered).await;
    release.send(true).unwrap();
    finish(&mut host, "new-host").await;
    host.send(&message(156, "conv-1", "after-displacement", "displaced-host"))
        .await;
    finish(&mut host, "displaced-host").await;
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_cancel_current_unknown_and_queued() {
    let (provider, hits, _, _release, mut entered) = HoldProvider::new();
    let (_dir, channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        vec![],
        Default::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&message(160, "conv-1", "live", "cancel-host")).await;
    wait_hits(&mut host, &hits, 1, &mut entered).await;
    let internal = channel.turnid_live("conv-1", "cancel-host").unwrap();
    for (id, turn, expected) in [
        (161, "unknown", false),
        (162, internal.as_str(), false),
        (163, "cancel-host", true),
    ] {
        host.send(&json!({"jsonrpc":"2.0","id":id,"method":"turn.cancel","params":{"reason":"host request","conversation_id":"conv-1","turn_id":turn}})).await;
        assert_eq!(recv_id(&mut host, id, true).await["result"]["ok"], expected);
    }
    // recv_id can have consumed the terminal frame while awaiting cancel ACK.
    loop {
        tokio::select! {
            () = released(&channel, "conv-1", "cancel-host") => break,
            frame = host.recv() => { if frame.get("method").is_some() { ack(&mut host, &frame).await; } }
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_surface_action_validation_duplicate_and_failed_admission() {
    let (provider, hits, _, release, mut entered) = HoldProvider::new();
    let (_dir, channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        vec![],
        Default::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&message(170, "conv-1", "seed", "seed-host")).await;
    wait_hits(&mut host, &hits, 1, &mut entered).await;
    host.send(&action(171, "bad/id", "conv-1")).await;
    assert_eq!(recv_id(&mut host, 171, true).await["error"]["code"], -32602);
    host.send(&action(172, "seed-host", "conv-1")).await;
    assert_eq!(recv_id(&mut host, 172, true).await["error"]["code"], -32602);
    host.send(&action(173, "action-host", "conv-1")).await;
    assert_eq!(recv_id(&mut host, 173, true).await["result"]["ok"], true);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // A conversation without metadata fails asynchronously after ACK and must
    // immediately release its reserved host correlation.
    for id in [174, 175] {
        host.send(&action(id, "unbound-action", "missing-chat")).await;
        assert_eq!(recv_id(&mut host, id, true).await["result"]["ok"], true);
        channel.turnid_wait_unreserved("missing-chat", "unbound-action").await;
    }
    // Reserve an action behind a blocked listener, then erase its source
    // before that listener can derive/admit the action's message.
    let paused = channel.turnid_pause_forward();
    host.send(&message(176, "conv-blocker", "blocker", "blocker-host"))
        .await;
    assert_eq!(recv_id(&mut host, 176, true).await["result"]["ok"], true);
    paused.await.unwrap();
    let mut pending = action(177, "pending-action", "conv-1");
    pending["params"]["message_id"] = "pre-listener-source".into();
    host.send(&pending).await;
    assert_eq!(recv_id(&mut host, 177, true).await["result"]["ok"], true);
    host.send(&json!({"jsonrpc":"2.0","id":178,"method":"inbound.message_deleted","params":{"conversation_id":"conv-1","message_id":"pre-listener-source","deleted_by":sender(),"timestamp_ms":3}})).await;
    assert_eq!(recv_id(&mut host, 178, true).await["result"]["erased"], true);
    channel.turnid_wait_unreserved("conv-1", "pending-action").await;
    pending["id"] = 179.into();
    host.send(&pending).await;
    assert_eq!(recv_id(&mut host, 179, true).await["result"]["ok"], true);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    channel.turnid_abort_forward();
    release.send(true).unwrap();
    finish(&mut host, "seed-host").await;
    finish(&mut host, "action-host").await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    channel.shutdown();
}

#[test]
fn turnid_failure_codes_use_controlled_internal_uuids() {
    let ids = super::super::super::ramflux::turnid_code_fixture();
    assert_ne!(ids[0], ids[1]);
    assert_eq!(ids, ["12345678", "87654321"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_unacked_replay_blocks_reuse_and_delayed_cancel_targets_successor() {
    let (_dir, channel, _memory, mut host, _provider) = identity_boot().await;
    host.send(&message(180, "conv-1", "replay-first", "replayed-host"))
        .await;
    // Keep one reply unacknowledged, but allow the turn to complete after the
    // existing request timeout; reconnect replay is a same-runtime operation.
    let mut unacked = None;
    loop {
        let f = host.recv().await;
        if f["method"] == "reply.text" && unacked.is_none() {
            unacked = Some(f);
            continue;
        }
        if f.get("method").is_some() {
            assert_eq!(f["params"]["turn_id"], "replayed-host");
            ack(&mut host, &f).await;
        }
        if matches!(f["params"]["state"].as_str(), Some("done" | "failed")) {
            break;
        }
    }
    released(&channel, "conv-1", "replayed-host").await;
    host.send(&message(181, "conv-1", "premature", "replayed-host")).await;
    assert_eq!(recv_id(&mut host, 181, true).await["error"]["code"], -32602);
    drop(host);
    channel.turnid_wait_disconnect().await;
    let mut host = Host::connect(channel.socket_path()).await;
    hello(&mut host, "full").await;
    let replay = host.recv().await;
    assert_eq!(replay, unacked.unwrap());
    assert_eq!(replay["params"]["turn_id"], "replayed-host");
    ack(&mut host, &replay).await;
    // This response is ordered after the ACK on the reader.
    host.send(&json!({"jsonrpc":"2.0","id":182,"method":"turn.cancel","params":{"reason":"host request","conversation_id":"conv-1","turn_id":"replayed-host"}})).await;
    assert_eq!(recv_id(&mut host, 182, true).await["result"]["ok"], false);
    let paused = channel.turnid_pause_forward();
    host.send(&message(183, "conv-1", "successor", "replayed-host")).await;
    assert_eq!(recv_id(&mut host, 183, true).await["result"]["ok"], true);
    paused.await.unwrap();
    channel.turnid_resume_forward();
    // Wait for started so the successor is bound, then send the delayed cancel.
    loop {
        let f = host.recv().await;
        if f.get("method").is_some() {
            ack(&mut host, &f).await;
        }
        if f["params"]["state"] == "started" {
            break;
        }
    }
    host.send(&json!({"jsonrpc":"2.0","id":184,"method":"turn.cancel","params":{"reason":"host request","conversation_id":"conv-1","turn_id":"replayed-host"}})).await;
    assert_eq!(recv_id(&mut host, 184, true).await["result"]["ok"], true);
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_forward_abort_drops_prebind_owner_and_allows_reuse() {
    let (_dir, channel, _memory, mut host, provider) = identity_boot().await;
    let entered = channel.turnid_pause_forward();
    host.send(&message(190, "conv-1", "aborted", "abort-host")).await;
    assert_eq!(recv_id(&mut host, 190, true).await["result"]["ok"], true);
    entered.await.unwrap();
    channel.turnid_abort_forward();
    channel.turnid_wait_unreserved("conv-1", "abort-host").await;
    assert!(provider.runs.lock().is_empty());
    host.send(&message(191, "conv-1", "replacement", "abort-host")).await;
    finish(&mut host, "abort-host").await;
    assert_eq!(provider.runs.lock().len(), 1);
    let entered = channel.turnid_pause_forward();
    host.send(&message(192, "conv-1", "deleted-before-bind", "deleted-prebind"))
        .await;
    assert_eq!(recv_id(&mut host, 192, true).await["result"]["ok"], true);
    entered.await.unwrap();
    host.send(&json!({"jsonrpc":"2.0","id":193,"method":"inbound.message_deleted","params":{"conversation_id":"conv-1","message_id":"deleted-before-bind","deleted_by":sender(),"timestamp_ms":3}})).await;
    assert_eq!(recv_id(&mut host, 193, true).await["result"]["erased"], true);
    channel.turnid_wait_unreserved("conv-1", "deleted-prebind").await;
    assert_eq!(provider.runs.lock().len(), 1, "deleted pre-bind work called provider");
    channel.turnid_abort_forward();
    host.send(&message(194, "conv-1", "after-prebind-delete", "deleted-prebind"))
        .await;
    finish(&mut host, "deleted-prebind").await;
    assert_eq!(provider.runs.lock().len(), 2);
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_stale_prebind_owner_cannot_release_successor_generation() {
    let (_dir, channel, _memory, mut host, provider) = identity_boot().await;
    let entered = channel.turnid_pause_forward();
    host.send(&message(200, "conv-1", "same-input", "stale-host")).await;
    assert_eq!(recv_id(&mut host, 200, true).await["result"]["ok"], true);
    entered.await.unwrap();
    let old = channel.turnid_hold_owner("conv-1", "same-input");
    channel.turnid_retire_unbound();
    channel.turnid_abort_forward();
    channel.turnid_wait_slot_released("conv-1", "same-input").await;
    let entered = channel.turnid_pause_forward();
    host.send(&message(201, "conv-1", "same-input", "stale-host")).await;
    assert_eq!(recv_id(&mut host, 201, true).await["result"]["ok"], true);
    entered.await.unwrap();
    drop(old);
    host.send(&message(202, "conv-1", "duplicate", "stale-host")).await;
    assert_eq!(recv_id(&mut host, 202, true).await["error"]["code"], -32602);
    channel.turnid_resume_forward();
    finish(&mut host, "stale-host").await;
    assert_eq!(provider.runs.lock().len(), 1);
    channel.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turnid_restart_keeps_legacy_durable_records_and_clears_volatile_state() {
    let (dir, channel, memory, mut host, provider) = identity_boot().await;
    // Pre-change records are produced by the unchanged no-host UUID path.
    host.send(&inbound_message(210, "conv-1", "legacy", "legacy durable input", 1))
        .await;
    let mut old_run = String::new();
    loop {
        let f = host.recv().await;
        if f.get("method").is_some() {
            old_run = f["params"]["turn_id"].as_str().unwrap().to_owned();
            ack(&mut host, &f).await;
        }
        if f["params"]["state"] == "done" {
            break;
        }
    }
    released(&channel, "conv-1", &old_run).await;
    host.send(&message(211, "conv-1", "deleted", "restart-host")).await;
    finish(&mut host, "restart-host").await;
    released(&channel, "conv-1", "restart-host").await;
    let deleted_run = provider.runs.lock()[1].clone();
    host.send(&json!({"jsonrpc":"2.0","id":212,"method":"inbound.message_deleted","params":{"conversation_id":"conv-1","message_id":"deleted","deleted_by":sender(),"timestamp_ms":3}})).await;
    assert_eq!(recv_id(&mut host, 212, true).await["result"]["erased"], true);
    channel.shutdown();
    drop(host);
    drop(channel);
    drop(memory);
    let reopened = Arc::new(crate::memory::SqliteMemory::new(dir.path()).unwrap());
    let restarted = Arc::new(
        RamfluxChannel::new(
            RamfluxConfig {
                socket_path: Some(dir.path().join("restart.sock")),
                ..Default::default()
            },
            dir.path(),
        )
        .unwrap(),
    );
    assert!(restarted.turnid_volatile_empty());
    let provider = Arc::new(IdentityProvider::default());
    let ctx = runtime(
        dir.path(),
        restarted.clone(),
        provider.clone(),
        vec![],
        reopened.clone(),
        Default::default(),
    );
    spawn_turns(restarted.clone(), ctx);
    let mut host = Host::connect(restarted.socket_path()).await;
    hello(&mut host, "full").await;
    host.send(&message(213, "conv-1", "restart-new", "restart-host")).await;
    finish(&mut host, "restart-host").await;
    released(&restarted, "conv-1", "restart-host").await;
    let scoped = restarted.memory_for_turn(reopened, "conv-1");
    let workspace = dir.path().to_str().unwrap();
    assert!(
        scoped
            .find_message_event_by_idempotency_key(workspace, &format!("turn:{old_run}:final"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        scoped
            .find_message_event_by_idempotency_key(workspace, &format!("turn:{deleted_run}:final"))
            .await
            .unwrap()
            .is_none()
    );
    let new_run = provider.runs.lock()[0].clone();
    assert_ne!(new_run, old_run);
    assert_ne!(new_run, deleted_run);
    let histories = scoped
        .load_recent_conversation_histories(&memory_principal(), 20, 10)
        .await
        .unwrap();
    let rows: Vec<_> = histories.values().flatten().collect();
    assert_eq!(rows.iter().filter(|r| r.role == "assistant").count(), 2);
    assert!(rows.iter().any(|r| r.content == "legacy durable input"));
    restarted.shutdown();
}
