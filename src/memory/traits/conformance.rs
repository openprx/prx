use super::{Memory, MemoryCategory, MemoryReadMode};
use crate::memory::principal::MemoryWriteContext;

pub(crate) async fn assert_scoped_memory_acl_conformance(mem: &dyn Memory, key_prefix: &str) {
    let alice_ctx = MemoryWriteContext {
        agent_partition: None,
        session_key: None,
        workspace_id: Some("workspace-a".into()),
        channel: Some("telegram".into()),
        chat_type: Some("private".into()),
        chat_id: Some(format!("{key_prefix}:dm-alice")),
        sender_id: None,
        raw_sender: Some("alice".into()),
        owner_id: None,
        require_owner_match: false,
    };
    let bob_ctx = MemoryWriteContext {
        agent_partition: None,
        session_key: None,
        workspace_id: Some("workspace-a".into()),
        channel: Some("telegram".into()),
        chat_type: Some("private".into()),
        chat_id: Some(format!("{key_prefix}:dm-bob")),
        sender_id: None,
        raw_sender: Some("bob".into()),
        owner_id: None,
        require_owner_match: false,
    };
    let alice_key = format!("{key_prefix}:alice-private");
    let bob_key = format!("{key_prefix}:bob-private");

    mem.store_with_context(
        &alice_key,
        "shared conformance keyword alice private",
        MemoryCategory::Conversation,
        None,
        Some(&alice_ctx),
    )
    .await
    .expect("store alice scoped memory");
    mem.store_with_context(
        &bob_key,
        "shared conformance keyword bob private",
        MemoryCategory::Conversation,
        None,
        Some(&bob_ctx),
    )
    .await
    .expect("store bob scoped memory");

    let alice_results = mem
        .recall_with_context("shared conformance keyword", 10, None, Some(&alice_ctx))
        .await
        .expect("recall alice scoped memory");
    let alice_keys = alice_results.iter().map(|entry| entry.key.as_str()).collect::<Vec<_>>();
    assert!(alice_keys.contains(&alice_key.as_str()), "{alice_keys:?}");
    assert!(!alice_keys.contains(&bob_key.as_str()), "{alice_keys:?}");

    assert!(
        mem.get_with_context_mode(&bob_key, Some(&alice_ctx), MemoryReadMode::Enforce)
            .await
            .expect("enforced keyed read")
            .is_none(),
        "enforce mode must hide another sender's private memory"
    );
    assert!(
        mem.get_with_context_mode(&bob_key, Some(&alice_ctx), MemoryReadMode::Observe)
            .await
            .expect("observed keyed read")
            .is_some(),
        "observe mode must preserve legacy visibility while recording the would-deny decision"
    );
    let enforced_search = mem
        .recall_with_context_mode(
            "shared conformance keyword bob private",
            10,
            None,
            Some(&alice_ctx),
            MemoryReadMode::Enforce,
        )
        .await
        .expect("enforced scoped search");
    assert!(!enforced_search.iter().any(|entry| entry.key == bob_key));
    let observed_search = mem
        .recall_with_context_mode(
            "shared conformance keyword bob private",
            10,
            None,
            Some(&alice_ctx),
            MemoryReadMode::Observe,
        )
        .await
        .expect("observed scoped search");
    assert!(observed_search.iter().any(|entry| entry.key == bob_key));

    assert!(
        !mem.forget_with_context(&bob_key, Some(&alice_ctx))
            .await
            .expect("alice cannot delete bob memory"),
        "alice context must not delete bob scoped memory"
    );
    assert!(
        mem.forget_with_context(&alice_key, Some(&alice_ctx))
            .await
            .expect("alice deletes alice memory"),
        "alice context should delete alice scoped memory"
    );
    assert!(
        mem.get(&bob_key).await.expect("bob memory remains").is_some(),
        "bob memory should remain after denied alice delete"
    );
}
