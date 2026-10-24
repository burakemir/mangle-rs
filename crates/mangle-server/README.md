# mangle-server

HTTP server for evaluating [Mangle](https://codeberg.org/TauCeti/mangle-rs) programs. Wraps the Rust `mangle-driver` compilation and execution pipeline behind a **Connect RPC API** (serving the Connect, gRPC and gRPC-Web protocols on one port).

The RPC interface (`mangle.MangleService`) is defined in
[`mangle-proto`](../mangle-proto) (`proto/mangle/{value,service}.proto`);
see [`RPC_DESIGN.md`](RPC_DESIGN.md) for the design, including the canonical
fact encoding (row and columnar) and streaming semantics.

> The JSON-over-HTTP API that preceded the Connect RPC API has been
> removed. Use the Connect RPC API below; the Connect JSON codec works
> with `curl` and standard tooling.

## Usage

```bash
cargo run -p mangle-server -- [OPTIONS]
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--port <PORT>` | `8090` | Port to listen on |
| `--programs-dir <DIR>` | none | Directory of `.mg` files to load on startup (program name = file stem) |

### Replicated (Raft) mode

Setting `raft_dir` enables replicated mode: every write goes through Raft
consensus (WAL fsync before the RPC responds), and any node serves reads
(local state, eventually consistent). Cluster configuration keys:

| Key | Description |
|-----|-------------|
| `raft_dir(Path)` | Raft data directory (WAL + snapshots); enables replicated mode |
| `node_id(Id)` | Stable node identity; must not change across restarts (default 1) |
| `raft_bootstrap(1)` | Bootstrap a new cluster, exactly once, on a fresh `raft_dir` |
| `raft_addr(Addr)` | Address other nodes use to reach this node (default `127.0.0.1:{port}`) |
| `raft_bootstrap_node(Id, Addr)` | Additional members of the cluster being bootstrapped |
| `raft_join(Addr)` | Address of an existing node to join through (fresh `raft_dir`) |

A 3-node cluster, statically bootstrapped:

```text
# node 1's config:
config_schema.raft_dir("/data/raft").
config_schema.raft_bootstrap(1).
config_schema.raft_bootstrap_node(2, "10.0.0.2:8090").
config_schema.raft_bootstrap_node(3, "10.0.0.3:8090").

# nodes 2 and 3: set raft_dir and node_id, start them; they join when
# node 1 bootstraps. (Or skip static bootstrap and use raft_join instead.)
```

Writes must target the leader; followers answer with `failed_precondition`
and the leader's address. `NodeStatus` reports the current leader.

Reads (`Query`) accept a `consistency` level:

- `NONE` (default) — any node answers from local state (eventually
  consistent).
- `WEAK` — leader-only; followers reject with the leader hint.
- `STRONG` — the leader proposes a no-op through Raft, waits for it to
  apply, then answers: the result reflects everything committed before
  the read.

### Example

```bash
# Start with pre-loaded programs
cargo run -p mangle-server -- --port 8090 --programs-dir ./programs/
```

## API

The `MangleService` is mounted under `/mangle.MangleService/` and speaks the
Connect protocol (also gRPC and gRPC-Web). Streaming RPCs:

- `Query(QueryRequest) returns (stream QueryEvent)` — query a loaded
  program; results stream in `FactBatch` chunks (row or columnar encoded,
  `batch_size`/`limit` controllable).
- `Eval(EvalRequest) returns (stream QueryEvent)` — stateless compile +
  run, streaming per-relation results.
- `InsertFacts(stream MutationRequest) returns (MutationSummary)` /
  `RetractFacts(stream MutationRequest) returns (MutationSummary)` —
  bulk mutations, streamed as single `Fact`s or columnar `FactBatch`es.

Unary RPCs: `ListPrograms`, `LoadProgram`, `GetProgram`, `DeleteProgram`,
`ReloadProgram`, `ReloadAll`.

The canonical value encoding preserves Mangle type distinctions a plain
JSON encoding cannot express (names vs strings, time/duration as nanosecond
integers, non-finite floats). Example with `curl` using the Connect JSON
codec (streaming RPCs use enveloped `application/connect+json`):

```bash
python3 -c 'import struct,sys; m=sys.argv[1].encode(); sys.stdout.buffer.write(b"\x00"+struct.pack(">I",len(m))+m)' \
  '{"program":"social","text":"friend(X, Y)"}' > /tmp/req.bin
curl -X POST http://localhost:8090/mangle.MangleService/Query \
  -H 'content-type: application/connect+json' --data-binary @/tmp/req.bin
```

## Mangle syntax notes

String constants in Mangle source must be double-quoted: `greeting("hello")`, not `greeting(hello)`. Unquoted lowercase identifiers are not valid constant syntax. Numbers are unquoted: `p(1, 2)`. Variables start with an uppercase letter: `q(X) :- p(X)`.

## Container image

A Containerfile is provided for building with Podman:

```bash
podman build -t localhost/mangle-server:latest -f rust/server/Containerfile rust/
podman run -p 8090:8090 localhost/mangle-server:latest
```

Pass arguments after the image name:

```bash
podman run -p 8090:8090 -v ./programs:/programs:ro \
  localhost/mangle-server:latest --programs-dir /programs
```
