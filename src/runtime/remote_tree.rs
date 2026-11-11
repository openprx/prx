//! Durable remote causal-tree counters (C2). Dormant: no production spawn site reads this yet.

use std::path::{Path, PathBuf};

use crate::config::schema::RemoteTreeLimits;
use crate::memory::sqlite::remote_tree as sql;

pub use crate::memory::sqlite::remote_tree::{RemoteTreeError, RemoteTreeId, RemoteTreeResource, RemoteTreeSnapshot};

/// Durable counter store. Caps come from `[a2a.remote_tree]`.
pub struct RemoteTreeStore {
    path: PathBuf,
    limits: RemoteTreeLimits,
}

impl RemoteTreeStore {
    /// Open the store on an already-migrated SQLite file.
    pub fn open(path: impl AsRef<Path>, limits: RemoteTreeLimits) -> anyhow::Result<Self> {
        Ok(Self {
            path: path.as_ref().to_path_buf(),
            limits,
        })
    }

    const fn caps(&self) -> sql::RemoteTreeCaps {
        sql::RemoteTreeCaps {
            max_spawned: self.limits.max_spawned,
            max_scheduled: self.limits.max_scheduled,
            max_connections: self.limits.max_connections,
            max_sends: self.limits.max_sends,
        }
    }

    /// Charge `resource` against `id`. Cumulative kinds never go down.
    pub fn charge(
        &self,
        id: &RemoteTreeId,
        resource: RemoteTreeResource,
        resource_id: &str,
        ma_scope_id: Option<&str>,
    ) -> Result<(), RemoteTreeError> {
        let conn = sql::open_connection(&self.path).map_err(|_| RemoteTreeError::Closed)?;
        sql::charge(&conn, id, resource, resource_id, ma_scope_id, self.caps())
    }

    /// Refund occupancy only. Cumulative `Spawned`/`Send` counters stay.
    pub fn refund(
        &self,
        id: &RemoteTreeId,
        resource: RemoteTreeResource,
        resource_id: &str,
    ) -> Result<(), RemoteTreeError> {
        let conn = sql::open_connection(&self.path).map_err(|_| RemoteTreeError::Closed)?;
        sql::refund(&conn, id, resource, resource_id)
    }

    /// Recompute occupancy from the resources table after reopen.
    pub fn rebuild(&self, id: &RemoteTreeId) -> anyhow::Result<RemoteTreeSnapshot> {
        let conn = sql::open_connection(&self.path)?;
        sql::rebuild(&conn, id)
    }

    /// Close the tree. Refused while live occupancy remains or a resumable turn
    /// still references the trigger.
    pub fn close(&self, id: &RemoteTreeId, has_resumable_turn: bool) -> anyhow::Result<()> {
        let conn = sql::open_connection(&self.path)?;
        sql::close(&conn, id, has_resumable_turn)
    }

    /// Resume a tree. Closed trees return [`RemoteTreeError::Closed`].
    pub fn resume(&self, id: &RemoteTreeId) -> Result<(), RemoteTreeError> {
        let conn = sql::open_connection(&self.path).map_err(|_| RemoteTreeError::Closed)?;
        sql::resume(&conn, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::SqliteMemory;

    fn store(dir: &tempfile::TempDir) -> (RemoteTreeStore, RemoteTreeId, RemoteTreeId) {
        let memory = SqliteMemory::new(dir.path()).expect("test: sqlite");
        drop(memory);
        let path = dir.path().join("memory").join("brain.db");
        let store = RemoteTreeStore::open(&path, RemoteTreeLimits::default()).expect("test: open");
        let a = RemoteTreeId {
            workspace_id: "ws".into(),
            owner_id: "owner-a".into(),
            trigger_message_id: "trigger-1".into(),
        };
        let b = RemoteTreeId {
            workspace_id: "ws".into(),
            owner_id: "owner-b".into(),
            trigger_message_id: "trigger-1".into(),
        };
        (store, a, b)
    }

    #[test]
    fn remote_tree_store_rebuilds_after_reopen() {
        let dir = tempfile::TempDir::new().expect("test: temp");
        let path = dir.path().join("memory").join("brain.db");
        let id = {
            let (store, id, _) = store(&dir);
            store
                .charge(&id, RemoteTreeResource::Spawned, "child-1", None)
                .expect("test: spawn");
            store
                .charge(&id, RemoteTreeResource::Scheduled, "job-1", None)
                .expect("test: scheduled");
            id
        };
        let store = RemoteTreeStore::open(&path, RemoteTreeLimits::default()).expect("test: reopen");
        let snap = store.rebuild(&id).expect("test: rebuild");
        assert_eq!(snap.spawned, 1);
        assert_eq!(snap.scheduled_live, 1);
        assert_eq!(snap.state, "open");
    }

    #[test]
    fn remote_tree_store_refund_only_occupancy() {
        let dir = tempfile::TempDir::new().expect("test: temp");
        let (store, id, _) = store(&dir);
        store
            .charge(&id, RemoteTreeResource::Spawned, "child-1", None)
            .expect("test: spawn");
        store
            .charge(&id, RemoteTreeResource::Send, "send-1", None)
            .expect("test: send");
        store
            .charge(&id, RemoteTreeResource::Scheduled, "job-1", None)
            .expect("test: scheduled");
        store
            .charge(&id, RemoteTreeResource::Connection, "conn-1", None)
            .expect("test: connection");
        store
            .refund(&id, RemoteTreeResource::Spawned, "child-1")
            .expect("test: refund spawn occupancy");
        store
            .refund(&id, RemoteTreeResource::Send, "send-1")
            .expect("test: refund send is a no-op on the counter");
        store
            .refund(&id, RemoteTreeResource::Scheduled, "job-1")
            .expect("test: refund scheduled");
        store
            .refund(&id, RemoteTreeResource::Connection, "conn-1")
            .expect("test: refund connection");
        let snap = store.rebuild(&id).expect("test: snap");
        assert_eq!(snap.spawned, 1, "cumulative spawn is never refunded");
        assert_eq!(snap.sends, 1, "cumulative send is never refunded");
        assert_eq!(snap.scheduled_live, 0);
        assert_eq!(snap.connection_live, 0);
        assert_eq!(snap.descendant_live, 0);
    }

    #[test]
    fn equal_trigger_ids_in_different_owners_are_independent() {
        let dir = tempfile::TempDir::new().expect("test: temp");
        let (store, a, b) = store(&dir);
        for i in 0..16 {
            store
                .charge(&a, RemoteTreeResource::Spawned, &format!("a-{i}"), None)
                .expect("test: owner-a spawn");
        }
        let err = store
            .charge(&a, RemoteTreeResource::Spawned, "a-overflow", None)
            .expect_err("test: owner-a cap");
        assert_eq!(
            err,
            RemoteTreeError::Exhausted {
                resource: RemoteTreeResource::Spawned
            }
        );
        assert_eq!(err.to_string(), "remote_tree_resource_exhausted: spawned");
        store
            .charge(&b, RemoteTreeResource::Spawned, "b-1", None)
            .expect("test: owner-b independent");
        let snap_a = store.rebuild(&a).expect("test: a");
        let snap_b = store.rebuild(&b).expect("test: b");
        assert_eq!(snap_a.spawned, 16);
        assert_eq!(snap_b.spawned, 1);
    }
}
