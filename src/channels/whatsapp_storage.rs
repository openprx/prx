//! Custom wa-rs storage backend using OpenPRX's rusqlite
//!
//! This module implements all 4 wa-rs storage traits using rusqlite directly,
//! avoiding the Diesel/libsqlite3-sys dependency conflict from wa-rs-sqlite-storage.
//!
//! # Traits Implemented
//!
//! - [`SignalStore`]: Signal protocol cryptographic operations
//! - [`AppSyncStore`]: WhatsApp app state synchronization
//! - [`ProtocolStore`]: WhatsApp Web protocol alignment
//! - [`DeviceStore`]: Device persistence operations
//!
//! # Concurrency model
//!
//! Every trait method is `async`, but rusqlite is a *synchronous* library. Two
//! rules follow from that, and both are load-bearing for an agent runtime that
//! deliberately places no cap on how many sessions run at once:
//!
//! 1. **No synchronous SQL ever runs in an `async fn` body.** Every statement
//!    goes through [`RusqliteStore::with_read`] / [`RusqliteStore::with_write`],
//!    which hand the work to [`crate::runtime::blocking`]. Executing it inline
//!    would park a tokio *worker* thread — of which there are only as many as
//!    the machine has cores — for the duration of a disk write, starving every
//!    unrelated future in the process.
//! 2. **The database handle is pooled, not shared.** Offloading alone would
//!    only move the queue from the worker threads into the blocking pool if all
//!    callers still contended for one connection.
//!
//! SQLite's concurrency rules shape the pool: in WAL mode any number of readers
//! run concurrently with one writer, but there is exactly one writer at a time.
//! So the pool is deliberately asymmetric — a lazily grown pool of read
//! connections, plus a single writer connection behind a mutex. A symmetric
//! pool (as used for PostgreSQL in `crate::memory::postgres`) would buy nothing
//! here: the extra write connections would simply collide on SQLite's file lock
//! instead of on our mutex, and fail rather than queue.
//!
//! Acquiring either kind of connection **never times out**. A saturated pool
//! makes callers wait and is recorded in [`WhatsAppStoragePoolStats`]; it never
//! turns work that is merely queued into an error.

#[cfg(feature = "whatsapp-web")]
use crate::runtime::sqlite_pool::{SqliteConnectionPool, default_read_pool_size};
#[cfg(feature = "whatsapp-web")]
use async_trait::async_trait;
#[cfg(feature = "whatsapp-web")]
use rusqlite::{Connection, params};
#[cfg(feature = "whatsapp-web")]
use std::path::Path;
#[cfg(feature = "whatsapp-web")]
use std::sync::Arc;

#[cfg(feature = "whatsapp-web")]
use prost::Message;
#[cfg(feature = "whatsapp-web")]
use wa_rs_binary::jid::Jid;
#[cfg(feature = "whatsapp-web")]
use wa_rs_core::appstate::hash::HashState;
#[cfg(feature = "whatsapp-web")]
use wa_rs_core::appstate::processor::AppStateMutationMAC;
#[cfg(feature = "whatsapp-web")]
use wa_rs_core::store::Device as CoreDevice;
#[cfg(feature = "whatsapp-web")]
use wa_rs_core::store::error::{Result as StoreResult, StoreError};
#[cfg(feature = "whatsapp-web")]
use wa_rs_core::store::traits::DeviceInfo;
#[cfg(feature = "whatsapp-web")]
use wa_rs_core::store::traits::DeviceStore as DeviceStoreTrait;
#[cfg(feature = "whatsapp-web")]
use wa_rs_core::store::traits::{
    AppStateSyncKey, AppSyncStore, DeviceListRecord, LidPnMappingEntry, ProtocolStore, SignalStore, TcTokenEntry,
};

/// Helper macro to convert rusqlite errors to StoreError
/// For execute statements that return usize, maps to ()
macro_rules! to_store_err {
    // For expressions returning Result<usize, E>
    (execute: $expr:expr) => {
        $expr
            .map(|_| ())
            .map_err(|e| wa_rs_core::store::error::StoreError::Database(e.to_string()))
    };
    // For other expressions
    ($expr:expr) => {
        $expr.map_err(|e| wa_rs_core::store::error::StoreError::Database(e.to_string()))
    };
}

// ── Connection pool ─────────────────────────────────────────────

/// Extra connection-scoped PRAGMA for both kinds of connection.
///
/// `journal_mode = WAL` is set by the pool itself (pooled readers are unsound
/// without it) and `query_only` is added to every reader by the pool, so the
/// only thing left to say here is the durability trade: `NORMAL` costs one
/// fsync per checkpoint instead of one per commit, which is still crash-safe
/// under WAL.
#[cfg(feature = "whatsapp-web")]
const SESSION_DB_PRAGMAS: &str = "PRAGMA synchronous = NORMAL;";

/// Point-in-time view of the WhatsApp session-store connection pool.
///
/// Exposed so a health endpoint can tell "slow because the database is busy"
/// apart from "slow because the model is thinking" without attaching a
/// debugger. This is the shared pool's snapshot type: the session store and the
/// memory brain report saturation identically, because they run the same pool.
#[cfg(feature = "whatsapp-web")]
pub use crate::runtime::sqlite_pool::SqlitePoolStats as WhatsAppStoragePoolStats;

/// Custom wa-rs storage backend using rusqlite
///
/// This implements all 4 storage traits required by wa-rs.
/// The backend uses OpenPRX's existing rusqlite setup, avoiding the
/// Diesel/libsqlite3-sys conflict from wa-rs-sqlite-storage.
#[cfg(feature = "whatsapp-web")]
#[derive(Clone)]
pub struct RusqliteStore {
    /// Database file path
    db_path: Arc<str>,
    /// Pooled connections; shared by every clone of this store.
    pool: Arc<SqliteConnectionPool>,
    /// Device ID for this session
    device_id: i32,
}

#[cfg(feature = "whatsapp-web")]
impl RusqliteStore {
    /// Create a new rusqlite-based storage backend
    ///
    /// # Arguments
    ///
    /// * `db_path` - Path to the SQLite database file (will be created if needed)
    pub fn new<P: AsRef<Path>>(db_path: P) -> anyhow::Result<Self> {
        Self::with_read_pool_size(db_path, None)
    }

