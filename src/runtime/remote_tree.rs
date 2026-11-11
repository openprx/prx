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
        let conn = sql::open_connection(&self.path).map_err(sql::storage_error)?;
        sql::charge(&conn, id, resource, resource_id, ma_scope_id, self.caps())
    }

    /// Refund occupancy only. Cumulative `Spawned`/`Send` counters stay.
    pub fn refund(
        &self,
        id: &RemoteTreeId,
        resource: RemoteTreeResource,
        resource_id: &str,
    ) -> Result<(), RemoteTreeError> {
        let conn = sql::open_connection(&self.path).map_err(sql::storage_error)?;
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
        let conn = sql::open_connection(&self.path).map_err(sql::storage_error)?;
        sql::resume(&conn, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::SqliteMemory;

    fn store(dir: &tempfile::TempDir) -> (RemoteTreeStore, RemoteTreeId, RemoteTreeId) {
        store_with_limits(dir, RemoteTreeLimits::default())
    }

    fn store_with_limits(
        dir: &tempfile::TempDir,
        limits: RemoteTreeLimits,
    ) -> (RemoteTreeStore, RemoteTreeId, RemoteTreeId) {
        let memory = SqliteMemory::new(dir.path()).expect("test: sqlite");
        drop(memory);
        let path = dir.path().join("memory").join("brain.db");
        let store = RemoteTreeStore::open(&path, limits).expect("test: open");
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
    #[test]
    fn remote_tree_close_refuses_live_or_resumable() {
        let dir = tempfile::TempDir::new().expect("tree database");
        let (store, id, _) = store(&dir);
        store
            .charge(&id, RemoteTreeResource::Scheduled, "job", None)
            .expect("live job");
        assert!(store.close(&id, false).is_err());
        store
            .refund(&id, RemoteTreeResource::Scheduled, "job")
            .expect("refund job");
        assert!(store.close(&id, true).is_err());
        store.resume(&id).expect("open tree can resume");
        store.close(&id, false).expect("quiescent tree closes");
        assert_eq!(store.rebuild(&id).expect("closed tombstone").state, "closed");
    }

    #[test]
    fn resume_of_dropped_remote_tree_is_refused() {
        let dir = tempfile::TempDir::new().expect("tree database");
        let (store, id, _) = store(&dir);
        store
            .charge(&id, RemoteTreeResource::Send, "send", None)
            .expect("create tree");
        store.close(&id, false).expect("close tree");
        drop(store);
        let store =
            RemoteTreeStore::open(dir.path().join("memory/brain.db"), RemoteTreeLimits::default()).expect("reopen");
        assert_eq!(store.resume(&id), Err(RemoteTreeError::Closed));
        for resource in [
            RemoteTreeResource::Spawned,
            RemoteTreeResource::Send,
            RemoteTreeResource::Scheduled,
            RemoteTreeResource::Connection,
        ] {
            assert_eq!(
                store.charge(&id, resource, "after-close", None),
                Err(RemoteTreeError::Closed)
            );
        }
    }

    #[test]
    fn concurrent_connections_cannot_exceed_remote_tree_occupancy_cap() {
        let dir = tempfile::TempDir::new().expect("tree database");
        let (store, id, _) = store(&dir);
        let store = std::sync::Arc::new(store);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(8);
            for index in 0..8 {
                let store = std::sync::Arc::clone(&store);
                let barrier = std::sync::Arc::clone(&barrier);
                let id = id.clone();
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    store.charge(&id, RemoteTreeResource::Scheduled, &format!("job-{index}"), None)
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("charge thread"))
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 4);
        assert!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .all(|error| *error
                    == RemoteTreeError::Exhausted {
                        resource: RemoteTreeResource::Scheduled
                    })
        );
        assert_eq!(store.rebuild(&id).expect("atomic occupancy").scheduled_live, 4);
    }

    fn limits(maximum: u32) -> RemoteTreeLimits {
        RemoteTreeLimits {
            max_spawned: maximum,
            max_scheduled: maximum,
            max_connections: maximum,
            max_sends: maximum,
        }
    }

    fn charge_each(store: &RemoteTreeStore, id: &RemoteTreeId, index: usize) {
        for resource in [
            RemoteTreeResource::Spawned,
            RemoteTreeResource::Send,
            RemoteTreeResource::Scheduled,
            RemoteTreeResource::Connection,
        ] {
            store
                .charge(id, resource, &format!("{resource}-{index}"), None)
                .expect("charge resource");
        }
    }

    #[test]
    fn remote_tree_spawn_breadth_cap_is_enforced() {
        let dir = tempfile::TempDir::new().expect("tree database");
        let (store, id, _) = store_with_limits(
            &dir,
            RemoteTreeLimits {
                max_spawned: 2,
                ..Default::default()
            },
        );
        for index in 0..2 {
            store
                .charge(&id, RemoteTreeResource::Spawned, &format!("child-{index}"), None)
                .expect("spawn");
        }
        assert_eq!(
            store.charge(&id, RemoteTreeResource::Spawned, "extra", None),
            Err(RemoteTreeError::Exhausted {
                resource: RemoteTreeResource::Spawned
            })
        );
        let snapshot = store.rebuild(&id).expect("snapshot");
        assert_eq!(snapshot.spawned, 2);
        assert_eq!(snapshot.descendant_live, 2);
    }

    #[test]
    fn remote_tree_scheduled_entry_cap_is_enforced() {
        let dir = tempfile::TempDir::new().expect("tree database");
        let (store, id, _) = store_with_limits(
            &dir,
            RemoteTreeLimits {
                max_scheduled: 2,
                ..Default::default()
            },
        );
        for index in 0..2 {
            store
                .charge(&id, RemoteTreeResource::Scheduled, &format!("job-{index}"), None)
                .expect("schedule");
        }
        assert_eq!(
            store.charge(&id, RemoteTreeResource::Scheduled, "extra", None),
            Err(RemoteTreeError::Exhausted {
                resource: RemoteTreeResource::Scheduled
            })
        );
        store
            .refund(&id, RemoteTreeResource::Scheduled, "job-0")
            .expect("remove job");
        store
            .charge(&id, RemoteTreeResource::Scheduled, "extra", None)
            .expect("released slot");
        assert_eq!(store.rebuild(&id).expect("snapshot").scheduled_live, 2);
    }

    #[test]
    fn remote_tree_live_connection_cap_is_enforced() {
        let dir = tempfile::TempDir::new().expect("tree database");
        let (store, id, _) = store_with_limits(
            &dir,
            RemoteTreeLimits {
                max_connections: 2,
                ..Default::default()
            },
        );
        for index in 0..2 {
            store
                .charge(
                    &id,
                    RemoteTreeResource::Connection,
                    &format!("connection-{index}"),
                    None,
                )
                .expect("connect");
        }
        assert_eq!(
            store.charge(&id, RemoteTreeResource::Connection, "extra", None),
            Err(RemoteTreeError::Exhausted {
                resource: RemoteTreeResource::Connection
            })
        );
        store
            .refund(&id, RemoteTreeResource::Connection, "connection-0")
            .expect("close connection");
        store
            .charge(&id, RemoteTreeResource::Connection, "extra", None)
            .expect("released slot");
        assert_eq!(store.rebuild(&id).expect("snapshot").connection_live, 2);
    }

    #[test]
    fn remote_tree_counters_survive_clock_advance_with_live_resources() {
        let dir = tempfile::TempDir::new().expect("tree database");
        let (store, id, _) = store_with_limits(&dir, limits(1));
        charge_each(&store, &id, 0);
        // Age the durable creation stamp relative to the actual wall clock.
        // A forward clock jump must not make live resources or totals expire.
        let connection = sql::open_connection(&store.path).expect("tree connection");
        let updated = connection
            .execute(
                "UPDATE prx_remote_trees SET created_at = 0
             WHERE workspace_id = ?1 AND owner_id = ?2 AND trigger_message_id = ?3",
                rusqlite::params![id.workspace_id, id.owner_id, id.trigger_message_id],
            )
            .expect("age tree");
        assert_eq!(updated, 1, "the live trigger must actually be aged");
        assert_eq!(
            store.rebuild(&id).expect("aged snapshot"),
            RemoteTreeSnapshot {
                spawned: 1,
                sends: 1,
                scheduled_live: 1,
                connection_live: 1,
                descendant_live: 1,
                state: "open".into(),
            }
        );
        for resource in [
            RemoteTreeResource::Spawned,
            RemoteTreeResource::Send,
            RemoteTreeResource::Scheduled,
            RemoteTreeResource::Connection,
        ] {
            assert_eq!(
                store.charge(&id, resource, "extra", None),
                Err(RemoteTreeError::Exhausted { resource })
            );
        }
    }

    #[test]
    fn remote_tree_counters_rebuilt_after_restart() {
        let dir = tempfile::TempDir::new().expect("tree database");
        let (store, id, _) = store_with_limits(&dir, limits(2));
        charge_each(&store, &id, 0);
        charge_each(&store, &id, 1);
        let path = store.path.clone();
        drop(store);
        let reopened = RemoteTreeStore::open(path, limits(2)).expect("reopen store");
        assert_eq!(
            reopened.rebuild(&id).expect("durable snapshot"),
            RemoteTreeSnapshot {
                spawned: 2,
                sends: 2,
                scheduled_live: 2,
                connection_live: 2,
                descendant_live: 2,
                state: "open".into(),
            }
        );
        for resource in [
            RemoteTreeResource::Spawned,
            RemoteTreeResource::Send,
            RemoteTreeResource::Scheduled,
            RemoteTreeResource::Connection,
        ] {
            assert_eq!(
                reopened.charge(&id, resource, "extra", None),
                Err(RemoteTreeError::Exhausted { resource })
            );
        }
    }

    #[test]
    fn closed_connection_refunds_occupancy_but_spawn_and_send_totals_never_refund() {
        let dir = tempfile::TempDir::new().expect("tree database");
        let (store, id, _) = store_with_limits(&dir, limits(1));
        charge_each(&store, &id, 0);
        store
            .refund(&id, RemoteTreeResource::Connection, "connection-0")
            .expect("close connection");
        store
            .refund(&id, RemoteTreeResource::Spawned, "spawned-0")
            .expect("descendant exit");
        store
            .refund(&id, RemoteTreeResource::Send, "send-0")
            .expect("send has no occupancy");
        store
            .charge(&id, RemoteTreeResource::Connection, "replacement", None)
            .expect("connection slot reused");
        for resource in [
            RemoteTreeResource::Spawned,
            RemoteTreeResource::Send,
            RemoteTreeResource::Scheduled,
        ] {
            assert_eq!(
                store.charge(&id, resource, "extra", None),
                Err(RemoteTreeError::Exhausted { resource })
            );
        }
        assert_eq!(
            store.rebuild(&id).expect("snapshot"),
            RemoteTreeSnapshot {
                spawned: 1,
                sends: 1,
                scheduled_live: 1,
                connection_live: 1,
                descendant_live: 0,
                state: "open".into(),
            }
        );
    }

    #[test]
    fn remote_tree_storage_failure_is_not_closed() {
        let dir = tempfile::TempDir::new().expect("missing database root");
        let store = RemoteTreeStore::open(dir.path().join("missing/brain.db"), RemoteTreeLimits::default())
            .expect("store handle");
        let id = RemoteTreeId {
            workspace_id: "ws".into(),
            owner_id: "owner-a".into(),
            trigger_message_id: "trigger".into(),
        };
        assert_eq!(
            store.charge(&id, RemoteTreeResource::Send, "send", None),
            Err(RemoteTreeError::Storage)
        );
        assert_eq!(store.resume(&id), Err(RemoteTreeError::Storage));
    }
}
