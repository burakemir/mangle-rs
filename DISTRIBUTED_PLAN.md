# Distributed Mangle Server — Gap Analysis & Plan

Goal: turn `mangle-server` into a rqlite-style distributed, replicated database:
a Raft-replicated cluster where every write goes through consensus and every
node can serve queries. The plan builds on what already exists (ConnectRPC +
protobufs, `ProgramStore`/`Database` mutation + recompute machinery) and adds
the two missing pieces: a real write-ahead log and Raft consensus.

---

## 1. Where we are today

### What exists and is reusable

| rqlite concept | Mangle equivalent (today) | State |
|---|---|---|
| SQLite database | `mangle_db::Database` (per program) | ✅ exists |
| SQL execute/query dispatch | `ProgramStore::{load, remove, insert_fact, retract_fact, execute_query}` | ✅ exists |
| Deferred recompute per transaction | `Database::batch()` (deferred IDB recompute on `commit`) | ✅ exists — maps well to "one command = one log entry" |
| Incremental maintenance | `RecomputeStrategy::Incremental` (DRed + provenance) | ✅ exists (determinism must be verified) |
| RPC API | ConnectRPC `MangleService` (`mangle-proto`, buffa) | ✅ exists |
| Durability for writes | `MutationLog` (`mutations.mg` text append) | ⚠️ not a WAL (see below) |
| Fast restart / IDB cache | `IdbBackend` fingerprint cache (program hash + EDB fingerprint) | ✅ exists — analog of rqlite's clean-snapshot fingerprint idea |
| External data sources | `FileEdbSource`, Parquet, Delta adapters | ✅ exists (not replicated — see §4.6) |

### Why `MutationLog` is not a WAL

1. **Apply-then-log, not log-then-apply.** `ProgramStore::insert_fact` applies
   the mutation to the in-memory `Database` first, then appends to
   `mutations.mg`. A crash in between loses a write the client was told
   succeeded. A WAL requires the durable append to happen *before* the
   response.
2. **No fsync / group commit.** The file is opened with `append(true)` and
   written with no `sync_all` — a crash can truncate it.
3. **No log metadata.** No indexes, terms, checksums, or ability to truncate
   (it grows forever; compaction = rewriting the whole file).
4. **Not the recovery source of truth.** Restart actually rebuilds from
   `programs_dir` + `FileEdbSource` post-processing of `__retract__` facts,
   which is an implicit, fragile replay — and it says nothing about program
   load/delete operations, which are also mutable state today (a `LoadProgram`
   via RPC is not durable at all unless the operator also writes the `.mg`
   file).
5. **No ordering guarantee relative to evaluation**, no notion of a committed
   index, so no way to express "read what I wrote".

### What is completely missing

- **Consensus**: nothing replicates state between nodes; no leader, no
  followers, no membership.
- **A replicated command abstraction**: writes arrive as RPCs and mutate local
  state directly; there is no serializable "command" unit that could be
  proposed to a log and applied identically on every node.
- **Log storage**: no durable, index-addressable log store (rqlite uses
  `raft-boltdb` on top of bbolt).
- **Snapshots of the state machine**: no way to capture/restore the FSM
  (program registry + EDB facts), and no log compaction.
- **Inter-node transport**: no RPC channel between mangle servers.
- **Leader awareness / forwarding**: no NotLeader errors, no leader hint, no
  read consistency levels.
- **Cluster administration**: no bootstrap/join/remove/status.

---

## 2. rqlite architecture (the template), in brief

```
HTTP/Connect client
      │
      ▼
Store ──── executes through ───► raft.Apply(marshaled proto.Command)   [leader only]
  │                                     │
  │                                     ▼
  │                        Raft log (raft-boltdb, fsynced)  ◄── this IS the WAL
  │                                     │  consensus (hashicorp/raft over TCP mux)
  │                                     ▼
  └── FSM.Apply ──► CommandProcessor ──► SQLite
        │
        ├── FSM.Snapshot / FSM.Restore  (snapshot store, WAL streaming)
        └── reads: NONE (any node) / WEAK (leader local) /
                   STRONG (through the log) / LINEARIZABLE (read-index)
```

Key ideas to port:

