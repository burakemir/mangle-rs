//! Binary codec for Raft metadata persisted in the WAL.
//!
//! Openraft's own serde impls are gated behind its `serde` feature, which
//! would force serde bounds onto `mangle_proto::Command` and the internal
//! `Value` tree. Instead the WAL hand-encodes the small, fixed Raft
//! metadata structures (votes, log ids, memberships) and stores command
//! payloads as canonical protobuf bytes. Every encoding is little-endian
//! and covered by round-trip tests.

use openraft::{BasicNode, CommittedLeaderId, LeaderId, LogId, Membership, StoredMembership, Vote};
use buffa::Message as _;
use crate::TypeConfig;

// --- Primitive readers/writers --------------------------------------------

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bool(out: &mut Vec<u8>, v: bool) {
    out.push(u8::from(v));
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u64(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// Simple cursor over the encoded bytes; `anyhow` errors on underflow
/// (a truncated or corrupt record).
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("length overflow")?;
        if end > self.buf.len() {
            return Err(format!(
                "truncated record: need {end} bytes, have {}",
                self.buf.len()
            ));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u64(&mut self) -> Result<u64, String> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn bool(&mut self) -> Result<bool, String> {
        Ok(self.take(1)?[0] != 0)
    }

    fn str(&mut self) -> Result<String, String> {
        let len = self.u64()? as usize;
        let b = self.take(len)?;
        String::from_utf8(b.to_vec()).map_err(|e| format!("invalid utf-8: {e}"))
    }

    fn done(&self) -> Result<(), String> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(format!(
                "{} trailing bytes in record",
                self.buf.len() - self.pos
            ))
        }
    }
}

// --- CommittedLeaderId / LogId ---------------------------------------------

pub fn encode_committed_leader_id(id: &CommittedLeaderId<u64>) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    put_u64(&mut out, id.term);
    put_u64(&mut out, id.node_id);
    out
}

pub fn decode_committed_leader_id(buf: &[u8]) -> Result<CommittedLeaderId<u64>, String> {
    let mut r = Reader::new(buf);
    let term = r.u64()?;
    let node_id = r.u64()?;
    r.done()?;
    Ok(CommittedLeaderId::new(term, node_id))
}

pub fn encode_log_id(id: &LogId<u64>) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&encode_committed_leader_id(&id.leader_id));
    put_u64(&mut out, id.index);
    out
}

pub fn decode_log_id(buf: &[u8]) -> Result<LogId<u64>, String> {
    if buf.len() < 24 {
        return Err(format!("truncated log id: {} bytes", buf.len()));
    }
    let leader_id = decode_committed_leader_id(&buf[..16])?;
    let mut r = Reader::new(&buf[16..]);
    let index = r.u64()?;
    r.done()?;
    Ok(LogId::new(leader_id, index))
}

// --- Vote -------------------------------------------------------------------

pub fn encode_vote(v: &Vote<u64>) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    put_u64(&mut out, v.leader_id.term);
    put_u64(&mut out, v.leader_id.node_id);
    put_bool(&mut out, v.committed);
    out
}

pub fn decode_vote(buf: &[u8]) -> Result<Vote<u64>, String> {
    let mut r = Reader::new(buf);
    let term = r.u64()?;
    let node_id = r.u64()?;
    let committed = r.bool()?;
    r.done()?;
    Ok(Vote {
        leader_id: LeaderId::new(term, node_id),
        committed,
    })
}

// --- Membership / StoredMembership ------------------------------------------

pub fn encode_membership(m: &Membership<u64, BasicNode>) -> Vec<u8> {
    let mut out = Vec::new();
    put_u64(&mut out, m.get_joint_config().len() as u64);
    for config in m.get_joint_config() {
        put_u64(&mut out, config.len() as u64);
        for id in config {
            put_u64(&mut out, *id);
        }
    }
    let nodes: Vec<_> = m.nodes().collect();
    put_u64(&mut out, nodes.len() as u64);
    for (id, node) in nodes {
        put_u64(&mut out, *id);
        put_str(&mut out, &node.addr);
    }
    out
}

