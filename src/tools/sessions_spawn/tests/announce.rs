#![cfg(test)]

use super::*;

/// A task's terminal row is committed before its announcement. Await the
/// retained task owner, including its delivery/refusal, rather than assuming
/// that a fixed scheduler delay or a terminal status proves delivery finished.
async fn await_announcement_owners(tool: &SessionsSpawnTool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let runs = tool.active_runs_snapshot().await;
            assert!(!runs.is_empty(), "announcement test has no spawned run");
            if runs.iter().all(|run| {
                run.abort_handle
                    .as_ref()
                    .is_some_and(tokio::task::AbortHandle::is_finished)
            }) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("announcement owner did not finish delivery/refusal within the bounded deadline");
}

/// Regression for the cross-channel mis-routing bug: a sub-agent spawned from
/// wacli (a `@g.us` group recipient) must announce its result back on the
/// wacli channel — not on the construction-time default channel (which, in a
/// multi-channel deployment, was Signal). The channel/gateway loop calls
/// `set_active_channel` per message; this test simulates that switch and
/// asserts the announcement lands only on the active (wacli) channel.
#[tokio::test]
async fn announce_routes_to_active_channel_not_construction_default() {
    // Tool is built with a "signal" default channel (mirrors the deployment
    // default that caused the bug).
    let (signal_ch, signal_sent) = NamedRecordingChannel::new("signal");
    let tool = make_tool(
        signal_ch,
        Arc::new(EchoProvider {
            response: "sub-agent done".into(),
        }),
    );

    // A wacli group message arrives: the gateway switches the active channel
    // and recipient before the spawn turn (exactly as channels/mod.rs does).
    let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
    tool.set_active_channel(wacli_ch as Arc<dyn Channel>).await;
    tool.set_active_recipient("120363000000000000@g.us").await;

    let result = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "do the thing"})))
        .await
        .unwrap();
    assert!(result.success, "spawn should succeed: {:?}", result.error);

    await_announcement_owners(&tool).await;

    let on_wacli = wacli_sent.lock().await;
    let on_signal = signal_sent.lock().await;
    assert_eq!(
        on_wacli.len(),
        1,
        "result must be announced on the originating (wacli) channel"
    );
    assert!(on_wacli[0].contains("sub-agent done"));
    assert!(
        on_signal.is_empty(),
        "result must NOT be announced on the construction-time default (signal) channel"
    );
}

/// P0 concurrency race: announce must route by the run's *per-turn* channel +
/// recipient (captured atomically from the launching message's scope), NOT by
/// the shared "active" channel/recipient that a concurrently-processed message
/// can overwrite between the spawning turn entering the LLM loop and the spawn
/// actually executing.
///
/// Scenario: message A arrives on `wacli` and begins a turn; before A's spawn
/// executes, message B (on `signal`) overwrites the shared active
/// channel/recipient (the gateway loop calls `set_active_*` per message). With
/// the old shared-state model A's result would leak onto signal+B's recipient
/// (cross-channel privacy leak). The fix binds A's announce to A's own scope.
#[tokio::test]
async fn announce_uses_per_turn_channel_not_shared_state() {
    let (signal_ch, signal_sent) = NamedRecordingChannel::new("signal");
    let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
    let tool = make_tool_with_channels(
        signal_ch.clone(),
        Arc::new(EchoProvider {
            response: "A's private result".into(),
        }),
        vec![signal_ch, wacli_ch],
    );

    // Message B (signal) has already overwritten the shared active state — this
    // is the racing message whose values would corrupt A under the old model.
    tool.set_active_channel({
        let (b_ch, _) = NamedRecordingChannel::new("signal");
        b_ch as Arc<dyn Channel>
    })
    .await;
    tool.set_active_recipient("B-signal-recipient").await;

    // A's spawn now executes, carrying A's own per-turn scope (wacli + A's
    // recipient). No explicit `recipient` arg, so it must come from the scope.
    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(with_scope(json!({"task": "do A's work"}), "wacli", "A-wacli-recipient")),
    )
    .await
    .unwrap();
    assert!(result.success, "spawn should succeed: {:?}", result.error);

    await_announcement_owners(&tool).await;

    let on_wacli = wacli_sent.lock().await;
    let on_signal = signal_sent.lock().await;
    assert_eq!(
        on_wacli.len(),
        1,
        "A's result must announce on its own (wacli) channel, not the shared active (signal) one"
    );
    assert!(on_wacli[0].contains("A's private result"));
    assert!(
        on_signal.is_empty(),
        "A's result must NOT leak onto signal (the racing message B's channel)"
    );

    // And the recipient must be A's scope chat_id, not B's shared recipient.
    let runs = tool.active_runs_snapshot().await;
    let a_run = runs.first().expect("one run registered");
    assert_eq!(a_run.recipient.as_deref(), Some("A-wacli-recipient"));
    assert_eq!(a_run.channel_name.as_deref(), Some("wacli"));
}