    /// Create a backend with an explicit read-pool size.
    ///
    /// # Arguments
    ///
    /// * `db_path` - Path to the SQLite database file (will be created if needed)
    /// * `read_pool_size` - Concurrent read connections; `None` derives it from
    ///   the available CPUs (see [`default_read_pool_size`])
    pub fn with_read_pool_size<P: AsRef<Path>>(db_path: P, read_pool_size: Option<usize>) -> anyhow::Result<Self> {
        let db_path = db_path.as_ref().to_string_lossy().to_string();

        // Create parent directory if needed
        if let Some(parent) = Path::new(&db_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        // `reset_transactions_on_release` stays off: every multi-statement
        // write here goes through `rusqlite::Transaction`, which rolls itself
        // back on drop, and this module never issues a bare `BEGIN`.
        let pool = SqliteConnectionPool::builder(Path::new(&db_path))
            .max_readers(read_pool_size.unwrap_or_else(default_read_pool_size))
            .writer_pragmas(SESSION_DB_PRAGMAS)
            .reader_pragmas(SESSION_DB_PRAGMAS)
            .build()?;

        let pool = Arc::new(pool);
        SqliteConnectionPool::publish_metrics(&pool);
        let store = Self {
            db_path: Arc::from(db_path.as_str()),
            pool,
            device_id: 1, // Default device ID
        };

        store.init_schema()?;

        Ok(store)
    }

    /// Current connection-pool occupancy and cumulative wait counters.
    #[must_use]
    pub fn pool_stats(&self) -> WhatsAppStoragePoolStats {
        self.pool.stats()
    }

    /// Await a storage task, turning a lost worker into a storage error rather
    /// than a panic.
    async fn join<T>(handle: tokio::task::JoinHandle<StoreResult<T>>) -> StoreResult<T> {
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(StoreError::Database(format!(
                "WhatsApp session storage task did not complete: {err}"
            ))),
        }
    }

    /// Run a read-only statement on the blocking pool against a pooled read
    /// connection.
    async fn with_read<T, F>(&self, f: F) -> StoreResult<T>
    where
        F: FnOnce(&Connection) -> StoreResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        Self::join(crate::runtime::blocking::spawn_blocking(move || {
            let reader = to_store_err!(pool.read())?;
            let conn = to_store_err!(reader.connection())?;
            f(conn)
        }))
        .await
    }

    /// Run a mutating statement on the blocking pool against the write
    /// connection.
    async fn with_write<T, F>(&self, f: F) -> StoreResult<T>
    where
        F: FnOnce(&mut Connection) -> StoreResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        Self::join(crate::runtime::blocking::spawn_blocking(move || f(&mut pool.write()))).await
    }

    /// Initialize all database tables
    fn init_schema(&self) -> anyhow::Result<()> {
        let conn = self.pool.write();
        to_store_err!(conn.execute_batch(
            "-- Main device table
            CREATE TABLE IF NOT EXISTS device (
                id INTEGER PRIMARY KEY,
                lid TEXT,
                pn TEXT,
                registration_id INTEGER NOT NULL,
                noise_key BLOB NOT NULL,
                identity_key BLOB NOT NULL,
                signed_pre_key BLOB NOT NULL,
                signed_pre_key_id INTEGER NOT NULL,
                signed_pre_key_signature BLOB NOT NULL,
                adv_secret_key BLOB NOT NULL,
                account BLOB,
                push_name TEXT NOT NULL,
                app_version_primary INTEGER NOT NULL,
                app_version_secondary INTEGER NOT NULL,
                app_version_tertiary INTEGER NOT NULL,
                app_version_last_fetched_ms INTEGER NOT NULL,
                edge_routing_info BLOB,
                props_hash TEXT
            );

            -- Signal identity keys
            CREATE TABLE IF NOT EXISTS identities (
                address TEXT NOT NULL,
                key BLOB NOT NULL,
                device_id INTEGER NOT NULL,
                PRIMARY KEY (address, device_id)
            );

            -- Signal protocol sessions
            CREATE TABLE IF NOT EXISTS sessions (
                address TEXT NOT NULL,
                record BLOB NOT NULL,
                device_id INTEGER NOT NULL,
                PRIMARY KEY (address, device_id)
            );

            -- Pre-keys for key exchange
            CREATE TABLE IF NOT EXISTS prekeys (
                id INTEGER NOT NULL,
                key BLOB NOT NULL,
                uploaded INTEGER NOT NULL DEFAULT 0,
                device_id INTEGER NOT NULL,
                PRIMARY KEY (id, device_id)
            );

            -- Signed pre-keys
            CREATE TABLE IF NOT EXISTS signed_prekeys (
                id INTEGER NOT NULL,
                record BLOB NOT NULL,
                device_id INTEGER NOT NULL,
                PRIMARY KEY (id, device_id)
            );

            -- Sender keys for group messaging
            CREATE TABLE IF NOT EXISTS sender_keys (
                address TEXT NOT NULL,
                record BLOB NOT NULL,
                device_id INTEGER NOT NULL,
                PRIMARY KEY (address, device_id)
            );

            -- App state sync keys
            CREATE TABLE IF NOT EXISTS app_state_keys (
                key_id BLOB NOT NULL,
                key_data BLOB NOT NULL,
                device_id INTEGER NOT NULL,
                PRIMARY KEY (key_id, device_id)
            );

            -- App state versions
            CREATE TABLE IF NOT EXISTS app_state_versions (
                name TEXT NOT NULL,
                state_data BLOB NOT NULL,
                device_id INTEGER NOT NULL,
                PRIMARY KEY (name, device_id)
            );

            -- App state mutation MACs
            CREATE TABLE IF NOT EXISTS app_state_mutation_macs (
                name TEXT NOT NULL,
                version INTEGER NOT NULL,
                index_mac BLOB NOT NULL,
                value_mac BLOB NOT NULL,
                device_id INTEGER NOT NULL,
                PRIMARY KEY (name, index_mac, device_id)
            );

            -- LID to phone number mapping
            CREATE TABLE IF NOT EXISTS lid_pn_mapping (
                lid TEXT NOT NULL,
                phone_number TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                learning_source TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                device_id INTEGER NOT NULL,
                PRIMARY KEY (lid, device_id)
            );

            -- SKDM recipients tracking
            CREATE TABLE IF NOT EXISTS skdm_recipients (
                group_jid TEXT NOT NULL,
                device_jid TEXT NOT NULL,
                device_id INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (group_jid, device_jid, device_id)
            );

            -- Device registry for multi-device
            CREATE TABLE IF NOT EXISTS device_registry (
                user_id TEXT NOT NULL,
                devices_json TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                phash TEXT,
                device_id INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (user_id, device_id)
            );

            -- Base keys for collision detection
            CREATE TABLE IF NOT EXISTS base_keys (
                address TEXT NOT NULL,
                message_id TEXT NOT NULL,
                base_key BLOB NOT NULL,
                device_id INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (address, message_id, device_id)
            );

            -- Sender key status for lazy deletion
            CREATE TABLE IF NOT EXISTS sender_key_status (
                group_jid TEXT NOT NULL,
                participant TEXT NOT NULL,
                device_id INTEGER NOT NULL,
                marked_at INTEGER NOT NULL,
                PRIMARY KEY (group_jid, participant, device_id)
            );

            -- Trusted contact tokens
            CREATE TABLE IF NOT EXISTS tc_tokens (
                jid TEXT NOT NULL,
                token BLOB NOT NULL,
                token_timestamp INTEGER NOT NULL,
                sender_timestamp INTEGER,
                device_id INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (jid, device_id)
            );",
        ))?;
        Ok(())
    }
}

