impl Fixture {
    fn sender_port(
        &self,
        inherited: Option<crate::messaging::InheritedProvenance>,
    ) -> Arc<dyn crate::messaging::send::MailboxSender> {
        Arc::new(
            crate::memory::sqlite::mailbox::SqliteMailboxSender::new(
                self.store.clone(),
                self.source.clone(),
                self.sender.clone(),
                Arc::new(LocalResolver {
                    store: self.store.clone(),
                    authority: self.authority.clone(),
                    scope: self.recipient.clone(),
                }),
                inherited,
            )
            .expect("test: sender_port: SqliteMailboxSender::new succeeds"),
        )
    }
}

#[tokio::test]
async fn sessions_send_tool_emits_one_external_send_envelope() {
    use crate::tools::Tool;
    let f = Fixture::new();
    let tool = crate::tools::SessionsSendTool::new(Arc::new(tokio::sync::RwLock::new(Vec::new())))
        .with_mailbox(f.sender_port(None));
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(
        crate::tools::SessionsSendTool::new(Arc::new(tokio::sync::RwLock::new(Vec::new())))
            .with_mailbox(f.sender_port(None)),
    )];
    let catalog = crate::tools::capability::catalog::CapabilityCatalog::build(
        &registry,
        &crate::tools::capability::catalog::CatalogSources::default(),
        &crate::tools::capability::catalog::SurfacePolicy::default(),
    )
    .expect("test: sessions_send_tool_emits_one_external_send_envelope: fixture setup succeeds");
    assert_eq!(
        catalog
            .entry_for_public_name("sessions_send")
            .expect("test: sessions_send_tool_emits_one_external_send_envelope: entry_for_public_name succeeds")
            .effect,
        crate::tools::capability::contract::SideEffectClass::LocalWrite,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    let result = tool
        .execute(serde_json::json!({"to":{"session":"target"},"message":"ordinary Data"}))
        .await
        .expect("test: sessions_send_tool_emits_one_external_send_envelope: execute succeeds");
    assert!(result.success, "{}", result.output);
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        1,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    let out = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: sessions_send_tool_emits_one_external_send_envelope: pending_outbox succeeds");
    assert_eq!(
        out[0].message().kind,
        MessageKind::Data,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    assert_eq!(
        out[0].message().body.bytes,
        b"ordinary Data",
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    assert_eq!(
        out[0].message().from,
        f.source,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "sender tool does not execute the recipient"
    );
    for invalid in [
        serde_json::json!({"to":{"session":"target","owner":null},"text":"x"}),
        serde_json::json!({"to":{"session":"target"},"text":"x","in_reply_to":null}),
    ] {
        assert!(
            tool.execute(invalid).await.is_err(),
            "prx message tool emits one external send envelope: expected messaging contract changed"
        );
    }
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        1,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
}

#[tokio::test]
async fn sessions_send_tool_send_is_stamped_with_runtime_causal_chain() {
    use crate::tools::Tool;
    let f = Fixture::new();
    let accepted = f.receive(1, 1);
    let inherited = crate::messaging::InheritedProvenance {
        remote: crate::messaging::RemotePeerAttr {
            issuer: "prx-issuer".into(),
            subject: "prx-peer".into(),
            message_id: accepted.message_id,
        },
        chain: accepted.chain,
    };
    let tool = crate::tools::SessionsSendTool::new(Arc::new(tokio::sync::RwLock::new(Vec::new())))
        .with_mailbox(f.sender_port(Some(inherited.clone())));
    let result = tool
        .execute(serde_json::json!({"to":{"session":"target"},"message":"reply"}))
        .await
        .expect("test: sessions_send_tool_send_is_stamped_with_runtime_causal_chain: execute succeeds");
    assert!(result.success, "{}", result.output);
    let out = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: sessions_send_tool_send_is_stamped_with_runtime_causal_chain: pending_outbox succeeds");
    assert_eq!(
        out[0].message().in_reply_to,
        Some(inherited.chain.trigger),
        "prx message tool send is stamped with runtime causal chain: expected messaging contract changed"
    );
    assert_eq!(
        out[0].message().hop_count,
        1,
        "prx message tool send is stamped with runtime causal chain: expected messaging contract changed"
    );
}

#[tokio::test]
async fn sessions_send_remote_addr_is_not_steer() {
    use crate::tools::Tool;
    let f = Fixture::new();
    let tool = crate::tools::sessions_send::SessionsSendTool::new(Arc::new(tokio::sync::RwLock::new(Vec::new())))
        .with_mailbox(f.sender_port(None));
    assert!(
        tool.parameters_schema().get("type").and_then(serde_json::Value::as_str) == Some("object"),
        "sessions send remote addr is not steer: expected messaging contract changed"
    );
    assert!(
        tool.execute(serde_json::json!({"to":{"session":"target"},"message":"mailbox Data"}))
            .await
            .expect("test: sessions_send_remote_addr_is_not_steer: execute succeeds")
            .success,
        "sessions send remote addr is not steer: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        1,
        "sessions send remote addr is not steer: expected messaging contract changed"
    );
    assert!(
        tool.execute(serde_json::json!({"run_id":"run-target","to":{"session":"target"},"message":"ambiguous"}))
            .await
            .is_err(),
        "sessions send remote addr is not steer: expected messaging contract changed"
    );
}

#[test]
fn slash_msg_builds_envelope_to_peer_session() {
    let f = Fixture::new();
    let (target, text) = crate::chat::commands::msg::parse("/msg target hello peer", &f.source)
        .expect("test: slash_msg_builds_envelope_to_peer_session: fixture setup succeeds");
    assert_eq!(
        target,
        SessionAddr::new("prx-node", O, "target", None),
        "slash msg builds envelope to peer session: expected messaging contract changed"
    );
    assert_eq!(
        text, "hello peer",
        "slash msg builds envelope to peer session: expected messaging contract changed"
    );
    assert!(
        crate::chat::commands::msg::parse("/msg local target/ ", &f.source).is_err(),
        "slash msg builds envelope to peer session: expected messaging contract changed"
    );
}

#[test]
fn mailbox_formats_and_natural_key_collisions_fail_closed() {
    let f = Fixture::new();
    let original = f.message(1, 1);
    f.store
        .accept(&f.authority, original.clone())
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: accept succeeds");
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta=json_set(receipt_meta,'$.unknown',1)",
            [],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    assert_eq!(
        f.store.accept(&f.authority, original.clone()).unwrap_err(),
        MailboxError::InvalidMetadata,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta=json_remove(receipt_meta,'$.unknown')",
            [],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    let canonical = f
        .memory
        .pool
        .write()
        .query_row("SELECT receipt_meta FROM prx_mailbox_receipts", [], |r| {
            r.get::<_, String>(0)
        })
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: query_row succeeds");
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta=?1",
            [format!(" {canonical}")],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    assert_eq!(
        f.store.accept(&f.authority, original.clone()).unwrap_err(),
        MailboxError::InvalidMetadata,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_receipts SET receipt_meta=?1", [canonical])
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_seq SET wire_stream_id=zeroblob(15)", [])
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    assert_eq!(
        f.store.accept(&f.authority, original.clone()).unwrap_err(),
        MailboxError::CorruptState,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET wire_stream_id=?1",
            [Uuid::from_u128(STREAM).as_bytes().as_slice()],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    let mut changed = f.target.clone();
    changed.session = "other-target".into();
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET to_addr=?1",
            [serde_json::to_string(&changed).expect(
                "test: mailbox_formats_and_natural_key_collisions_fail_closed: serde_json::to_string succeeds",
            )],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    assert_eq!(
        f.store.accept(&f.authority, original).unwrap_err(),
        MailboxError::NaturalKeyCollision,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
}

#[test]
fn mailbox_scope_pin_role_and_run_checks_fail_closed() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.exec = Some(ExecRef {
        run: Some("replaced-run".into()),
    });
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::StaleRun,
        "mailbox scope pin role and run checks fail closed: recipient authority or scope isolation changed"
    );
    f.receive(1, 1);
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_seq SET recipient_generation=NULL", [])
        .expect("test: mailbox_scope_pin_role_and_run_checks_fail_closed: execute succeeds");
    assert!(
        matches!(
            f.store.deliver(&f.recipient, &f.target),
            Err(MailboxError::CorruptState)
        ),
        "mailbox scope pin role and run checks fail closed: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        0,
        "mailbox scope pin role and run checks fail closed: recipient authority or scope isolation changed"
    );
}

#[test]
fn mailbox_identity_and_state_transitions_are_immutable() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_messages SET body_bytes=?1",
            [b"tampered".as_slice()],
        )
        .expect("test: mailbox_identity_and_state_transitions_are_immutable: execute succeeds");
    assert_eq!(
        f.store.adopt(&delivery).unwrap_err(),
        MailboxError::IdentityImmutable,
        "mailbox identity and state transitions are immutable: replay identity or receipt stability changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        0,
        "mailbox identity and state transitions are immutable: replay identity or receipt stability changed"
    );
    assert_eq!(
        f.scalar("SELECT state FROM prx_mailbox_receipts"),
        1,
        "mailbox identity and state transitions are immutable: replay identity or receipt stability changed"
    );
}

#[test]
fn mailbox_transport_and_chain_metadata_is_runtime_owned() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.from = SessionAddr::new("forged-instance", "forged-owner", "forged-session", None);
    message.origin = Some(crate::messaging::MessageOrigin::RemotePeer);
    message.auth = Some(crate::messaging::MessageAuth {
        issuer: "forged".into(),
        subject: "forged".into(),
        method: "mtls".into(),
    });
    message.hop_count = 8;
    message.ancestry = vec![Uuid::nil(); 8];
    let receipt = f
        .store
        .accept(&f.authority, message)
        .expect("test: mailbox_transport_and_chain_metadata_is_runtime_owned: accept succeeds");
    assert_eq!(
        receipt.chain.depth, 0,
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
    assert!(
        receipt.remote.is_none(),
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
    let delivered = f.deliver();
    assert_eq!(
        delivered.message().from,
        f.source,
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
    assert_eq!(
        delivered.message().origin,
        Some(crate::messaging::MessageOrigin::LocalModel),
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
    assert!(
        delivered.message().auth.is_none(),
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
}

#[test]
fn mailbox_replay_slot_and_message_id_reuse_are_checked() {
    let f = Fixture::new();
    let message = f.message(1, 1);
    let first = f
        .store
        .accept(&f.authority, message.clone())
        .expect("test: mailbox_replay_slot_and_message_id_reuse_are_checked: accept succeeds");
    let mut occupied = f.message(2, 1);
    assert_eq!(
        f.store.accept(&f.authority, occupied.clone()).unwrap_err(),
        MailboxError::SeqConflict,
        "mailbox replay slot and message id reuse are checked: replay identity or receipt stability changed"
    );
    occupied.seq = 2;
    occupied.id = message.id;
    assert_eq!(
        f.store.accept(&f.authority, occupied).unwrap_err(),
        MailboxError::IdReuse,
        "mailbox replay slot and message id reuse are checked: replay identity or receipt stability changed"
    );
    assert_eq!(
        f.store
            .accept(&f.authority, message)
            .expect("test: mailbox_replay_slot_and_message_id_reuse_are_checked: accept succeeds"),
        first,
        "mailbox replay slot and message id reuse are checked: replay identity or receipt stability changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "mailbox replay slot and message id reuse are checked: replay identity or receipt stability changed"
    );
}

#[test]
fn mailbox_progress_and_gap_transitions_are_checked() {
    let f = Fixture::new();
    let message = f.message(1, 1026);
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::Quota("seq"),
        "mailbox progress and gap transitions are checked: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        0,
        "gap reservation rolls back stream insert"
    );
    f.receive(1, 1);
    let delivery = f.deliver();
    assert_eq!(
        f.store.recover_delivery(&f.recipient, &f.target).unwrap_err(),
        MailboxError::RoleBusy,
        "mailbox progress and gap transitions are checked: expected messaging contract changed"
    );
    f.store
        .adopt(&delivery)
        .expect("test: mailbox_progress_and_gap_transitions_are_checked: adopt succeeds");
    drop(delivery);
    f.store
        .recover_delivery(&f.recipient, &f.target)
        .expect("test: mailbox_progress_and_gap_transitions_are_checked: recover_delivery succeeds");
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: mailbox_progress_and_gap_transitions_are_checked: deliver succeeds")
            .is_none(),
        "mailbox progress and gap transitions are checked: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET progress=json_set(progress,'$.next_seq',0)",
            [],
        )
        .expect("test: mailbox_progress_and_gap_transitions_are_checked: execute succeeds");
    assert!(
        matches!(
            f.store.deliver(&f.recipient, &f.target),
            Err(MailboxError::CorruptState)
        ),
        "mailbox progress and gap transitions are checked: expected messaging contract changed"
    );
}

#[test]
fn fresh_sender_session_labels_after_receipt_gc_hit_stream_quota() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    f.store
        .adopt(&delivery)
        .expect("test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: adopt succeeds");
    drop(delivery);
    f.clock.advance(604801);
    f.store
        .gc_scope(&f.recipient)
        .expect("test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: gc_scope succeeds");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        0,
        "fresh sender session labels after receipt gc hit stream quota: capacity enforcement or bounded delivery changed"
    );
    // Populate the remaining bounded tombstone inventory from the real retired
    // row; no messages/receipts remain to provide accidental quota coverage.
    f.memory.pool.write().execute_batch("WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<4095) INSERT INTO prx_mailbox_seq(workspace_id,owner_id,sender_ma_scope_id,sender_incarnation,sender_generation,recipient_ma_scope_id,recipient_incarnation,recipient_generation,from_addr,to_addr,wire_stream_id,state,progress,updated_at) SELECT workspace_id,owner_id,sender_ma_scope_id,sender_incarnation,sender_generation,recipient_ma_scope_id,recipient_incarnation,recipient_generation,json_set(from_addr,'$.session','retired-'||v),to_addr,randomblob(16),state,progress,updated_at FROM prx_mailbox_seq,n WHERE stream_id=1;").expect("test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: fixture setup succeeds");
    let mut source = f.source.clone();
    source.session = "fresh-sender-label".into();
    let authority = MailboxAuthority::local(W.into(), source, f.target.clone()).expect(
        "test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: MailboxAuthority::local succeeds",
    );
    let mut message = f.message(2, 1);
    message.stream_id = Some(Uuid::new_v4());
    message.to_pin = f
        .store
        .recipient_pin(&authority, &f.recipient, "canonical-target")
        .expect("test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: recipient_pin succeeds");
    assert_eq!(
        f.store.accept(&authority, message).unwrap_err(),
        MailboxError::Quota("streams"),
        "fresh sender session labels after receipt gc hit stream quota: capacity enforcement or bounded delivery changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        4096,
        "fresh sender session labels after receipt gc hit stream quota: capacity enforcement or bounded delivery changed"
    );
}

#[tokio::test]
async fn child_message_reaches_main_mailbox() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let mut f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(f.source.clone(), None))
        .await
        .expect("test: child_message_reaches_main_mailbox: insert succeeds");
    let child = registry
        .reconstruct(&f.source)
        .await
        .expect("test: child_message_reaches_main_mailbox: reconstruct succeeds");
    f.sender = child
        .mailbox_scope()
        .expect("test: child_message_reaches_main_mailbox: mailbox_scope succeeds")
        .clone();
    let provisional = f.message(1, 0);
    f.store
        .publish(&f.authority, provisional, &f.sender, f.clock.now() + 300, None)
        .expect("test: child_message_reaches_main_mailbox: publish succeeds");
    let outgoing = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: child_message_reaches_main_mailbox: pending_outbox succeeds");
    f.store
        .accept(&f.authority, outgoing[0].message().clone())
        .expect("test: child_message_reaches_main_mailbox: accept succeeds");
    let delivery = f.deliver();
    let receipt = f
        .store
        .adopt(&delivery)
        .expect("test: child_message_reaches_main_mailbox: adopt succeeds");
    assert_eq!(
        receipt.state,
        DeliveryState::Consumed,
        "child message reaches main mailbox: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        1,
        "child message reaches main mailbox: expected messaging contract changed"
    );
}