// ── Outbound ACL coverage for the announcement path ──────────────────
//
// `message_send` has cleared `SecurityPolicy::is_outbound_allowed` since the
// ACL landed; the announcement did not. Since `recipient` is a plain
// `sessions_spawn` parameter the model fills in, that made "spawn a
// throwaway task naming any recipient" a complete bypass of every
// `send_deny` an operator wrote. These four tests pin the fix from both
// sides: the gate bites, and it stays inert where no rules exist.

/// A scope rule carrying only outbound entries — the shape an operator
/// writes to restrict who the agent may reach, with no tool restrictions.
fn announce_scope_rule(send_allow: &[&str], send_deny: &[&str]) -> crate::config::ScopeRule {
    crate::config::ScopeRule {
        user: None,
        channel: None,
        chat_type: None,
        tools_allow: vec![],
        tools_deny: vec![],
        send_allow: send_allow.iter().map(|entry| (*entry).to_string()).collect(),
        send_deny: send_deny.iter().map(|entry| (*entry).to_string()).collect(),
        assign_allow: vec![],
        assign_deny: vec![],
    }
}

fn announce_policy(rules: Vec<crate::config::ScopeRule>) -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy {
        scope_rules: rules,
        ..SecurityPolicy::default()
    })
}

/// A `send_deny` hit stops the announcement from leaving — and says so.
///
/// The second half matters as much as the first: a security refusal that
/// drops the message and logs nothing an operator can find is
/// indistinguishable from the run having crashed. The refusal is recorded on
/// the run's own history, which `action='history'` and `session_status`
/// surface.
///
/// MUTATION GUARD: send straight to the channel from the task-mode announce
/// block, or drop the gate inside `deliver_or_withhold_notice`, and this test
/// goes red on the first assertion.
#[tokio::test]
async fn announce_to_a_denied_recipient_is_withheld_and_recorded_on_the_run() {
    let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
    let tool = make_tool_with_channels_and_security(
        wacli_ch.clone(),
        Arc::new(EchoProvider {
            response: "sub-agent done".into(),
        }),
        vec![wacli_ch],
        announce_policy(vec![announce_scope_rule(&[], &["wacli:+15550001111"])]),
    );

    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(with_scope(json!({"task": "do the thing"}), "wacli", "+15550001111")),
    )
    .await
    .unwrap();
    assert!(
        result.success,
        "the ACL gates delivery, not the spawn itself: {:?}",
        result.error
    );

    await_announcement_owners(&tool).await;

    assert!(
        wacli_sent.lock().await.is_empty(),
        "a send_deny hit must stop the announcement from reaching the channel"
    );

    let runs = tool.active_runs_snapshot().await;
    let run = runs.first().expect("one run registered");
    let history = run.history.read().await;
    assert!(
        history.iter().any(|entry| {
            entry.content.contains("Announcement withheld")
                && entry.content.contains("not permitted by the configured scope rules")
        }),
        "the refusal must be recorded on the run, not silently swallowed; history was: {:?}",
        history.iter().map(|entry| entry.content.as_str()).collect::<Vec<_>>()
    );
}

