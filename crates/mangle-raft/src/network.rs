//! Inter-node network. Phase 1: a single-node cluster never contacts
//! peers, so this is a stub whose RPCs always fail. Phase 2 replaces it
//! with a ConnectRPC client speaking `RaftService` (see
//! DISTRIBUTED_PLAN.md §3.5).

use openraft::error::{RaftError, RPCError, Unreachable};
use openraft::network::{RaftNetwork, RaftNetworkFactory};
use openraft::network::RPCOption;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{BasicNode, RPCTypes};

use mangle_wal::TypeConfig;

/// Network factory for a single-node cluster: every RPC is an error (a
/// single-node cluster has no peers).
#[derive(Clone, Copy, Default)]
pub struct NoNetwork;

#[allow(refining_impl_trait)]
impl RaftNetworkFactory<TypeConfig> for NoNetwork {
    type Network = NoNetwork;

    async fn new_client(&mut self, _target: u64, _node: &BasicNode) -> Self::Network {
        NoNetwork
    }
}

fn unreachable<E: std::error::Error>(rpc: RPCTypes) -> RPCError<u64, BasicNode, E> {
    // The error type parameter is unused: the stub always reports the
    // peer as unreachable.
    let err = std::io::Error::other(format!(
        "no network configured (single-node cluster); {rpc} should never be sent"
    ));
    RPCError::Unreachable(Unreachable::new(&err))
}

impl RaftNetwork<TypeConfig> for NoNetwork {
    async fn append_entries(
        &mut self,
        _rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        Err(unreachable(RPCTypes::AppendEntries))
    }

    async fn install_snapshot(
        &mut self,
        _rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, openraft::error::InstallSnapshotError>>,
    > {
        Err(unreachable(RPCTypes::InstallSnapshot))
    }

    async fn vote(
        &mut self,
        _rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        Err(unreachable(RPCTypes::Vote))
    }
}