#[tokio::test]
async fn sibling_message_is_owner_scoped() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    for address in [&f.source, &f.target] {
        registry
            .insert(&ChildSession::new(address.clone(), None))
            .await
            .expect("test: sibling_message_is_owner_scoped: insert succeeds");
    }
    let target = registry
        .reconstruct(&f.target)
        .await
        .expect("test: sibling_message_is_owner_scoped: reconstruct succeeds");
    let scope = target
        .mailbox_scope()
        .expect("test: sibling_message_is_owner_scoped: mailbox_scope succeeds")
        .clone();
    let canonical = ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: sibling_message_is_owner_scoped: ChildSessionRegistry::canonical_session_key succeeds");
    let mut message = f.message(1, 1);
    message.to_pin = f
        .store
        .recipient_pin(&f.authority, &scope, &canonical)
        .expect("test: sibling_message_is_owner_scoped: recipient_pin succeeds");
    f.store
        .accept(&f.authority, message.clone())
        .expect("test: sibling_message_is_owner_scoped: accept succeeds");
    let mut foreign = f.source.clone();
    foreign.owner = "other-owner".into();
    assert!(
        matches!(
            MailboxAuthority::local(W.into(), foreign, f.target.clone()),
            Err(MailboxError::Unauthorized)
        ),
        "sibling message is owner scoped: recipient authority or scope isolation changed"
    );
    let mut foreign_target = f.target.clone();
    foreign_target.owner = "other-owner".into();
    message.to = foreign_target;
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::Unauthorized,
        "sibling message is owner scoped: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "sibling message is owner scoped: recipient authority or scope isolation changed"
    );
}