/// Zero regression: with no outbound rules configured — every deployment
/// today — the announcement goes out exactly as it always did.
#[tokio::test]
async fn announce_is_unchanged_when_no_outbound_rules_are_configured() {
    let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
    let tool = make_tool_with_channels(
        wacli_ch.clone(),
        Arc::new(EchoProvider {
            response: "sub-agent done".into(),
        }),
        vec![wacli_ch],
    );

    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(with_scope(json!({"task": "do the thing"}), "wacli", "+15550001111")),
    )
    .await
    .unwrap();
    assert!(result.success, "spawn should succeed: {:?}", result.error);

    await_announcement_owners(&tool).await;

    let sent = wacli_sent.lock().await;
    assert_eq!(
        sent.len(),
        1,
        "an unconfigured deployment must keep announcing exactly as before"
    );
    assert!(sent[0].contains("sub-agent done"));
}

/// The zero-regression tripwire for *how* the gate is called.
///
/// This run's captured scope names channel `wacli`, but the tool has no
/// channel registry (the single-channel and CLI paths, plus every caller
/// that never calls `with_channels`), so `resolve_announce_channel` falls
/// back to the shared active channel — here named `signal`. That fallback is
/// the pre-existing route, not a cross-channel jump anybody requested. Judge
/// the announcement with the captured scope channel as its source and the
/// destination would differ from it, the ACL's channel default would flip to
/// deny, and *every* announcement in a deployment with no rules at all would
/// disappear without a trace. Hence `announce_is_authorized` anchors source
/// and destination on the channel actually carrying the message.
#[tokio::test]
async fn announce_is_not_denied_by_a_channel_registry_fallback() {
    let (signal_ch, signal_sent) = NamedRecordingChannel::new("signal");
    let tool = make_tool(
        signal_ch as Arc<dyn Channel>,
        Arc::new(EchoProvider {
            response: "fallback result".into(),
        }),
    );

    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(with_scope(json!({"task": "do the thing"}), "wacli", "+15550001111")),
    )
    .await
    .unwrap();
    assert!(result.success, "spawn should succeed: {:?}", result.error);

    await_announcement_owners(&tool).await;

    let sent = signal_sent.lock().await;
    assert_eq!(
        sent.len(),
        1,
        "an announcement that fell back to the shared active channel must still be delivered"
    );
    assert!(sent[0].contains("fallback result"));
}

/// The bypass this task closes, stated directly: `recipient` is a parameter
/// the model writes, so it must not be able to address someone the operator
/// denied. The control spawn in the same test proves the rule is a targeted
/// refusal and not a blanket kill of the announce path.
#[tokio::test]
async fn a_model_supplied_recipient_cannot_bypass_send_deny() {
    let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
    let tool = make_tool_with_channels_and_security(
        wacli_ch.clone(),
        Arc::new(EchoProvider {
            response: "sub-agent done".into(),
        }),
        vec![wacli_ch],
        announce_policy(vec![announce_scope_rule(&[], &["wacli:+15559999999"])]),
    );

    // The turn is anchored to an allowed chat, and the model overrides the
    // announcement's destination with the denied one.
    let denied = execute_admitted_spawn(
        &tool,
        with_spawn_grant(with_scope(
            json!({"task": "reach the denied recipient", "recipient": "+15559999999"}),
            "wacli",
            "+15550002222",
        )),
    )
    .await
    .unwrap();
    assert!(denied.success, "spawn should succeed: {:?}", denied.error);

    await_announcement_owners(&tool).await;
    assert!(
        wacli_sent.lock().await.is_empty(),
        "a model-chosen recipient must not escape the operator's send_deny"
    );

    // Control: the very same override to a recipient outside the deny list
    // is delivered, so the gate is not simply refusing everything.
    let allowed = execute_admitted_spawn(
        &tool,
        with_spawn_grant(with_scope(
            json!({"task": "reach an allowed recipient", "recipient": "+15550003333"}),
            "wacli",
            "+15550002222",
        )),
    )
    .await
    .unwrap();
    assert!(allowed.success, "spawn should succeed: {:?}", allowed.error);

    await_announcement_owners(&tool).await;
    let sent = wacli_sent.lock().await;
    assert_eq!(sent.len(), 1, "the undenied recipient must still receive the result");
    assert!(sent[0].contains("sub-agent done"));
}