1. **The Raft log is the WAL.** rqlite has no separate write-ahead log; every
   write is a serialized `proto.Command` proposed to Raft. Durability = log
   fsync before the commit is acknowledged.
2. **One command per client write batch.** rqlite marshals an entire statement
   list into one log entry, so the FSM applies it as one atomic unit.
3. **FSM = a thin, deterministic command processor** (`CommandProcessor`) that
   is deliberately decoupled from the Store so it can be reused for replay
   (quorum recovery).
4. **Membership through the log** (join/remove/demote), cluster bootstrapping
   (`Bootstrap` / `Notify` + `BootstrapExpect` / `Join`).
5. **Consistency levels** for reads, from local-cheap to through-the-log.
6. **Snapshot + restore** of the whole database file, with log truncation.

---

## 3. Proposed architecture for Mangle

```
ConnectRPC client
      │  (mangle.MangleService — unchanged client API, plus consistency fields)
      ▼
MangleServiceImpl
      │  writes: build Command, forward to leader if follower
      ▼
mangle-raft (new crate): Raft<...> (openraft)
      │                                     │
      │                                     ▼
      │                  mangle-wal (new): RaftLogStore over redb  ◄── the WAL
      │                                     │  Vote/AppendEntries over
      │                                     │  ConnectRPC RaftService (same port)
      ▼                                     ▼
ProgramStore::apply(Command)   ───────────► every node applies the same
  (the FSM: Database::batch, load, delete)     command in the same order
      │
      ├── build_snapshot / install_snapshot (EDB + registry, recompute on restore)
      └── reads: local query on applied state (consistency levels control freshness)
```

### 3.1 Command envelope (new in `mangle-proto`)

Direct analog of rqlite's `command.proto`. The FSM state is: **program
registry (name → source)** + **mutated EDB facts per program**. Every write to
that state becomes a `Command`:

```proto
message Command {
  enum Type {
    COMMAND_TYPE_UNKNOWN = 0;
    COMMAND_TYPE_MUTATE = 1;      // insert/retract fact batches
    COMMAND_TYPE_LOAD_PROGRAM = 2;
    COMMAND_TYPE_DELETE_PROGRAM = 3;
    COMMAND_TYPE_NOOP = 4;        // used for strong reads / leader commit
  }
  Type type = 1;
  oneof payload {
    MutateCommand mutate = 2;
    LoadProgramCommand load_program = 3;
    DeleteProgramCommand delete_program = 4;
    NoopCommand noop = 5;
  }
}

message MutateCommand {
  string program = 1;
  // The whole client-streamed mutation request collapses into ONE command,
  // applied via Database::batch() → one IDB recompute per log entry.
  // Enforce a configurable cap on ops per command server-side (§7.2);
  // bulk loads should use batch payloads, not unbounded streams.
  repeated MutationOp ops = 2;
}

message MutationOp {
  enum Op { UNKNOWN = 0; INSERT = 1; RETRACT = 2; }
  Op op = 1;
  string relation = 2;
  repeated Value tuple = 3;   // reuse mangle/value.proto
  // or batch-form: FactBatch from the existing proto
}
```

The existing `MangleService` RPC signatures stay the same —
`InsertFacts`/`RetractFacts` still stream `MutationRequest`s to the node the
client talks to; the *node* aggregates the stream into one `MutateCommand` and
proposes it. `LoadProgram`/`DeleteProgram` likewise become commands.

**`ReloadProgram`/`ReloadAll` are disabled on the replicated write path**
(they return an error once the Raft-backed store is active, which after
Phase 1 is always). They read the operator-managed local `programs_dir`,
which is per-node filesystem state — applying them would change one node's
state without going through the log and instantly diverge the cluster.
`programs_dir` is consulted exactly once, at first startup of the bootstrap
node when the Raft log is empty: each `.mg` file is proposed as a
`LoadProgram` command, so the file-based workflow imports into the
replicated registry. From then on the registry lives in the log +
snapshots, and the "edit and reload" workflow is replaced by `LoadProgram`
with the new source.

### 3.2 Consensus library: openraft

rqlite uses `hashicorp/raft`; that's Go-only. The Rust landscape:

