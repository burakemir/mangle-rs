//! Wire conversions between openraft RPC types and the generated
//! `mangle.RaftService` proto messages. Entry/membership payloads ride as
//! bytes encoded by [`mangle_wal::codec`], so there is exactly one
//! serialization of each.

use buffa::Message as _;
use openraft::raft::InstallSnapshotRequest as ISReq;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{storage::SnapshotMeta, BasicNode, EntryPayload, LogId, Vote};

use mangle_proto::pb::mangle as pb;
use mangle_wal::{Entry, TypeConfig};

// --- Vote --------------------------------------------------------------------

pub fn vote_to_proto(v: &Vote<u64>) -> pb::RaftVote {
    pb::RaftVote {
        term: v.leader_id.term,
        node_id: v.leader_id.node_id,
        committed: v.committed,
        ..Default::default()
    }
}

pub fn vote_from_proto(v: &pb::RaftVote) -> Vote<u64> {
    Vote {
        leader_id: openraft::LeaderId::new(v.term, v.node_id),
        committed: v.committed,
    }
}

// --- LogId -------------------------------------------------------------------

pub fn log_id_to_proto(id: &LogId<u64>) -> pb::RaftLogId {
    pb::RaftLogId {
        leader_term: id.leader_id.term,
        leader_node_id: id.leader_id.node_id,
        index: id.index,
        ..Default::default()
    }
}

pub fn log_id_from_proto(id: &pb::RaftLogId) -> LogId<u64> {
    LogId::new(
        openraft::CommittedLeaderId::new(id.leader_term, id.leader_node_id),
        id.index,
    )
}

fn log_id_field(id: &Option<LogId<u64>>) -> ::buffa::MessageField<pb::RaftLogId, ::buffa::Inline<pb::RaftLogId>> {
    match id {
        Some(id) => ::buffa::MessageField::some(log_id_to_proto(id)),
        None => ::buffa::MessageField::none(),
    }
}

// --- Entries -----------------------------------------------------------------

pub fn entry_to_proto(entry: &Entry) -> pb::RaftEntry {
    let payload = match &entry.payload {
        EntryPayload::Blank => Some(pb::__buffa::oneof::raft_entry::Payload::Blank(Box::default())),
        EntryPayload::Normal(cmd) => {
            Some(pb::__buffa::oneof::raft_entry::Payload::Normal(
                cmd.to_proto().encode_to_vec(),
            ))
        }
        EntryPayload::Membership(m) => {
            Some(pb::__buffa::oneof::raft_entry::Payload::Membership(
                mangle_wal::codec::encode_membership(m),
            ))
        }
    };
    pb::RaftEntry {
        log_id: log_id_field(&Some(entry.log_id)),
        payload,
        ..Default::default()
    }
}

pub fn entry_from_proto(e: &pb::RaftEntry) -> Result<Entry, String> {
    use pb::__buffa::oneof::raft_entry::Payload as P;

    let log_id = e
        .log_id
        .as_option()
        .map(log_id_from_proto)
        .ok_or("entry without log id")?;
    let payload = match e.payload.as_ref() {
        None => return Err("entry without payload".to_string()),
        Some(P::Blank(_)) => EntryPayload::Blank,
        Some(P::Normal(bytes)) => {
            let pb_cmd = pb::Command::decode_from_slice(bytes).map_err(|e| e.to_string())?;
            EntryPayload::Normal(
                mangle_proto::Command::from_proto(&pb_cmd).map_err(|e| e.to_string())?,
            )
        }
        Some(P::Membership(bytes)) => {
            EntryPayload::Membership(mangle_wal::codec::decode_membership(bytes)?)
        }
    };
    Ok(Entry { log_id, payload })
}

// --- SnapshotMeta -------------------------------------------------------------

pub fn membership_to_meta_fields(
    meta: &SnapshotMeta<u64, BasicNode>,
) -> (String, ::buffa::MessageField<pb::RaftLogId, ::buffa::Inline<pb::RaftLogId>>, Vec<u8>) {
    (
        meta.snapshot_id.clone(),
        log_id_field(&meta.last_log_id),
        mangle_wal::codec::encode_stored_membership(&meta.last_membership),
    )
}

pub fn meta_from_proto(
    snapshot_id: &str,
    last_log_id: Option<&pb::RaftLogId>,
    membership: &[u8],
) -> Result<SnapshotMeta<u64, BasicNode>, String> {
    Ok(SnapshotMeta {
        snapshot_id: snapshot_id.to_string(),
        last_log_id: last_log_id.map(log_id_from_proto),
        last_membership: mangle_wal::codec::decode_stored_membership(membership)?,
    })
}

// --- Request/Response conversions ----------------------------------------------

pub fn vote_req_to_proto(req: &VoteRequest<u64>) -> pb::VoteRequest {
    pb::VoteRequest {
        vote: ::buffa::MessageField::some(vote_to_proto(&req.vote)),
        last_log_id: log_id_field(&req.last_log_id),
        ..Default::default()
    }
}

pub fn vote_req_from_proto(req: &pb::VoteRequest) -> Result<VoteRequest<u64>, String> {
    Ok(VoteRequest {
        vote: vote_from_proto(req.vote.as_option().ok_or("vote request without vote")?),
        last_log_id: req.last_log_id.as_option().map(log_id_from_proto),
    })
}