#[cfg(feature = "whatsapp-web")]
#[async_trait]
impl SignalStore for RusqliteStore {
    // --- Identity Operations ---

    async fn put_identity(&self, address: &str, key: [u8; 32]) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let address = address.to_owned();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO identities (address, key, device_id)
                 VALUES (?1, ?2, ?3)",
                params![address, key.to_vec(), device_id],
            ))
        })
        .await
    }

    async fn load_identity(&self, address: &str) -> wa_rs_core::store::error::Result<Option<Vec<u8>>> {
        let device_id = self.device_id;
        let address = address.to_owned();
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT key FROM identities WHERE address = ?1 AND device_id = ?2",
                params![address, device_id],
                |row| row.get::<_, Vec<u8>>(0),
            );

            match result {
                Ok(key) => Ok(Some(key)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn delete_identity(&self, address: &str) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let address = address.to_owned();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "DELETE FROM identities WHERE address = ?1 AND device_id = ?2",
                params![address, device_id],
            ))
        })
        .await
    }

    // --- Session Operations ---

    async fn get_session(&self, address: &str) -> wa_rs_core::store::error::Result<Option<Vec<u8>>> {
        let device_id = self.device_id;
        let address = address.to_owned();
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT record FROM sessions WHERE address = ?1 AND device_id = ?2",
                params![address, device_id],
                |row| row.get::<_, Vec<u8>>(0),
            );

            match result {
                Ok(record) => Ok(Some(record)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn put_session(&self, address: &str, session: &[u8]) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let address = address.to_owned();
        let session = session.to_vec();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO sessions (address, record, device_id)
                 VALUES (?1, ?2, ?3)",
                params![address, session, device_id],
            ))
        })
        .await
    }

    async fn delete_session(&self, address: &str) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let address = address.to_owned();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "DELETE FROM sessions WHERE address = ?1 AND device_id = ?2",
                params![address, device_id],
            ))
        })
        .await
    }

    // --- PreKey Operations ---

    async fn store_prekey(&self, id: u32, record: &[u8], uploaded: bool) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let record = record.to_vec();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO prekeys (id, key, uploaded, device_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, record, uploaded, device_id],
            ))
        })
        .await
    }

    async fn load_prekey(&self, id: u32) -> wa_rs_core::store::error::Result<Option<Vec<u8>>> {
        let device_id = self.device_id;
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT key FROM prekeys WHERE id = ?1 AND device_id = ?2",
                params![id, device_id],
                |row| row.get::<_, Vec<u8>>(0),
            );

            match result {
                Ok(key) => Ok(Some(key)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn remove_prekey(&self, id: u32) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "DELETE FROM prekeys WHERE id = ?1 AND device_id = ?2",
                params![id, device_id],
            ))
        })
        .await
    }

    // --- Signed PreKey Operations ---

    async fn store_signed_prekey(&self, id: u32, record: &[u8]) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let record = record.to_vec();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO signed_prekeys (id, record, device_id)
                 VALUES (?1, ?2, ?3)",
                params![id, record, device_id],
            ))
        })
        .await
    }

    async fn load_signed_prekey(&self, id: u32) -> wa_rs_core::store::error::Result<Option<Vec<u8>>> {
        let device_id = self.device_id;
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT record FROM signed_prekeys WHERE id = ?1 AND device_id = ?2",
                params![id, device_id],
                |row| row.get::<_, Vec<u8>>(0),
            );

            match result {
                Ok(record) => Ok(Some(record)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn load_all_signed_prekeys(&self) -> wa_rs_core::store::error::Result<Vec<(u32, Vec<u8>)>> {
        let device_id = self.device_id;
        self.with_read(move |conn: &Connection| {
            let mut stmt = to_store_err!(conn.prepare("SELECT id, record FROM signed_prekeys WHERE device_id = ?1"))?;

            let rows = to_store_err!(stmt.query_map(params![device_id], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
            }))?;

            let mut result = Vec::new();
            for row in rows {
                result.push(to_store_err!(row)?);
            }

            Ok(result)
        })
        .await
    }

    async fn remove_signed_prekey(&self, id: u32) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "DELETE FROM signed_prekeys WHERE id = ?1 AND device_id = ?2",
                params![id, device_id],
            ))
        })
        .await
    }

    // --- Sender Key Operations ---

    async fn put_sender_key(&self, address: &str, record: &[u8]) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let address = address.to_owned();
        let record = record.to_vec();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO sender_keys (address, record, device_id)
                 VALUES (?1, ?2, ?3)",
                params![address, record, device_id],
            ))
        })
        .await
    }

    async fn get_sender_key(&self, address: &str) -> wa_rs_core::store::error::Result<Option<Vec<u8>>> {
        let device_id = self.device_id;
        let address = address.to_owned();
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT record FROM sender_keys WHERE address = ?1 AND device_id = ?2",
                params![address, device_id],
                |row| row.get::<_, Vec<u8>>(0),
            );

            match result {
                Ok(record) => Ok(Some(record)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn delete_sender_key(&self, address: &str) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let address = address.to_owned();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "DELETE FROM sender_keys WHERE address = ?1 AND device_id = ?2",
                params![address, device_id],
            ))
        })
        .await
    }
}

#[cfg(feature = "whatsapp-web")]
#[async_trait]
impl AppSyncStore for RusqliteStore {
    async fn get_sync_key(&self, key_id: &[u8]) -> wa_rs_core::store::error::Result<Option<AppStateSyncKey>> {
        let device_id = self.device_id;
        let key_id = key_id.to_vec();
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT key_data FROM app_state_keys WHERE key_id = ?1 AND device_id = ?2",
                params![key_id, device_id],
                |row| {
                    let key_data: Vec<u8> = row.get(0)?;
                    serde_json::from_slice(&key_data).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
                },
            );

