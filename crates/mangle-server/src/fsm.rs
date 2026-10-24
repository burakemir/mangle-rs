//! The Raft state machine over [`ProgramStore`] (DISTRIBUTED_PLAN.md
//! §3.4, §3.6): applies committed commands, builds and installs
//! snapshots, and reports applied state to openraft.
//!
//! The FSM shares the [`AppState`] with the RPC handlers: applies take
//! the write lock, queries the read lock. Snapshot data is the canonical
//! protobuf form of the registry + replicated-fact ledgers
//! (`mangle_proto::Snapshot`); IDB is derived and recomputed on restore.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock as StdRwLock};

use anyhow::{Result, anyhow};
use buffa::Message as _;
use mangle_proto::ApplyResult;
use mangle_wal::{Entry, TypeConfig};
use openraft::storage::{RaftSnapshotBuilder, RaftStateMachine};
use openraft::{
    BasicNode, EntryPayload, LogId, Snapshot, SnapshotMeta, StorageError, StoredMembership,
};

use crate::store::AppState;

/// Files kept in the raft dir:
const SNAPSHOT_DATA: &str = "snapshot.data";
const SNAPSHOT_META: &str = "snapshot.meta";
const SNAPSHOT_RECEIVING: &str = "snapshot.receiving";
const SNAPSHOT_TMP: &str = "snapshot.data.tmp";

/// Applied progress tracked by the FSM (and persisted in snapshot.meta).
#[derive(Clone, Debug)]
struct FsmProgress {
    last_applied: Option<LogId<u64>>,
    last_membership: StoredMembership<u64, BasicNode>,
}

impl Default for FsmProgress {
    fn default() -> Self {
        Self {
            last_applied: None,
            last_membership: StoredMembership::default(),
        }
    }
}

/// The state machine: `ProgramStore` + applied-progress bookkeeping +
/// snapshot files in `dir`.
pub struct FsmStore {
    state: AppState,
    dir: PathBuf,
    progress: StdRwLock<FsmProgress>,
}

fn sto_err(msg: impl std::fmt::Display) -> StorageError<u64> {
    let source = std::io::Error::other(msg.to_string());
    StorageError::IO {
        source: openraft::StorageIOError::new(
            openraft::ErrorSubject::Store,
            openraft::ErrorVerb::Write,
            &source,
        ),
    }
}

impl FsmStore {
    pub fn new(state: AppState, dir: impl Into<PathBuf>) -> Self {
        Self {
            state,
            dir: dir.into(),
            progress: StdRwLock::new(FsmProgress::default()),
        }
    }

    fn data_path(&self) -> PathBuf {
        self.dir.join(SNAPSHOT_DATA)
    }

    fn meta_path(&self) -> PathBuf {
        self.dir.join(SNAPSHOT_META)
    }

    fn receiving_path(&self) -> PathBuf {
        self.dir.join(SNAPSHOT_RECEIVING)
    }

    /// Whether a snapshot exists on disk (restored by [`Self::restore`]).
    pub fn has_snapshot(&self) -> bool {
        self.data_path().is_file() && self.meta_path().is_file()
    }

    /// Restore the FSM from the snapshot on disk, if any. Called at
    /// startup, before the Raft engine is created. Returns whether a
    /// snapshot was restored.
    pub fn restore(&self) -> Result<bool> {
        if !self.has_snapshot() {
            return Ok(false);
        }
        let bytes = std::fs::read(self.data_path())?;
        let pb_snap = mangle_wal::pb::mangle::Snapshot::decode_from_slice(&bytes)
            .map_err(|e| anyhow!("cannot decode snapshot {}: {}", self.data_path().display(), e))?;
        let snap = mangle_proto::Snapshot::from_proto(&pb_snap)?;

        let meta_bytes = std::fs::read(self.meta_path())?;
        let meta = decode_meta(&meta_bytes)?;

        self.state
            .write()
            .map_err(|_| anyhow!("lock poisoned"))?
            .install_snapshot_state(&snap)?;
        *self
            .progress
            .write()
            .map_err(|_| anyhow!("lock poisoned"))? = FsmProgress {
            last_applied: meta.last_log_id,
            last_membership: meta.last_membership,
        };
        Ok(true)
    }

    fn current_snapshot_meta(&self) -> Result<Option<SnapshotMeta<u64, BasicNode>>> {
        if !self.has_snapshot() {
            return Ok(None);
        }
        let bytes = std::fs::read(self.meta_path())?;
        Ok(Some(decode_meta(&bytes)?))
    }
}

// --- Snapshot meta codec ----------------------------------------------------
//
// [has_last_log: u8][log id (25 bytes, per mangle_wal codec)][stored
// membership]