pub fn vote_resp_to_proto(resp: &VoteResponse<u64>, error: Option<&str>) -> pb::VoteResponse {
    pb::VoteResponse {
        vote: ::buffa::MessageField::some(vote_to_proto(&resp.vote)),
        vote_granted: resp.vote_granted,
        last_log_id: log_id_field(&resp.last_log_id),
        error: error.unwrap_or_default().to_string(),
        ..Default::default()
    }
}

pub fn vote_resp_from_proto(resp: &pb::VoteResponse) -> Result<VoteResponse<u64>, String> {
    if !resp.error.is_empty() {
        return Err(resp.error.to_string());
    }
    Ok(VoteResponse {
        vote: vote_from_proto(resp.vote.as_option().ok_or("vote response without vote")?),
        vote_granted: resp.vote_granted,
        last_log_id: resp.last_log_id.as_option().map(log_id_from_proto),
    })
}

pub fn append_req_to_proto(req: &AppendEntriesRequest<TypeConfig>) -> pb::AppendEntriesRequest {
    pb::AppendEntriesRequest {
        vote: ::buffa::MessageField::some(vote_to_proto(&req.vote)),
        prev_log_id: log_id_field(&req.prev_log_id),
        entries: req.entries.iter().map(entry_to_proto).collect(),
        leader_commit: log_id_field(&req.leader_commit),
        ..Default::default()
    }
}

pub fn append_req_from_proto(
    req: &pb::AppendEntriesRequest,
) -> Result<AppendEntriesRequest<TypeConfig>, String> {
    let entries = req
        .entries
        .iter()
        .map(entry_from_proto)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AppendEntriesRequest {
        vote: vote_from_proto(req.vote.as_option().ok_or("append request without vote")?),
        prev_log_id: req.prev_log_id.as_option().map(log_id_from_proto),
        entries,
        leader_commit: req.leader_commit.as_option().map(log_id_from_proto),
    })
}

pub fn append_resp_to_proto(
    resp: &AppendEntriesResponse<u64>,
    error: Option<&str>,
) -> pb::AppendEntriesResponse {
    use pb::__buffa::oneof::append_entries_response::Result as R;
    let result = match resp {
        AppendEntriesResponse::Success => Some(R::Success(Box::default())),
        AppendEntriesResponse::PartialSuccess(id) => match id {
            Some(id) => Some(R::PartialSuccess(Box::new(log_id_to_proto(id)))),
            None => Some(R::Success(Box::default())),
        },
        AppendEntriesResponse::Conflict => Some(R::Conflict(Box::default())),
        AppendEntriesResponse::HigherVote(v) => Some(R::HigherVote(Box::new(vote_to_proto(v)))),
    };
    pb::AppendEntriesResponse {
        result,
        error: error.unwrap_or_default().to_string(),
        ..Default::default()
    }
}

pub fn append_resp_from_proto(
    resp: &pb::AppendEntriesResponse,
) -> Result<AppendEntriesResponse<u64>, String> {
    if !resp.error.is_empty() {
        return Err(resp.error.to_string());
    }
    use pb::__buffa::oneof::append_entries_response::Result as OwnedR;
    Ok(match resp.result.as_ref() {
        Some(OwnedR::Success(_)) => AppendEntriesResponse::Success,
        Some(OwnedR::PartialSuccess(id)) => {
            AppendEntriesResponse::PartialSuccess(Some(log_id_from_proto(id)))
        }
        Some(OwnedR::Conflict(_)) => AppendEntriesResponse::Conflict,
        Some(OwnedR::HigherVote(v)) => AppendEntriesResponse::HigherVote(vote_from_proto(v)),
        None => return Err("append response without result".to_string()),
    })
}

pub fn install_req_to_proto(req: &ISReq<TypeConfig>) -> pb::InstallSnapshotRequest {
    let (snapshot_id, last_log_id, membership) = membership_to_meta_fields(&req.meta);
    pb::InstallSnapshotRequest {
        vote: ::buffa::MessageField::some(vote_to_proto(&req.vote)),
        snapshot_id,
        last_log_id,
        membership,
        offset: req.offset,
        data: req.data.clone(),
        done: req.done,
        ..Default::default()
    }
}

pub fn install_req_from_proto(
    req: &pb::InstallSnapshotRequest,
) -> Result<ISReq<TypeConfig>, String> {
    let meta = meta_from_proto(
        &req.snapshot_id,
        req.last_log_id.as_option(),
        &req.membership,
    )?;
    Ok(ISReq {
        vote: vote_from_proto(
            req.vote
                .as_option()
                .ok_or("install-snapshot request without vote")?,
        ),
        meta,
        offset: req.offset,
        data: req.data.to_vec(),
        done: req.done,
    })
}

pub fn install_resp_to_proto(
    resp: &InstallSnapshotResponse<u64>,
    error: Option<&str>,
) -> pb::InstallSnapshotResponse {
    pb::InstallSnapshotResponse {
        vote: ::buffa::MessageField::some(vote_to_proto(&resp.vote)),
        error: error.unwrap_or_default().to_string(),
        ..Default::default()
    }
}

pub fn install_resp_from_proto(
    resp: &pb::InstallSnapshotResponse,
) -> Result<InstallSnapshotResponse<u64>, String> {
    if !resp.error.is_empty() {
        return Err(resp.error.to_string());
    }
    Ok(InstallSnapshotResponse {
        vote: vote_from_proto(
            resp.vote
                .as_option()
                .ok_or("install-snapshot response without vote")?,
        ),
    })
}