| Option | Assessment |
|---|---|
| **openraft** (recommended) | Actively maintained, async-first (tokio), typed `Raft<T>`, requires implementing `RaftLogStore` + `RaftStateMachine` + a `Network` trait. Well-documented examples. Closest match to the hashicorp/raft programming model (Apply → FSM), so the rqlite architecture translates nearly 1:1. |
| tikv/raft-rs | Low-level (log + state machine roles, no `Apply` plumbing, no storage, no transport). Would mean writing a lot of what openraft and rqlite already give us. |
| Write our own Raft | Explicitly rejected: consensus is the one thing you should never hand-roll. |

openraft gives us, mapping to rqlite concepts:

- `Raft::client_write(AppData)` → `raft.Apply()` (leader-only writes, returns
  after commit + apply).
- `RaftStateMachine` (`handle_apply` / `begin_receiving_snapshot` / ...) →
  rqlite's `FSM` (`Apply`/`Snapshot`/`Restore`).
- `RaftLogStore` (append logs, truncate, vote store) → raft-boltdb role.
- Native membership changes (`change_membership`) → rqlite's Join/Remove
  (which internally do `raft.AddVoter`/`RemoveServer` anyway).
- Built-in snapshot triggering by log-entry count → rqlite's
  `SnapshotThreshold`; we add size-based triggers as needed later.

### 3.3 Log storage: the WAL (new crate `mangle-wal`)

rqlite: `raft-boltdb` = Raft log + stable store on **bbolt**.
Mangle: implement `RaftLogStore` on **redb** — a pure-Rust, single-file, ACID
B+tree embedded database, i.e. the closest thing to bbolt in Rust.

- One `raft.redb` file in the node's data dir holding: vote/term state, log
  entries (`(index) → entry bytes`), and the snapshot metadata pointer.
- redb gives us commit-durability with `Durability::Immediate` (= fsync per
  commit), which is exactly the WAL property we need; group commit falls out
  of batching writes per append call.
