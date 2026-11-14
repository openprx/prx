// Included into `agent::terminal::tests` (D§4.5, D§4.6).

/// E2–E7b through the production encoders; the key is installed under its
/// real content-derived key id. Expected values are independent literals.
#[test]
fn golden_repository_terminal_vectors() {
    use crate::memory::recovery_authority::{RecoveryCostState, RecoveryMacDomain, RecoveryRepository};
    use crate::memory::sqlite::golden_vectors;
    let dir = crate::runtime::ingress_recovery::tombstone_fixture::home_dir();
    let memory = crate::memory::SqliteMemory::new(dir.path()).expect("test: memory");
    let key: [u8; 32] = std::array::from_fn(|index| u8::try_from(index).unwrap_or(0));
    let key_id = golden_vectors::install_key(&memory, &key).expect("test: install key");
    assert_eq!(key_id, "630dcd29-66c4-3366-9112-5448bbb25b4f");
    assert!(
        dir.path()
            .join("memory/.recovery_keys/630dcd29-66c4-3366-9112-5448bbb25b4f.key")
            .exists()
            || std::fs::read_dir(dir.path())
                .expect("test: list")
                .filter_map(Result::ok)
                .any(|entry| entry.path().join(".recovery_keys/630dcd29-66c4-3366-9112-5448bbb25b4f.key").exists())
    );

    // E2: objects inserted in non-alphabetical order.
    let semantic = serde_json::json!({"version": 2, "terminal_id": "term-1", "status": null});
    let mut preimage = serde_json::to_vec(&semantic).expect("test: semantic bytes");
    preimage.extend_from_slice(key_id.as_bytes());
    let e2 = memory
        .recovery_mac(Some(&key_id), RecoveryMacDomain::ClaimV3, &preimage)
        .expect("test: E2");
    assert_eq!(hex::encode(e2.tag), "6753707e3b4143a3fe912c32b01e9690182a4ffbbe51fcca400e8dc095267ca6");

    // E3 and E4.
    use rusqlite::types::Value as Column;
    let record = golden_vectors::row_codec(&[
        Column::Null,
        Column::Integer(7),
        Column::Real(1.5),
        Column::Text("a".into()),
        Column::Blob(vec![1]),
    ])
    .expect("test: E3");
    assert_eq!(
        hex::encode(&record),
        "00000000000000000001000000000000000800000000000000070200000000000000083ff80000000000000300000000000000016104000000000000000101"
    );
    assert_eq!(
        hex::encode(golden_vectors::event_digest(&memory, &key_id, &record).expect("test: E4")),
        "8284a1331851e473abf0974ca55b458255d932d1df079f93113215208ef9e788"
    );

    // E5.
    assert_eq!(
        hex::encode(
            golden_vectors::receipt_mac(&memory, &key_id, "00000000-0000-4000-8000-000000000002", "settled", b"{}")
                .expect("test: E5")
        ),
        "6e093173590e466b0b4ee7428e97fffc2f50c05ebe3849281d69eb55c1ae2036"
    );

    // E6a.
    let usage = Some(serde_json::json!({"total_tokens": 3, "model": "m"}));
    let e6a = memory
        .recovery_mac(
            Some(&key_id),
            RecoveryMacDomain::UsageDigestV1,
            &serde_json::to_vec(&usage).expect("test: usage bytes"),
        )
        .expect("test: E6a");
    assert_eq!(hex::encode(e6a.tag), "13d02222f37daa5592a5be4034c54405b193ce68e842c9b69b5b5623f6b2556e");

    // E7a and E7b.
    let (e7a, e7b) = golden_vectors::outbox_macs(
        &memory,
        &key_id,
        ["ws-1", "owner-1", "term-1"],
        &[0xaa; 32],
        br#"{"v":1}"#,
        RecoveryCostState::Acknowledged,
    )
    .expect("test: E7");
    assert_eq!(hex::encode(e7a), "7cfcb66554298962d718b1a9d658d65df6b25a4826275601abca9b171b741c3a");
    assert_eq!(hex::encode(e7b), "4bce3d188eda9c8009348a44c57eff46890c0a9eff6ac35b4d3718ea7d6c9aa5");
}

/// One committed terminal of a restart cell.
struct RestartCell {
    _dir: tempfile::TempDir,
    workspace: std::path::PathBuf,
    terminal_id: String,
    owner: String,
}

