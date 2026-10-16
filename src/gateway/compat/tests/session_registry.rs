#![cfg(test)]

use super::*;

/// `sessions_list` through the production registry: reachable, and still
/// isolated by issuer.
///
/// MUTATION GUARD: drop the `sessions_list` push from
/// `register_gateway_turn_tools` and the reachability assertions go red;
/// clear `with_owner_isolation` in `mcp_execution_context` and the
/// isolation assertions go red.
#[tokio::test]
async fn mcp_sessions_list_isolates_issuers_through_the_production_gateway_registry() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());
    let issuer_b = production_isolation_identity("issuer-b.example", workspace.path());
    assert_ne!(
        issuer_a.prx_owner_id, issuer_b.prx_owner_id,
        "sanity: the two identities differ only by issuer"
    );

    seed_owned_message_event(
        &memory,
        &issuer_a,
        crate::memory::MemoryVisibility::Session,
        "run-a-session",
        "issuer-a-session-secret",
    )
    .await;
    seed_owned_message_event(
        &memory,
        &issuer_a,
        crate::memory::MemoryVisibility::Private,
        "run-a-private",
        "issuer-a-private-secret",
    )
    .await;

    let by_b = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &issuer_b,
        "sessions_list",
        serde_json::json!({}),
    )
    .await;
    let seen_by_b = response_text(&by_b);
    assert!(
        !by_b.is_error,
        "the production registry must resolve sessions_list rather than refuse it: {seen_by_b}"
    );
    // The refusal an unresolvable name produces on this seam is the
    // execution service's, and it is lowercase
    // (`src/tools/execution.rs`: `unknown tool '{name}'; available tools:`).
    // The handler's own 404 text is not reachable from here: these tests
    // call `execute_mcp_tool_call` directly, below both admission gates.
    assert!(
        !seen_by_b.contains("unknown tool"),
        "V02-R7: the production registry must reach the tool, not answer an unknown-tool refusal: {seen_by_b}"
    );
    assert!(
        !seen_by_b.contains("issuer-a-session-secret"),
        "issuer B must not read issuer A's session-visibility row: {seen_by_b}"
    );
    assert!(
        !seen_by_b.contains("issuer-a-private-secret"),
        "issuer B must not read issuer A's private-visibility row: {seen_by_b}"
    );
    assert!(
        !seen_by_b.contains("run-a-session") && !seen_by_b.contains("run-a-private"),
        "issuer B must not learn issuer A's run ids: {seen_by_b}"
    );

    let by_a = run_sessions_tool_on(registry, &config, &issuer_a, "sessions_list", serde_json::json!({})).await;
    let seen_by_a = response_text(&by_a);
    assert!(!by_a.is_error, "issuer A's own call must succeed: {seen_by_a}");
    // Isolation satisfied by answering nothing to everybody would be a
    // capability regression wearing the fix's clothes.
    assert!(
        seen_by_a.contains("run-a-session") && seen_by_a.contains("run-a-private"),
        "issuer A must still read its own rows through the production registry: {seen_by_a}"
    );
}

/// The same, for `sessions_history`, which recovers one run's transcript
/// from the same read model.
#[tokio::test]
async fn mcp_sessions_history_isolates_issuers_through_the_production_gateway_registry() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());
    let issuer_b = production_isolation_identity("issuer-b.example", workspace.path());

    seed_owned_message_event(
        &memory,
        &issuer_a,
        crate::memory::MemoryVisibility::Session,
        "run-a-session",
        "issuer-a-session-secret",
    )
    .await;

    let by_b = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &issuer_b,
        "sessions_history",
        serde_json::json!({"run_id": "run-a-session"}),
    )
    .await;
    let seen_by_b = response_text(&by_b);
    assert!(
        !seen_by_b.contains("issuer-a-session-secret"),
        "issuer B must not read issuer A's run transcript: {seen_by_b}"
    );
    // See the note in the `sessions_list` test above: the only refusal an
    // unregistered name can produce here is the execution service's
    // lowercase `unknown tool '…'`.
    assert!(
        !seen_by_b.contains("unknown tool"),
        "V02-R7: issuer B must be refused by the owner filter, not by an unresolvable name: {seen_by_b}"
    );

    let by_a = run_sessions_tool_on(
        registry,
        &config,
        &issuer_a,
        "sessions_history",
        serde_json::json!({"run_id": "run-a-session"}),
    )
    .await;
    let seen_by_a = response_text(&by_a);
    assert!(!by_a.is_error, "issuer A's own call must succeed: {seen_by_a}");
    assert!(
        seen_by_a.contains("issuer-a-session-secret"),
        "issuer A must still read its own run transcript through the production registry: {seen_by_a}"
    );
}

