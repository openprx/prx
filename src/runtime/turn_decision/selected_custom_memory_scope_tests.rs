#![cfg(test)]
use super::selected_custom_memory_read_scope;
use crate::memory::Memory;
use crate::memory::principal::MemoryWriteContext;

#[test]
fn source_owner_read_scope_is_stricter_and_rejects_conflicting_identity() {
    let write = MemoryWriteContext {
        workspace_id: Some("prx_workspace".into()),
        chat_type: Some("private".into()),
        chat_id: Some("prx_session".into()),
        ..Default::default()
    };
    let read = selected_custom_memory_read_scope(&write, "prx_workspace", "prx_owner").unwrap();
    assert_eq!(read.owner_id.as_deref(), Some("prx_owner"));
    assert!(read.require_owner_match);
    assert_eq!(read.chat_id, write.chat_id);
    assert_eq!(read.chat_type, write.chat_type);
    assert!(write.owner_id.is_none());
    assert!(!write.require_owner_match);
    assert!(selected_custom_memory_read_scope(&write, "prx_other_workspace", "prx_owner").is_err());
    let mut conflicting = write.clone();
    conflicting.owner_id = Some("prx_foreign_owner".into());
    assert!(selected_custom_memory_read_scope(&conflicting, "prx_workspace", "prx_owner").is_err());
    assert!(selected_custom_memory_read_scope(&write, "prx_workspace", "").is_err());
}

#[tokio::test]
async fn selected_custom_loader_reads_actual_owner_rows_only() {
    struct Loader;
    #[async_trait::async_trait]
    impl crate::agent::memory_loader::MemoryLoader for Loader {
        async fn load_context(&self, memory: &dyn Memory, input: &str) -> anyhow::Result<String> {
            let hits = memory.recall(input, 5, None).await?;
            assert_eq!(hits.len(), 1);
            let hit = hits.first().expect("exactly one owned memory hit");
            assert_eq!(hit.content, "prx_query owned evidence");
            Ok(hit.content.clone())
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let memory = crate::memory::SqliteMemory::new(tmp.path()).unwrap();
    let write = MemoryWriteContext {
        workspace_id: Some("prx_workspace".into()),
        channel: Some("agent_api".into()),
        raw_sender: Some("prx_user".into()),
        chat_type: Some("private".into()),
        chat_id: Some("prx_session".into()),
        ..Default::default()
    };
    for (owner, content) in [
        ("prx_owner", "prx_query owned evidence"),
        ("prx_foreign_owner", "prx_query foreign evidence"),
    ] {
        let mut scope = write.clone();
        scope.owner_id = Some(owner.into());
        scope.require_owner_match = true;
        memory
            .store_with_context_and_metadata(
                owner,
                content,
                crate::memory::MemoryCategory::Core,
                Some("prx_session"),
                Some(&scope),
                crate::memory::MemoryStoreMetadata {
                    workspace_id: Some("prx_workspace".into()),
                    owner_id: Some(owner.into()),
                    channel: Some("agent_api".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let scope = selected_custom_memory_read_scope(&write, "prx_workspace", "prx_owner").unwrap();
    let loaded = crate::agent::memory_loader::load_selected_memory(
        &Loader,
        &memory,
        "prx_query",
        &scope,
        "prx_session",
        std::time::Instant::now() + std::time::Duration::from_millis(500),
    )
    .await
    .unwrap();
    assert_eq!(loaded.hits.len(), 1);
    assert_eq!(loaded.context, "prx_query owned evidence");
}