            match result {
                Ok(key) => Ok(Some(key)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn set_sync_key(&self, key_id: &[u8], key: AppStateSyncKey) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let key_id = key_id.to_vec();
        // Serialised before the hand-off so only plain bytes cross the
        // blocking-pool boundary.
        let key_data = to_store_err!(serde_json::to_vec(&key))?;
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO app_state_keys (key_id, key_data, device_id)
                 VALUES (?1, ?2, ?3)",
                params![key_id, key_data, device_id],
            ))
        })
        .await
    }

    async fn get_version(&self, name: &str) -> wa_rs_core::store::error::Result<HashState> {
        let device_id = self.device_id;
        let name = name.to_owned();
        self.with_read(move |conn: &Connection| {
            let state_data: Vec<u8> = to_store_err!(conn.query_row(
                "SELECT state_data FROM app_state_versions WHERE name = ?1 AND device_id = ?2",
                params![name, device_id],
                |row| row.get(0),
            ))?;

            to_store_err!(serde_json::from_slice(&state_data))
        })
        .await
    }

    async fn set_version(&self, name: &str, state: HashState) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let name = name.to_owned();
        let state_data = to_store_err!(serde_json::to_vec(&state))?;
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO app_state_versions (name, state_data, device_id)
                 VALUES (?1, ?2, ?3)",
                params![name, state_data, device_id],
            ))
        })
        .await
    }

    async fn put_mutation_macs(
        &self,
        name: &str,
        version: u64,
        mutations: &[AppStateMutationMAC],
    ) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let name = name.to_owned();
        let version = i64::try_from(version).unwrap_or(i64::MAX);

        let mut encoded = Vec::with_capacity(mutations.len());
        for mutation in mutations {
            let index_mac = to_store_err!(serde_json::to_vec(&mutation.index_mac))?;
            let value_mac = to_store_err!(serde_json::to_vec(&mutation.value_mac))?;
            encoded.push((index_mac, value_mac));
        }

        self.with_write(move |conn: &mut Connection| {
            // One transaction for the batch: a partially applied MAC batch
            // would make the app-state patch unverifiable on the next sync.
            let tx = to_store_err!(conn.transaction())?;
            for (index_mac, value_mac) in encoded {
                to_store_err!(execute: tx.execute(
                    "INSERT OR REPLACE INTO app_state_mutation_macs
                     (name, version, index_mac, value_mac, device_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![name, version, index_mac, value_mac, device_id],
                ))?;
            }
            to_store_err!(tx.commit())
        })
        .await
    }

    async fn get_mutation_mac(
        &self,
        name: &str,
        index_mac: &[u8],
    ) -> wa_rs_core::store::error::Result<Option<Vec<u8>>> {
        let device_id = self.device_id;
        let name = name.to_owned();
        let index_mac_json = to_store_err!(serde_json::to_vec(index_mac))?;

        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT value_mac FROM app_state_mutation_macs
                 WHERE name = ?1 AND index_mac = ?2 AND device_id = ?3",
                params![name, index_mac_json, device_id],
                |row| row.get::<_, Vec<u8>>(0),
            );

            match result {
                Ok(mac) => Ok(Some(mac)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn delete_mutation_macs(&self, name: &str, index_macs: &[Vec<u8>]) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let name = name.to_owned();

        let mut encoded = Vec::with_capacity(index_macs.len());
        for index_mac in index_macs {
            encoded.push(to_store_err!(serde_json::to_vec(index_mac))?);
        }

        self.with_write(move |conn: &mut Connection| {
            let tx = to_store_err!(conn.transaction())?;
            for index_mac_json in encoded {
                to_store_err!(execute: tx.execute(
                    "DELETE FROM app_state_mutation_macs
                     WHERE name = ?1 AND index_mac = ?2 AND device_id = ?3",
                    params![name, index_mac_json, device_id],
                ))?;
            }
            to_store_err!(tx.commit())
        })
        .await
    }
}

#[cfg(feature = "whatsapp-web")]
#[async_trait]
impl ProtocolStore for RusqliteStore {
    // --- SKDM Tracking ---

    async fn get_skdm_recipients(&self, group_jid: &str) -> wa_rs_core::store::error::Result<Vec<Jid>> {
        let device_id = self.device_id;
        let group_jid = group_jid.to_owned();
        self.with_read(move |conn: &Connection| {
            let mut stmt = to_store_err!(
                conn.prepare("SELECT device_jid FROM skdm_recipients WHERE group_jid = ?1 AND device_id = ?2")
            )?;

            let rows = to_store_err!(stmt.query_map(params![group_jid, device_id], |row| { row.get::<_, String>(0) }))?;

            let mut result = Vec::new();
            for row in rows {
                let jid_str = to_store_err!(row)?;
                if let Ok(jid) = jid_str.parse() {
                    result.push(jid);
                }
            }

            Ok(result)
        })
        .await
    }

    async fn add_skdm_recipients(&self, group_jid: &str, device_jids: &[Jid]) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let group_jid = group_jid.to_owned();
        let device_jids: Vec<String> = device_jids.iter().map(ToString::to_string).collect();
        let now = chrono::Utc::now().timestamp();

