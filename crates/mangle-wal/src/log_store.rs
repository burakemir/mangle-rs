//! The write-ahead log: openraft's [`RaftLogStorage`] + [`RaftLogReader`]
//! backed by a single redb database file (`raft.redb`).
//!
//! Durability: every write transaction commits with
//! `Durability::Immediate` — redb fsyncs before the commit returns — which
//! is exactly the WAL property: a command acknowledged to a client is
//! already on disk. Vote, log and purge/truncate writes all go through
//! the same serialized transaction path.
//!
//! Layout: one table `log` (index → encoded entry) and one `meta` table
//! (`vote` → encoded vote, `last_purged` → encoded log id).

use std::ops::RangeBounds;
use std::path::Path;
use std::sync::{Arc, Mutex};

use openraft::storage::{LogFlushed, RaftLogReader, RaftLogStorage};
use openraft::{ErrorSubject as ErrSubject, ErrorVerb, LogId, LogState, StorageError, StorageIOError, Vote};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};

use crate::codec::{
    decode_entry, decode_log_id, decode_vote, encode_entry, encode_log_id, encode_vote,
};
use crate::{Entry, TypeConfig};

/// Error conversion into openraft's StorageError::IO.
fn io_err(subject: ErrSubject<u64>, verb: ErrorVerb, msg: impl std::fmt::Display) -> StorageError<u64> {
    let source = std::io::Error::other(msg.to_string());
    StorageError::IO {
        source: StorageIOError::new(subject, verb, &source),
    }
}

fn read_err(msg: impl std::fmt::Display) -> StorageError<u64> {
    io_err(ErrSubject::Store, ErrorVerb::Read, msg)
}

fn write_err(msg: impl std::fmt::Display) -> StorageError<u64> {
    io_err(ErrSubject::Store, ErrorVerb::Write, msg)
}

fn corrupt(subject: ErrSubject<u64>, msg: String) -> StorageError<u64> {
    io_err(subject, ErrorVerb::Read, format!("corrupt WAL record: {msg}"))
}

/// Errors opening/using the WAL database. All underlying redb error
/// types are flattened into their display form (the WAL either opens or
/// it doesn't; recovery is not possible mid-flight).
#[derive(Debug)]
pub struct WalError(pub std::io::Error);

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wal error: {}", self.0)
    }
}

impl std::error::Error for WalError {}

macro_rules! from_redb {
    ($($t:ty),* $(,)?) => {
        $(
            impl From<$t> for WalError {
                fn from(e: $t) -> Self {
                    WalError(std::io::Error::other(e.to_string()))
                }
            }
        )*
    };
}

from_redb!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::SetDurabilityError,
    std::io::Error,
);

const LOG_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("mangle.log");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("mangle.meta");

const KEY_VOTE: &str = "vote";
const KEY_LAST_PURGED: &str = "last_purged";

/// The redb-backed Raft log + vote store.
pub struct RaftLog {
    db: Arc<Database>,
    /// redb `Database` is `Send` but transactions take `&mut`; serialize
    /// writers with a mutex (openraft serializes writes anyway, this makes
    /// it a structural guarantee).
    write_lock: Mutex<()>,
}