/// The task and result text a spawned run records, seeded at the
/// `workspace` visibility `sessions_spawn` actually writes.
const SPAWN_TASK_TEXT: &str = "prx spawn task text owned by the first caller";

const SPAWN_RESULT_TEXT: &str = "prx spawn result text produced by the sub-agent";

const SPAWN_RUN_ID: &str = "run-workspace-spawn";

/// Record one sub-agent run's events exactly as `sessions_spawn` records
/// them: the scope comes from `RuntimeEnvelope::sessions_spawn`, so its
/// `workspace` visibility is production's choice and not the test's, and
/// the four writes are the ones `record_spawn_request_event` and
/// `record_spawn_result_event` make — the `task.spawned` lifecycle event,
/// the role-`user` request message carrying the full task string, the
/// role-`event` worker result carrying the full result text, and the
/// terminal lifecycle event carrying the result preview.
async fn seed_workspace_visibility_spawn_run(
    memory: &Arc<crate::memory::SqliteMemory>,
    identity: &ExternalAgentIdentity,
    run_id: &str,
    task: &str,
    result_text: &str,
) {
    let shared: Arc<dyn crate::memory::Memory> = memory.clone();
    let fabric = crate::memory::MemoryFabric::new(shared, identity.workspace_id.clone());
    let session_key = format!("mcp:{}", identity.external_subject);
    let mut scope =
        crate::runtime::envelope::RuntimeEnvelope::sessions_spawn(&identity.workspace_id, &session_key, run_id)
            .with_channel("gateway")
            .with_sender(&identity.external_subject)
            .message_scope();
    scope.owner_id = Some(identity.prx_owner_id.clone());
    assert_eq!(
        scope.visibility,
        crate::memory::MemoryVisibility::Workspace,
        "test: this fixture is only meaningful while sessions_spawn writes at workspace visibility"
    );

    let lineage = serde_json::json!({ "owner_id": identity.prx_owner_id });
    fabric
        .record_inbound_user_message(scope.clone(), task, None, Some(lineage.to_string()))
        .await
        .expect("test: record the spawn request message");
    fabric
        .record_task_event(
            scope.clone(),
            run_id,
            "task.spawned",
            Some(serde_json::json!({ "task": task, "owner_id": identity.prx_owner_id }).to_string()),
        )
        .await
        .expect("test: record task.spawned");
    fabric
        .record_worker_result(
            scope.clone(),
            result_text,
            Some(serde_json::json!({ "success": true, "owner_id": identity.prx_owner_id }).to_string()),
        )
        .await
        .expect("test: record the worker result message");
    fabric
        .record_task_event(
            scope,
            run_id,
            "task.completed",
            Some(
                serde_json::json!({
                    "success": true,
                    "result_preview": result_text,
                    "owner_id": identity.prx_owner_id,
                })
                .to_string(),
            ),
        )
        .await
        .expect("test: record task.completed");
}