        self.with_write(move |conn: &mut Connection| {
            let tx = to_store_err!(conn.transaction())?;
            for device_jid in device_jids {
                to_store_err!(execute: tx.execute(
                    "INSERT OR IGNORE INTO skdm_recipients (group_jid, device_jid, device_id, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![group_jid, device_jid, device_id, now],
                ))?;
            }
            to_store_err!(tx.commit())
        })
        .await
    }

    async fn clear_skdm_recipients(&self, group_jid: &str) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let group_jid = group_jid.to_owned();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "DELETE FROM skdm_recipients WHERE group_jid = ?1 AND device_id = ?2",
                params![group_jid, device_id],
            ))
        })
        .await
    }

    // --- LID-PN Mapping ---

    async fn get_lid_mapping(&self, lid: &str) -> wa_rs_core::store::error::Result<Option<LidPnMappingEntry>> {
        let device_id = self.device_id;
        let lid = lid.to_owned();
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT lid, phone_number, created_at, learning_source, updated_at
                 FROM lid_pn_mapping WHERE lid = ?1 AND device_id = ?2",
                params![lid, device_id],
                |row| {
                    Ok(LidPnMappingEntry {
                        lid: row.get(0)?,
                        phone_number: row.get(1)?,
                        created_at: row.get(2)?,
                        learning_source: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            );

            match result {
                Ok(entry) => Ok(Some(entry)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn get_pn_mapping(&self, phone: &str) -> wa_rs_core::store::error::Result<Option<LidPnMappingEntry>> {
        let device_id = self.device_id;
        let phone = phone.to_owned();
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT lid, phone_number, created_at, learning_source, updated_at
                 FROM lid_pn_mapping WHERE phone_number = ?1 AND device_id = ?2
                 ORDER BY updated_at DESC LIMIT 1",
                params![phone, device_id],
                |row| {
                    Ok(LidPnMappingEntry {
                        lid: row.get(0)?,
                        phone_number: row.get(1)?,
                        created_at: row.get(2)?,
                        learning_source: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            );

            match result {
                Ok(entry) => Ok(Some(entry)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn put_lid_mapping(&self, entry: &LidPnMappingEntry) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let entry = entry.clone();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO lid_pn_mapping
                 (lid, phone_number, created_at, learning_source, updated_at, device_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    entry.lid,
                    entry.phone_number,
                    entry.created_at,
                    entry.learning_source,
                    entry.updated_at,
                    device_id,
                ],
            ))
        })
        .await
    }

    async fn get_all_lid_mappings(&self) -> wa_rs_core::store::error::Result<Vec<LidPnMappingEntry>> {
        let device_id = self.device_id;
        self.with_read(move |conn: &Connection| {
            let mut stmt = to_store_err!(conn.prepare(
                "SELECT lid, phone_number, created_at, learning_source, updated_at
                 FROM lid_pn_mapping WHERE device_id = ?1"
            ))?;

            let rows = to_store_err!(stmt.query_map(params![device_id], |row| {
                Ok(LidPnMappingEntry {
                    lid: row.get(0)?,
                    phone_number: row.get(1)?,
                    created_at: row.get(2)?,
                    learning_source: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            }))?;

            let mut result = Vec::new();
            for row in rows {
                result.push(to_store_err!(row)?);
            }

            Ok(result)
        })
        .await
    }

    // --- Base Key Collision Detection ---

    async fn save_base_key(
        &self,
        address: &str,
        message_id: &str,
        base_key: &[u8],
    ) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let address = address.to_owned();
        let message_id = message_id.to_owned();
        let base_key = base_key.to_vec();
        let now = chrono::Utc::now().timestamp();

        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO base_keys (address, message_id, base_key, device_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![address, message_id, base_key, device_id, now],
            ))
        })
        .await
    }

    async fn has_same_base_key(
        &self,
        address: &str,
        message_id: &str,
        current_base_key: &[u8],
    ) -> wa_rs_core::store::error::Result<bool> {
        let device_id = self.device_id;
        let address = address.to_owned();
        let message_id = message_id.to_owned();
        let current_base_key = current_base_key.to_vec();

        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT base_key FROM base_keys
                 WHERE address = ?1 AND message_id = ?2 AND device_id = ?3",
                params![address, message_id, device_id],
                |row| {
                    let saved_key: Vec<u8> = row.get(0)?;
                    Ok(saved_key == current_base_key)
                },
            );

            match result {
                Ok(same) => Ok(same),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn delete_base_key(&self, address: &str, message_id: &str) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let address = address.to_owned();
        let message_id = message_id.to_owned();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "DELETE FROM base_keys WHERE address = ?1 AND message_id = ?2 AND device_id = ?3",
                params![address, message_id, device_id],
            ))
        })
        .await
    }

    // --- Device Registry ---

    async fn update_device_list(&self, record: DeviceListRecord) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let devices_json = to_store_err!(serde_json::to_string(&record.devices))?;
        let now = chrono::Utc::now().timestamp();

        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO device_registry
                 (user_id, devices_json, timestamp, phash, device_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    record.user,
                    devices_json,
                    record.timestamp,
                    record.phash,
                    device_id,
                    now,
                ],
            ))
        })
        .await
    }

    async fn get_devices(&self, user: &str) -> wa_rs_core::store::error::Result<Option<DeviceListRecord>> {
        let device_id = self.device_id;
        let user = user.to_owned();
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT user_id, devices_json, timestamp, phash
                 FROM device_registry WHERE user_id = ?1 AND device_id = ?2",
                params![user, device_id],
                |row| {
                    // Helper to convert errors to rusqlite::Error
                    fn to_rusqlite_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> rusqlite::Error {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(e))
                    }

                    let devices_json: String = row.get(1)?;
                    let devices: Vec<DeviceInfo> = serde_json::from_str(&devices_json).map_err(to_rusqlite_err)?;
                    Ok(DeviceListRecord {
                        user: row.get(0)?,
                        devices,
                        timestamp: row.get(2)?,
                        phash: row.get(3)?,
                    })
                },
            );

            match result {
                Ok(record) => Ok(Some(record)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    // --- Sender Key Status (Lazy Deletion) ---

    async fn mark_forget_sender_key(&self, group_jid: &str, participant: &str) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let group_jid = group_jid.to_owned();
        let participant = participant.to_owned();
        let now = chrono::Utc::now().timestamp();

        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO sender_key_status (group_jid, participant, device_id, marked_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![group_jid, participant, device_id, now],
            ))
        })
        .await
    }

    async fn consume_forget_marks(&self, group_jid: &str) -> wa_rs_core::store::error::Result<Vec<String>> {
        let device_id = self.device_id;
        let group_jid = group_jid.to_owned();

        // Consuming is read-then-delete, so it runs on the writer inside a
        // transaction: split across two connections a concurrent caller could
        // observe the same marks and forget a sender key twice.
        self.with_write(move |conn: &mut Connection| {
            let tx = to_store_err!(conn.transaction())?;

            let mut result = Vec::new();
            {
                let mut stmt = to_store_err!(tx.prepare(
                    "SELECT participant FROM sender_key_status
                     WHERE group_jid = ?1 AND device_id = ?2"
                ))?;

                let rows =
                    to_store_err!(stmt.query_map(params![group_jid, device_id], |row| { row.get::<_, String>(0) }))?;

                for row in rows {
                    result.push(to_store_err!(row)?);
                }
            }

            // Delete the marks after consuming them
            to_store_err!(execute: tx.execute(
                "DELETE FROM sender_key_status WHERE group_jid = ?1 AND device_id = ?2",
                params![group_jid, device_id],
            ))?;
            to_store_err!(tx.commit())?;

            Ok(result)
        })
        .await
    }

    // --- TcToken Storage ---

    async fn get_tc_token(&self, jid: &str) -> wa_rs_core::store::error::Result<Option<TcTokenEntry>> {
        let device_id = self.device_id;
        let jid = jid.to_owned();
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row(
                "SELECT token, token_timestamp, sender_timestamp FROM tc_tokens
                 WHERE jid = ?1 AND device_id = ?2",
                params![jid, device_id],
                |row| {
                    Ok(TcTokenEntry {
                        token: row.get(0)?,
                        token_timestamp: row.get(1)?,
                        sender_timestamp: row.get(2)?,
                    })
                },
            );

            match result {
                Ok(entry) => Ok(Some(entry)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn put_tc_token(&self, jid: &str, entry: &TcTokenEntry) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let jid = jid.to_owned();
        let entry = entry.clone();
        let now = chrono::Utc::now().timestamp();

        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO tc_tokens
                 (jid, token, token_timestamp, sender_timestamp, device_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    jid,
                    entry.token,
                    entry.token_timestamp,
                    entry.sender_timestamp,
                    device_id,
                    now,
                ],
            ))
        })
        .await
    }

    async fn delete_tc_token(&self, jid: &str) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;
        let jid = jid.to_owned();
        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "DELETE FROM tc_tokens WHERE jid = ?1 AND device_id = ?2",
                params![jid, device_id],
            ))
        })
        .await
    }

    async fn get_all_tc_token_jids(&self) -> wa_rs_core::store::error::Result<Vec<String>> {
        let device_id = self.device_id;
        self.with_read(move |conn: &Connection| {
            let mut stmt = to_store_err!(conn.prepare("SELECT jid FROM tc_tokens WHERE device_id = ?1"))?;

            let rows = to_store_err!(stmt.query_map(params![device_id], |row| { row.get::<_, String>(0) }))?;

            let mut result = Vec::new();
            for row in rows {
                result.push(to_store_err!(row)?);
            }

            Ok(result)
        })
        .await
    }

    async fn delete_expired_tc_tokens(&self, cutoff_timestamp: i64) -> wa_rs_core::store::error::Result<u32> {
        let device_id = self.device_id;
        self.with_write(move |conn: &mut Connection| {
            let deleted = to_store_err!(conn.execute(
                "DELETE FROM tc_tokens WHERE token_timestamp < ?1 AND device_id = ?2",
                params![cutoff_timestamp, device_id],
            ))?;

            u32::try_from(deleted)
                .map_err(|_| StoreError::Database(format!("Affected row count overflowed u32: {deleted}")))
        })
        .await
    }
}