#[tokio::test]
async fn child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    use crate::memory::child_sessions::{ChildSessionContext, ChildSessionError, ChildSessionStore};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(f.target.clone(), None))
        .await
        .expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: insert succeeds");
    let key = ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: ChildSessionRegistry::canonical_session_key succeeds");
    let context = ChildSessionContext {
        workspace_id: W.into(),
        addr: f.target.clone(),
    };
    let original = f
        .memory
        .pool
        .write()
        .query_row("SELECT record FROM child_sessions WHERE key=?1", [&key], |r| {
            r.get::<_, String>(0)
        })
        .expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: query_row succeeds");
    f.memory.pool.write().execute("UPDATE child_sessions SET workspace_id=NULL,owner_id=NULL,instance_id=NULL,session_id=NULL,agent_key=NULL,ma_scope_id=NULL,incarnation=NULL,generation=NULL WHERE key=?1",[&key]).expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: fixture setup succeeds");
    let refused = match ChildSessionStore::load(&f.memory, &key, &context) {
        Ok(_) => panic!("quarantined legacy row loaded"),
        Err(error) => error,
    };
    assert!(
        refused.is::<ChildSessionError>(),
        "child sessions migration 22 preserves records and quarantines unbound rows: capacity enforcement or bounded delivery changed"
    );
    let recovered =
        ChildSessionStore::update(&f.memory, &key, &context, &mut |old| Ok(old.expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: fixture setup succeeds").to_owned())).expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: ChildSessionStore::update succeeds");
    assert_eq!(
        recovered.record, original,
        "child sessions migration 22 preserves records and quarantines unbound rows: capacity enforcement or bounded delivery changed"
    );
    drop(recovered);
    // A partially populated row is never a backfill candidate.
    f.memory
        .pool
        .write()
        .execute("UPDATE child_sessions SET generation=NULL WHERE key=?1", [&key])
        .expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: execute succeeds");
    assert!(
        ChildSessionStore::update(&f.memory, &key, &context, &mut |_| Ok(original.clone())).is_err(),
        "child sessions migration 22 preserves records and quarantines unbound rows: capacity enforcement or bounded delivery changed"
    );
    assert_eq!(
        f.memory
            .pool
            .write()
            .query_row("SELECT record FROM child_sessions WHERE key=?1", [&key], |r| r
                .get::<_, String>(0))
            .expect(
                "test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: query_row succeeds"
            ),
        original,
        "child sessions migration 22 preserves records and quarantines unbound rows: capacity enforcement or bounded delivery changed"
    );
}

