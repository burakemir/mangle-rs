//! The write-ahead log (WAL) crate for a distributed mangle-server.
//!
//! Hosts the Raft [`TypeConfig`] shared by the mangle Raft crates and the
//! redb-backed [`RaftLogStorage`] implementation (see `README.md` and
//! `DISTRIBUTED_PLAN.md`).

pub mod codec;
pub mod log_store;

use openraft::impls::OneshotResponder;
pub use log_store::{RaftLog, WalError};

/// Re-export of the generated protobuf module, for the FSM crate's
/// encode/decode needs.
pub use mangle_proto::pb;

openraft::declare_raft_types!(
    /// The Mangle Raft type configuration: commands in, deterministic
    /// apply results out; u64 node ids; file-based snapshots.
    pub TypeConfig:
        D = mangle_proto::Command,
        R = mangle_proto::ApplyResult,
        NodeId = u64,
        Node = openraft::BasicNode,
        Entry = openraft::Entry<Self>,
        SnapshotData = tokio::fs::File,
        AsyncRuntime = openraft::TokioRuntime,
        Responder = OneshotResponder<Self>,
);

/// Alias re-exports matching openraft's naming.
pub type Entry = openraft::Entry<TypeConfig>;
pub type SnapshotData = tokio::fs::File;