fn encode_meta(meta: &SnapshotMeta<u64, BasicNode>) -> Vec<u8> {
    let mut out = Vec::new();
    match &meta.last_log_id {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&mangle_wal::codec::encode_log_id(id));
        }
        None => out.push(0),
    }
    out.extend_from_slice(&mangle_wal::codec::encode_stored_membership(
        &meta.last_membership,
    ));
    out
}

fn decode_meta(buf: &[u8]) -> Result<SnapshotMeta<u64, BasicNode>> {
    let (tag, rest) = buf
        .split_first()
        .ok_or_else(|| anyhow!("empty snapshot meta"))?;
    let (last_log_id, rest) = if *tag == 1 {
        if rest.len() < 24 {
            return Err(anyhow!("truncated snapshot meta log id"));
        }
        (
            Some(mangle_wal::codec::decode_log_id(&rest[..24]).map_err(|e| anyhow!("{e}"))?),
            &rest[24..],
        )
    } else {
        (None, rest)
    };
    let last_membership = mangle_wal::codec::decode_stored_membership(rest)
        .map_err(|e| anyhow!("{e}"))?;
    Ok(SnapshotMeta {
        snapshot_id: String::new(),
        last_log_id,
        last_membership,
    })
}

/// A snapshot id: derived from the last applied log id plus a build
/// counter, so two snapshots built at the same applied point differ
/// (openraft uses the id to dedup installs; it is transport metadata,
/// not FSM state).
fn snapshot_id_for(last_log_id: Option<LogId<u64>>) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    match last_log_id {
        Some(id) => format!("{}-{}-{}", id.leader_id.term, id.index, n),
        None => format!("empty-{n}"),
    }
}

// --- RaftStateMachine --------------------------------------------------------