#[test]
fn mailbox_index_plans_use_scope_and_stream_prefixes() {
    let f = Fixture::new();
    let writer = f.memory.pool.write();
    for (query, index) in [
        (
            "SELECT message_id FROM prx_mailbox_receipts WHERE ma_scope_id='scope' AND expires_at>=0",
            "mailbox_receipt_scope_expiry",
        ),
        (
            "SELECT id FROM prx_mailbox_messages WHERE stream_id=1 AND state=0 AND seq>x'0000000000000000' ORDER BY seq",
            "mailbox_message_delivery",
        ),
        (
            "SELECT id FROM prx_mailbox_outbox WHERE stream_id=1 AND state=0 ORDER BY seq",
            "mailbox_outbox_delivery",
        ),
        (
            "SELECT stream_id FROM prx_mailbox_seq WHERE recipient_ma_scope_id='scope'",
            "mailbox_seq_recipient",
        ),
        (
            "SELECT key FROM child_sessions WHERE ma_scope_id='scope'",
            "child_sessions_scope",
        ),
    ] {
        let mut statement = writer
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .expect("test: mailbox_index_plans_use_scope_and_stream_prefixes: prepare succeeds");
        let details = statement
            .query_map([], |r| r.get::<_, String>(3))
            .expect("test: mailbox_index_plans_use_scope_and_stream_prefixes: query_map succeeds")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("test: mailbox_index_plans_use_scope_and_stream_prefixes: collect succeeds")
            .join(" ");
        assert!(details.contains(index), "{query}: {details}");
        if index == "mailbox_receipt_scope_expiry" {
            assert!(
                details.contains("COVERING"),
                "id-reuse probe must not load receipt bodies: {details}"
            );
        }
    }
}