#[cfg(feature = "whatsapp-web")]
#[async_trait]
impl DeviceStoreTrait for RusqliteStore {
    async fn save(&self, device: &CoreDevice) -> wa_rs_core::store::error::Result<()> {
        let device_id = self.device_id;

        // Serialize KeyPairs to bytes
        let noise_key = {
            let mut bytes = Vec::new();
            let priv_key = device.noise_key.private_key.serialize();
            bytes.extend_from_slice(priv_key.as_slice());
            bytes.extend_from_slice(device.noise_key.public_key.public_key_bytes());
            bytes
        };

        let identity_key = {
            let mut bytes = Vec::new();
            let priv_key = device.identity_key.private_key.serialize();
            bytes.extend_from_slice(priv_key.as_slice());
            bytes.extend_from_slice(device.identity_key.public_key.public_key_bytes());
            bytes
        };

        let signed_pre_key = {
            let mut bytes = Vec::new();
            let priv_key = device.signed_pre_key.private_key.serialize();
            bytes.extend_from_slice(priv_key.as_slice());
            bytes.extend_from_slice(device.signed_pre_key.public_key.public_key_bytes());
            bytes
        };

        let account = device.account.as_ref().map(Message::encode_to_vec);
        let lid = device.lid.as_ref().map(ToString::to_string);
        let pn = device.pn.as_ref().map(ToString::to_string);
        let registration_id = device.registration_id;
        let signed_pre_key_id = device.signed_pre_key_id;
        let signed_pre_key_signature = device.signed_pre_key_signature.to_vec();
        let adv_secret_key = device.adv_secret_key.to_vec();
        let push_name = device.push_name.clone();
        let app_version_primary = device.app_version_primary;
        let app_version_secondary = device.app_version_secondary;
        let app_version_tertiary = device.app_version_tertiary;
        let app_version_last_fetched_ms = device.app_version_last_fetched_ms;
        let edge_routing_info = device.edge_routing_info.clone();
        let props_hash = device.props_hash.clone();

        self.with_write(move |conn: &mut Connection| {
            to_store_err!(execute: conn.execute(
                "INSERT OR REPLACE INTO device (
                    id, lid, pn, registration_id, noise_key, identity_key,
                    signed_pre_key, signed_pre_key_id, signed_pre_key_signature,
                    adv_secret_key, account, push_name, app_version_primary,
                    app_version_secondary, app_version_tertiary, app_version_last_fetched_ms,
                    edge_routing_info, props_hash
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                params![
                    device_id,
                    lid,
                    pn,
                    registration_id,
                    noise_key,
                    identity_key,
                    signed_pre_key,
                    signed_pre_key_id,
                    signed_pre_key_signature,
                    adv_secret_key,
                    account,
                    push_name,
                    app_version_primary,
                    app_version_secondary,
                    app_version_tertiary,
                    app_version_last_fetched_ms,
                    edge_routing_info,
                    props_hash,
                ],
            ))
        })
        .await
    }