// ── Outbound ACL coverage for the kill receipt ───────────────────────
//
// The kill notice is addressed to the run's `recipient` — the same value the
// model wrote on the *spawn* call — so closing the announcement alone left
// the identical bypass one action away: spawn naming any recipient, kill the
// run, receipt delivered unauthorized. These four tests are the announcement
// set restated for the kill path: the gate bites on a denied recipient, it
// records rather than swallows, a model-chosen recipient cannot escape a
// `send_deny`, and it stays inert both where no rules exist and where the
// channel registry falls back.

/// A long-running provider, so a spawned run is still `Running` when the
/// test kills it and a receipt is actually emitted.
fn unfinishing_provider() -> Arc<dyn crate::providers::Provider> {
    Arc::new(SleepyProvider {
        delay_ms: 5_000,
        response: "never reached".into(),
    })
}

/// The argument shape the runtime hands this tool for `action='kill'`: the
/// resource-operation grant plus the trusted `_zc_scope` that
/// `normalize_arguments` injects into *every* tool call.
fn kill_args(run_id: &str, channel: &str, chat_id: &str) -> serde_json::Value {
    let grant = serde_json::to_value(ApprovalGrant::for_resource_operation(
        "sessions_spawn",
        &format!("sessions_spawn:kill:{run_id}"),
        "test",
        None,
    ))
    .expect("kill grant serializes");
    with_scope(
        json!({
            "action": "kill",
            "run_id": run_id,
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG: grant,
        }),
        channel,
        chat_id,
    )
}

/// Spawn one task-mode run under `tool` and return its run id.
async fn spawn_long_run(tool: &SessionsSpawnTool, args: serde_json::Value, channel: &str, chat_id: &str) -> String {
    let spawned = execute_admitted_spawn(&tool, with_spawn_grant(with_scope(args, channel, chat_id)))
        .await
        .expect("spawn executes");
    assert!(spawned.success, "spawn should succeed: {:?}", spawned.error);
    tool.active_runs_snapshot()
        .await
        .last()
        .expect("a run was registered")
        .id
        .clone()
}

/// A `send_deny` hit stops the kill receipt from leaving — and says so on the
/// run, exactly as a withheld announcement does.
///
/// MUTATION GUARD: send straight to the channel in `execute_kill` and this
/// test goes red on the first assertion.
#[tokio::test]
async fn a_kill_notice_to_a_denied_recipient_is_withheld_and_recorded_on_the_run() {
    let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
    let tool = make_tool_with_channels_and_security(
        wacli_ch.clone(),
        unfinishing_provider(),
        vec![wacli_ch],
        announce_policy(vec![announce_scope_rule(&[], &["wacli:+15550001111"])]),
    );

    let run_id = spawn_long_run(
        &tool,
        json!({"task": "long work", "mode": "task"}),
        "wacli",
        "+15550001111",
    )
    .await;

    let kill = tool
        .execute(kill_args(&run_id, "wacli", "+15550001111"))
        .await
        .expect("kill executes");
    assert!(
        kill.success,
        "the ACL gates the receipt, not the kill itself: {:?}",
        kill.error
    );

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        wacli_sent.lock().await.is_empty(),
        "a send_deny hit must stop the kill notice from reaching the channel"
    );

    let runs = tool.active_runs_snapshot().await;
    let run = runs.iter().find(|r| r.id == run_id).expect("killed run still listed");
    let history = run.history.read().await;
    assert!(
        history.iter().any(|entry| {
            entry.content.contains("Kill notice withheld")
                && entry.content.contains("not permitted by the configured scope rules")
        }),
        "the refusal must be recorded on the run, not silently swallowed; history was: {:?}",
        history.iter().map(|entry| entry.content.as_str()).collect::<Vec<_>>()
    );
}

