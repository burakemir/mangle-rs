# mangle-server

HTTP server for evaluating [Mangle](https://codeberg.org/TauCeti/mangle-rs) programs. Wraps the Rust `mangle-driver` compilation and execution pipeline behind a **Connect RPC API** (serving the Connect, gRPC and gRPC-Web protocols on one port), with a **deprecated JSON-over-HTTP API** kept for one release of migration coexistence.

The RPC interface (`mangle.MangleService`) is defined in
[`mangle-proto`](../mangle-proto) (`proto/mangle/{value,service}.proto`);
see [`RPC_DESIGN.md`](RPC_DESIGN.md) for the design, including the canonical
fact encoding (row and columnar), streaming semantics, and the deprecation
plan for the JSON API.

## Usage

```bash
cargo run -p mangle-server -- [OPTIONS]
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--port <PORT>` | `8090` | Port to listen on |
| `--programs-dir <DIR>` | none | Directory of `.mg` files to load on startup (program name = file stem) |

### Example

```bash
# Start with pre-loaded programs
cargo run -p mangle-server -- --port 8090 --programs-dir ./programs/
```

## API

### Connect RPC (current)

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

The canonical value encoding preserves Mangle type distinctions the JSON
API cannot express (names vs strings, time/duration as nanosecond
integers, non-finite floats). Example with `curl` using the Connect JSON
codec (streaming RPCs use enveloped `application/connect+json`):

```bash
python3 -c 'import struct,sys; m=sys.argv[1].encode(); sys.stdout.buffer.write(b"\x00"+struct.pack(">I",len(m))+m)' \
  '{"program":"social","text":"friend(X, Y)"}' > /tmp/req.bin
curl -X POST http://localhost:8090/mangle.MangleService/Query \
  -H 'content-type: application/connect+json' --data-binary @/tmp/req.bin
```

### JSON HTTP API (deprecated)

> **Deprecation notice:** the JSON HTTP API below is deprecated. It will be
> removed in the release after the Connect RPC API ships. Every response
> carries `Deprecation: true` (RFC 8594) and a `Sunset` header. Migrate to
> the Connect RPC API above — the Connect JSON codec also works with
> `curl` and standard tooling.

All endpoints accept and return `application/json`. On error, the response body is `{ "error": "<message>" }` with an appropriate HTTP status code (400, 404, or 500).

### POST /programs

Load a named program. The source is compiled to extract predicate names and stored for later querying.

```bash
curl -X POST http://localhost:8090/programs \
  -H 'Content-Type: application/json' \
  -d '{"name": "social", "source": "friend(\"alice\", \"bob\"). friend(\"bob\", \"carol\")."}'
```

**Response:**
```json
{ "name": "social", "predicates": ["friend"] }
```

### GET /programs

List all loaded programs and their predicates.

```bash
curl http://localhost:8090/programs
```

**Response:**
```json
{ "programs": [{ "name": "social", "predicates": ["friend"] }] }
```

### POST /query

Query a relation from a loaded program. The program is recompiled and executed on each query.

```bash
curl -X POST http://localhost:8090/query \
  -H 'Content-Type: application/json' \
  -d '{"program": "social", "query": "friend(X, Y)"}'
```

**Response:**
```json
{ "results": [["alice", "bob"], ["bob", "carol"]] }
```

The `query` string must be a valid Mangle atom (e.g. `predicate(X, Y)`). The predicate name is extracted to determine which relation to scan. All tuples from that relation are returned.

### POST /eval

Compile and execute ephemeral source without storing it. Useful for one-off evaluation.

```bash
curl -X POST http://localhost:8090/eval \
  -H 'Content-Type: application/json' \
  -d '{
    "source": "edge(1,2). edge(2,3). path(X,Y) :- edge(X,Y). path(X,Z) :- edge(X,Y), path(Y,Z).",
    "query": "path(1, X)"
  }'
```

**Response:**
```json
{ "results": [[2], [3]] }
```

If `query` is omitted, all derived facts across all relations are returned.

## Value encoding

Mangle values map to JSON as follows:

| Mangle | JSON | Example |
|--------|------|---------|
| `Value::Number(n)` | number | `42` |
| `Value::String(s)` | string | `"hello"` |
| `Value::Null` | null | `null` |

Result tuples are returned as arrays of arrays: `[[val, ...], ...]`.

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
