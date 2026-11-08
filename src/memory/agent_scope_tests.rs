//! MA-01b2: Named partitions fail closed on backends without agent scope.
#![cfg(test)]

use crate::config::agent_name::AgentName;
use crate::memory::encrypted_scope::EncryptedScope;
use crate::memory::lucid::LucidMemory;
use crate::memory::markdown::MarkdownMemory;
use crate::memory::postgres::{PostgresMemory, live_postgres_url};
use crate::memory::{AgentPartition, AgentScopeUnsupported, Memory, MemoryPrincipal, PartitionKey, SqliteMemory};
use crate::security::ramflux_aead::AccountCrypto;
use std::sync::Arc;
use uuid::Uuid;

fn named_principal() -> MemoryPrincipal {
    MemoryPrincipal {
        workspace_id: "prx_workspace".to_owned(),
        agent_id: None,
        persona_id: None,
        session_key: Some("session".to_owned()),
        channel: None,
        sender: None,
        owner_id: Some("prx_owner".to_owned()),
        require_owner_match: false,
        legacy_session_key: None,
        agent_partition: Some(AgentPartition {
            workspace_id: "prx_workspace".to_owned(),
            owner_id: "prx_owner".to_owned(),
            key: PartitionKey::Named {
                name: AgentName::parse("x").expect("test: agent name"),
                incarnation: Uuid::parse_str("00000000-0000-4000-8000-0000000000a1").expect("test: uuid"),
            },
        }),
    }
}

fn is_unsupported(error: &anyhow::Error) -> bool {
    error.downcast_ref::<AgentScopeUnsupported>().is_some()
}

async fn named_reads_are_unsupported(memory: &dyn Memory) {
    assert!(!memory.supports_agent_scope());
    let principal = named_principal();
    let turns = memory
        .list_console_conversation_turns(&principal, "session", 10, 0)
        .await
        .expect_err("test: named turns");
    assert!(is_unsupported(&turns), "{turns:#}");
    let events = memory
        .list_message_events_since(&principal, 0, 10)
        .await
        .expect_err("test: named events");
    assert!(is_unsupported(&events), "{events:#}");
}

#[tokio::test]
async fn markdown_targeted_scope_rejected() {
    let dir = tempfile::tempdir().expect("test: tempdir");
    named_reads_are_unsupported(&MarkdownMemory::new(dir.path())).await;
}

#[tokio::test]
async fn lucid_targeted_scope_rejected() {
    let dir = tempfile::tempdir().expect("test: tempdir");
    let local = SqliteMemory::new(dir.path()).expect("test: sqlite");
    let lucid = LucidMemory::new(dir.path(), local, &crate::config::MemoryConfig::default());
    named_reads_are_unsupported(&lucid).await;
}

#[tokio::test]
async fn encrypted_scope_targeted_scope_rejected() {
    let dir = tempfile::tempdir().expect("test: tempdir");
    let inner = Arc::new(SqliteMemory::new(dir.path()).expect("test: sqlite"));
    let crypto = Arc::new(AccountCrypto::new(dir.path(), dir.path(), true));
    let scoped = EncryptedScope::new(inner, crypto, "account", "conversation");
    named_reads_are_unsupported(&scoped).await;
}

#[test]
fn postgres_targeted_scope_rejected() {
    let production = include_str!("postgres/memory_impl.rs");
    for declaration in [
        "    async fn list_console_conversation_turns(",
        "    async fn list_message_events_since(",
    ] {
        let start = production
            .find(declaration)
            .unwrap_or_else(|| panic!("test: missing {declaration}"));
        let rest = &production[start + declaration.len()..];
        let end = rest.find("\n    async fn ").unwrap_or(rest.len());
        let method = &production[start..start + declaration.len() + end];
        let body = method
            .split_once('{')
            .map(|(_, rest)| rest.trim_start())
            .expect("test: method body");
        assert!(
            body.starts_with("principal.reject_named_scope()?;"),
            "{declaration} must reject Named first:\n{method}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_targeted_scope_rejected_live() {
    let db_url = live_postgres_url();
    let schema = format!("prx_ma_01b2_{}", Uuid::new_v4().simple());
    let memory = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    named_reads_are_unsupported(&memory).await;
}