pub fn decode_membership(buf: &[u8]) -> Result<Membership<u64, BasicNode>, String> {
    let mut r = Reader::new(buf);
    let n_configs = r.u64()? as usize;
    let mut configs = Vec::with_capacity(n_configs);
    for _ in 0..n_configs {
        let n = r.u64()? as usize;
        let mut config = std::collections::BTreeSet::new();
        for _ in 0..n {
            config.insert(r.u64()?);
        }
        configs.push(config);
    }
    let n_nodes = r.u64()? as usize;
    let mut nodes = std::collections::BTreeMap::new();
    for _ in 0..n_nodes {
        let id = r.u64()?;
        let addr = r.str()?;
        nodes.insert(id, BasicNode { addr });
    }
    r.done()?;
    Ok(Membership::new(configs, nodes))
}

pub fn encode_stored_membership(m: &StoredMembership<u64, BasicNode>) -> Vec<u8> {
    let mut out = Vec::new();
    match m.log_id() {
        Some(id) => {
            put_bool(&mut out, true);
            out.extend_from_slice(&encode_log_id(id));
        }
        None => put_bool(&mut out, false),
    }
    out.extend_from_slice(&encode_membership(m.membership()));
    out
}

pub fn decode_stored_membership(buf: &[u8]) -> Result<StoredMembership<u64, BasicNode>, String> {
    let mut r = Reader::new(buf);
    let has_log_id = r.bool()?;
    let log_id = if has_log_id {
        let term = r.u64()?;
        let node_id = r.u64()?;
        let index = r.u64()?;
        Some(LogId::new(CommittedLeaderId::new(term, node_id), index))
    } else {
        None
    };
    // The remainder is the membership; re-slice since Reader owns position.
    let rest = &buf[r.pos..];
    let membership = decode_membership(rest)?;
    Ok(StoredMembership::new(log_id, membership))
}

// --- Log entries ------------------------------------------------------------

/// Payload-kind tag stored with every log entry.
const TAG_BLANK: u8 = 0;
const TAG_NORMAL: u8 = 1;
const TAG_MEMBERSHIP: u8 = 2;

/// Encode a log entry: `[tag][leader(term,node,index)][payload]`.
/// Normal payload = canonical protobuf bytes of `pb::Command`; membership
/// payload = the hand-encoded membership.
pub fn encode_entry(entry: &openraft::Entry<TypeConfig>) -> Result<Vec<u8>, String> {
    use openraft::EntryPayload;
    let mut out = Vec::new();
    put_u64(&mut out, entry.log_id.leader_id.term);
    put_u64(&mut out, entry.log_id.leader_id.node_id);
    put_u64(&mut out, entry.log_id.index);
    match &entry.payload {
        EntryPayload::Blank => out.push(TAG_BLANK),
        EntryPayload::Normal(cmd) => {
            out.push(TAG_NORMAL);
            let bytes = cmd.to_proto().encode_to_vec();
            put_u64(&mut out, bytes.len() as u64);
            out.extend_from_slice(&bytes);
        }
        EntryPayload::Membership(m) => {
            out.push(TAG_MEMBERSHIP);
            let bytes = encode_membership(m);
            put_u64(&mut out, bytes.len() as u64);
            out.extend_from_slice(&bytes);
        }
    }
    Ok(out)
}

