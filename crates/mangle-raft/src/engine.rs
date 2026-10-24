//! The [`Engine`]: a thin facade around `openraft::Raft` providing the
//! operations mangle-server needs — bootstrap, replicated writes, manual
//! snapshot triggering, metrics, shutdown.

use std::sync::Arc;

use mangle_proto::{ApplyResult, Command};
use mangle_wal::TypeConfig;
use openraft::storage::RaftStateMachine;
use openraft::storage::RaftLogStorage;
use openraft::network::RaftNetworkFactory;
use openraft::{BasicNode, Config, Raft, SnapshotPolicy};

use crate::network::NoNetwork;

/// Errors from proposing a command through Raft.
#[derive(Debug, Clone)]
pub enum WriteError {
    /// This node is not the leader. Carries the believed leader's id and
    /// advertised address, when known — clients use it to redirect.
    NotLeader {
        leader_id: Option<u64>,
        leader_addr: Option<String>,
    },
    /// Anything else (storage, shutdown, timeout).
    Other(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::NotLeader { leader_id, leader_addr } => match (leader_id, leader_addr) {
                (Some(id), Some(addr)) => write!(f, "not leader; leader is node {id} at {addr}"),
                (Some(id), None) => write!(f, "not leader; leader is node {id}"),
                _ => write!(f, "not leader; leader unknown"),
            },
            WriteError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for WriteError {}

/// A running Raft node. Clone-safe (openraft's `Raft` handle is a clone
/// of channels to the core task).
#[derive(Clone)]
pub struct Engine {
    raft: Raft<TypeConfig>,
    id: u64,
}

impl Engine {
    /// Build the default Raft config for mangle-server.
    pub fn raft_config(snapshot_threshold: u64) -> Config {
        let mut config = Config::default();
        config.cluster_name = "mangle".to_string();
        config.election_timeout_min = 500;
        config.election_timeout_max = 1000;
        config.heartbeat_interval = 250;
        config.snapshot_policy = SnapshotPolicy::LogsSinceLast(snapshot_threshold);
        config.max_payload_entries = 1024;
        config
    }

    /// Start a Raft node with the single-node stub network (no peers).
    pub async fn new<LS, SM>(
        id: u64,
        config: Config,
        log_store: LS,
        state_machine: SM,
    ) -> anyhow::Result<Self>
    where
        LS: RaftLogStorage<TypeConfig>,
        SM: RaftStateMachine<TypeConfig>,
    {
        Self::with_network(id, config, NoNetwork, log_store, state_machine).await
    }

    /// Start a Raft node with a real inter-node network (e.g.
    /// [`crate::ConnectNetworkFactory`]).
    pub async fn with_network<N, LS, SM>(
        id: u64,
        config: Config,
        network: N,
        log_store: LS,
        state_machine: SM,
    ) -> anyhow::Result<Self>
    where
        N: RaftNetworkFactory<TypeConfig>,
        LS: RaftLogStorage<TypeConfig>,
        SM: RaftStateMachine<TypeConfig>,
    {
        let config = Arc::new(config.validate()?);
        let raft = Raft::new(id, config, network, log_store, state_machine).await?;
        Ok(Self { raft, id })
    }

    /// Bootstrap a cluster with an explicit membership (single- or
    /// multi-node). Exactly once, on the node with an empty log and no
    /// snapshot; all bootstrap nodes must agree on the membership.
    pub async fn bootstrap(&self, members: std::collections::BTreeMap<u64, BasicNode>) -> anyhow::Result<()> {
        self.raft.initialize(members).await?;
        Ok(())
    }

    /// Bootstrap a single-node cluster.
    pub async fn bootstrap_single(&self, node_addr: &str) -> anyhow::Result<()> {
        let mut members = std::collections::BTreeMap::new();
        members.insert(
            self.id,
            BasicNode {
                addr: node_addr.to_string(),
            },
        );
        self.bootstrap(members).await
    }

    /// Propose a command through Raft: appends to the WAL (fsync), waits
    /// for commit and apply, and returns the log index plus the
    /// deterministic apply result.
    pub async fn write(&self, cmd: Command) -> Result<(u64, ApplyResult), WriteError> {
        let res = self.raft.client_write(cmd).await.map_err(|e| self.write_error(e))?;
        Ok((res.log_id.index, res.data))
    }

    /// Map a client_write error to a [`WriteError`], extracting the
    /// leader hint from ForwardToLeader.
    fn write_error(&self, e: openraft::error::RaftError<u64, openraft::error::ClientWriteError<u64, BasicNode>>) -> WriteError {
        use openraft::error::ClientWriteError;
        match e {
            openraft::error::RaftError::APIError(ClientWriteError::ForwardToLeader(f)) => {
                WriteError::NotLeader {
                    leader_id: f.leader_id,
                    leader_addr: f.leader_node.map(|n| n.addr),
                }
            }
            other => WriteError::Other(other.to_string()),
        }
    }

    /// The believed leader's id and advertised address, if known.
    pub fn leader_hint(&self) -> (Option<u64>, Option<String>) {
        let rx = self.raft.metrics();
        let m = rx.borrow();
        let leader_id = m.current_leader;
        let leader_addr = leader_id.and_then(|id| {
            m.membership_config
                .membership()
                .get_node(&id)
                .map(|n| n.addr.clone())
        });
        (leader_id, leader_addr)
    }

    /// Cluster membership as (node id, addr, is-voter) triples.
    pub fn membership_summary(&self) -> Vec<(u64, String, bool)> {
        let rx = self.raft.metrics();
        let m = rx.borrow();
        let membership = m.membership_config.membership();
        let mut out = Vec::new();
        for (id, node) in membership.nodes() {
            let is_voter = membership.get_joint_config().iter().any(|c| c.contains(id));
            out.push((*id, node.addr.clone(), is_voter));
        }
        out
    }

    /// (term, last_applied_index, last_log_index) from metrics.
    pub fn progress(&self) -> (u64, u64, u64) {
        let rx = self.raft.metrics();
        let m = rx.borrow();
        (
            m.current_term,
            m.last_applied.map(|id| id.index).unwrap_or(0),
            m.last_log_index.unwrap_or(0),
        )
    }

    /// The underlying Raft handle (admin RPCs, advanced use).
    pub fn raft(&self) -> &Raft<TypeConfig> {
        &self.raft
    }

    /// Manually trigger a snapshot build (also happens automatically per
    /// the configured snapshot policy).
    pub async fn trigger_snapshot(&self) -> anyhow::Result<()> {
        self.raft
            .trigger()
            .snapshot()
            .await
            .map_err(|e| anyhow::anyhow!("snapshot trigger failed: {}", e))?;
        Ok(())
    }

    /// Whether this node currently believes it is the leader. In a
    /// single-node cluster: true once bootstrapped and electing itself.
    pub async fn is_leader(&self) -> bool {
        self.raft.metrics().borrow().current_leader == Some(self.id)
    }

    /// The node's id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Raw metrics receiver (watch channel).
    pub fn metrics(&self) -> tokio::sync::watch::Receiver<openraft::RaftMetrics<u64, BasicNode>> {
        self.raft.metrics()
    }

    /// Wait until the local FSM has applied at least `index` (used at
    /// startup to confirm replay finished, and by STRONG reads to wait
    /// for their barrier entry).
    pub async fn wait_applied(&self, index: u64) {
        let mut rx = self.metrics();
        loop {
            {
                let m = rx.borrow();
                if m.last_applied.map(|id| id.index).unwrap_or(0) >= index {
                    return;
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// The index of the last locally applied log entry (0 if none).
    pub fn last_applied(&self) -> u64 {
        self.raft
            .metrics()
            .borrow()
            .last_applied
            .map(|id| id.index)
            .unwrap_or(0)
    }

    /// STRONG-read barrier (plan §3.7): propose a no-op through Raft and
    /// wait until it is applied locally. Once it returns, the local state
    /// reflects everything committed before this read. Fails with
    /// [`WriteError::NotLeader`] on followers, like any write.
    pub async fn read_barrier(&self) -> Result<u64, WriteError> {
        let (index, _) = self.write(mangle_proto::Command::Noop).await?;
        self.wait_applied(index).await;
        Ok(index)
    }

    /// Shut down the Raft core task.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.raft
            .shutdown()
            .await
            .map_err(|e| anyhow::anyhow!("raft shutdown failed: {}", e))?;
        Ok(())
    }
}