    async fn load(&self) -> wa_rs_core::store::error::Result<Option<CoreDevice>> {
        let device_id = self.device_id;
        self.with_read(move |conn: &Connection| {
            let result = conn.query_row("SELECT * FROM device WHERE id = ?1", params![device_id], |row| {
                // Helper to convert errors to rusqlite::Error
                fn to_rusqlite_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> rusqlite::Error {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
                }

                // Deserialize KeyPairs from bytes (64 bytes each)
                let noise_key_bytes: Vec<u8> = row.get("noise_key")?;
                let identity_key_bytes: Vec<u8> = row.get("identity_key")?;
                let signed_pre_key_bytes: Vec<u8> = row.get("signed_pre_key")?;

                if noise_key_bytes.len() != 64 || identity_key_bytes.len() != 64 || signed_pre_key_bytes.len() != 64 {
                    return Err(rusqlite::Error::InvalidParameterName("key_pair".into()));
                }

                use wa_rs_core::libsignal::protocol::{KeyPair, PrivateKey, PublicKey};

                // Length validated as 64 above; slicing is safe.
                #[allow(clippy::indexing_slicing)]
                let noise_key = KeyPair::new(
                    PublicKey::from_djb_public_key_bytes(&noise_key_bytes[32..64]).map_err(to_rusqlite_err)?,
                    PrivateKey::deserialize(&noise_key_bytes[0..32]).map_err(to_rusqlite_err)?,
                );

                #[allow(clippy::indexing_slicing)]
                let identity_key = KeyPair::new(
                    PublicKey::from_djb_public_key_bytes(&identity_key_bytes[32..64]).map_err(to_rusqlite_err)?,
                    PrivateKey::deserialize(&identity_key_bytes[0..32]).map_err(to_rusqlite_err)?,
                );

                #[allow(clippy::indexing_slicing)]
                let signed_pre_key = KeyPair::new(
                    PublicKey::from_djb_public_key_bytes(&signed_pre_key_bytes[32..64]).map_err(to_rusqlite_err)?,
                    PrivateKey::deserialize(&signed_pre_key_bytes[0..32]).map_err(to_rusqlite_err)?,
                );

                let lid_str: Option<String> = row.get("lid")?;
                let pn_str: Option<String> = row.get("pn")?;
                let signature_bytes: Vec<u8> = row.get("signed_pre_key_signature")?;
                let adv_secret_bytes: Vec<u8> = row.get("adv_secret_key")?;
                let account_bytes: Option<Vec<u8>> = row.get("account")?;

                let mut signature = [0u8; 64];
                let mut adv_secret = [0u8; 32];
                if signature_bytes.len() != 64 {
                    return Err(rusqlite::Error::InvalidParameterName(format!(
                        "signed_pre_key_signature has invalid length ({}, expected 64)",
                        signature_bytes.len()
                    )));
                }
                if adv_secret_bytes.len() != 32 {
                    return Err(rusqlite::Error::InvalidParameterName(format!(
                        "adv_secret_key has invalid length ({}, expected 32)",
                        adv_secret_bytes.len()
                    )));
                }
                signature.copy_from_slice(&signature_bytes);
                adv_secret.copy_from_slice(&adv_secret_bytes);

                let account = if let Some(bytes) = account_bytes {
                    Some(wa_rs_proto::whatsapp::AdvSignedDeviceIdentity::decode(&*bytes).map_err(to_rusqlite_err)?)
                } else {
                    None
                };

                Ok(CoreDevice {
                    lid: lid_str.and_then(|s| s.parse().ok()),
                    pn: pn_str.and_then(|s| s.parse().ok()),
                    registration_id: row.get("registration_id")?,
                    noise_key,
                    identity_key,
                    signed_pre_key,
                    signed_pre_key_id: row.get("signed_pre_key_id")?,
                    signed_pre_key_signature: signature,
                    adv_secret_key: adv_secret,
                    account,
                    push_name: row.get("push_name")?,
                    app_version_primary: row.get("app_version_primary")?,
                    app_version_secondary: row.get("app_version_secondary")?,
                    app_version_tertiary: row.get("app_version_tertiary")?,
                    app_version_last_fetched_ms: row.get("app_version_last_fetched_ms")?,
                    edge_routing_info: row.get("edge_routing_info")?,
                    props_hash: row.get("props_hash")?,
                    ..Default::default()
                })
            });

            match result {
                Ok(device) => Ok(Some(device)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(StoreError::Database(e.to_string())),
            }
        })
        .await
    }

    async fn exists(&self) -> wa_rs_core::store::error::Result<bool> {
        let device_id = self.device_id;
        self.with_read(move |conn: &Connection| {
            let count: i64 = to_store_err!(conn.query_row(
                "SELECT COUNT(*) FROM device WHERE id = ?1",
                params![device_id],
                |row| row.get(0),
            ))?;

            Ok(count > 0)
        })
        .await
    }

    async fn create(&self) -> wa_rs_core::store::error::Result<i32> {
        // Device already created in constructor, just return the ID
        Ok(self.device_id)
    }

    async fn snapshot_db(&self, name: &str, extra_content: Option<&[u8]>) -> wa_rs_core::store::error::Result<()> {
        // Create a snapshot by copying the database file
        let db_path = Arc::clone(&self.db_path);
        let snapshot_path = format!("{db_path}.snapshot.{name}");
        let extra_content = extra_content.map(<[u8]>::to_vec);

        self.with_write(move |conn: &mut Connection| {
            // Fold the write-ahead log back into the main database file first.
            // In WAL mode the newest committed pages live in the `-wal`
            // sidecar, so copying the main file alone would silently produce a
            // snapshot missing the most recent writes. Runs while this task
            // holds the writer, so no write can land between checkpoint and
            // copy.
            let (busy, _, _): (i64, i64, i64) = to_store_err!(conn.query_row(
                "PRAGMA wal_checkpoint(FULL)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ))?;
            if busy != 0 {
                tracing::warn!(
                    snapshot = %snapshot_path,
                    "WAL checkpoint could not complete before snapshot; snapshot may lag the live database"
                );
            }

            to_store_err!(std::fs::copy(&*db_path, &snapshot_path))?;

            // If extra_content is provided, save it alongside
            if let Some(content) = extra_content {
                let content_path = format!("{snapshot_path}.extra");
                to_store_err!(std::fs::write(&content_path, content))?;
            }

            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "whatsapp-web")]
    use std::sync::atomic::Ordering;
    #[cfg(feature = "whatsapp-web")]
    use std::time::{Duration, Instant};
    #[cfg(feature = "whatsapp-web")]
    use wa_rs_core::store::traits::{LidPnMappingEntry, ProtocolStore, SignalStore, TcTokenEntry};

    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn rusqlite_store_creates_database() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = RusqliteStore::new(tmp.path()).unwrap();
        assert_eq!(store.device_id, 1);
    }

    #[cfg(feature = "whatsapp-web")]
    #[tokio::test]
    async fn lid_mapping_round_trip_preserves_learning_source_and_updated_at() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = RusqliteStore::new(tmp.path()).unwrap();
        let entry = LidPnMappingEntry {
            lid: "100000012345678".to_string(),
            phone_number: "15551234567".to_string(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_100,
            learning_source: "usync".to_string(),
        };

        ProtocolStore::put_lid_mapping(&store, &entry).await.unwrap();

        let loaded = ProtocolStore::get_lid_mapping(&store, &entry.lid)
            .await
            .unwrap()
            .expect("expected lid mapping to be present");
        assert_eq!(loaded.learning_source, entry.learning_source);
        assert_eq!(loaded.updated_at, entry.updated_at);

        let loaded_by_pn = ProtocolStore::get_pn_mapping(&store, &entry.phone_number)
            .await
            .unwrap()
            .expect("expected pn mapping to be present");
        assert_eq!(loaded_by_pn.learning_source, entry.learning_source);
        assert_eq!(loaded_by_pn.updated_at, entry.updated_at);
    }

    #[cfg(feature = "whatsapp-web")]
    #[tokio::test]
    async fn delete_expired_tc_tokens_returns_deleted_row_count() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = RusqliteStore::new(tmp.path()).unwrap();

        let expired = TcTokenEntry {
            token: vec![1, 2, 3],
            token_timestamp: 10,
            sender_timestamp: None,
        };
        let fresh = TcTokenEntry {
            token: vec![4, 5, 6],
            token_timestamp: 1000,
            sender_timestamp: Some(1000),
        };

        ProtocolStore::put_tc_token(&store, "15550000001", &expired)
            .await
            .unwrap();
        ProtocolStore::put_tc_token(&store, "15550000002", &fresh)
            .await
            .unwrap();

        let deleted = ProtocolStore::delete_expired_tc_tokens(&store, 100).await.unwrap();
        assert_eq!(deleted, 1);
        assert!(
            ProtocolStore::get_tc_token(&store, "15550000001")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            ProtocolStore::get_tc_token(&store, "15550000002")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn read_pool_size_defaults_to_the_derived_value_and_honours_an_explicit_one() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let derived = RusqliteStore::new(tmp.path()).unwrap();
        assert_eq!(
            derived.pool_stats().max_readers,
            default_read_pool_size(),
            "an unset channels_config.whatsapp.read_pool_size derives from the CPUs"
        );

        let explicit = tempfile::NamedTempFile::new().unwrap();
        let sized = RusqliteStore::with_read_pool_size(explicit.path(), Some(3)).unwrap();
        assert_eq!(
            sized.pool_stats().max_readers,
            3,
            "the WhatsApp key sizes this database on its own"
        );
    }

    #[cfg(feature = "whatsapp-web")]
    #[tokio::test]
    async fn consume_forget_marks_is_atomic_across_concurrent_consumers() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = RusqliteStore::new(tmp.path()).unwrap();

        for i in 0..32 {
            ProtocolStore::mark_forget_sender_key(&store, "group@g.us", &format!("participant-{i}"))
                .await
                .unwrap();
        }

        // Two consumers race for the same marks. Because read-then-delete runs
        // in one transaction on the writer, every mark is handed to exactly one
        // of them: duplicates here would mean a sender key gets forgotten
        // twice.
        let a = store.clone();
        let b = store.clone();
        let (first, second) = tokio::join!(
            tokio::spawn(async move { ProtocolStore::consume_forget_marks(&a, "group@g.us").await }),
            tokio::spawn(async move { ProtocolStore::consume_forget_marks(&b, "group@g.us").await }),
        );

        let mut all = first.unwrap().unwrap();
        all.extend(second.unwrap().unwrap());
        all.sort();
        let unique = all.len();
        all.dedup();
        assert_eq!(all.len(), unique, "a mark was consumed twice");
        assert_eq!(all.len(), 32, "every mark must be consumed exactly once");
    }

    /// Concurrent storage tasks used by the starvation probe.
    #[cfg(feature = "whatsapp-web")]
    const LOAD_TASKS: usize = 64;
    /// Store round trips (one write plus one read) each load task performs.
    #[cfg(feature = "whatsapp-web")]
    const OPS_PER_TASK: usize = 60;
    /// Session-record size; roughly the size of a real Signal session record,
    /// large enough that each write is a genuine page write rather than a
    /// no-op the page cache absorbs.
    #[cfg(feature = "whatsapp-web")]
    const LOAD_RECORD_BYTES: usize = 4096;
    /// Heartbeat period. The probe asserts on how far actual ticks drift from
    /// this, which is the observable symptom of a starved worker thread.
    #[cfg(feature = "whatsapp-web")]
    const HEARTBEAT_PERIOD: Duration = Duration::from_millis(5);

    #[cfg(feature = "whatsapp-web")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn heavy_storage_load_does_not_starve_async_worker_threads() {
        // The runtime deliberately has only two worker threads. If any SQL ran
        // inline in the `async fn` bodies, `LOAD_TASKS` concurrent callers
        // would occupy both workers for the whole run and the pure-async
        // heartbeat below could not tick at all. With every statement offloaded
        // to the blocking pool, the workers stay free and the heartbeat keeps
        // its schedule.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = RusqliteStore::new(tmp.path()).unwrap();

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let heartbeat_stop = Arc::clone(&stop);
        let heartbeat = tokio::spawn(async move {
            let mut ticks = 0_u32;
            let mut worst_gap = Duration::ZERO;
            let mut last = Instant::now();
            while !heartbeat_stop.load(Ordering::Relaxed) {
                tokio::time::sleep(HEARTBEAT_PERIOD).await;
                let now = Instant::now();
                worst_gap = worst_gap.max(now.duration_since(last));
                last = now;
                ticks += 1;
            }
            (ticks, worst_gap)
        });

        let mut load = Vec::with_capacity(LOAD_TASKS);
        for task in 0..LOAD_TASKS {
            let store = store.clone();
            load.push(tokio::spawn(async move {
                for op in 0..OPS_PER_TASK {
                    let address = format!("task-{task}-{op}@s.whatsapp.net");
                    SignalStore::put_session(&store, &address, &[1_u8; LOAD_RECORD_BYTES])
                        .await
                        .unwrap();
                    let loaded = SignalStore::get_session(&store, &address).await.unwrap();
                    assert_eq!(loaded.map(|record| record.len()), Some(LOAD_RECORD_BYTES));
                }
            }));
        }
        for task in load {
            task.await.unwrap();
        }

        stop.store(true, Ordering::Relaxed);
        let (ticks, worst_gap) = heartbeat.await.unwrap();

        let stats = store.pool_stats();
        assert!(
            stats.reader_connects > 1,
            "reads must run on several pooled connections, not one: {stats:?}"
        );
        assert_eq!(stats.readers_in_use, 0, "read slots leaked: {stats:?}");
        assert!(
            stats.reader_acquisitions >= (LOAD_TASKS * OPS_PER_TASK) as u64,
            "every read must check a connection out of the pool: {stats:?}"
        );

        // Measured on the reference machine: with every statement offloaded the
        // heartbeat ticks 64 times with a worst gap of ~7ms against a 5ms
        // period. Running the same load with the statements inline in the
        // `async fn` bodies (the pre-fix shape) lets it tick twice, with a
        // worst gap of ~267ms — the heartbeat is frozen for the whole run. The
        // bounds below sit far enough from both to survive a slow or loaded
        // machine while still rejecting a regression back to inline SQL.
        assert!(
            ticks >= 20,
            "async heartbeat ticked only {ticks} times during the storage load; worker threads were starved (stats: {stats:?})"
        );
        assert!(
            worst_gap < Duration::from_millis(200),
            "async heartbeat stalled for {worst_gap:?}; worker threads were starved by storage work (stats: {stats:?})"
        );
    }
}