pub fn decode_entry(buf: &[u8]) -> Result<openraft::Entry<TypeConfig>, String> {
    use openraft::EntryPayload;
    let mut r = Reader::new(buf);
    let term = r.u64()?;
    let node_id = r.u64()?;
    let index = r.u64()?;
    let log_id = LogId::new(CommittedLeaderId::new(term, node_id), index);
    let tag = r.take(1)?[0];
    let payload = match tag {
        TAG_BLANK => EntryPayload::Blank,
        TAG_NORMAL => {
            let len = r.u64()? as usize;
            let bytes = r.take(len)?.to_vec();
            let pb_cmd = <crate::pb::mangle::Command as buffa::Message>::decode_from_slice(&bytes)
                .map_err(|e| e.to_string())?;
            EntryPayload::Normal(
                mangle_proto::Command::from_proto(&pb_cmd).map_err(|e| e.to_string())?,
            )
        }
        TAG_MEMBERSHIP => {
            let len = r.u64()? as usize;
            let bytes = r.take(len)?;
            EntryPayload::Membership(decode_membership(bytes)?)
        }
        other => return Err(format!("unknown entry payload tag {other}")),
    };
    r.done()?;
    Ok(openraft::Entry { log_id, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangle_proto::{Command, LoadProgramCommand, MutateCommand, MutationOp, MutationOpKind};
    use mangle_common::Value;

    #[test]
    fn vote_roundtrip() {
        for v in [
            Vote { leader_id: LeaderId::new(0, 0), committed: false },
            Vote { leader_id: LeaderId::new(3, 7), committed: false },
            Vote { leader_id: LeaderId::new(u64::MAX, 42), committed: true },
        ] {
            assert_eq!(decode_vote(&encode_vote(&v)).unwrap(), v);
        }
    }

    #[test]
    fn vote_truncated_errors() {
        assert!(decode_vote(&encode_vote(&Vote { leader_id: LeaderId::new(1, 2), committed: false })[..10]).is_err());
    }

    #[test]
    fn log_id_roundtrip() {
        let id = LogId::new(CommittedLeaderId::new(5, 9), 1234);
        assert_eq!(decode_log_id(&encode_log_id(&id)).unwrap(), id);
    }

    fn sample_membership() -> Membership<u64, BasicNode> {
        let mut nodes = std::collections::BTreeMap::new();
        nodes.insert(1u64, BasicNode { addr: "10.0.0.1:8090".into() });
        nodes.insert(2u64, BasicNode { addr: "10.0.0.2:8090".into() });
        Membership::new(vec![std::collections::BTreeSet::from([1, 2])], nodes)
    }

    #[test]
    fn membership_roundtrip() {
        let m = sample_membership();
        assert_eq!(decode_membership(&encode_membership(&m)).unwrap(), m);
    }

    #[test]
    fn stored_membership_roundtrip() {
        let sm = StoredMembership::new(
            Some(LogId::new(CommittedLeaderId::new(2, 1), 9)),
            sample_membership(),
        );
        assert_eq!(decode_stored_membership(&encode_stored_membership(&sm)).unwrap(), sm);

        let sm_none = StoredMembership::new(None, sample_membership());
        assert_eq!(
            decode_stored_membership(&encode_stored_membership(&sm_none)).unwrap(),
            sm_none
        );
    }

    #[test]
    fn entry_blank_roundtrip() {
        let e = openraft::Entry::<TypeConfig> {
            log_id: LogId::new(CommittedLeaderId::new(1, 1), 5),
            payload: openraft::EntryPayload::Blank,
        };
        let bytes = encode_entry(&e).unwrap();
        assert_eq!(&decode_entry(&bytes).unwrap(), &e);
    }

    #[test]
    fn entry_normal_roundtrip() {
        let cmd = Command::Mutate(MutateCommand {
            program: "graph".into(),
            ops: vec![MutationOp {
                kind: MutationOpKind::Insert,
                relation: "edge".into(),
                tuple: vec![Value::Number(1), Value::String("x".into())],
            }],
        });
        let e = openraft::Entry::<TypeConfig> {
            log_id: LogId::new(CommittedLeaderId::new(4, 2), 17),
            payload: openraft::EntryPayload::Normal(cmd),
        };
        let bytes = encode_entry(&e).unwrap();
        assert_eq!(&decode_entry(&bytes).unwrap(), &e);
    }

    #[test]
    fn entry_membership_roundtrip() {
        let e = openraft::Entry::<TypeConfig> {
            log_id: LogId::new(CommittedLeaderId::new(1, 1), 1),
            payload: openraft::EntryPayload::Membership(sample_membership()),
        };
        let bytes = encode_entry(&e).unwrap();
        assert_eq!(&decode_entry(&bytes).unwrap(), &e);
    }

    #[test]
    fn entry_corrupt_tag_errors() {
        let e = openraft::Entry::<TypeConfig> {
            log_id: LogId::new(CommittedLeaderId::new(1, 1), 5),
            payload: openraft::EntryPayload::Blank,
        };
        let mut bytes = encode_entry(&e).unwrap();
        let last = bytes.len() - 1;
        bytes[last] = 0xFF;
        assert!(decode_entry(&bytes).is_err());
    }
}
