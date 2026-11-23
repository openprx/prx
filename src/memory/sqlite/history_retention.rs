//! Owner memory survives a plain history erase; snapshots and paging do not.
use rusqlite::{Connection, params};
const MAX_RETAINED: usize = 20_000;

const MEMORIES: &str =
    "SELECT key FROM memories WHERE ma_scope_id IN(SELECT value FROM json_each(?1)) AND key NOT GLOB 'chat_session:*'";
const MEMORY_IDS: &str =
    "SELECT id FROM memories WHERE ma_scope_id IN(SELECT value FROM json_each(?1)) AND key NOT GLOB 'chat_session:*'";
const DOCUMENTS: &str = "SELECT document_id FROM documents WHERE ma_scope_id IN(SELECT value FROM json_each(?1)) AND source_kind<>'context_page'";
const TOPICS: &str = "SELECT topic_id FROM memories WHERE ma_scope_id IN(SELECT value FROM json_each(?1)) AND key NOT GLOB 'chat_session:*' UNION SELECT topic_id FROM documents WHERE ma_scope_id IN(SELECT value FROM json_each(?1)) AND source_kind<>'context_page'";

pub(super) fn kept(table: &str) -> String {
    match table {
        "memories" => "key NOT GLOB 'chat_session:*'".into(),
        "documents" => "source_kind<>'context_page'".into(),
        "document_chunks" => format!("document_id IN({DOCUMENTS})"),
        "memory_trash" => format!("memory_key IN({MEMORIES})"),
        "memory_drafts" => format!("key IN({MEMORIES})"),
        "memory_links" => format!("memory_key IN({MEMORIES}) OR document_id IN({DOCUMENTS})"),
        "memory_events" => format!(
            "(subject_table='memories' AND (subject_id IN({MEMORIES}) OR subject_id IN({MEMORY_IDS}))) OR (subject_table='documents' AND subject_id IN({DOCUMENTS}))"
        ),
        "topics" => format!("id IN({TOPICS})"),
        "topic_participants" => format!("topic_id IN({TOPICS})"),
        "topic_aliases" => format!("from_topic_id IN({TOPICS}) OR to_topic_id IN({TOPICS})"),
        // Derived vectors and the content-free attribution log remain owned by
        // the retained memories. Deleting snapshots/pages evicts their vectors.
        "embedding_cache" => format!("EXISTS({MEMORIES}) OR EXISTS({DOCUMENTS})"),
        "memory_write_log" => "1".into(),
        _ => "0".into(),
    }
}

pub(super) fn list(conn: &Connection, scopes: &str) -> anyhow::Result<serde_json::Value> {
    let mut memories = conn.prepare(&format!(
        "SELECT id,key FROM memories WHERE ma_scope_id IN(SELECT value FROM json_each(?1)) AND ({}) ORDER BY id LIMIT 20001",
        kept("memories")
    ))?;
    let memories = memories
        .query_map(params![scopes], |row| {
            Ok(serde_json::json!({"id":row.get::<_,String>(0)?,"key":row.get::<_,String>(1)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        memories.len() <= MAX_RETAINED,
        crate::memory::RfForgetRefused::FamilyTooLarge
    );
    let mut documents = conn.prepare(&format!("SELECT document_id FROM documents WHERE ma_scope_id IN(SELECT value FROM json_each(?1)) AND ({}) ORDER BY document_id LIMIT 20001", kept("documents")))?;
    let documents = documents
        .query_map(params![scopes], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        documents.len() <= MAX_RETAINED,
        crate::memory::RfForgetRefused::FamilyTooLarge
    );
    Ok(serde_json::json!({"memories":memories,"documents":documents}))
}