/// F-1: the recovered memory projection must not hand caller B the task or
/// result text of caller A's sub-agent run.
///
/// `sessions_spawn` writes a run's request message, its result message and
/// its lifecycle events at `MemoryVisibility::Workspace`, and the
/// `workspace` arm of the event read model carries no owner predicate. When
/// the in-process registry denies the run — which it always does for an
/// owner-isolated caller that does not own it — both session read tools
/// fall through to `sessions_read_model`, which rendered those rows: 200
/// character previews of the transcript in `sessions_history`, the task
/// string and a 60 character result preview in `sessions_list`.
///
/// MUTATION GUARD: remove the owner qualification from
/// `sessions_read_model::recover_task_runs` / `recover_run_history` and
/// every assertion below goes red.
#[tokio::test]
async fn mcp_session_reads_withhold_another_callers_workspace_visibility_spawn_content() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());
    let issuer_b = production_isolation_identity("issuer-b.example", workspace.path());
    assert_ne!(
        issuer_a.prx_owner_id, issuer_b.prx_owner_id,
        "sanity: the two identities differ only by issuer"
    );

    seed_workspace_visibility_spawn_run(&memory, &issuer_a, SPAWN_RUN_ID, SPAWN_TASK_TEXT, SPAWN_RESULT_TEXT).await;

    let history = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &issuer_b,
        "sessions_history",
        serde_json::json!({ "run_id": SPAWN_RUN_ID }),
    )
    .await;
    let history_text = response_text(&history);
    assert!(
        !history_text.contains("unknown tool"),
        "V02-R7: the refusal must come from the owner boundary, not from an unresolvable name: {history_text}"
    );
    assert!(
        !history_text.contains(SPAWN_TASK_TEXT),
        "issuer B must not read issuer A's spawn task text: {history_text}"
    );
    assert!(
        !history_text.contains(SPAWN_RESULT_TEXT),
        "issuer B must not read issuer A's sub-agent result text: {history_text}"
    );
    assert!(
        history.is_error && history_text.contains("No run found"),
        "issuer B must get the ordinary not-found refusal for a run it does not own: {history_text}"
    );

    let listed = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &issuer_b,
        "sessions_list",
        serde_json::json!({}),
    )
    .await;
    let listed_text = response_text(&listed);
    assert!(!listed.is_error, "the call itself must be reached: {listed_text}");
    assert!(
        !listed_text.contains(SPAWN_TASK_TEXT),
        "issuer B must not read issuer A's spawn task text: {listed_text}"
    );
    assert!(
        !listed_text.contains(SPAWN_RESULT_TEXT),
        "issuer B must not read issuer A's sub-agent result preview: {listed_text}"
    );
    assert!(
        !listed_text.contains(SPAWN_RUN_ID),
        "issuer B must not learn issuer A's run id: {listed_text}"
    );
    assert!(
        !listed_text.contains(&issuer_a.prx_owner_id),
        "issuer B must not learn issuer A's owner id: {listed_text}"
    );
}

/// The companion to the test above: owner-qualifying the recovered
/// projection must not be satisfied by answering nothing to everybody.
///
/// Caller A owns the run, so A still reads its own recovered transcript and
/// still finds the run in its own listing.
#[tokio::test]
async fn mcp_session_reads_still_recover_the_callers_own_workspace_visibility_run() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());

    seed_workspace_visibility_spawn_run(&memory, &issuer_a, SPAWN_RUN_ID, SPAWN_TASK_TEXT, SPAWN_RESULT_TEXT).await;

    let history = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &issuer_a,
        "sessions_history",
        serde_json::json!({ "run_id": SPAWN_RUN_ID }),
    )
    .await;
    let history_text = response_text(&history);
    assert!(!history.is_error, "issuer A's own call must succeed: {history_text}");
    assert!(
        history_text.contains(SPAWN_TASK_TEXT) && history_text.contains(SPAWN_RESULT_TEXT),
        "issuer A must still recover its own run's transcript: {history_text}"
    );

    let listed = run_sessions_tool_on(registry, &config, &issuer_a, "sessions_list", serde_json::json!({})).await;
    let listed_text = response_text(&listed);
    assert!(!listed.is_error, "issuer A's own call must succeed: {listed_text}");
    assert!(
        listed_text.contains(SPAWN_RUN_ID) && listed_text.contains(SPAWN_TASK_TEXT),
        "issuer A must still see its own run in the recovered listing: {listed_text}"
    );
}