async fn commit_restart_cell(
    terminal_id: &str,
    shape: impl FnOnce(&mut TurnTerminalDraft),
    prepared: bool,
) -> RestartCell {
    let dir = crate::runtime::ingress_recovery::tombstone_fixture::home_dir();
    let workspace = dir.path().canonicalize().expect("test: workspace");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&workspace).expect("test: memory"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut draft = completed_commit(terminal_id);
    shape(&mut draft);
    let _service = if prepared {
        bind_terminal_parameter_fixture(&workspace, Arc::clone(&memory), &mut draft, &cost).await
    } else {
        bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut draft, &cost).await
    };
    let receipt = finalize_turn(&fabric, draft, &cost, &workspace)
        .await
        .expect("test: committed terminal");
    let owner = receipt
        .terminal_event
        .owner_id
        .clone()
        .expect("test: terminal owner");
    RestartCell {
        _dir: dir,
        workspace,
        terminal_id: terminal_id.to_owned(),
        owner,
    }
}

/// Reopen the repository in a fresh handle (a restart) and rejoin through
/// the D§4.4 verifier.
async fn rejoin_restart_cell(cell: &RestartCell) -> anyhow::Result<()> {
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&cell.workspace)?);
    let mut config = crate::config::Config::default();
    config.workspace_dir = cell.workspace.clone();
    config.memory.events.record_user_messages = true;
    config.cost = priced_cost_config();
    let generation = crate::config::ConfigGenerationManager::new(config).pin();
    let tracker = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let recovery = crate::runtime::ingress_recovery::RecoveryScope::prepare(
        &generation,
        memory,
        crate::tools::capability::turn_state::TurnSurfaceKind::AgentLoop,
        None,
        &tracker,
        None,
        crate::runtime::ingress_recovery::PreparedAdvisory::default(),
    )
    .await?;
    recovery.rejoin_terminal(&cell.terminal_id, &cell.owner).await.map(|_| ())
}

fn no_legacy_files(workspace: &Path) {
    assert!(!workspace.join("self/causal_tree").exists(), "no legacy decision store or key");
    assert!(!workspace.join("self/child_authority").exists(), "no child authority journal");
}

