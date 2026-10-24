# mangle-raft

openraft glue for a distributed `mangle-server`: the [`Engine`] facade
around `openraft::Raft` (bootstrap, replicated writes, snapshots) and the
inter-node network implementation.

- Phase 1 (single node): the network is a stub — a single-node cluster
  never contacts peers.
- Phase 2: the network becomes a ConnectRPC client (`RaftService` on the
  main port), adding multi-node consensus.

The state machine (`RaftStateMachine` over `ProgramStore`) lives in
`mangle-server` (it owns `ProgramStore`); this crate is generic glue plus
the wire-level types. See `DISTRIBUTED_PLAN.md` at the workspace root.