impl RaftLog {
    /// Open (or create) the WAL database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WalError> {
        let db = Database::create(path.as_ref())?;
        // Ensure the tables exist.
        let txn = db.begin_write()?;
        {
            txn.open_table(LOG_TABLE)?;
            txn.open_table(META_TABLE)?;
        }
        txn.commit()?;
        Ok(Self {
            db: Arc::new(db),
            write_lock: Mutex::new(()),
        })
    }

    /// First and last present log indexes, if any.
    pub fn log_range(&self) -> Result<(Option<u64>, Option<u64>), WalError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LOG_TABLE)?;
        let first = table.first()?.map(|(k, _)| k.value());
        let last = table.last()?.map(|(k, _)| k.value());
        Ok((first, last))
    }

    /// The inner append: write entries, commit with fsync. Split out of
    /// the trait method so it is unit-testable without a `LogFlushed`
    /// callback (whose constructor is not public).
    fn append_inner(&self, entries: impl IntoIterator<Item = Entry>) -> Result<(), StorageError<u64>> {
        let mut txn = self
            .db
            .begin_write()
            .map_err(write_err)?;
        {
            let mut log = txn.open_table(LOG_TABLE).map_err(write_err)?;
            for entry in entries {
                let bytes = encode_entry(&entry).map_err(|e| corrupt(ErrSubject::Logs, e))?;
                log.insert(entry.log_id.index, bytes.as_slice())
                    .map_err(write_err)?;
            }
        }
        txn.set_durability(Durability::Immediate)
            .map_err(|e| write_err(e))?;
        txn.commit().map_err(write_err)?;
        Ok(())
    }

    fn write_txn(
        &self,
        f: impl FnOnce(
            &mut redb::Table<'_, u64, &[u8]>,
            &mut redb::Table<'_, &str, &[u8]>,
        ) -> Result<(), StorageError<u64>>,
    ) -> Result<(), StorageError<u64>> {
        let mut txn = self
            .db
            .begin_write()
            .map_err(write_err)?;
        {
            let mut log = txn.open_table(LOG_TABLE).map_err(write_err)?;
            let mut meta = txn.open_table(META_TABLE).map_err(write_err)?;
            f(&mut log, &mut meta)?;
        }
        txn.set_durability(Durability::Immediate)
            .map_err(|e| write_err(e))?;
        txn.commit().map_err(write_err)?;
        Ok(())
    }

    /// A cheap clone sharing the same database (used as the log reader
    /// handle; redb readers open their own read transactions).
    fn clone_read_handle(&self) -> Self {
        Self {
            db: Arc::clone(&self.db),
            write_lock: Mutex::new(()),
        }
    }
}

// --- RaftLogReader ----------------------------------------------------------

impl RaftLogReader<TypeConfig> for RaftLog {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + core::fmt::Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, StorageError<u64>> {
        let txn = self.db.begin_read().map_err(read_err)?;
        let table = txn.open_table(LOG_TABLE).map_err(read_err)?;

        let start = match range.start_bound() {
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => i.checked_add(1).unwrap_or(u64::MAX),
            std::ops::Bound::Unbounded => 0,
        };

        let mut out = Vec::new();
        for row in table.range(start..).map_err(read_err)? {
            let (k, v) = row.map_err(read_err)?;
            let index = k.value();
            if !range.contains(&index) {
                break;
            }
            let entry = decode_entry(v.value()).map_err(|e| corrupt(ErrSubject::Logs, e))?;
            out.push(entry);
        }
        Ok(out)
    }
}

// --- RaftLogStorage ---------------------------------------------------------

impl RaftLogStorage<TypeConfig> for RaftLog {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<u64>> {
        let txn = self.db.begin_read().map_err(read_err)?;
        let meta = txn.open_table(META_TABLE).map_err(read_err)?;
        let log = txn.open_table(LOG_TABLE).map_err(read_err)?;

        let last_purged = match meta.get(KEY_LAST_PURGED).map_err(read_err)? {
            Some(v) => Some(
                decode_log_id(v.value()).map_err(|e| corrupt(ErrSubject::Store, e))?,
            ),
            None => None,
        };

