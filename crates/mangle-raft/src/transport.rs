//! The inter-node transport: [`RaftNetwork`] over ConnectRPC clients
//! speaking `mangle.RaftService` (DISTRIBUTED_PLAN.md §3.5). Each node's
//! address comes from its `BasicNode` in the cluster membership — no
//! separate peer table.


use connectrpc::client::{ClientConfig, HttpClient};
use mangle_proto::pb::mangle::RaftServiceClient;
use openraft::network::RPCOption;
use openraft::network::{RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::error::{RaftError, RPCError, Unreachable};
use openraft::{BasicNode, ChangeMembers};

use mangle_proto::pb::mangle as pb;
use mangle_wal::TypeConfig;

use crate::wire;

/// Factory producing per-target ConnectRPC clients.
#[derive(Clone, Default)]
pub struct ConnectNetworkFactory;

impl ConnectNetworkFactory {
    pub fn new() -> Self {
        Self
    }
}

#[allow(refining_impl_trait)]
impl RaftNetworkFactory<TypeConfig> for ConnectNetworkFactory {
    type Network = ConnectNetwork;

    async fn new_client(&mut self, _target: u64, node: &BasicNode) -> Self::Network {
        ConnectNetwork::new(node.addr.clone())
    }
}

/// A ConnectRPC client bound to one target node address.
pub struct ConnectNetwork {
    addr: String,
    client: RaftServiceClient<HttpClient>,
}

impl ConnectNetwork {
    pub fn new(addr: String) -> Self {
        let client = RaftServiceClient::new(
            HttpClient::plaintext(),
            ClientConfig::new(format!("http://{addr}").parse().expect("valid url")),
        );
        Self { addr, client }
    }

    /// Errors are mapped to `Unreachable` (openraft backs off and
    /// retries); a remote `error` field on a response is likewise a
    /// retryable condition.
    fn unreachable<E: std::error::Error + 'static>(
        &self,
        msg: impl std::fmt::Display,
    ) -> RPCError<u64, BasicNode, E> {
        let err = std::io::Error::other(format!("raft rpc to {}: {}", self.addr, msg));
        RPCError::Unreachable(Unreachable::new(&err))
    }
}

impl ConnectNetwork {
    async fn vote_inner(
        &mut self,
        rpc: VoteRequest<u64>,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let resp = self
            .client
            .clone()
            .vote(wire::vote_req_to_proto(&rpc))
            .await
            .map_err(|e| self.unreachable(e))?
            .into_owned();
        wire::vote_resp_from_proto(&resp).map_err(|e| self.unreachable(e))
    }

    async fn append_inner(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let resp = self
            .client
            .clone()
            .append_entries(wire::append_req_to_proto(&rpc))
            .await
            .map_err(|e| self.unreachable(e))?
            .into_owned();
        wire::append_resp_from_proto(&resp).map_err(|e| self.unreachable(e))
    }

    #[allow(deprecated)]
    async fn install_snapshot_inner(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, openraft::error::InstallSnapshotError>>,
    > {
        // Snapshot chunks can be large; honor the caller's hard TTL.
        let ttl = option.hard_ttl();
        let req = wire::install_req_to_proto(&rpc);
        let client = self.client.clone();
        let call = client.install_snapshot(req);
        let resp = match tokio::time::timeout(ttl, call).await {
            Err(_) => return Err(self.unreachable(format!("timeout after {ttl:?}"))),
            Ok(Err(e)) => return Err(self.unreachable(e)),
            Ok(Ok(resp)) => resp.into_owned(),
        };
        wire::install_resp_from_proto(&resp).map_err(|e| self.unreachable(e))
    }
}

impl RaftNetwork<TypeConfig> for ConnectNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        self.append_inner(rpc).await
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, openraft::error::InstallSnapshotError>>,
    > {
        self.install_snapshot_inner(rpc, option).await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        self.vote_inner(rpc).await
    }
}

/// Membership-change helper shared by the admin RPC handlers: add the node
/// as a learner, wait for it to catch up, then promote it to voter.
pub async fn join_member(
    raft: &openraft::Raft<TypeConfig>,
    id: u64,
    addr: String,
) -> anyhow::Result<()> {
    use anyhow::anyhow;
    raft.add_learner(id, BasicNode { addr }, true)
        .await
        .map_err(|e| anyhow!("add learner: {e}"))?;
    raft.change_membership(ChangeMembers::AddVoterIds([id].into()), false)
        .await
        .map_err(|e| anyhow!("promote to voter: {e}"))?;
    Ok(())
}

/// Remove a node from the cluster (voter or learner).
pub async fn remove_member(
    raft: &openraft::Raft<TypeConfig>,
    id: u64,
) -> anyhow::Result<()> {
    use anyhow::anyhow;
    raft.change_membership(ChangeMembers::RemoveNodes([id].into()), false)
        .await
        .map_err(|e| anyhow!("remove node: {e}"))?;
    Ok(())
}
