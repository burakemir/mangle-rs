//! Rust mirror of the replicated `Command` envelope
//! (`proto/mangle/command.proto`, see DISTRIBUTED_PLAN.md §3.1).
//!
//! The generated protobuf types are the wire form; these owned types carry
//! internal `mangle_common::Value` tuples and are what the state machine
//! (`ProgramStore::apply`) consumes. Conversions in both directions keep
//! the two forms in lockstep.

use anyhow::{Result, anyhow};

use mangle_common::Value as MValue;

use crate::pb::mangle as pb;
use pb::__buffa::oneof::command::Payload;

/// One replicated command. Applying the same command sequence to two
/// stores must produce identical state and identical apply outcomes.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Mutate(MutateCommand),
    LoadProgram(LoadProgramCommand),
    DeleteProgram(DeleteProgramCommand),
    Noop,
}

/// Inserts and/or retracts of facts in one program, applied atomically
/// (one IDB recompute per command).
#[derive(Clone, Debug, PartialEq)]
pub struct MutateCommand {
    pub program: String,
    pub ops: Vec<MutationOp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationOpKind {
    Insert,
    Retract,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MutationOp {
    pub kind: MutationOpKind,
    pub relation: String,
    pub tuple: Vec<MValue>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadProgramCommand {
    pub name: String,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeleteProgramCommand {
    pub name: String,
}

/// The outcome of applying one [`Command`] to the state machine — the
/// `R` (AppDataResponse) of the replicated protocol. Deterministic: the
/// same command against the same state yields the same outcome on every
/// node.
#[derive(Clone, Debug, PartialEq)]
pub enum ApplyOutcome {
    /// Facts were inserted/retracted. Ops that changed nothing (duplicate
    /// inserts, retracts of absent facts, duplicates within the command)
    /// count as skipped.
    Mutated { num_applied: u64, num_skipped: u64 },
    /// A program was loaded (or re-loaded with new source).
    Loaded { name: String, predicates: Vec<String> },
    /// A program was deleted.
    Deleted,
    /// A no-op (commit barrier for strong reads).
    Noop,
}

/// The response of applying one command: either the deterministic
/// outcome, or a deterministic error message (a command that fails on
/// the leader fails identically on every follower).
pub type ApplyResult = Result<ApplyOutcome, String>;

// --- Snapshot mirrors (§3.6) ---------------------------------------------

/// Mirror of `SnapshotLedgerEntry`.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotLedgerEntry {
    pub relation: String,
    pub tuple: Vec<MValue>,
}

/// Mirror of `SnapshotLedger`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapshotLedger {
    pub replicated: Vec<SnapshotLedgerEntry>,
    pub retracted: Vec<SnapshotLedgerEntry>,
}

/// Mirror of `SnapshotProgram`.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotProgram {
    pub name: String,
    pub source: String,
    pub ledger: SnapshotLedger,
}

/// Mirror of `Snapshot`: the whole FSM state in canonical form.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub programs: Vec<SnapshotProgram>,
}

