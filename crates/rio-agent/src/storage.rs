//! Raft storage implementation using RocksDB
//!
//! Provides persistent storage for Raft log entries and state machine.
//! Separates concerns into LogStore (Raft log) and StateMachineStore (application state).

use anyhow::{Context, Result};
use camino::Utf8Path;
use openraft::storage::{LogFlushed, LogState, RaftLogStorage, RaftStateMachine, Snapshot};
use openraft::{
    AnyError, Entry, EntryPayload, ErrorSubject, ErrorVerb, LogId, RaftLogReader,
    RaftSnapshotBuilder, SnapshotMeta, StorageError, StorageIOError, StoredMembership, Vote,
};
use parking_lot::RwLock;
use rocksdb::{ColumnFamilyDescriptor, DB, Options};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::Arc;

use crate::state_machine::{ClusterState, Node, RaftCommand, RaftResponse};

// NodeId is now the agent's UUID (not truncated to u64)
pub type NodeId = uuid::Uuid;

// Define our Raft type configuration using the macro
openraft::declare_raft_types!(
    pub TypeConfig:
        D = RaftCommand,
        R = RaftResponse,
        Node = Node,
        NodeId = NodeId,
);

/// Snapshot data stored in RocksDB
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSnapshot {
    pub meta: SnapshotMeta<NodeId, Node>,
    pub data: Vec<u8>,
}

/// Column family names
const CF_LOGS: &str = "logs";
const CF_STORE: &str = "store";

/// Convert log index to bytes for RocksDB key
fn id_to_bin(id: u64) -> Vec<u8> {
    id.to_be_bytes().to_vec()
}

/// Convert bytes back to log index
fn bin_to_id(buf: &[u8]) -> u64 {
    u64::from_be_bytes(
        buf.try_into()
            .expect("log index must be 8 bytes (created by id_to_bin)"),
    )
}

/// Raft log storage backed by RocksDB
#[derive(Debug, Clone)]
pub struct LogStore {
    db: Arc<DB>,
}

impl LogStore {
    fn store(&self) -> &rocksdb::ColumnFamily {
        self.db
            .cf_handle(CF_STORE)
            .expect("CF_STORE column family must exist (created at DB init)")
    }

    fn logs(&self) -> &rocksdb::ColumnFamily {
        self.db
            .cf_handle(CF_LOGS)
            .expect("CF_LOGS column family must exist (created at DB init)")
    }

    fn flush(
        &self,
        subject: ErrorSubject<NodeId>,
        verb: ErrorVerb,
    ) -> Result<(), Box<StorageIOError<NodeId>>> {
        self.db
            .flush_wal(true)
            .map_err(|e| Box::new(StorageIOError::new(subject, verb, AnyError::new(&e))))?;
        Ok(())
    }