#[test]
fn mailbox_u64_sequence_exhaustion_never_wraps_or_truncates() {
    let f = Fixture::new();
    f.store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: publish succeeds");
    let progress = f
        .memory
        .pool
        .write()
        .query_row("SELECT progress FROM prx_mailbox_seq", [], |r| r.get::<_, String>(0))
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: query_row succeeds");
    let progress = progress.replacen("\"next_seq\":2", &format!("\"next_seq\":{}", u64::MAX), 1);
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_seq SET progress=?1", [progress])
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: execute succeeds");
    let last = f
        .store
        .publish(&f.authority, f.message(2, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: publish succeeds");
    assert_eq!(
        last.seq,
        u64::MAX,
        "mailbox u64 sequence exhaustion never wraps or truncates: expected messaging contract changed"
    );
    assert_eq!(
        f.store
            .publish(&f.authority, f.message(3, 0), &f.sender, f.clock.now() + 300, None)
            .unwrap_err(),
        MailboxError::Quota("seq"),
        "mailbox u64 sequence exhaustion never wraps or truncates: expected messaging contract changed"
    );
    let raw = f
        .memory
        .pool
        .write()
        .query_row(
            "SELECT hex(seq) FROM prx_mailbox_outbox WHERE id=?1",
            [last.row_id],
            |r| r.get::<_, String>(0),
        )
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: fixture setup succeeds");
    assert_eq!(
        raw, "FFFFFFFFFFFFFFFF",
        "mailbox u64 sequence exhaustion never wraps or truncates: expected messaging contract changed"
    );
}

#[tokio::test]
async fn process_worker_message_frame_roundtrips() {
    use crate::session_worker::protocol::{WorkerControlFrame, WorkerOutputFrame};
    let f = Fixture::new();
    let published = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: process_worker_message_frame_roundtrips: publish succeeds");
    let outgoing = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: process_worker_message_frame_roundtrips: pending_outbox succeeds");
    let message = outgoing
        .first()
        .expect("test: process_worker_message_frame_roundtrips: first succeeds")
        .message()
        .clone();
    f.store
        .accept(&f.authority, message.clone())
        .expect("test: process_worker_message_frame_roundtrips: accept succeeds");
    let delivered = f.deliver();
    let frame = WorkerControlFrame::Message {
        envelope: Box::new(delivered.message().clone()),
    };
    let decoded: WorkerControlFrame = serde_json::from_slice(
        &serde_json::to_vec(&frame)
            .expect("test: process_worker_message_frame_roundtrips: serde_json::to_vec succeeds"),
    )
    .expect("test: process_worker_message_frame_roundtrips: serde_json::from_slice succeeds");
    let (mut sender, mut inbox) = crate::session_worker::mailbox::worker_mailbox(f.target.clone(), "run-target".into())
        .expect("test: process_worker_message_frame_roundtrips: mailbox::worker_mailbox succeeds");
    let (sender, ack) = tokio::task::spawn_blocking(move || {
        let WorkerControlFrame::Message { envelope } = decoded else {
            panic!("Data frame became another control kind");
        };
        let ack = sender
            .enqueue(*envelope)
            .expect("test: process_worker_message_frame_roundtrips: enqueue succeeds");
        (sender, ack)
    })
    .await
    .expect("test: process_worker_message_frame_roundtrips: task::spawn_blocking succeeds");
    assert_eq!(
        ack,
        WorkerOutputFrame::Ack {
            message_id: published.message_id,
            state: DeliveryState::Delivered
        },
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    let queued = inbox
        .try_next()
        .expect("test: process_worker_message_frame_roundtrips: try_next succeeds");
    assert_eq!(
        crate::session_worker::mailbox::user_text(&queued)
            .expect("test: process_worker_message_frame_roundtrips: mailbox::user_text succeeds"),
        "[message from source/main]\nrequest-1",
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    let consumed = inbox
        .consumed(&queued)
        .expect("test: process_worker_message_frame_roundtrips: consumed succeeds");
    let saved = f
        .store
        .adopt(&delivered)
        .expect("test: process_worker_message_frame_roundtrips: adopt succeeds");
    assert_eq!(
        consumed,
        WorkerOutputFrame::Ack {
            message_id: saved.message_id,
            state: saved.state
        },
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    let duplicate = tokio::task::spawn_blocking(move || {
        let mut sender = sender;
        sender
            .enqueue(message)
            .expect("test: process_worker_message_frame_roundtrips: enqueue succeeds")
    })
    .await
    .expect("test: process_worker_message_frame_roundtrips: task::spawn_blocking succeeds");
    assert_eq!(
        duplicate, ack,
        "a pruned worker slot may requeue a transport replay; durable adoption remains idempotent"
    );
    assert!(
        inbox.try_next().is_some(),
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.store
            .adopt(&delivered)
            .expect("test: process_worker_message_frame_roundtrips: adopt succeeds"),
        saved,
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        1,
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
}

#[test]
fn worker_data_reaches_parent_mailbox() {
    use crate::session_worker::protocol::WorkerOutputFrame;
    let f = Fixture::new();
    // This worker is the source endpoint; the destination parent is bound by
    // its real MA scope and an HMAC recipient pin, not by asserted frame.from.
    let parent_key = crate::chat::sessions::child::ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: canonical parent key");
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE agent_scopes SET session_key=?1 WHERE scope_id=?2",
            params![parent_key, f.recipient.scope_id.to_string()],
        )
        .expect("test: bind parent canonical scope");
    let mut parent = crate::session_worker::mailbox_parent::WorkerMailboxParent::new(
        f.store.clone(),
        f.source.clone(),
        f.sender.clone(),
        f.target.clone(),
        f.recipient.clone(),
        "run-source".into(),
        None,
    )
    .expect("test: worker_data_reaches_parent_mailbox: WorkerMailboxParent::new succeeds");
    let mut message = f.message(1, 0);
    message.to_pin.token.clear();
    message.from = SessionAddr::new("forged-instance", "forged-owner", "forged-session", None);
    let receipt = parent
        .receive(WorkerOutputFrame::Message {
            envelope: Box::new(message.clone()),
        })
        .expect("test: worker_data_reaches_parent_mailbox: receive succeeds")
        .expect("test: worker_data_reaches_parent_mailbox: receive returns a value");
    assert_eq!(
        receipt.state,
        DeliveryState::Accepted,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    assert_eq!(
        receipt.seq, 1,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    let duplicate = parent
        .receive(WorkerOutputFrame::Message {
            envelope: Box::new(message),
        })
        .expect("test: worker_data_reaches_parent_mailbox: receive succeeds")
        .expect("test: worker_data_reaches_parent_mailbox: receive returns a value");
    assert_eq!(
        duplicate, receipt,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    let delivered = f.deliver();
    assert_eq!(
        delivered.message().from,
        f.source,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        1,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        1,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
}

#[test]
fn worker_data_keeps_sealed_provenance_and_cannot_reset_parent() {
    use crate::session_worker::protocol::WorkerOutputFrame;
    let f = Fixture::new();
    let parent_key = crate::chat::sessions::child::ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: canonical parent key");
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE agent_scopes SET session_key=?1 WHERE scope_id=?2",
            params![parent_key, f.recipient.scope_id.to_string()],
        )
        .expect("test: bind parent canonical scope");
    let mut remote = f.source.clone();
    remote.instance = "prx-peer".into();
    let peer = MailboxAuthority::verified_peer(
        W.into(),
        remote,
        f.target.clone(),
        crate::messaging::MessageAuth {
            issuer: "prx-issuer".into(),
            subject: "prx-peer".into(),
            method: "mtls".into(),
        },
    )
    .expect(
        "test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: MailboxAuthority::verified_peer succeeds",
    );
    let mut root = f.message(1, 1);
    root.to_pin = f
        .store
        .recipient_pin(&peer, &f.recipient, &parent_key)
        .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: recipient_pin succeeds");
    let accepted = f
        .store
        .accept(&peer, root)
        .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: accept succeeds");
    let inherited = crate::messaging::InheritedProvenance {
        remote: accepted.remote.expect(
            "test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: required fixture value is present",
        ),
        chain: accepted.chain,
    };
    let mut actor = crate::session_worker::mailbox_parent::WorkerMailboxParent::new(
        f.store.clone(),
        f.source.clone(),
        f.sender.clone(),
        f.target.clone(),
        f.recipient.clone(),
        "run-source".into(),
        Some(inherited.clone()),
    )
    .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: WorkerMailboxParent::new succeeds");
    let forged_root = f.message(2, 0);
    assert!(
        forged_root.in_reply_to.is_none(),
        "worker data keeps sealed provenance and cannot reset parent: IPC identity or delivery state changed"
    );
    let receipt = actor
        .receive(WorkerOutputFrame::Message {
            envelope: Box::new(forged_root),
        })
        .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: receive succeeds")
        .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: receive returns a value");
    assert_eq!(
        receipt.chain.ancestry,
        vec![inherited.chain.trigger],
        "worker data keeps sealed provenance and cannot reset parent: IPC identity or delivery state changed"
    );
    assert_eq!(
        receipt.remote,
        Some(inherited.remote),
        "worker data keeps sealed provenance and cannot reset parent: IPC identity or delivery state changed"
    );
}

#[test]
fn permanent_seq_gap_nacks_reachable_sender_outbox() {
    let f = Fixture::new();
    let first = f.message(1, 0);
    f.store
        .publish(&f.authority, first.clone(), &f.sender, f.clock.now() + 300, None)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: publish succeeds");
    let second = f
        .store
        .publish(&f.authority, f.message(2, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: publish succeeds");
    let pending = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: pending_outbox succeeds");
    let message = pending
        .iter()
        .find(|item| item.message().id == second.message_id)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: find succeeds")
        .message()
        .clone();
    drop(pending);
    f.store
        .accept(&f.authority, message)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: accept succeeds");
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: deliver succeeds")
            .is_none(),
        "permanent seq gap nacks reachable sender outbox: expected messaging contract changed"
    );
    f.clock.advance(crate::messaging::mailbox::GAP_SECONDS);
    let delivered = f.deliver();
    assert_eq!(
        delivered.message().seq,
        2,
        "permanent seq gap nacks reachable sender outbox: expected messaging contract changed"
    );
    f.store
        .adopt(&delivered)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: adopt succeeds");
    let failed = f
        .store
        .publish(&f.authority, first, &f.sender, f.clock.now() + 300, None)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: publish succeeds");
    assert_eq!(
        failed.state,
        crate::messaging::mailbox::MailboxSendState::Failed,
        "permanent seq gap nacks reachable sender outbox: expected messaging contract changed"
    );
    assert_eq!(
        failed.error.as_deref(),
        Some("seq_gap"),
        "permanent seq gap nacks reachable sender outbox: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "Nack must not create an Ack receipt"
    );
}

#[test]
fn worker_mime_projection_matches_durable_transcript() {
    let f = Fixture::new();
    for (index, (mime, bytes)) in [
        ("text/plain", b"/approve remains ordinary input".to_vec()),
        ("text/markdown", b"**ordinary input**".to_vec()),
        ("text/plain", vec![0xff, 0]),
        ("application/octet-stream", vec![0, 1, 0xff]),
    ]
    .into_iter()
    .enumerate()
    {
        let seq = u64::try_from(index + 1)
            .expect("test: worker_mime_projection_matches_durable_transcript: u64::try_from succeeds");
        let mut message = f.message(seq, seq);
        message.body = MessageBody {
            mime: mime.into(),
            bytes,
        };
        f.store
            .accept(&f.authority, message)
            .expect("test: worker_mime_projection_matches_durable_transcript: accept succeeds");
        let delivered = f.deliver();
        let worker_text = crate::session_worker::mailbox::user_text(delivered.message())
            .expect("test: worker_mime_projection_matches_durable_transcript: mailbox::user_text succeeds");
        f.store
            .adopt(&delivered)
            .expect("test: worker_mime_projection_matches_durable_transcript: adopt succeeds");
        let durable_text: String = f
            .memory
            .pool
            .write()
            .query_row(
                "SELECT content FROM conversation_turns WHERE message_id=?1",
                [delivered.message().id.to_string()],
                |row| row.get(0),
            )
            .expect("test: worker_mime_projection_matches_durable_transcript: query_row succeeds");
        assert_eq!(
            worker_text, durable_text,
            "worker mime projection matches durable transcript: atomic adoption or history projection changed"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_sender_ports_share_one_durable_stream() {
    let f = Fixture::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for text in ["first", "second"] {
        let port = f.sender_port(None);
        let target = f.target.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            port.send(target, text.into(), "text/plain".into())
                .await
                .expect("test: concurrent_sender_ports_share_one_durable_stream: send succeeds")
        }));
    }
    let mut published = Vec::new();
    for task in tasks {
        published.push(
            task.await
                .expect("test: concurrent_sender_ports_share_one_durable_stream: awaited task completes"),
        );
    }
    published.sort_by_key(|sent| sent.seq);
    assert_eq!(
        published.iter().map(|sent| sent.seq).collect::<Vec<_>>(),
        vec![1, 2],
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    let wire = published
        .first()
        .expect("test: concurrent_sender_ports_share_one_durable_stream: first succeeds")
        .stream_id;
    assert!(
        published.iter().all(|sent| sent.stream_id == wire),
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    let reopened = f
        .sender_port(None)
        .send(f.target.clone(), "reopened port".into(), "text/plain".into())
        .await
        .expect("test: concurrent_sender_ports_share_one_durable_stream: send succeeds");
    assert_eq!(
        reopened.stream_id, wire,
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    assert_eq!(
        reopened.seq, 3,
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        1,
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        3,
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
}

#[tokio::test]
async fn pending_send_resolver_keeps_payload_in_scope_until_cancel_join() {
    struct PausedResolver {
        inner: LocalResolver,
        entered: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: tokio::sync::Notify,
    }
    #[async_trait::async_trait]
    impl crate::messaging::send::MailboxResolver for PausedResolver {
        async fn resolve(
            &self,
            target: &SessionAddr,
        ) -> crate::messaging::mailbox::MailboxResult<crate::messaging::send::MailboxDestination> {
            let resolved = crate::messaging::send::MailboxResolver::resolve(&self.inner, target).await?;
            let entered = self.entered.lock().take();
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            self.release.notified().await;
            Ok(resolved)
        }
    }
    use crate::messaging::send::MailboxSender as _;
    let f = Fixture::new();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let resolver = Arc::new(PausedResolver {
        inner: LocalResolver {
            store: f.store.clone(),
            authority: f.authority.clone(),
            scope: f.recipient.clone(),
        },
        entered: parking_lot::Mutex::new(Some(entered)),
        release: tokio::sync::Notify::new(),
    });
    let port = crate::memory::sqlite::mailbox::SqliteMailboxSender::new(
        f.store.clone(),
        f.source.clone(),
        f.sender.clone(),
        resolver,
        None,
    )
    .expect("test: resolve: SqliteMailboxSender::new succeeds");
    let target = f.target.clone();
    let sending = tokio::spawn(async move {
        port.send(target, "pending scoped payload".into(), "text/plain".into())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), ready)
        .await
        .expect("test: resolve: time::timeout succeeds")
        .expect("test: resolve: time::timeout returns a value");
    let kept_payload_scope = f.memory.mailbox_runtime.has_scope_leases(f.sender.scope_id);
    sending.abort();
    assert!(
        sending.await.unwrap_err().is_cancelled(),
        "pending resolver task must join after cancellation"
    );
    assert!(
        kept_payload_scope,
        "queued send lost its body lease while resolver was pending"
    );
    assert!(
        !f.memory.mailbox_runtime.has_scope_leases(f.sender.scope_id),
        "cancel join retained the send payload"
    );
    let erased = f
        .store
        .erase_scope(&f.sender, Duration::from_secs(5), &|| Ok(()))
        .expect("test: resolve: erase_scope succeeds");
    assert_eq!(
        erased.remaining, 0,
        "joining the cancelled resolver must allow complete scope erasure"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        0,
        "cancelled resolver must not persist an outbox body"
    );
}

fn mailbox_parent(f: &Fixture) -> crate::session_worker::mailbox_parent::WorkerMailboxParent {
    crate::session_worker::mailbox_parent::WorkerMailboxParent::new(
        f.store.clone(),
        f.target.clone(),
        f.recipient.clone(),
        f.source.clone(),
        f.sender.clone(),
        "run-target".into(),
        None,
    )
    .expect("test: construct persistent parent")
}

fn stale_parent_releases_delivery_before_erase() {
    use crate::session_worker::protocol::{WorkerControlFrame, WorkerOutputFrame};
    let f = Fixture::new();
    let mut parent = mailbox_parent(&f);
    let mut message = f.message(1, 1);
    message.exec = Some(ExecRef {
        run: Some("run-target".into()),
    });
    f.store
        .accept(&f.authority, message)
        .expect("test: accept pinned parent input");
    let Some(WorkerControlFrame::Message { envelope }) = parent.next_delivery().expect("test: parent delivery") else {
        panic!("test: parent must hold the delivered input");
    };
    let ack = WorkerOutputFrame::Ack {
        message_id: envelope.id,
        state: DeliveryState::Consumed,
    };
    drop(envelope);
    f.store
        .set_execution(&f.recipient, &f.target, Some("run-next".into()))
        .expect("test: change parent run");
    let result = parent.receive(ack.clone());
    let repeated = parent.receive(ack);
    // Quiesce the parent's execution cache but retain any leaked deliveries.
    // The old early ? must still fail at the erase deadline, not a cache lease.
    let pending = parent.into_pending();
    let erased = f
        .store
        .erase_scope(&f.recipient, Duration::from_millis(20), &|| Ok(()))
        .expect("test: terminal parent adoption must release its lease before erase deadline");
    assert_eq!(erased.remaining, 0, "terminal parent input must be fully erased");
    assert!(
        result.expect("test: stale parent Ack is harmless").is_none(),
        "stale Ack must return no receipt"
    );
    assert_eq!(pending.len(), 0, "terminal adoption retained a pending delivery");
    assert!(
        repeated.expect("test: repeat stale Ack").is_none(),
        "repeated Ack must be harmless"
    );
}

#[test]
fn parent_terminal_adopt_errors_release_pending_and_retryable_errors_retain_it() {
    for error in [
        MailboxError::ReceiptExpired,
        MailboxError::Unauthorized,
        MailboxError::Fenced,
        MailboxError::IdentityImmutable,
        MailboxError::ScopeErasing,
        MailboxError::SeqConflict,
        MailboxError::Storage,
        MailboxError::CorruptState,
    ] {
        let f = Fixture::new();
        let mut parent = mailbox_parent(&f);
        let message = f.message(1, 1);
        let id = message.id;
        f.store
            .accept(&f.authority, message)
            .expect("test: accept matrix input");
        parent
            .next_delivery()
            .expect("test: hold matrix input")
            .expect("test: pending matrix frame");
        assert_eq!(
            parent.pending_count(),
            1,
            "matrix must start with a real delivery lease"
        );
        let sql = match error {
            MailboxError::ReceiptExpired => Some("DELETE FROM prx_mailbox_receipts"),
            MailboxError::Unauthorized => Some(
                "UPDATE prx_mailbox_receipts SET ma_scope_id='00000000-0000-0000-0000-00000000000a',receipt_meta=json_set(receipt_meta,'$.canonical_session_key','canonical-source')",
            ),
            MailboxError::IdentityImmutable => Some("UPDATE prx_mailbox_messages SET body_bytes=x'6368616e676564'"),
            MailboxError::ScopeErasing => Some(
                "INSERT INTO agent_erase_fences VALUES ('00000000-0000-0000-0000-000000000014','00000000-0000-0000-0000-000000000063','draining',2)",
            ),
            _ => None,
        };
        let result = if let Some(sql) = sql {
            f.memory
                .pool
                .write()
                .execute(sql, [])
                .expect("test: set terminal store condition");
            parent.receive(crate::session_worker::protocol::WorkerOutputFrame::Ack {
                message_id: id,
                state: DeliveryState::Consumed,
            })
        } else {
            // Fenced is no longer emitted by adopt after L7. Keep its parent
            // policy covered, along with injected retryable storage outcomes.
            parent.finish_adopt(id, Err(error.clone()))
        };
        assert_eq!(
            result.expect_err("test: propagate typed adopt error"),
            error,
            "parent changed the terminal error"
        );
        let retryable = matches!(
            error,
            MailboxError::SeqConflict | MailboxError::Storage | MailboxError::CorruptState
        );
        assert_eq!(
            parent.pending_count(),
            usize::from(retryable),
            "wrong lease policy for {error:?}"
        );
        assert_eq!(
            f.memory.mailbox_runtime.has_leases(f.recipient.scope_id),
            retryable,
            "wrong runtime lease policy for {error:?}"
        );
    }
}

#[test]
fn parent_acknowledge_error_after_adopt_releases_pending() {
    let f = Fixture::new();
    let mut parent = mailbox_parent(&f);
    let message = f.message(1, 1);
    let id = message.id;
    f.store
        .accept(&f.authority, message)
        .expect("test: accept without local outbox");
    parent
        .next_delivery()
        .expect("test: deliver without outbox")
        .expect("test: pending delivery");
    let error = parent
        .receive(crate::session_worker::protocol::WorkerOutputFrame::Ack {
            message_id: id,
            state: DeliveryState::Consumed,
        })
        .expect_err("test: acknowledge requires a sender outbox");
    assert_eq!(
        error,
        MailboxError::InvalidMetadata,
        "adoption must finish before missing-outbox Ack fails"
    );
    assert_eq!(
        parent.pending_count(),
        0,
        "committed adoption retained a pending lease after Ack error"
    );
    assert!(
        !f.memory.mailbox_runtime.has_leases(f.recipient.scope_id),
        "Ack error retained the consumed body"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts WHERE state=2"),
        1,
        "Ack failure must follow committed adoption"
    );
}

#[test]
fn consumed_then_erased_adopt_replays_saved_result() {
    let f = Fixture::new();
    f.receive(1, 1);
    let held = f.deliver();
    let saved = f.store.adopt(&held).expect("test: consume receipt before erasure");
    // Erasure normally joins the holder. Preserve this delivery solely to exercise replay.
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta=json_set(receipt_meta,'$.body_erased_at',1000) WHERE id=?1",
            [saved.row_id],
        )
        .expect("test: mark consumed receipt erased");
    f.memory
        .pool
        .write()
        .execute("DELETE FROM prx_mailbox_messages WHERE id=?1", [saved.row_id])
        .expect("test: remove consumed receipt body");
    let replay = f.store.adopt(&held).expect("test: replay erased consumed receipt");
    assert_eq!(replay, saved, "erasure must preserve the saved adoption checkpoint");
}

#[test]
fn erased_nonconsumed_replays_report_scope_erasing_when_fenced() {
    let f = Fixture::new();
    let message = f.message(1, 1);
    f.store
        .accept(&f.authority, message.clone())
        .expect("test: accept fenced replay input");
    let held = f.deliver();
    {
        let writer = f.memory.pool.write();
        writer
            .execute(
                "UPDATE prx_mailbox_receipts SET receipt_meta=json_set(receipt_meta,'$.body_erased_at',1000)",
                [],
            )
            .expect("test: erase replay body metadata");
        writer
            .execute(
                "INSERT INTO agent_erase_fences VALUES (?1,?2,'draining',2)",
                params![f.recipient.scope_id.to_string(), Uuid::new_v4().to_string()],
            )
            .expect("test: fence erased receipt scope");
    }
    assert_eq!(
        f.store
            .accept(&f.authority, message)
            .expect_err("test: replay fenced accept"),
        MailboxError::ScopeErasing,
        "fenced replay must report erasure instead of stale run"
    );
    assert_eq!(
        f.store.adopt(&held).expect_err("test: replay fenced adopt"),
        MailboxError::ScopeErasing,
        "fenced adoption must report erasure instead of stale run"
    );
}

fn stale_slot_case(adopt_race: bool) {
    if adopt_race {
        stale_parent_releases_delivery_before_erase();
    }
    let f = Fixture::new();
    let mut first = f.message(1, 0);
    first.exec = Some(ExecRef {
        run: Some("run-target".into()),
    });
    let published = f
        .store
        .publish(&f.authority, first, &f.sender, i64::MAX, None)
        .expect("test: publish run-pinned slot");
    f.store
        .publish(&f.authority, f.message(2, 0), &f.sender, i64::MAX, None)
        .expect("test: publish later unpinned slot");
    let outgoing = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: load accepted work");
    for slot in &outgoing {
        let accepted = f
            .store
            .accept(&f.authority, slot.message().clone())
            .expect("test: accept pending slot");
        f.store
            .acknowledge(&f.sender, &f.target, slot.message().id, accepted.state)
            .expect("test: acknowledge accepted work before the run changes");
    }
    let replay = outgoing.first().expect("test: first published slot").message().clone();
    drop(outgoing);
    let held = if adopt_race {
        let held = f.deliver();
        f.store
            .acknowledge(&f.sender, &f.target, held.message().id, DeliveryState::Delivered)
            .expect("test: acknowledge delivery before the adoption race");
        Some(held)
    } else {
        None
    };
    f.store
        .set_execution(&f.recipient, &f.target, Some("run-next".into()))
        .expect("test: switch recipient execution");
    if let Some(held) = held {
        assert_eq!(
            f.store.adopt(&held).expect_err("test: stale adoption is refused"),
            MailboxError::StaleRun,
            "run change must terminalize only the held slot"
        );
        assert_eq!(
            f.store.adopt(&held).expect_err("test: replay stale adoption"),
            MailboxError::StaleRun,
            "an erased stale receipt must replay its terminal outcome"
        );
        drop(held);
    }
    let next = f.deliver();
    assert_eq!(
        f.store
            .accept(&f.authority, replay)
            .expect_err("test: replay stale accept"),
        MailboxError::StaleRun,
        "re-accept must report the stale slot instead of its old delivery state"
    );
    assert_eq!(next.message().seq, 2, "later accepted work must survive a stale run");
    assert_eq!(
        f.store.adopt(&next).expect("test: adopt surviving slot").state,
        DeliveryState::Consumed,
        "later unpinned slot must reach its transcript"
    );
    let writer = f.memory.pool.write();
    let (state, meta): (i64, String) = writer
        .query_row(
            "SELECT state,envelope_meta FROM prx_mailbox_outbox WHERE id=?1",
            [published.row_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("test: inspect local sender nack");
    assert_eq!(state, 3, "stale slot must terminate its local sender outbox");
    let failed: i64 = writer
        .query_row("SELECT count(*) FROM prx_mailbox_outbox WHERE state=3", [], |row| {
            row.get(0)
        })
        .expect("test: inspect the single targeted nack");
    assert_eq!(failed, 1, "a stale run must nack only its own slot");
    let meta: serde_json::Value = serde_json::from_str(&meta).expect("test: decode nack metadata");
    assert_eq!(
        meta.get("last_error").and_then(serde_json::Value::as_str),
        Some("stale_run"),
        "sender must see the one-slot stale_run nack"
    );
    let live: i64 = writer
        .query_row("SELECT count(*) FROM prx_mailbox_seq WHERE state=0", [], |row| {
            row.get(0)
        })
        .expect("test: inspect stream lifetime");
    assert_eq!(live, 1, "stale work must not retire the shared stream");
    let erased: i64 = writer.query_row("SELECT count(*) FROM prx_mailbox_receipts WHERE seq=?1 AND json_extract(receipt_meta,'$.body_erased_at') IS NOT NULL", [1u64.to_be_bytes().as_slice()],|row|row.get(0))
        .expect("test: inspect terminal receipt");
    assert_eq!(
        erased, 1,
        "stale payload must be erased in the terminal-slot transaction"
    );
}

#[test]
fn stale_run_slot_is_nacked_and_later_unpinned_slots_still_deliver() {
    stale_slot_case(false);
}

#[test]
fn stale_run_during_adopt_is_nacked_and_later_unpinned_slots_still_deliver() {
    stale_slot_case(true);
}

#[test]
fn worker_run_and_parent_binding_survive_more_than_max_rows_frames() {
    use crate::session_worker::protocol::{WorkerControlFrame, WorkerOutputFrame};
    const FRAMES: i64 = crate::messaging::mailbox::MAX_ROWS + 1;
    let f = Fixture::new();
    let mut parent = crate::session_worker::mailbox_parent::WorkerMailboxParent::new(
        f.store.clone(),
        f.target.clone(),
        f.recipient.clone(),
        f.source.clone(),
        f.sender.clone(),
        "run-target".into(),
        None,
    )
    .expect("test: one persistent parent binding");
    let (mut sender, mut inbox) = crate::session_worker::mailbox::worker_mailbox(f.target.clone(), "run-target".into())
        .expect("test: one persistent worker inbox");
    f.store
        .publish(&f.authority, f.message(1, 0), &f.sender, i64::MAX, None)
        .expect("test: publish first long-lived worker frame");
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    for index in 1..=FRAMES {
        assert!(
            std::time::Instant::now() < deadline,
            "bounded long-lived worker regression exceeded deadline"
        );
        let pending = f
            .store
            .pending_outbox(&f.sender, &f.source)
            .expect("test: pending single slot");
        let message = pending.first().expect("test: one pending envelope").message().clone();
        f.store.accept(&f.authority, message).expect("test: accept next frame");
        drop(pending);
        let WorkerControlFrame::Message { envelope } = parent
            .next_delivery()
            .expect("test: next parent delivery")
            .expect("test: queued frame")
        else {
            panic!("test: parent produced non-Data control");
        };
        let delivered = sender
            .enqueue(*envelope)
            .expect("test: bounded inbox must not grow by run lifetime");
        parent.receive(delivered).expect("test: parent accepts delivered ack");
        let message = inbox.try_next().expect("test: one in-flight worker input");
        let consumed = inbox
            .consumed(&message)
            .expect("test: consume prunes the in-flight dedupe entry");
        parent
            .receive(consumed.clone())
            .expect("test: parent must not fail after committed adoption");
        parent
            .receive(consumed)
            .expect("test: consumed acknowledgement replay is harmless");
        parent
            .receive(WorkerOutputFrame::Ack {
                message_id: message.id,
                state: DeliveryState::Consumed,
            })
            .expect("test: late acknowledgement remains idempotent");
        f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
        // Keep the stream live while expiring old ledger rows; only the
        // worker's in-flight window, rather than durable storage quota, is tested.
        if index < FRAMES {
            let next = u64::try_from(index + 1).expect("test: bounded frame index");
            f.store
                .publish(&f.authority, f.message(next, 0), &f.sender, i64::MAX, None)
                .expect("test: publish next frame before retention GC");
        }
        f.store
            .gc_scope(&f.sender)
            .expect("test: expire sender's durable replay records");
        f.store
            .gc_scope(&f.recipient)
            .expect("test: expire consumed receiver records");
    }
}