/// A non-isolated caller's view of the same projection is unchanged.
///
/// Only an owner-isolated origin is confined. An in-process turn — chat,
/// channel, agent, cron, xin, sub-agent, and the gateway's own turns —
/// carries no `owner_isolated` marker, so it keeps the workspace-shared
/// view of every run in the workspace, including runs owned by someone
/// else.
///
/// MUTATION GUARD: make `RunOwnerScope::retain_admitted` filter
/// unconditionally, or key the confinement off anything other than the
/// service-authored marker, and this goes red.
#[tokio::test]
async fn session_reads_keep_the_workspace_shared_view_for_a_non_isolated_caller() {
    const OTHER_RUN_ID: &str = "run-workspace-spawn-b";
    const OTHER_TASK_TEXT: &str = "prx spawn task text owned by the second caller";
    const OTHER_RESULT_TEXT: &str = "prx spawn result text for the second caller";

    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());
    let issuer_b = production_isolation_identity("issuer-b.example", workspace.path());

    seed_workspace_visibility_spawn_run(&memory, &issuer_a, SPAWN_RUN_ID, SPAWN_TASK_TEXT, SPAWN_RESULT_TEXT).await;
    seed_workspace_visibility_spawn_run(&memory, &issuer_b, OTHER_RUN_ID, OTHER_TASK_TEXT, OTHER_RESULT_TEXT).await;

    // An in-process call: no `_zc_scope_trusted`, no `owner_isolated`
    // marker, which is what every non-MCP origin reaches these tools with.
    let list = registry
        .iter()
        .find(|tool| tool.supports_name("sessions_list"))
        .expect("test: the production registry must publish sessions_list");
    let listed = list
        .execute(serde_json::json!({}))
        .await
        .expect("test: sessions_list must answer");
    assert!(
        listed.output.contains(SPAWN_RUN_ID) && listed.output.contains(SPAWN_TASK_TEXT),
        "a non-isolated caller keeps the workspace-shared view of the first caller's run: {}",
        listed.output
    );
    assert!(
        listed.output.contains(OTHER_RUN_ID) && listed.output.contains(OTHER_TASK_TEXT),
        "a non-isolated caller keeps the workspace-shared view of the second caller's run: {}",
        listed.output
    );

    let history = registry
        .iter()
        .find(|tool| tool.supports_name("sessions_history"))
        .expect("test: the production registry must publish sessions_history");
    for (run_id, task, result) in [
        (SPAWN_RUN_ID, SPAWN_TASK_TEXT, SPAWN_RESULT_TEXT),
        (OTHER_RUN_ID, OTHER_TASK_TEXT, OTHER_RESULT_TEXT),
    ] {
        let recovered = history
            .execute(serde_json::json!({ "run_id": run_id }))
            .await
            .expect("test: sessions_history must answer");
        assert!(
            recovered.success && recovered.output.contains(task) && recovered.output.contains(result),
            "a non-isolated caller keeps the recovered transcript of `{run_id}`: {}",
            recovered.output
        );
    }
}