    fn get_last_purged_(&self) -> Result<Option<LogId<NodeId>>, Box<StorageError<NodeId>>> {
        Ok(self
            .db
            .get_cf(self.store(), b"last_purged_log_id")
            .map_err(|e| {
                Box::new(StorageError::IO {
                    source: StorageIOError::read(&e),
                })
            })?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }

    fn set_last_purged_(&self, log_id: LogId<NodeId>) -> Result<(), Box<StorageError<NodeId>>> {
        self.db
            .put_cf(
                self.store(),
                b"last_purged_log_id",
                serde_json::to_vec(&log_id)
                    .expect("LogId serialization cannot fail")
                    .as_slice(),
            )
            .map_err(|e| {
                Box::new(StorageError::IO {
                    source: StorageIOError::write(&e),
                })
            })?;

        self.flush(ErrorSubject::Store, ErrorVerb::Write)
            .map_err(|e| Box::new(StorageError::IO { source: *e }))?;
        Ok(())
    }

    fn set_committed_(
        &self,
        committed: &Option<LogId<NodeId>>,
    ) -> Result<(), Box<StorageError<NodeId>>> {
        let json = serde_json::to_vec(committed).expect("Option<LogId> serialization cannot fail");
        self.db
            .put_cf(self.store(), b"committed", json)
            .map_err(|e| {
                Box::new(StorageError::IO {
                    source: StorageIOError::write(&e),
                })
            })?;
        self.flush(ErrorSubject::Store, ErrorVerb::Write)
            .map_err(|e| Box::new(StorageError::IO { source: *e }))?;
        Ok(())
    }

    fn get_committed_(&self) -> Result<Option<LogId<NodeId>>, Box<StorageError<NodeId>>> {
        Ok(self
            .db
            .get_cf(self.store(), b"committed")
            .map_err(|e| {
                Box::new(StorageError::IO {
                    source: StorageIOError::read(&e),
                })
            })?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }

    fn set_vote_(&self, vote: &Vote<NodeId>) -> Result<(), Box<StorageError<NodeId>>> {
        self.db
            .put_cf(
                self.store(),
                b"vote",
                serde_json::to_vec(vote).expect("Vote serialization cannot fail"),
            )
            .map_err(|e| {
                Box::new(StorageError::IO {
                    source: StorageIOError::write_vote(&e),
                })
            })?;
        self.flush(ErrorSubject::Vote, ErrorVerb::Write)
            .map_err(|e| Box::new(StorageError::IO { source: *e }))?;
        Ok(())
    }

    fn get_vote_(&self) -> Result<Option<Vote<NodeId>>, Box<StorageError<NodeId>>> {
        Ok(self
            .db
            .get_cf(self.store(), b"vote")
            .map_err(|e| {
                Box::new(StorageError::IO {
                    source: StorageIOError::read_vote(&e),
                })
            })?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        let start = match range.start_bound() {
            std::ops::Bound::Included(x) => id_to_bin(*x),
            std::ops::Bound::Excluded(x) => id_to_bin(*x + 1),
            std::ops::Bound::Unbounded => id_to_bin(0),
        };

        self.db
            .iterator_cf(
                self.logs(),
                rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
            )
            .map(|res| {
                let (id, val) = res.expect("RocksDB iterator should not fail");
                let entry: Result<Entry<TypeConfig>, StorageError<NodeId>> =
                    serde_json::from_slice(&val).map_err(|e| StorageError::IO {
                        source: StorageIOError::read_logs(&e),
                    });
                let id = bin_to_id(&id);
                assert_eq!(Ok(id), entry.as_ref().map(|e| e.log_id.index));
                (id, entry)
            })
            .take_while(|(id, _)| range.contains(id))
            .map(|x| x.1)
            .collect()
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        let last = self
            .db
            .iterator_cf(self.logs(), rocksdb::IteratorMode::End)
            .next()
            .and_then(|res| {
                let (_, ent) = res.expect("RocksDB iterator should not fail");
                Some(
                    serde_json::from_slice::<Entry<TypeConfig>>(&ent)
                        .ok()?
                        .log_id,
                )
            });

        let last_purged_log_id = self.get_last_purged_().map_err(|e| *e)?;

        let last_log_id = match last {
            None => last_purged_log_id,
            Some(x) => Some(x),
        };

        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<NodeId>>,
    ) -> Result<(), StorageError<NodeId>> {
        self.set_committed_(&committed).map_err(|e| *e)?;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        self.get_committed_().map_err(|e| *e)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.set_vote_(vote).map_err(|e| *e)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        self.get_vote_().map_err(|e| *e)
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        for entry in entries {
            let id = id_to_bin(entry.log_id.index);
            self.db
                .put_cf(
                    self.logs(),
                    id,
                    serde_json::to_vec(&entry).map_err(|e| StorageIOError::write_logs(&e))?,
                )
                .map_err(|e| StorageIOError::write_logs(&e))?;
        }

        callback.log_io_completed(Ok(()));
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        tracing::debug!("truncate logs >= {:?}", log_id);

        let from = id_to_bin(log_id.index);
        let to = id_to_bin(u64::MAX);
        self.db
            .delete_range_cf(self.logs(), &from, &to)
            .map_err(|e| StorageIOError::write_logs(&e).into())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        tracing::debug!("purge logs <= {:?}", log_id);

        self.set_last_purged_(log_id).map_err(|e| *e)?;
        let from = id_to_bin(0);
        let to = id_to_bin(log_id.index + 1);
        self.db
            .delete_range_cf(self.logs(), &from, &to)
            .map_err(|e| StorageIOError::write_logs(&e).into())
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }
}

/// State machine data
#[derive(Debug, Clone)]
pub struct StateMachineData {
    pub last_applied_log_id: Option<LogId<NodeId>>,
    pub last_membership: StoredMembership<NodeId, Node>,
    /// Cluster state managed by Raft
    pub cluster: ClusterState,
}

/// Inner state machine store structure
#[derive(Debug)]
pub struct StateMachineStoreInner {
    pub data: RwLock<StateMachineData>,
    snapshot_idx: RwLock<u64>,
    db: Arc<DB>,
}

/// Raft state machine store backed by RocksDB
/// Wrapped in Arc so all clones share the same underlying data
pub type StateMachineStore = Arc<StateMachineStoreInner>;

impl StateMachineStoreInner {
    async fn new(db: Arc<DB>) -> Result<StateMachineStore, StorageError<NodeId>> {
        let sm = Arc::new(Self {
            data: RwLock::new(StateMachineData {
                last_applied_log_id: None,
                last_membership: Default::default(),
                cluster: ClusterState::default(),
            }),
            snapshot_idx: RwLock::new(0),
            db,
        });

        let snapshot = sm.get_current_snapshot_().map_err(|e| *e)?;
        if let Some(snap) = snapshot {
            sm.update_state_machine_(snap).await?;
        }

        Ok(sm)
    }

    fn store(&self) -> &rocksdb::ColumnFamily {
        self.db
            .cf_handle(CF_STORE)
            .expect("CF_STORE column family must exist (created at DB init)")
    }

    fn flush(
        &self,
        subject: ErrorSubject<NodeId>,
        verb: ErrorVerb,
    ) -> Result<(), Box<StorageIOError<NodeId>>> {
        self.db
            .flush_wal(true)
            .map_err(|e| Box::new(StorageIOError::new(subject, verb, AnyError::new(&e))))?;
        Ok(())
    }

    fn get_current_snapshot_(&self) -> Result<Option<StoredSnapshot>, Box<StorageError<NodeId>>> {
        Ok(self
            .db
            .get_cf(self.store(), b"snapshot")
            .map_err(|e| {
                Box::new(StorageError::IO {
                    source: StorageIOError::read(&e),
                })
            })?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }

    fn set_current_snapshot_(&self, snap: StoredSnapshot) -> Result<(), Box<StorageError<NodeId>>> {
        self.db
            .put_cf(
                self.store(),
                b"snapshot",
                serde_json::to_vec(&snap)
                    .expect("StoredSnapshot serialization cannot fail")
                    .as_slice(),
            )
            .map_err(|e| {
                Box::new(StorageError::IO {
                    source: StorageIOError::write_snapshot(Some(snap.meta.signature()), &e),
                })
            })?;
        self.flush(
            ErrorSubject::Snapshot(Some(snap.meta.signature())),
            ErrorVerb::Write,
        )
        .map_err(|e| Box::new(StorageError::IO { source: *e }))?;
        Ok(())
    }

    async fn update_state_machine_(
        &self,
        snapshot: StoredSnapshot,
    ) -> Result<(), StorageError<NodeId>> {
        // Phase 2: Will deserialize actual state machine data
        let mut data = self.data.write();
        data.last_applied_log_id = snapshot.meta.last_log_id;
        data.last_membership = snapshot.meta.last_membership.clone();
        Ok(())
    }
}

// Implement traits on Arc<StateMachineStoreInner> so Raft can use it
impl RaftSnapshotBuilder<TypeConfig> for Arc<StateMachineStoreInner> {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let data_read = self.data.read();
        let last_applied_log = data_read.last_applied_log_id;
        let last_membership = data_read.last_membership.clone();
        drop(data_read);

        // Phase 2: Will serialize actual state machine data
        let data = vec![];

        let snapshot_idx = self.snapshot_idx.write();
        let snapshot_id = if let Some(last) = last_applied_log {
            format!("{}-{}-{}", last.leader_id, last.index, *snapshot_idx)
        } else {
            format!("--{}", *snapshot_idx)
        };

        let meta = SnapshotMeta {
            last_log_id: last_applied_log,
            last_membership,
            snapshot_id,
        };

        let snapshot = StoredSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        };

        self.set_current_snapshot_(snapshot).map_err(|e| *e)?;

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftStateMachine<TypeConfig> for Arc<StateMachineStoreInner> {
    type SnapshotBuilder = Arc<StateMachineStoreInner>;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, Node>), StorageError<NodeId>> {
        let data = self.data.read();
        Ok((data.last_applied_log_id, data.last_membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<RaftResponse>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut replies = vec![];
        let mut data = self.data.write();

        for entry in entries {
            data.last_applied_log_id = Some(entry.log_id);

            let response = match entry.payload {
                EntryPayload::Blank => RaftResponse::InternalOp,
                EntryPayload::Normal(cmd) => {
                    // Apply command to cluster state
                    data.cluster.apply(cmd)
                }
                EntryPayload::Membership(mem) => {
                    data.last_membership = StoredMembership::new(Some(entry.log_id), mem);
                    RaftResponse::InternalOp
                }
            };

            replies.push(response);
        }

        Ok(replies)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        *self.snapshot_idx.write() += 1;
        // Return a clone of the Arc (shares the same underlying data)
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, Node>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<NodeId>> {
        let new_snapshot = StoredSnapshot {
            meta: meta.clone(),
            data: snapshot.into_inner(),
        };

        self.update_state_machine_(new_snapshot.clone()).await?;
        self.set_current_snapshot_(new_snapshot).map_err(|e| *e)?;

        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        let snap = self.get_current_snapshot_().map_err(|e| *e)?;
        Ok(snap.map(|s| Snapshot {
            meta: s.meta.clone(),
            snapshot: Box::new(Cursor::new(s.data.clone())),
        }))
    }
}

/// Create new Raft storage (log store and state machine store)
pub async fn new_storage(data_dir: &Utf8Path) -> Result<(LogStore, StateMachineStore)> {
    // Ensure directory exists
    tokio::fs::create_dir_all(data_dir)
        .await
        .with_context(|| format!("Failed to create directory: {}", data_dir))?;

    let db_path = data_dir.join("raft.rocksdb");

    let mut db_opts = Options::default();
    db_opts.create_missing_column_families(true);
    db_opts.create_if_missing(true);
    db_opts.set_compression_type(rocksdb::DBCompressionType::Zstd);

    let store_cf = ColumnFamilyDescriptor::new(CF_STORE, Options::default());
    let logs_cf = ColumnFamilyDescriptor::new(CF_LOGS, Options::default());

    let db = DB::open_cf_descriptors(&db_opts, db_path.as_str(), vec![store_cf, logs_cf])
        .with_context(|| format!("Failed to open RocksDB at: {}", db_path))?;

    let db = Arc::new(db);

    tracing::info!("Opened Raft storage at: {}", db_path);

    let log_store = LogStore { db: db.clone() };
    let sm_store = StateMachineStoreInner::new(db)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create state machine store: {}", e))?;

    Ok((log_store, sm_store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[tokio::test]
    async fn test_storage_creation() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        let _result = new_storage(temp_path).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_vote_persistence() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        let (mut log_store, _sm_store) = new_storage(temp_path).await?;

        // Save a vote
        let node_id = uuid::Uuid::new_v4();
        let vote = Vote::new(5, node_id);

        log_store.save_vote(&vote).await?;

        // Read it back
        let read_vote = log_store.read_vote().await?;
        assert_eq!(read_vote, Some(vote));
        Ok(())
    }

    #[tokio::test]
    async fn test_get_log_state_empty() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        let (mut log_store, _sm_store) = new_storage(temp_path).await?;

        // Get log state on empty database
        let log_state = log_store.get_log_state().await?;

        assert_eq!(log_state.last_log_id, None);
        assert_eq!(log_state.last_purged_log_id, None);
        Ok(())
    }

    // Note: test_log_append_and_read requires LogFlushed which is pub(crate) to openraft
    // Log append functionality will be tested when we integrate the actual Raft instance
    // in Phase 2.4, where openraft creates the LogFlushed callback internally
}