#[tokio::test]
async fn restart_rejoin_verifies_every_terminal_status() {
    let cells = [
        commit_restart_cell("restart-completed", |_| {}, true).await,
        commit_restart_cell(
            "restart-oversized",
            |draft| {
                if let Some(history) = draft.history.as_mut() {
                    history.assistant_content = "x".repeat(300_000);
                }
            },
            false,
        )
        .await,
        commit_restart_cell(
            "restart-silent",
            |draft| {
                draft.status = TurnTerminalStatus::Silent;
                draft.history = None;
            },
            false,
        )
        .await,
        commit_restart_cell(
            "restart-failed",
            |draft| {
                draft.status = TurnTerminalStatus::Failed;
                draft.history = None;
            },
            false,
        )
        .await,
        commit_restart_cell(
            "restart-cancelled",
            |draft| {
                draft.status = TurnTerminalStatus::Cancelled;
                draft.history = None;
                draft.provider_outcome = None;
            },
            false,
        )
        .await,
    ];
    for cell in &cells {
        rejoin_restart_cell(cell)
            .await
            .unwrap_or_else(|error| panic!("test: {} must rejoin after restart: {error:#}", cell.terminal_id));
        no_legacy_files(&cell.workspace);
    }

    // Negative cells, each against a reopened store.
    type Mutation = fn(&RestartCell, &crate::memory::SqliteMemory, &crate::memory::MessageEvent, &crate::memory::MessageEvent);
    fn claim_semantic_edit(cell: &RestartCell, memory: &crate::memory::SqliteMemory, claim: &crate::memory::MessageEvent, edit: fn(&mut serde_json::Value)) {
        let _ = cell;
        let mut payload: serde_json::Value =
            serde_json::from_str(claim.raw_payload_json.as_deref().expect("test: claim payload")).expect("test: claim JSON");
        edit(&mut payload);
        memory
            .replace_event_payload_for_test(&claim.event_id, &payload.to_string())
            .expect("test: edit claim");
    }
    let mutations: [(&str, Mutation); 11] = [
        ("flipped tag byte", |cell, memory, claim, _terminal| {
            claim_semantic_edit(cell, memory, claim, |payload| {
                let tag = payload["semantic_mac_v3"].as_str().unwrap_or_default();
                let rest = tag.get(1..).unwrap_or_default();
                let flipped = if tag.starts_with('0') { format!("1{rest}") } else { format!("0{rest}") };
                payload["semantic_mac_v3"] = serde_json::Value::String(flipped);
            });
        }),
        ("flipped final-row byte", |_cell, memory, _claim, terminal| {
            memory
                .replace_event_channel_for_test(&terminal.event_id, "tampered")
                .expect("test: flip final row");
        }),
        ("missing .key", |cell, _memory, _claim, _terminal| {
            for entry in walk_keys(&cell.workspace) {
                std::fs::remove_file(entry).expect("test: remove key");
            }
        }),
        (".key hash prefix differs", |cell, _memory, _claim, _terminal| {
            for entry in walk_keys(&cell.workspace) {
                std::fs::write(entry, [0x42; 32]).expect("test: replace key");
            }
        }),
        ("pending-domain receipt", |cell, _memory, _claim, _terminal| {
            let connection = rusqlite::Connection::open(cell.workspace.join("memory/brain.db")).expect("test: db");
            connection
                .execute("UPDATE recovery_attestations SET state='pending'", [])
                .expect("test: pending receipt");
        }),
        ("unknown facts key", |cell, _memory, _claim, _terminal| {
            let connection = rusqlite::Connection::open(cell.workspace.join("memory/brain.db")).expect("test: db");
            connection
                .execute("UPDATE recovery_attestations SET facts=CAST('{\"x\":1}' AS BLOB)", [])
                .expect("test: unknown facts key");
        }),
        ("semantic edited", |cell, memory, claim, _terminal| {
            claim_semantic_edit(cell, memory, claim, |payload| {
                payload["semantic"]["telemetry"] = serde_json::json!({"summary": "edited"});
            });
        }),
        ("semantic removed", |cell, memory, claim, _terminal| {
            claim_semantic_edit(cell, memory, claim, |payload| {
                if let Some(object) = payload.as_object_mut() {
                    object.remove("semantic");
                }
            });
        }),
        ("status differs", |cell, memory, claim, _terminal| {
            claim_semantic_edit(cell, memory, claim, |payload| {
                payload["semantic"]["status"] = serde_json::json!("failed");
            });
        }),
        ("prepared parameters dropped from S", |cell, memory, claim, _terminal| {
            claim_semantic_edit(cell, memory, claim, |payload| {
                if let Some(object) = payload["semantic"].as_object_mut() {
                    object.remove("prepared_parameters");
                }
            });
        }),
        ("metered facts without outbox row", |cell, _memory, _claim, _terminal| {
            crate::memory::sqlite::remove_cost_for_test(&cell.workspace, &cell.terminal_id).expect("test: remove outbox");
        }),
    ];
    for (name, mutate) in mutations {
        let cell = commit_restart_cell("restart-negative", |_| {}, true).await;
        let memory = crate::memory::SqliteMemory::new(&cell.workspace).expect("test: memory");
        let claim = memory
            .find_message_event_by_idempotency_key(
                &cell.workspace.to_string_lossy(),
                &format!("turn:{}:decision-claim", cell.terminal_id),
            )
            .await
            .expect("test: claim lookup")
            .expect("test: claim");
        let terminal = memory
            .find_message_event_by_idempotency_key(
                &cell.workspace.to_string_lossy(),
                &format!("turn:{}:final", cell.terminal_id),
            )
            .await
            .expect("test: final lookup")
            .expect("test: final");
        mutate(&cell, &memory, &claim, &terminal);
        drop(memory);
        assert!(rejoin_restart_cell(&cell).await.is_err(), "test: {name} must be refused");
        no_legacy_files(&cell.workspace);
    }

    // `replay_unavailable` on one side only.
    let cell = commit_restart_cell("restart-replay-one-side", |_| {}, false).await;
    let memory = crate::memory::SqliteMemory::new(&cell.workspace).expect("test: memory");
    let claim = memory
        .find_message_event_by_idempotency_key(
            &cell.workspace.to_string_lossy(),
            &format!("turn:{}:decision-claim", cell.terminal_id),
        )
        .await
        .expect("test: claim lookup")
        .expect("test: claim");
    claim_semantic_edit(&cell, &memory, &claim, |payload| {
        payload["semantic"]["replay_unavailable"] =
            serde_json::json!({"reply_sha256": "0".repeat(64), "reply_bytes": 4, "result_hash": "0".repeat(64)});
    });
    drop(memory);
    assert!(rejoin_restart_cell(&cell).await.is_err(), "test: one-sided replay_unavailable must be refused");
}

fn walk_keys(workspace: &Path) -> Vec<std::path::PathBuf> {
    let mut keys = Vec::new();
    let mut pending = vec![workspace.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.parent().and_then(Path::file_name).is_some_and(|name| name == ".recovery_keys")
                && path.extension().is_some_and(|extension| extension == "key")
            {
                keys.push(path);
            }
        }
    }
    keys
}
