//! The [`Engine`]: a thin facade around `openraft::Raft` providing the
//! operations mangle-server needs — bootstrap, replicated writes, manual
//! snapshot triggering, metrics, shutdown.

use std::sync::Arc;

use mangle_proto::{ApplyResult, Command};
use mangle_wal::TypeConfig;
use openraft::storage::RaftStateMachine;
use openraft::storage::RaftLogStorage;
use openraft::{BasicNode, Config, Raft, SnapshotPolicy};

use crate::network::NoNetwork;

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

    /// Start a Raft node: `Raft::new` reads the initial state from the
    /// log store and state machine (restoring a snapshot and replaying
    /// the log as needed — the caller restores the FSM from its snapshot
    /// files before calling this).
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
        let config = Arc::new(config.validate()?);
        let raft = Raft::new(id, config, NoNetwork, log_store, state_machine).await?;
        Ok(Self { raft, id })
    }

    /// Bootstrap a single-node cluster. Exactly once, on a node with an
    /// empty log and no snapshot. Idempotent-safe: re-initializing an
    /// initialized cluster is an error, which callers treat as "already
    /// bootstrapped".
    pub async fn bootstrap_single(&self, node_addr: &str) -> anyhow::Result<()> {
        let mut members = std::collections::BTreeMap::new();
        members.insert(
            self.id,
            BasicNode {
                addr: node_addr.to_string(),
            },
        );
        self.raft.initialize(members).await?;
        Ok(())
    }

    /// Propose a command through Raft: appends to the WAL (fsync), waits
    /// for commit and apply, and returns the log index plus the
    /// deterministic apply result.
    pub async fn write(&self, cmd: Command) -> anyhow::Result<(u64, ApplyResult)> {
        let res = self
            .raft
            .client_write(cmd)
            .await
            .map_err(|e| anyhow::anyhow!("raft write failed: {}", e))?;
        Ok((res.log_id.index, res.data))
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

    /// Wait until the node has applied at least `index` (used at startup
    /// to confirm replay finished before serving traffic).
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

    /// Shut down the Raft core task.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.raft
            .shutdown()
            .await
            .map_err(|e| anyhow::anyhow!("raft shutdown failed: {}", e))?;
        Ok(())
    }
}