#[allow(refining_impl_trait)]
impl RaftStateMachine<TypeConfig> for FsmStore {
    type SnapshotBuilder = SnapshotBuilder;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, BasicNode>), StorageError<u64>> {
        let progress = self
            .progress
            .read()
            .map_err(|_| sto_err("progress lock poisoned"))?;
        Ok((progress.last_applied, progress.last_membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<ApplyResult>, StorageError<u64>>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let mut results = Vec::new();
        for entry in entries {
            let log_id = entry.log_id;
            match entry.payload {
                EntryPayload::Blank => {
                    // Openraft requires one reply per applied entry.
                    results.push(Ok(mangle_proto::ApplyOutcome::Noop));
                }
                EntryPayload::Normal(cmd) => {
                    // A command that fails against the current state fails
                    // deterministically on every node; report it as the
                    // apply result rather than a storage error (which
                    // openraft treats as fatal).
                    let result = match self.state.write() {
                        Ok(mut store) => store.apply(&cmd).map_err(|e| e.to_string()),
                        Err(_) => Err("state lock poisoned".to_string()),
                    };
                    results.push(result);
                }
                EntryPayload::Membership(m) => {
                    let mut progress = self
                        .progress
                        .write()
                        .map_err(|_| sto_err("progress lock poisoned"))?;
                    progress.last_membership =
                        StoredMembership::new(Some(log_id), m);
                    results.push(Ok(mangle_proto::ApplyOutcome::Noop));
                }
            }
            self.progress
                .write()
                .map_err(|_| sto_err("progress lock poisoned"))?
                .last_applied = Some(log_id);
        }
        Ok(results)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        SnapshotBuilder {
            state: Arc::clone(&self.state),
            dir: self.dir.clone(),
            progress: self.progress.read().unwrap().clone(),
        }
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<tokio::fs::File>, StorageError<u64>> {
        std::fs::create_dir_all(&self.dir).map_err(|e| sto_err(e))?;
        let f = tokio::fs::File::create(self.receiving_path())
            .await
            .map_err(|e| sto_err(e))?;
        Ok(Box::new(f))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, BasicNode>,
        snapshot: Box<tokio::fs::File>,
    ) -> Result<(), StorageError<u64>> {
        // Read back what openraft streamed into the receiving file.
        let mut f = snapshot;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        f.seek(std::io::SeekFrom::Start(0))
            .await
            .map_err(|e| sto_err(e))?;
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).await.map_err(|e| sto_err(e))?;
        drop(f);

        let pb_snap = mangle_wal::pb::mangle::Snapshot::decode_from_slice(&bytes)
            .map_err(|e| sto_err(format!("cannot decode received snapshot: {e}")))?;
        let snap = mangle_proto::Snapshot::from_proto(&pb_snap)
            .map_err(|e| sto_err(format!("invalid received snapshot: {e}")))?;

        {
            let mut store = self
                .state
                .write()
                .map_err(|_| sto_err("state lock poisoned"))?;
            store
                .install_snapshot_state(&snap)
                .map_err(|e| sto_err(format!("cannot install snapshot: {e}")))?;
        }
        *self
            .progress
            .write()
            .map_err(|_| sto_err("progress lock poisoned"))? = FsmProgress {
            last_applied: meta.last_log_id,
            last_membership: meta.last_membership.clone(),
        };

        // Persist the received snapshot as the current one.
        tokio::fs::rename(self.receiving_path(), self.data_path())
            .await
            .map_err(|e| sto_err(e))?;
        std::fs::write(self.meta_path(), encode_meta(meta)).map_err(|e| sto_err(e))?;
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, StorageError<u64>> {
        let Some(meta) = self
            .current_snapshot_meta()
            .map_err(|e| sto_err(format!("cannot read snapshot meta: {e}")))?
        else {
            return Ok(None);
        };
        let f = tokio::fs::File::open(self.data_path())
            .await
            .map_err(|e| sto_err(e))?;
        Ok(Some(Snapshot {
            meta,
            snapshot: Box::new(f),
        }))
    }
}

// --- Snapshot builder --------------------------------------------------------

pub struct SnapshotBuilder {
    state: AppState,
    dir: PathBuf,
    progress: FsmProgress,
}

#[allow(refining_impl_trait)]
impl RaftSnapshotBuilder<TypeConfig> for SnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<u64>> {
        let snap = {
            let store = self
                .state
                .read()
                .map_err(|_| sto_err("state lock poisoned"))?;
            store.snapshot_state()
        };
        let bytes = snap.to_proto().encode_to_vec();
        let meta = SnapshotMeta {
            snapshot_id: snapshot_id_for(self.progress.last_applied),
            last_log_id: self.progress.last_applied,
            last_membership: self.progress.last_membership.clone(),
        };

        // Write data + meta atomically through a temporary FsmStore-like
        // path helper (duplicated here to avoid &mut self borrow issues).
        std::fs::create_dir_all(&self.dir).map_err(|e| sto_err(e))?;
        let tmp = self.dir.join(SNAPSHOT_TMP);
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| sto_err(e))?;
        use tokio::io::AsyncWriteExt;
        f.write_all(&bytes).await.map_err(|e| sto_err(e))?;
        f.sync_all().await.map_err(|e| sto_err(e))?;
        drop(f);
        tokio::fs::rename(&tmp, self.dir.join(SNAPSHOT_DATA))
            .await
            .map_err(|e| sto_err(e))?;

        let meta_tmp = self.dir.join(format!("{SNAPSHOT_META}.tmp"));
        let meta_bytes = encode_meta(&meta);
        let mut mf = tokio::fs::File::create(&meta_tmp)
            .await
            .map_err(|e| sto_err(e))?;
        mf.write_all(&meta_bytes).await.map_err(|e| sto_err(e))?;
        mf.sync_all().await.map_err(|e| sto_err(e))?;
        drop(mf);
        tokio::fs::rename(&meta_tmp, self.dir.join(SNAPSHOT_META))
            .await
            .map_err(|e| sto_err(e))?;

        let read = tokio::fs::File::open(self.dir.join(SNAPSHOT_DATA))
            .await
            .map_err(|e| sto_err(e))?;
        Ok(Snapshot {
            meta,
            snapshot: Box::new(read),
        })
    }
}

/// Whether the raft dir is fresh: no snapshot files present.
pub fn snapshot_exists(dir: &Path) -> bool {
    dir.join(SNAPSHOT_DATA).is_file() && dir.join(SNAPSHOT_META).is_file()
}

/// Parse a `mutations.mg` migration file (the retired MutationLog format)
/// into mutation ops: each fact line becomes one op, `__retract__rel(...)`
/// becomes a retract of `rel(...)`.
pub fn mutations_to_ops(source: &str) -> Result<Vec<mangle_proto::MutationOp>> {
    use mangle_proto::{MutationOp, MutationOpKind};

    let arena = mangle_ast::Arena::new_with_global_interner();
    let (mut ir, stratified) = mangle_driver::compile_units(&[source], &arena)?;
    let store = Box::new(mangle_interpreter::MemStore::new());
    let interpreter = mangle_driver::execute(&mut ir, &stratified, store)?;

    let mut ops = Vec::new();
    for name in interpreter.store().relation_names() {
        let (kind, relation) = match name.strip_prefix("__retract__") {
            Some(rel) => (MutationOpKind::Retract, rel.to_string()),
            None => (MutationOpKind::Insert, name.clone()),
        };
        for tuple in interpreter.store().scan(&name)? {
            ops.push(MutationOp {
                kind,
                relation: relation.clone(),
                tuple,
            });
        }
    }
    Ok(ops)
}