impl Snapshot {
    pub fn to_proto(&self) -> pb::Snapshot {
        pb::Snapshot {
            programs: self
                .programs
                .iter()
                .map(|p| pb::SnapshotProgram {
                    name: p.name.clone(),
                    source: p.source.clone(),
                    ledger: ::buffa::MessageField::some(ledger_to_proto(&p.ledger)),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    pub fn from_proto(s: &pb::Snapshot) -> Result<Self> {
        Ok(Self {
            programs: s
                .programs
                .iter()
                .map(|p| {
                    Ok(SnapshotProgram {
                        name: p.name.to_string(),
                        source: p.source.to_string(),
                        ledger: p
                            .ledger
                            .as_option()
                            .map(ledger_from_proto)
                            .transpose()?
                            .unwrap_or_default(),
                    })
                })
                .collect::<Result<_>>()?,
        })
    }
}

fn ledger_to_proto(l: &SnapshotLedger) -> pb::SnapshotLedger {
    fn entries(entries: &[SnapshotLedgerEntry]) -> Vec<pb::SnapshotLedgerEntry> {
        entries
            .iter()
            .map(|e| pb::SnapshotLedgerEntry {
                relation: e.relation.clone(),
                tuple: e.tuple.iter().map(crate::convert::value_to_proto).collect(),
                ..Default::default()
            })
            .collect()
    }
    pb::SnapshotLedger {
        replicated: entries(&l.replicated),
        retracted: entries(&l.retracted),
        ..Default::default()
    }
}

fn ledger_from_proto(l: &pb::SnapshotLedger) -> Result<SnapshotLedger> {
    fn entries(entries: &[pb::SnapshotLedgerEntry]) -> Result<Vec<SnapshotLedgerEntry>> {
        entries
            .iter()
            .map(|e| {
                Ok(SnapshotLedgerEntry {
                    relation: e.relation.to_string(),
                    tuple: e
                        .tuple
                        .iter()
                        .map(crate::convert::value_from_proto)
                        .collect::<Result<_>>()?,
                })
            })
            .collect()
    }
    Ok(SnapshotLedger {
        replicated: entries(&l.replicated)?,
        retracted: entries(&l.retracted)?,
    })
}

impl Command {
    /// Serialize to the protobuf wire form.
    pub fn to_proto(&self) -> pb::Command {
        let (type_, payload) = match self {
            Command::Mutate(m) => (
                pb::command::Type::CommandTypeMutate,
                Some(Payload::Mutate(Box::new(m.to_proto()))),
            ),
            Command::LoadProgram(l) => (
                pb::command::Type::CommandTypeLoadProgram,
                Some(Payload::LoadProgram(Box::new(l.to_proto()))),
            ),
            Command::DeleteProgram(d) => (
                pb::command::Type::CommandTypeDeleteProgram,
                Some(Payload::DeleteProgram(Box::new(d.to_proto()))),
            ),
            Command::Noop => (
                pb::command::Type::CommandTypeNoop,
                Some(Payload::Noop(Box::default())),
            ),
        };
        pb::Command {
            r#type: type_.into(),
            payload,
            ..Default::default()
        }
    }

    /// Deserialize from the protobuf wire form.
    pub fn from_proto(cmd: &pb::Command) -> Result<Self> {
        let payload = cmd.payload.as_ref().ok_or_else(|| {
            anyhow!("command payload is required (mutate, load_program, delete_program or noop)")
        })?;
        Ok(match payload {
            Payload::Mutate(m) => Command::Mutate(MutateCommand::from_proto(m)?),
            Payload::LoadProgram(l) => Command::LoadProgram(LoadProgramCommand::from_proto(l)),
            Payload::DeleteProgram(d) => {
                Command::DeleteProgram(DeleteProgramCommand::from_proto(d))
            }
            Payload::Noop(_) => Command::Noop,
        })
    }
}

impl MutateCommand {
    pub fn to_proto(&self) -> pb::MutateCommand {
        pb::MutateCommand {
            program: self.program.clone(),
            ops: self.ops.iter().map(MutationOp::to_proto).collect(),
            ..Default::default()
        }
    }

    pub fn from_proto(cmd: &pb::MutateCommand) -> Result<Self> {
        Ok(Self {
            program: cmd.program.to_string(),
            ops: cmd
                .ops
                .iter()
                .map(MutationOp::from_proto)
                .collect::<Result<_>>()?,
        })
    }
}

impl MutationOp {
    pub fn to_proto(&self) -> pb::MutationOp {
        pb::MutationOp {
            op: match self.kind {
                MutationOpKind::Insert => pb::MutationOpKind::MutationOpInsert,
                MutationOpKind::Retract => pb::MutationOpKind::MutationOpRetract,
            }
            .into(),
            relation: self.relation.clone(),
            tuple: self.tuple.iter().map(crate::convert::value_to_proto).collect(),
            ..Default::default()
        }
    }

    pub fn from_proto(op: &pb::MutationOp) -> Result<Self> {
        let kind = match op.op.as_known() {
            Some(pb::MutationOpKind::MutationOpInsert) => MutationOpKind::Insert,
            Some(pb::MutationOpKind::MutationOpRetract) => MutationOpKind::Retract,
            _ => {
                return Err(anyhow!(
                    "unknown mutation op kind: {}",
                    op.op.to_i32()
                ))
            }
        };
        Ok(Self {
            kind,
            relation: op.relation.to_string(),
            tuple: op
                .tuple
                .iter()
                .map(crate::convert::value_from_proto)
                .collect::<Result<_>>()?,
        })
    }
}

impl LoadProgramCommand {
    pub fn to_proto(&self) -> pb::LoadProgramCommand {
        pb::LoadProgramCommand {
            name: self.name.clone(),
            source: self.source.clone(),
            ..Default::default()
        }
    }

    pub fn from_proto(cmd: &pb::LoadProgramCommand) -> Self {
        Self {
            name: cmd.name.to_string(),
            source: cmd.source.to_string(),
        }
    }
}

impl DeleteProgramCommand {
    pub fn to_proto(&self) -> pb::DeleteProgramCommand {
        pb::DeleteProgramCommand {
            name: self.name.clone(),
            ..Default::default()
        }
    }

    pub fn from_proto(cmd: &pb::DeleteProgramCommand) -> Self {
        Self {
            name: cmd.name.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_command() -> Command {
        Command::Mutate(MutateCommand {
            program: "runtime".into(),
            ops: vec![
                MutationOp {
                    kind: MutationOpKind::Insert,
                    relation: "container".into(),
                    tuple: vec![
                        MValue::String("web".into()),
                        MValue::Number(42),
                        MValue::Float(1.5),
                    ],
                },
                MutationOp {
                    kind: MutationOpKind::Retract,
                    relation: "container".into(),
                    tuple: vec![MValue::String("db".into()), MValue::Number(7), MValue::Float(0.0)],
                },
            ],
        })
    }

    #[test]
    fn command_roundtrip_mutate() {
        let cmd = sample_command();
        let proto = cmd.to_proto();
        let back = Command::from_proto(&proto).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn command_roundtrip_load_delete_noop() {
        for cmd in [
            Command::LoadProgram(LoadProgramCommand {
                name: "p".into(),
                source: "q(X) :- p(X).".into(),
            }),
            Command::DeleteProgram(DeleteProgramCommand { name: "p".into() }),
            Command::Noop,
        ] {
            let back = Command::from_proto(&cmd.to_proto()).unwrap();
            assert_eq!(cmd, back);
        }
    }

    #[test]
    fn from_proto_rejects_missing_payload() {
        let proto = pb::Command::default();
        assert!(Command::from_proto(&proto).is_err());
    }

    #[test]
    fn from_proto_rejects_unknown_op_kind() {
        let proto = pb::MutationOp {
            op: buffa::EnumValue::Unknown(99),
            relation: "p".into(),
            ..Default::default()
        };
        assert!(MutationOp::from_proto(&proto).is_err());
    }

    #[test]
    fn snapshot_roundtrip() {
        let snap = Snapshot {
            programs: vec![SnapshotProgram {
                name: "graph".into(),
                source: "path(X,Y) :- edge(X,Y).".into(),
                ledger: SnapshotLedger {
                    replicated: vec![SnapshotLedgerEntry {
                        relation: "edge".into(),
                        tuple: vec![MValue::Number(1), MValue::String("a".into())],
                    }],
                    retracted: vec![SnapshotLedgerEntry {
                        relation: "edge".into(),
                        tuple: vec![MValue::Number(2), MValue::Name("/n".into())],
                    }],
                },
            }],
        };
        let back = Snapshot::from_proto(&snap.to_proto()).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn snapshot_program_without_ledger_decodes_empty() {
        let proto = pb::SnapshotProgram {
            name: "p".into(),
            source: "".into(),
            ledger: ::buffa::MessageField::none(),
            ..Default::default()
        };
        let snap =
            Snapshot::from_proto(&pb::Snapshot { programs: vec![proto], ..Default::default() })
                .unwrap();
        assert_eq!(snap.programs[0].ledger, SnapshotLedger::default());
    }
}
