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
}