- Keep `mangle-wal` deliberately thin (like rqlite's `store/log` package):
  open, append, truncate, get range, indexes, plus small helpers for "last
  command index" used during recovery/bootstrap decisions.

Alternative considered: a hand-written segmented log file. More code, more
edge cases (partial writes, checksums, compaction); no benefit over redb.

**`MutationLog` is retired** (deprecated, then removed): the Raft log +
snapshots fully replace it, and fixing its apply-then-log ordering is not
worth doing given it's about to be superseded. Restart replay = openraft
replaying the log into the FSM after snapshot restore, not `.mg` text
post-processing.

### 3.4 The FSM: `ProgramStore` becomes a deterministic state machine

New method on `ProgramStore`:

```rust
impl ProgramStore {
    /// Apply one replicated command. Must be deterministic.
    pub fn apply(&mut self, cmd: &Command) -> Result<ApplyResult>;
}
```

- `MutateCommand` → look up program, use `Database::batch()` to apply all ops,
  `commit()` once (one IDB recompute per entry — the deferred-recompute batch
  machinery already exists and is exactly right for this).
- `LoadProgram` / `DeleteProgram` → existing `load()`/`remove()`.
- Returns an `ApplyResult` (per-relation counts, program info) that the leader
  ships back to the waiting RPC caller — the openraft analog of rqlite's
  `FSM.Apply` return value (`CommandProcessor` returns the SQLite result).

`MangleServiceImpl` handlers stop mutating `ProgramStore` directly for writes;
they build a `Command` and call `mangle-raft`'s write path. Reads
(`Query`, `Eval`, `GetProgram`, `ListPrograms`) keep hitting the shared
`ProgramStore` under the read lock, exactly as today.

### 3.5 Inter-node transport: a `RaftService` over ConnectRPC

rqlite multiplexes inter-node Raft traffic on its main TCP port with a custom
mux and hand-rolled protobuf RPCs. We already have ConnectRPC + protobuf
infrastructure, so we add a second service to the same router:

```proto
service RaftService {
  rpc Vote(VoteRequest) returns (VoteResponse);
  rpc AppendEntries(AppendEntriesRequest) returns (AppendEntriesResponse);
  // Leader → follower chunked snapshot transfer (leader is the client
  // streaming chunks; follower installs into a staging file then swaps).
  rpc InstallSnapshot(stream SnapshotChunk) returns (SnapshotResponse);
}
```

These messages are internal (node-to-node), wire-encoded with the same
protobuf/buffa stack; openraft's `Network` trait is implemented on top of a
ConnectRPC client. Payload compression (rqlite compresses commands above a
threshold) can be added later — Connect already supports gzip.

### 3.6 Snapshots

- **Content**: program registry (name → source) + the replicated (mutated)
  EDB facts. Both are mandatory — without the source the `Database` cannot
  be rebuilt (rules are needed to recompute IDB, and EDB/IDB relation
  classification comes from compiling the source), and after log compaction
  the snapshot is the only remaining record of a program's source. Facts
  loaded from *external* EDB sources (Parquet/Delta/file dirs) are
  per-node inputs (§4.4) and are **not** in the snapshot: on restore, the
  external sources are re-attached from local config and re-read, exactly
  as on first open. Since the FSM only ever writes replicated facts, the
  `MutateCommand` path must record facts in a way that survives the working
  store mixing them with externally loaded ones — e.g. the FSM keeps its
  own replicated-fact ledger per program (name → relation → tuples, with
  retractions applied), which is what gets serialized into the snapshot.
  IDB is *derived* and deliberately excluded from the canonical snapshot —
  every node recomputes deterministically. (Optionally include IDB later as a
  restart-acceleration cache, validated by the existing `IdbBackend`
  fingerprint scheme — the direct analog of rqlite's clean-snapshot
  fingerprint, where a mismatch just forces recompute rather than being
  fatal.)
- **Format**: a length-framed container with two sections — registry
  (name → source) and facts — with the fact section reusing the columnar
  `FactBatch` encoding (already the wire format for query results and fact
  mutations), so snapshot bytes are cheap to produce and parse.
- **Triggering**: openraft's log-count threshold initially; add
  "mutations.mg-grown-too-big"-style size triggers later.
- **Install**: streamed over `InstallSnapshot`, staged to disk, atomically
  swapped, then the FSM rebuilds `Database` objects (compile + load EDB +
  recompute — one `Database::open` per program).

### 3.7 Reads and consistency levels

Phase it like rqlite, cheapest first:

1. **NONE** — any node answers from its local applied state. Default.
2. **WEAK** — leader-only local read (`ErrNotLeader` otherwise).
3. **STRONG** — the read is serialized through the log: propose a `NOOP`,
   wait for it to be applied, then serve locally. (For a Datalog database the
   result set doesn't depend on evaluation-at-read-time, so a NOOP barrier
   after the client's previous write is sufficient for read-your-writes.)
4. **LINEARIZABLE** (later) — read-index protocol: confirm leadership with a
   quorum heartbeat, wait for the local applied index to reach the commit
   index.

Add a `ConsistencyLevel` enum to `QueryRequest` (one field, backward
compatible — defaults to NONE).

### 3.8 Leader forwarding & cluster admin

- Non-leader write → Connect error with `not_leader` semantics + leader
  address hint (rqlite pattern), plus an opt-in server-side forward so simple
  clients just work.
- New admin RPCs (either on `MangleService` or a separate `AdminService`):
  `NodeStatus` (node id, raft state, leader, commit/apply index, term),
  `Join`, `RemoveNode`, `Leader`. Bootstrap is a startup flag
  (`--bootstrap`), like rqlite's `-bootstrap`. `Join`/`RemoveNode` map onto
  openraft's `change_membership` executed on the leader.
- rqlite's `Notify`+`BootstrapExpect` discovery dance is explicitly **out of
  scope** for v1 — static `--join` lists and an explicit bootstrap node cover
  the operator experience (rqlite itself lists it as an advanced path).

### 3.9 Configuration (new `ServerConfig` fields)

```
--node-id <id>              # stable identity (must not change across restarts)
--raft-dir <dir>            # raft.redb + snapshot files (default: data dir)
--bootstrap                 # start a single-node cluster (exactly once)
--join <addr>[,<addr>...]   # contact these nodes to join an existing cluster
--raft-port <port>          # or mount RaftService on the main port (default)
```

Single-node clusters (openraft with one voter) remain fully supported — this
replaces the current "no consensus" mode, so there is exactly one write path
("always Raft", like rqlite). The write path in single-node mode is: append to
local WAL (fsync) → apply → respond; the durability fix happens on day one.

---

## 4. Key design decisions & risks

### 4.1 Determinism of the FSM (the big one)

Every node applies the same command sequence and must reach identical state.
Datalog evaluation is confluent (semi-naive bottom-up derives the same fact
set regardless of iteration order), and `Value` equality is exact
(`Float` compares via `to_bits`), so derived *facts* should be identical.
But this must be verified, not assumed:

- **Audit**: no wall-clock/random/UUID inputs inside `Database::apply`
  paths; check `IdbBackend` cache metadata writes (timestamps there are cache
  metadata, not FSM state — fine, but confirm they don't feed back).
- **Test**: replay a randomized command sequence against two independent
  `ProgramStore`s, byte-compare serialized state. Add as a CI fuzz test.
- Start with `RecomputeStrategy::Full` on the FSM path. The Incremental
  (DRed) strategy is a performance upgrade *after* its determinism is
  fuzz-tested the same way.

### 4.2 Cost of apply: full IDB recompute per committed entry

A single fact insert currently triggers `recompute_idb` (clear IDB, re-run).
Throughput of the cluster is bounded by this. Mitigations, in order:

1. **Batch aggressively** (already planned): the client-streaming
   `InsertFacts` RPC collapses into one command → one recompute.
2. **Debounced async recompute with applied-index watermark**: the FSM applies
   facts synchronously (cheap), schedules a recompute, and queries gate on the
   watermark for WEAK/STRONG reads. This is rqlite-shaped: durable log first,
   secondary indexes (here: IDB) catch up. Worth designing for, but **ship
   synchronous-first** and keep this as a recognized optimization seam.
3. Incremental DRed (existing code) once determinism is proven.

### 4.3 `IdbBackend` cache writes inside FSM apply

`recompute_idb` saves an IDB snapshot to the cache backend on every apply.
During log replay / snapshot installs on a lagging follower this could thrash
the disk. Keep it (it's what makes restart fast) but make it async or
skippable during bulk replay.

### 4.4 External EDB sources are inputs, not replicated state

Parquet/Delta/plain-file EDB sources are read-only *inputs* that each node
must be able to read independently (like rqlite's read-only filesystem
constraints, or more precisely: not replicated at all). Two consequences:

- Document it: cluster members need shared access to external source paths.
- The *fingerprint* of external EDB (already part of the IDB cache key) should
  also be checked for **cluster-wide consistency**: a node with a stale
  external source silently diverges. v1: expose fingerprints in `NodeStatus`
  so operators can compare. Later: refuse STRONG reads if fingerprints
  diverge from the leader's (recorded at apply time).

The server-managed per-program EDB directory (`FileEdbSource` over
`{edb_dir}/{program}/`) is *replaced* by the replicated mutated-EDB fact set
in the FSM; the `mutations.mg` file and its `__retract__` post-processing
path are retired along with `MutationLog`.

### 4.5 Same-port vs separate Raft port

rqlite multiplexes everything on one port. Mounting `RaftService` on the
existing ConnectRPC router is simpler (one listener, one TLS config) and
Connect protocol carries it fine. Decide only if/when we need unencrypted
low-latency LAN traffic separate from the client-facing TLS port.

### 4.6 What rqlite has that we explicitly defer or skip

- CDC (`cdc/`) — skip for now; a future "subscribe to applied commands"
  stream falls naturally out of the FSM apply loop.
- Auto-backup/restore (`auto/`), uploading snapshots to S3 — skip.
- Compressed inter-node snapshot transport, multi-transport (TLS mesh) — later.
- Multi-node store tests (`store_multi_test.go`) — needed; see test plan.
- Linearizable reads — phase 3, after STRONG exists.
- Notify/BootstrapExpect discovery — out of scope for v1 (see §3.8).

---

## 5. Implementation plan

### Phase 0 — FSM extraction (no consensus, ~no behavior change)

**Status: implemented** (JSON HTTP API removal pre-step: 0.10.0).

**Goal: make `ProgramStore` a deterministic command processor and fix the
durability-ordering bug conceptually (the bug itself is fixed by Phase 1).**

1. ✅ Define `Command` in `mangle-proto` (`command.proto` in the mangle
   package, reusing `Value`) and a Rust mirror enum with proto
   round-trip conversions.
2. ✅ Implement `ProgramStore::apply(&mut self, cmd) -> ApplyOutcome`;
   `MangleServiceImpl` writes route through it. `InsertFacts`/
   `RetractFacts` streams aggregate into one `MutateCommand` (single-program
   streams only) applied via `Database::batch()`. The replicated-fact
   ledger (§3.6) is in place and backs `MutationSummary`'s `num_skipped`
   accounting (retract-of-absent is a non-error no-op, §7.4); streams are
   capped at `MAX_MUTATIONS_PER_COMMAND` ops (§7.2).
3. ✅ `LoadProgram`/`DeleteProgram` go through `apply`. `ReloadProgram`/
   `ReloadAll` still work in 0.10.x (local admin ops); they error on the
   replicated path from Phase 1.
4. ✅ Determinism audit + two-store replay equivalence test (§4.1):
   `tests/determinism.rs` replays 400+200 pseudo-random commands against
   two independent stores, asserting per-command outcome equality and
   canonical-state equality. **Two findings, both fixed:**
   - `ProgramStore::load` returned predicate lists in HashMap iteration
     order — two identical stores produced different `LoadProgram`
     responses. Predicate lists are now sorted.
   - **Mutations of source-defined relations silently lost data**: relations
     defined by the program source (facts *or* rules) are re-derived by
     every IDB recompute, so inserts into them vanished on the next
     mutation and retracts resurrected. The command path now rejects such
     mutations up front (`invalid_argument`, whole command fails
     atomically) instead of losing the write — a deliberate behavior
     change: runtime-mutable data must live in EDB relations (declared
     without facts in source, seeded via commands or EDB source files).

**Deliverable** (met): all mutations flow through one serializable command
type, applied atomically with deterministic outcomes; the two-store replay
equivalence test is in CI. The one intended behavior change is the
rejection of mutations to source-defined relations (previously silently
lost, see finding above).

### Phase 1 — WAL + single-node Raft (openraft, cluster of one)

**Status: implemented** (opt-in via `config_schema.raft_dir(...)`; the
  pre-replication write path remains the default for this release).

**Goal: crash-safe single node. The Raft log replaces `MutationLog`.**

1. ✅ New crate `mangle-wal`: `RaftLogStorage` + `RaftLogReader` over redb
   (`raft.redb`), every write committed with `Durability::Immediate`
   (fsync). Entries serialize as canonical protobuf `Command` bytes plus
   hand-encoded Raft metadata (vote/log-id/membership) — openraft's serde
   feature is deliberately not enabled, so `Command`/`Value` stay
   serde-free. Hosts the shared `TypeConfig` (`D = Command`,
   `R = ApplyResult`, `SnapshotData = tokio::fs::File`).
2. ✅ New crate `mangle-raft`: `Engine` facade (bootstrap, `write` →
   fsync→commit→apply→respond, snapshot trigger, metrics, graceful
   shutdown) + `NoNetwork` stub (single node has no peers).
3. ✅ Snapshot support (§3.6): `mangle-proto` defines the `Snapshot`
   message (registry + fact ledgers); `FsmStore` (in `mangle-server`, it
   owns `ProgramStore`) implements `RaftStateMachine` — apply, build,
   receive/install, get-current — with snapshot data + meta written
   atomically (tmp+rename+fsync) in the raft dir.
4. ✅ Server startup: `config_schema.raft_dir/...` enables replicated
   mode: restore snapshot → open WAL → engine start (log replay) →
   bootstrap if fresh (`raft_bootstrap(1)`, exactly once) → import
   `programs_dir` as `LoadProgram` commands → one-time `mutations.mg`
   migration as `MutateCommand`s (file renamed `.migrated` so the file EDB
   source stops reading it). `ReloadProgram`/`ReloadAll` return
   `failed_precondition` in replicated mode (§3.1). Failed commands are
   deterministic: committed, applied as `Err(msg)` responses, replay
   identically.
5. ✅ Tests (`tests/raft.rs`): acknowledged writes survive restart (WAL
   replay); snapshot + post-snapshot writes restore correctly; failed
   commands replay identically without partial application; RPC writes go
   through Raft and Reload* is disabled; snapshot round-trip with
   retracted-external ledger entries; legacy `mutations.mg` parsing. Plus
   WAL/codec unit tests and a manual binary smoke test (bootstrap → RPC
   writes → kill → restart → derived facts intact).

**Deliverable** (met): single-node mangle-server with real WAL durability,
opt-in via config. The legacy `MutationLog` path still exists for
non-replicated mode; it is deleted when replicated mode becomes the
default (see Phase 2 wrap-up).

### Phase 2 — Multi-node consensus

**Status: implemented.**

**Goal: a 3-node replicated cluster.**

1. ✅ `raft.proto` (`mangle.RaftService`: Vote / AppendEntries /
   InstallSnapshot) + ConnectRPC implementation on the main port
   (`RaftServiceImpl`); openraft `RaftNetwork` over a ConnectRPC client
   (`mangle-raft` `ConnectNetwork`). Node addresses come from the Raft
   membership (`BasicNode.addr`) — no separate peer table. Entry and
   membership payloads ride as mangle-wal-coded bytes: one canonical
   serialization. RPC-level failures travel in-band (response `error`
   field) and map to retryable `Unreachable` on the sender.
2. ✅ Leader forwarding: follower writes fail with `failed_precondition`
   and a leader hint ("leader is node 3 at 127.0.0.1:18103"). Transparent
   server-side forwarding is deferred (clients redirect on the hint).
3. ✅ Membership: static multi-node bootstrap
   (`raft_bootstrap_node(Id, Addr)` on the bootstrapping node; other
   nodes start fresh and wait to be contacted) and dynamic join — the
   `Join` admin RPC (version-checked per §7.5) adds the caller as learner
   then voter; `RemoveNode` via `change_membership`; `NodeStatus`
   reports membership. New config keys: `raft_addr`, `raft_bootstrap_node`,
   `raft_join`.
4. ✅ Tests (`tests/cluster.rs`, real TCP): 3-node bootstrap, leader
   election, replicated writes visible on all nodes; follower writes
   rejected with hint; leader failover (new leader elected, writes
   continue, old leader restarted and catches up); join flow (a new node
   joins via RPC, receives state, becomes voter). `ReloadProgram` errors
   on any node (Phase 1 tests). Verified against real binaries:
   bootstrap → replicate → kill -9 leader → failover → restart → WAL
   replay catch-up.

**Deliverable** (met): 3-node mangle cluster, writes linearizable via
consensus, reads default to NONE (any node). Deferred from this phase:
transparent write forwarding, membership-change-under-load tests,
partitioned-follower catch-up via snapshot transfer (the chunked snapshot
path is exercised only via join), remove-leader tests, quorum-loss manual
recovery docs.

### Phase 3 — Consistency levels & client ergonomics

**Status: implemented** (LINEARIZABLE deferred — optional per plan).

**Goal: rqlite-parity read semantics.**

1. ✅ `ConsistencyLevel` on `QueryRequest` (`NONE` default / `WEAK` /
   `STRONG`):
   - **NONE** — any node answers from its local applied state (cheap,
     eventually consistent; outside replicated mode it is trivially
     exact, so all levels behave the same there).
   - **WEAK** — the leader answers locally; followers fail with
     `failed_precondition` + leader hint.
   - **STRONG** — the leader proposes a `Noop` command through Raft and
     waits for it to apply (`Engine::read_barrier`); the response then
     reflects everything committed before the read. Followers reject
     (a barrier is a write).
2. ✅ `NodeStatus` reports per-program fingerprints (program source
   hash + external-EDB fingerprint, hex-encoded) so operators can
   compare nodes for external-EDB divergence (§4.4). Node status/leader
   identification is via `NodeStatus` and error hints; separate
   leader-change event subscription is deferred (the client redirect
   workflow is covered).
3. ~~Optional: LINEARIZABLE via read-index.~~ Deferred to Phase 4+.

**Deliverable** (met): rqlite-parity read semantics. Cluster tests
cover all three levels (leader/follower acceptance and rejection,
barrier advancing the applied index, fingerprint agreement across
nodes); verified against real binaries.

### Phase 4 — Production hardening

1. Snapshot policy tuning (size thresholds, upload throttling), WAL
   maintenance, disk-usage metrics.
2. TLS + basic auth on both services (mangle-server already has auth
   scaffolding for the client API; extend to node-to-node).
3. Backpressure/throttling on writes (rqlite's `throttler` analog) —
   important because apply cost is recompute-bound (§4.2).
4. Debounced async IDB recompute with applied-index watermark (§4.2) if
   throughput requires it.
5. Ops docs: cluster formation, node replacement, quorum-loss manual
   recovery (openraft: start a fresh node, restore snapshot, re-bootstrap —
   document the exact recipe).

---

## 6. New crate layout

```
crates/mangle-wal/     # RaftLogStore + vote store over redb (rqlite: store/log, raft-boltdb)
crates/mangle-raft/    # openraft glue: Raft<Command>, FSM adapter, Network, config
crates/mangle-proto/   # + command.proto (Command envelope), + raft.proto (RaftService)
crates/mangle-server/  # + config, admin RPCs, forwarding; write path routes through mangle-raft
```

Dependency additions: `openraft`, `redb`. (Both pure-Rust, both maintained.)

---

## 7. Open questions for review

1. ~~**Program registry replication**~~ **Resolved**: the program registry
   *is* the multi-database layer — each program is an independent database
   (own source, own EDB/IDB). Only RPC-driven `LoadProgram`/`DeleteProgram`
   are replicated; `ReloadProgram`/`ReloadAll` are errors on the replicated
   write path (§3.1), with `programs_dir` used only for the one-time import
   at bootstrap.
2. ~~**MutateCommand granularity**~~ **Resolved**: one command per client
   stream. The node aggregates the entire `InsertFacts`/`RetractFacts` stream
   into a single `MutateCommand` → one log entry → one `Database::batch()`
   commit → one IDB recompute. This maximizes write throughput (recompute is
   the dominant cost, §4.2) and gives the whole stream atomic
   all-or-nothing semantics on the FSM — a stronger guarantee than the
   current single-node behavior, where a mid-stream error leaves earlier
   facts applied. Two details to pin down during Phase 0: (a) the RPC error
   semantics change — an invalid fact anywhere in the stream fails the whole
   command rather than partially applying; (b) a cap on command size (a
   client could stream unboundedly before finishing, and the command must be
   fully materialized before proposing) — enforce a configurable
   max-mutations-per-command and reject the stream with an error past that,
   steering bulk loads toward batched requests.
3. ~~**IDB in snapshots**~~ **Resolved**: canonical snapshots include the
   program registry (name → source) and the replicated mutated-EDB facts;
   IDB is excluded and recomputed on restore (§3.6). Restart-time recompute
   avoidance is handled by the existing `IdbBackend` fingerprint cache,
   which is validated against exactly the two things the snapshot carries
   (program hash + EDB fingerprint) — so it composes with snapshot restores
   for free.
4. ~~**Retract-of-absent-fact semantics**~~ **Resolved**: keep as
   non-error. Retracting an absent fact is a no-op — idempotent applies are
   exactly what Raft replay wants (a re-applied or re-delivered command
   produces identical state, so no dedup machinery is needed).
   `num_skipped` in `MutationSummary` is reported from the replicated-fact
   ledger (§3.6): a retract that removes nothing counts as skipped, an
   insert of an already-present tuple counts as skipped, and duplicates
   within a single command's op list count once.
5. ~~**Float determinism across platforms**~~ **Resolved**: `Value::Float`
   equality is already bit-pattern based (`to_bits`), and Datalog evaluation
   performs no float arithmetic beyond comparison, so identical inputs give
   identical results on any conforming IEEE-754 platform. Policy: a cluster's
   nodes must run the same mangle-server minor release (same policy as
   rqlite), which also covers any future evaluation changes; enforce by
   checking version compatibility during `--join` and rejecting mismatches.