/// A caller that carries no owner id is admitted to no owner-isolated row
/// and to no in-process run.
///
/// `RunOwnerScope::Owner(None)` and the `AND FALSE` owner predicate are the
/// fail-closed branches; this pins that the production gateway registry and
/// envelope really do produce that scope, for the persisted projection and
/// for the shared run registry alike. Without the second half the
/// absent-owner branch could degrade to "admit everything", which is the
/// original defect with an extra step.
///
/// Both halves are now asserted, not one. `sessions_spawn` records its
/// lifecycle events and its request/result messages at
/// `MemoryVisibility::Workspace`, whose SQL arm carries no owner predicate,
/// so the recovered projection used to hand the probe run back as
/// `source=memory` even to a caller admitted to nothing. The read model
/// now applies the same confinement to the rows it was handed, so an
/// owner-less isolated caller reaches no run at all, from either source.
/// The probe run's continued existence is proven by a non-isolated read of
/// the same registry at the end, so nothing here passes vacuously.
#[tokio::test]
async fn mcp_sessions_reads_admit_nothing_when_the_caller_carries_no_owner_id() {
    const UNOWNED_RUN_TASK: &str = "prx unowned registry run";
    const OWNED_RUN_TASK: &str = "prx owned registry run";

    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let owner = production_isolation_identity("issuer-a.example", workspace.path());
    // The same authenticated shape, but with a blank owner id: this is what
    // the envelope carries when identity derivation produced no usable
    // owner, and `RunOwnerScope::from_args` maps it to `Owner(None)`.
    let ownerless = ExternalAgentIdentity {
        prx_owner_id: "   ".to_string(),
        ..owner.clone()
    };

    seed_owned_message_event(
        &memory,
        &owner,
        crate::memory::MemoryVisibility::Session,
        "run-a-session",
        "issuer-a-session-secret",
    )
    .await;

    // Restore explicit historical metadata, including a legacy unowned row
    // predating owner enforcement. This is not a source-less spawn and gives
    // neither row a driver or an authority token. Both persist in SQLite and
    // occupy the same shared runtime-registry shape used by the production
    // session read tools. The test owns that fixture directly; production
    // registration contains no test-only seeding hook.
    let mut rows = Vec::new();
    for (run_id, task, row_owner) in [
        ("prx-legacy-unowned", UNOWNED_RUN_TASK, None),
        ("prx-existing-owned", OWNED_RUN_TASK, Some(owner.prx_owner_id.clone())),
    ] {
        use crate::memory::Memory as _;
        let input: crate::memory::MessageEventInput = serde_json::from_value(serde_json::json!({
            "workspace_id": workspace.path().to_string_lossy(),
            "owner_id": row_owner,
            "source": "sessions_spawn",
            "channel": "gateway",
            "session_key": "prx_historical_session",
            "run_id": run_id,
            "sender": "prx_fixture",
            "role": "user",
            "event_type": "message.user",
            "visibility": "workspace",
            "content": task,
        }))
        .expect("historical persisted row");
        memory
            .append_message_event(input)
            .await
            .expect("persist historical fixture row");
        rows.push(crate::tools::sessions_spawn::SubAgentRun {
            id: run_id.to_owned(),
            task: task.to_owned(),
            owner_id: row_owner,
            topic_id: None,
            source_message_event_id: None,
            started_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            status: crate::tools::sessions_spawn::SubAgentStatus::Completed("historical fixture".to_owned()),
            recipient: None,
            channel_name: Some("gateway".to_owned()),
            abort_handle: None,
            process_control: None,
            task_control: None,
            history: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "prx_historical_session".to_owned(),
            spawn_depth: 0,
            token_usage_records: Vec::new(),
            progress: crate::agent::idle::child_beat(),
            batch_id: None,
        });
    }
    let active_runs = Arc::new(tokio::sync::RwLock::new(rows));
    let shared_memory: Arc<dyn crate::memory::Memory> = memory.clone();
    let workspace_id = workspace.path().to_string_lossy().into_owned();
    let registry: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![
        Box::new(
            crate::tools::SessionsListTool::new(Arc::clone(&active_runs))
                .with_shared_memory(Arc::clone(&shared_memory), workspace_id.clone()),
        ),
        Box::new(crate::tools::SessionsHistoryTool::new(active_runs).with_shared_memory(shared_memory, workspace_id)),
    ]);

    let listed = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &ownerless,
        "sessions_list",
        serde_json::json!({}),
    )
    .await;
    let listed_text = response_text(&listed);
    assert!(!listed.is_error, "the call itself must be reached: {listed_text}");
    assert!(
        !listed_text.contains("issuer-a-session-secret") && !listed_text.contains("run-a-session"),
        "a caller with no owner id must not read an owner-isolated row: {listed_text}"
    );
    // `sessions_list` renders a run from the shared in-process registry as
    // `source=runtime, manageable=true` and a memory-recovered one as
    // `source=memory`. An owner-isolated caller never sees a registry run
    // it cannot be shown to own — including an unowned one, because the
    // registry has no way to prove such a run is the caller's — so no
    // runtime-sourced line may appear at all.
    assert!(
        !listed_text.contains("source=runtime"),
        "an owner-isolated caller with no owner id must be admitted to no run in the shared \
             registry, not even an unowned one: {listed_text}"
    );
    assert!(
        !listed_text.contains("manageable=true"),
        "a run the caller is not admitted to must never be handed back as steerable: {listed_text}"
    );
    assert!(
        !listed_text.contains(OWNED_RUN_TASK),
        "ownerless caller must not see the owned registry row"
    );
    // ...and not through the recovered projection either. The probe run's
    // lifecycle event is `workspace`-visibility, which SQL admits to every
    // principal of the workspace; the read model owner-qualifies the rows
    // it is handed, so this caller reaches no run from either source.
    assert!(
        !listed_text.contains(UNOWNED_RUN_TASK),
        "an owner-isolated caller with no owner id must reach no run in the recovered memory \
             projection either: {listed_text}"
    );
    assert!(
        listed_text.contains("No sessions found"),
        "an owner-isolated caller with no owner id must be told there is nothing, not shown \
             someone else's run: {listed_text}"
    );

    let history = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &ownerless,
        "sessions_history",
        serde_json::json!({"run_id": "run-a-session"}),
    )
    .await;
    let history_text = response_text(&history);
    assert!(
        history.is_error && history_text.contains("No run found"),
        "a caller with no owner id must get the not-found refusal, not the transcript: {history_text}"
    );
    assert!(
        !history_text.contains("issuer-a-session-secret"),
        "the refusal must not leak the transcript it withheld: {history_text}"
    );

    for run_id in ["prx-legacy-unowned", "prx-existing-owned"] {
        let hidden = run_sessions_tool_on(
            Arc::clone(&registry),
            &config,
            &ownerless,
            "sessions_history",
            serde_json::json!({"run_id": run_id}),
        )
        .await;
        let text = response_text(&hidden);
        assert!(hidden.is_error && text.contains("No run found"));
        assert!(!text.contains(UNOWNED_RUN_TASK) && !text.contains(OWNED_RUN_TASK));
    }

    // Non-vacuity: the probe run really is in the shared registry and in
    // memory. A non-isolated in-process read of the same registry names it,
    // so the assertions above measure the owner boundary and not an empty
    // fixture.
    let list = registry
        .iter()
        .find(|tool| tool.supports_name("sessions_list"))
        .expect("test: the production registry must publish sessions_list");
    let in_process = list
        .execute(serde_json::json!({}))
        .await
        .expect("test: sessions_list must answer");
    assert!(
        in_process.output.contains(UNOWNED_RUN_TASK),
        "sanity: the probe run must actually exist, or every assertion above passes \
             vacuously: {}",
        in_process.output
    );
    assert!(in_process.output.contains(OWNED_RUN_TASK));
    assert!(in_process.output.contains("source=runtime, manageable=true"));
    let shared: Arc<dyn crate::memory::Memory> = memory.clone();
    let recovered = crate::tools::sessions_read_model::recover_task_runs(
        Some(&shared),
        &workspace.path().to_string_lossy(),
        &serde_json::json!({}),
        20,
    )
    .await
    .expect("historical rows recover through the original read model");
    assert!(
        recovered
            .iter()
            .any(|run| run.run_id == "prx-legacy-unowned" && run.owner_id.is_none())
    );
    assert!(
        recovered
            .iter()
            .any(|run| run.run_id == "prx-existing-owned"
                && run.owner_id.as_deref() == Some(owner.prx_owner_id.as_str()))
    );
}