/// Zero regression: with no outbound rules configured — every deployment
/// today — the kill receipt goes out exactly as it always did.
#[tokio::test]
async fn a_kill_notice_is_unchanged_when_no_outbound_rules_are_configured() {
    let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
    let tool = make_tool_with_channels(wacli_ch.clone(), unfinishing_provider(), vec![wacli_ch]);

    let run_id = spawn_long_run(
        &tool,
        json!({"task": "long work", "mode": "task"}),
        "wacli",
        "+15550001111",
    )
    .await;
    let kill = tool
        .execute(kill_args(&run_id, "wacli", "+15550001111"))
        .await
        .expect("kill executes");
    assert!(kill.success, "kill should succeed: {:?}", kill.error);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let sent = wacli_sent.lock().await;
    assert_eq!(
        sent.len(),
        1,
        "an unconfigured deployment must keep sending kill receipts exactly as before"
    );
    assert!(sent[0].contains("killed"));
}

/// The zero-regression tripwire, kill variant.
///
/// The run's captured scope names `wacli`, but this tool has no channel
/// registry, so `resolve_announce_channel` falls back to the shared active
/// channel — here `signal`. Judge the receipt with the captured scope channel
/// as its source and source would differ from destination, the ACL's channel
/// default would flip to deny, and every kill receipt in a deployment with no
/// rules at all would vanish. Source and destination therefore both anchor on
/// the channel actually carrying the message.
#[tokio::test]
async fn a_kill_notice_is_not_denied_by_a_channel_registry_fallback() {
    let (signal_ch, signal_sent) = NamedRecordingChannel::new("signal");
    let tool = make_tool(signal_ch as Arc<dyn Channel>, unfinishing_provider());

    let run_id = spawn_long_run(
        &tool,
        json!({"task": "long work", "mode": "task"}),
        "wacli",
        "+15550001111",
    )
    .await;
    let kill = tool
        .execute(kill_args(&run_id, "wacli", "+15550001111"))
        .await
        .expect("kill executes");
    assert!(kill.success, "kill should succeed: {:?}", kill.error);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let sent = signal_sent.lock().await;
    assert_eq!(
        sent.len(),
        1,
        "a kill receipt that fell back to the shared active channel must still be delivered"
    );
    assert!(sent[0].contains("killed"));
}

/// The bypass this task closes, stated directly: `recipient` is written by
/// the model on the spawn call and the kill receipt is addressed to it, so it
/// must not reach someone the operator denied. The control run in the same
/// test proves the rule is a targeted refusal, not a blanket kill of the
/// receipt path.
#[tokio::test]
async fn a_model_supplied_recipient_cannot_bypass_send_deny_through_the_kill_notice() {
    let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
    let tool = make_tool_with_channels_and_security(
        wacli_ch.clone(),
        unfinishing_provider(),
        vec![wacli_ch],
        announce_policy(vec![announce_scope_rule(&[], &["wacli:+15559999999"])]),
    );

    // The turn is anchored to an allowed chat; the model overrides the run's
    // recipient with the denied one, then kills the run to trigger a receipt.
    let denied_run = spawn_long_run(
        &tool,
        json!({"task": "long work", "mode": "task", "recipient": "+15559999999"}),
        "wacli",
        "+15550002222",
    )
    .await;
    let denied_kill = tool
        .execute(kill_args(&denied_run, "wacli", "+15550002222"))
        .await
        .expect("kill executes");
    assert!(denied_kill.success, "kill should succeed: {:?}", denied_kill.error);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        wacli_sent.lock().await.is_empty(),
        "a model-chosen recipient must not escape the operator's send_deny via the kill receipt"
    );

    // Control: the same override to a recipient outside the deny list is
    // delivered.
    let allowed_run = spawn_long_run(
        &tool,
        json!({"task": "long work", "mode": "task", "recipient": "+15550003333"}),
        "wacli",
        "+15550002222",
    )
    .await;
    let allowed_kill = tool
        .execute(kill_args(&allowed_run, "wacli", "+15550002222"))
        .await
        .expect("kill executes");
    assert!(allowed_kill.success, "kill should succeed: {:?}", allowed_kill.error);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let sent = wacli_sent.lock().await;
    assert_eq!(sent.len(), 1, "the undenied recipient must still receive the receipt");
    assert!(sent[0].contains("killed"));
}
