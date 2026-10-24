//! The inter-node `RaftService` (DISTRIBUTED_PLAN.md §3.5): forwards
//! Vote/AppendEntries/InstallSnapshot RPCs into the local openraft node.
//! Mounted on the same ConnectRPC router as `MangleService` whenever the
//! server runs in replicated mode.
//!
//! RPC-level failures from the local Raft node (e.g. an uninitialized
//! node receiving AppendEntries) are reported **in-band** via the
//! response's `error` field: the sending side maps them to a retryable
//! `Unreachable`, which is what Raft consensus expects of an unavailable
//! peer. Transport-level failures stay Connect errors.

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use mangle_proto::pb::mangle as pb;
use mangle_raft::{wire, TypeConfig};
use openraft::{raft::InstallSnapshotResponse, raft::AppendEntriesResponse, raft::VoteResponse, Raft};

pub struct RaftServiceImpl {
    pub raft: Raft<TypeConfig>,
}

#[allow(refining_impl_trait)]
impl pb::RaftService for RaftServiceImpl {
    async fn vote(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::VoteRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<pb::VoteResponse> + Send + use<>> {
        let req = wire::vote_req_from_proto(&request.to_owned_message()).map_err(ConnectError::invalid_argument)?;
        match self.raft.vote(req).await {
            Ok(resp) => Response::ok(wire::vote_resp_to_proto(&resp, None)),
            Err(e) => Response::ok(wire::vote_resp_to_proto(
                &VoteResponse {
                    vote: openraft::Vote::default(),
                    vote_granted: false,
                    last_log_id: None,
                },
                Some(&e.to_string()),
            )),
        }
    }

    async fn append_entries(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::AppendEntriesRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<pb::AppendEntriesResponse> + Send + use<>> {
        let req =
            wire::append_req_from_proto(&request.to_owned_message()).map_err(ConnectError::invalid_argument)?;
        match self.raft.append_entries(req).await {
            Ok(resp) => Response::ok(wire::append_resp_to_proto(&resp, None)),
            Err(e) => Response::ok(wire::append_resp_to_proto(
                &AppendEntriesResponse::Conflict,
                Some(&e.to_string()),
            )),
        }
    }

    async fn install_snapshot(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::InstallSnapshotRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<pb::InstallSnapshotResponse> + Send + use<>> {
        let req =
            wire::install_req_from_proto(&request.to_owned_message()).map_err(ConnectError::invalid_argument)?;
        #[allow(deprecated)] // the chunked receive flow is exactly what we want
        match self.raft.install_snapshot(req).await {
            Ok(resp) => Response::ok(wire::install_resp_to_proto(&resp, None)),
            Err(e) => Response::ok(wire::install_resp_to_proto(
                &InstallSnapshotResponse {
                    vote: openraft::Vote::default(),
                },
                Some(&e.to_string()),
            )),
        }
    }
}
