# mangle-wal

The write-ahead log (WAL) for a distributed `mangle-server`: the Raft log
and vote stores, backed by [redb](https://redb.rs) (a pure-Rust, ACID,
single-file embedded B+tree — the direct analog of rqlite's `raft-boltdb`
on bbolt).

This crate also declares the Raft `TypeConfig` used across the mangle
Raft crates:

- `D` (AppData) = [`mangle_proto::Command`] — the replicated command
  envelope, serialized in the log as canonical protobuf bytes.
- `R` (AppDataResponse) = [`mangle_proto::ApplyResult`] — the
  deterministic apply outcome.
- `SnapshotData` = `tokio::fs::File` — snapshots are single files
  produced/consumed by the state machine (see `mangle-raft`).

See `DISTRIBUTED_PLAN.md` at the workspace root for the overall design.