        let last_log_id = match log.last().map_err(read_err)? {
            Some((_, v)) => {
                let entry = decode_entry(v.value()).map_err(|e| corrupt(ErrSubject::Logs, e))?;
                Some(entry.log_id)
            }
            None => last_purged,
        };

        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone_read_handle()
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let _guard = self.write_lock.lock().unwrap();
        self.write_txn(|_log, meta| {
            meta.insert(KEY_VOTE, encode_vote(vote).as_slice())
                .map_err(write_err)?;
            Ok(())
        })
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        let txn = self.db.begin_read().map_err(read_err)?;
        let meta = txn.open_table(META_TABLE).map_err(read_err)?;
        match meta.get(KEY_VOTE).map_err(read_err)? {
            Some(v) => decode_vote(v.value())
                .map(Some)
                .map_err(|e| corrupt(ErrSubject::Vote, e)),
            None => Ok(None),
        }
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let _guard = self.write_lock.lock().unwrap();
        let result = self.append_inner(entries);
        match &result {
            Ok(()) => callback.log_io_completed(Ok(())),
            Err(e) => callback.log_io_completed(Err(std::io::Error::other(e.to_string()))),
        }
        result
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let _guard = self.write_lock.lock().unwrap();
        self.write_txn(|log, _meta| {
            let keys: Vec<u64> = log
                .range(log_id.index..)
                .map_err(read_err)?
                .filter_map(|r| r.ok().map(|(k, _)| k.value()))
                .collect();
            for k in keys {
                log.remove(k).map_err(write_err)?;
            }
            Ok(())
        })
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let _guard = self.write_lock.lock().unwrap();
        self.write_txn(|log, meta| {
            let keys: Vec<u64> = log
                .range(..=log_id.index)
                .map_err(read_err)?
                .filter_map(|r| r.ok().map(|(k, _)| k.value()))
                .collect();
            for k in keys {
                log.remove(k).map_err(write_err)?;
            }
            meta.insert(KEY_LAST_PURGED, encode_log_id(&log_id).as_slice())
                .map_err(write_err)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::{CommittedLeaderId, EntryPayload};

    fn blank_entry(term: u64, index: u64) -> Entry {
        Entry {
            log_id: LogId::new(CommittedLeaderId::new(term, 1), index),
            payload: EntryPayload::Blank,
        }
    }

    #[tokio::test]
    async fn vote_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.redb");
        {
            let mut log = RaftLog::open(&path).unwrap();
            assert_eq!(log.read_vote().await.unwrap(), None);
            let vote = Vote {
                leader_id: openraft::LeaderId::new(2, 1),
                committed: true,
            };
            log.save_vote(&vote).await.unwrap();
            assert_eq!(log.read_vote().await.unwrap(), Some(vote));
        }

        // Reopen (redb requires exclusive access, so the first handle must
        // be dropped first): the vote persisted.
        let mut log2 = RaftLog::open(&path).unwrap();
        assert_eq!(
            log2.read_vote().await.unwrap(),
            Some(Vote {
                leader_id: openraft::LeaderId::new(2, 1),
                committed: true,
            })
        );
    }

    #[tokio::test]
    async fn append_read_truncate_purge() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = RaftLog::open(dir.path().join("raft.redb")).unwrap();

        log.append_inner(vec![blank_entry(1, 1), blank_entry(1, 2), blank_entry(1, 3)])
            .unwrap();

        let state = log.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.unwrap().index, 3);
        assert_eq!(state.last_purged_log_id, None);

        // Read a subrange.
        let got = log.try_get_log_entries(2..=3).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].log_id.index, 2);

        // Truncate from index 2: [1] remains.
        log.truncate(LogId::new(CommittedLeaderId::new(1, 1), 2))
            .await
            .unwrap();
        let got = log.try_get_log_entries(1..).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].log_id.index, 1);

        // Purge index 1: nothing remains, last_purged set.
        log.purge(LogId::new(CommittedLeaderId::new(1, 1), 1))
            .await
            .unwrap();
        let state = log.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.unwrap().index, 1);
        assert_eq!(state.last_purged_log_id.unwrap().index, 1);
        let got = log.try_get_log_entries(1..).await.unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn append_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.redb");
        {
            let log = RaftLog::open(&path).unwrap();
            log.append_inner(vec![blank_entry(1, 1)]).unwrap();
        }
        let mut log = RaftLog::open(&path).unwrap();
        let got = log.try_get_log_entries(1..).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].log_id.index, 1);
    }

    #[tokio::test]
    async fn reopen_requires_no_table_creation() {
        // Opening an existing database twice in a row works (create() on
        // an existing file opens it).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.redb");
        let _ = RaftLog::open(&path).unwrap();
        let _ = RaftLog::open(&path).unwrap();
    }
}
