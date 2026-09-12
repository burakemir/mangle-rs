# mangle-server RPC Protocol Design (Connect RPC / Protobuf)

Status: proposal
Author: design discussion, 2025
Scope: replace the JSON-over-HTTP interface of `mangle-server` with Connect RPC
([connect-rust](https://github.com/connectrpc/connect-rust)), with explicit
`.proto` interfaces and streaming support.

## 1. Motivation and constraints

Goals:

1. **Explicit interfaces.** Services and messages are declared in `.proto`
   files, not implied by ad-hoc JSON handler code.
2. **Streaming.** Query results and bulk fact ingestion should be streams,
   not single request/response documents.
3. **Facts as typed messages.** Mangle is typed; a predicate with a declared
   column signature should correspond to a protobuf message type in generated
   code, so that client code sees `Route { method, path, handler }` rather
   than `Vec<Vec<serde_json::Value>>`.

Hard constraints:

1. **The server must stay generic.** It loads arbitrary Mangle programs at
   runtime and cannot link against per-program generated code.
2. **Performance matters for facts.** Mangle is meant to work at scale; the
   wire format must not be the bottleneck when scanning millions of tuples.

These two constraints pull in opposite directions: typed per-program messages
are exactly what a generic server cannot know about. The resolution is a
**two-layer encoding**:

- A **canonical, schema-less encoding** (`Value`, `Fact`, `FactBatch`) that can
  represent *any* Mangle fact. This is what the server speaks. It is the
  "dynamic type" of Mangle, in the same way `google.protobuf.Value` is the
  dynamic type of JSON.
- A **typed overlay**: generated `.proto` files per Mangle program, where each
  predicate becomes a message. Typed messages are converted to/from the
  canonical encoding by *client-side* generated conversion code. The server
  never sees the typed form; the canonical encoding is the common
  denominator. This is the same trick protobuf itself uses
  (`google.protobuf.Value` ↔ concrete messages).

Within the canonical layer there are two physical encodings of the same
logical data:

- **Row encoding** (`Fact`): one message per fact. Simple, self-describing,
  used for single inserts/retracts, small results, and `/any`-typed data.
- **Columnar encoding** (`FactBatch`): typed columns of packed scalars, used
  for streaming query results and bulk ingestion. This mirrors what
  `mangle-simplecolumn` already does on disk and what the planner does
  internally; it amortizes per-message overhead and maps directly onto
  `Vec<Value>` tuples with straight-line conversion loops.

The Rust runtime for all of this is **buffa**, the protobuf runtime that
connect-rust is built on (connect-rust 0.9 depends on `buffa`, not `prost`);
see §7.

## 2. Canonical value encoding

### 2.1 The `Value` message

Mirrors `mangle_common::Value` / `ast::Const` exactly, with the additions the
JSON encoding currently cannot express (`Bool`, `Bytes`, and the
`Name`/`String` distinction):

```protobuf
syntax = "proto3";
package mangle;

// Canonical representation of a Mangle value. An unset `kind` is Mangle
// `null` (used for "no value"; also the encoding of an unbound column in
// /any-typed positions).
message Value {
  oneof kind {
    sint64 number_value   = 1;  // Mangle integer
    double float_value    = 2;
    string string_value   = 3;
    string name_value     = 4;  // name constant, e.g. "/role/admin" — NOT a string
    sfixed64 time_value   = 5;  // nanoseconds since Unix epoch
    sfixed64 duration_value = 6; // nanoseconds
    bytes  bytes_value    = 7;
    bool   bool_value     = 8;
    ListValue   list_value   = 9;
    PairValue   pair_value   = 10;
    MapValue    map_value    = 11;
    StructValue struct_value = 12;
  }
}

message ListValue   { repeated Value elems = 1; }
message PairValue   { Value first = 1; Value second = 2; }
message MapValue    { repeated Entry entries = 1; }
message Entry       { Value key = 1; Value value = 2; }
message StructValue { repeated Field fields = 1; }
message Field       { string name = 1; Value value = 2; }
```

(The implementation uses the `number_value`-style field naming — mirroring
`google.protobuf.Value` — rather than bare type keywords like `string`;
see `crates/mangle-proto/proto/mangle/value.proto` for the source of
truth.)

Design decisions:

- **`name` vs `string` as separate variants.** Mangle distinguishes them at
  runtime (`Value::Name` exists precisely so `/any` columns can tell them
  apart). The current JSON encoding collapses both to JSON strings — a lossy
  bug, not just a style issue. The proto encoding fixes this.
- **`time`/`duration` as raw `sfixed64` nanoseconds.** This is exactly the
  internal representation in `mangle_common::Value` (`Value::Time(i64)`), so
  conversion is a move, not a parse. Note `sfixed64` (fixed 8 bytes) rather
  than `sint64`: timestamps and durations cluster far from zero, where zigzag
  varints cost 9–10 bytes. `number` stays `sint64` because fact integers
  (ids, counts, small codes) are usually near zero.
- **`float` as `double`.** Mangle floats are f64. Protobuf transmits the bit
  pattern except for canonicalization of signaling NaNs — acceptable, and a
  strictly better fidelity story than the current JSON path (which maps
  non-finite doubles to `null`!).
- **`bytes`.** `ast::Const::Bytes` exists but edge `Value` currently lacks a
  `Bytes` variant. Adding `Value::Bytes` is a prerequisite (see §9); the wire
  format reserves the tag now.
- **Compounds as nested messages, not `(kind, flat Vec<Value>)`.** The
  internal `Compound(CompoundKind, Vec<Value>)` flattening is an
  interpreter-layout optimization; on the wire, explicit `ListValue` /
  `MapValue` / `StructValue` is self-describing and matches what a typed
  overlay generates (protobuf `repeated`, `map<>`, nested messages).
  Conversion is a single recursive function in each direction.
- **`null` = unset oneof.** No explicit `null_value` variant is needed; a
  message with no `kind` set *is* null. (This differs from
  `google.protobuf.Value`, which needs `NullValue` because proto3 `oneof`
  there must always have a case. Here an unset oneof suffices and saves a
  byte.)

### 2.2 Facts and temporal validity

```protobuf
// A single fact: a ground atom of some predicate.
message Fact {
  string relation = 1;      // predicate name, e.g. "route"
  repeated Value args = 2;  // one per column; length == arity

  // Temporal validity interval, present iff the predicate is temporal.
  Interval valid_time = 3;
}

message Interval {
  oneof start { sfixed64 start_ts = 1; Unbounded start_unbounded = 2; }
  oneof end   { sfixed64 end_ts   = 3; Unbounded end_unbounded   = 4; }
  enum Unbounded { UNBOUNDED = 0; }
}
```

- `relation` is a plain string per fact. It is *not* repeated per row inside
  batches (§3), so the per-fact cost is irrelevant at scale; in row encoding
  it buys self-containment (a `Fact` can be logged, queued, or stored
  standalone — same role the `mutations.mg` lines play today).
- Temporal bounds reuse the internal nanoseconds representation; unbounded
  covers `NegInf`/`PosInf`. (Variables in bounds, `TemporalBound::Variable`,
  are a query-language construct and never appear in ground facts, so they
  need no wire form.)

## 3. Columnar batch encoding

The row encoding costs one message header per value. For scans and bulk load
that overhead dominates. `FactBatch` encodes one relation, many rows:

```protobuf
// A batch of facts for a single relation, encoded column-wise.
message FactBatch {
  string relation = 1;
  uint32 num_rows = 2;
  repeated Column columns = 3;  // columns.len() == arity
  // For temporal predicates, one Interval per row.
  repeated Interval valid_times = 4;
}

message Column {
  ColumnType type = 1;

  // Exactly one of the following is populated, per `type`.
  // Packed repeated — contiguous varint/fixed64/fixed8 payloads on the wire.
  repeated sint64   numbers   = 2;  // NUMBER
  repeated double   floats    = 3;  // FLOAT
  repeated string   strings   = 4;  // STRING
  repeated string   names     = 5;  // NAME
  repeated sfixed64 times     = 6;  // TIME
  repeated sfixed64 durations = 7;  // DURATION
  repeated bytes    byte_strings = 8; // BYTES
  repeated bool     bools     = 9;  // BOOL

  // ANY (and, initially, all compound types): row-encoded values,
  // one per row.
  repeated Value values = 15;
}

enum ColumnType {
  NUMBER = 0;
  FLOAT = 1;
  STRING = 2;
  NAME = 3;
  TIME = 4;
  DURATION = 5;
  BYTES = 6;
  BOOL = 7;
  ANY = 8;   // heterogeneous / union-typed columns
}
```

Properties:

- **The type is declared once per column, not tagged per value.** This is the
  wire-level manifestation of Mangle's type system: a typed predicate gets a
  fully packed batch where each scalar costs 1–9 bytes and there is no
  per-value tag or message header. The server derives `Column.type` from the
  program's declared column types (the same information `TableStoreSchema` /
  `TableConfig` already carry), so batches are validated by construction.
- **NULLs:** column types are non-optional in Mangle; only `ANY` columns can
  hold null, and there null is just `Value` with unset `kind` inside
  `values`.
- **Compounds:** in v1, compound-typed columns are sent as `ANY` (row-encoded
  `Value`s). This is correct and simple. A later version can add Arrow-style
  offsets (`repeated uint32 offsets` + child `Column`) for flat `list<T>`
  columns — the schema for that extension slot is the same one
  `mangle-simplecolumn` uses on disk, so the encoding effort is shared.
- **The encoding is relation-typed but still schema-less for the server:**
  the server fills `type` from whatever the loaded program declares, and
  conversely *validates* incoming batches against the declared types. No
  generated code is involved on the server.

Conversion cost analysis (why this satisfies the performance constraint):

| path | current JSON | row proto | columnar proto |
|---|---|---|---|
| int column value | format + JSON parse | varint decode | packed varint decode, no per-value frame |
| string column | JSON string parse | length-delimited | length-delimited |
| per-fact overhead | JSON array/object frames | 1 message frame | amortized: 1 frame per batch |
| intermediate allocs | `serde_json::Value` tree | none (buffa zero-copy views) | none; `Vec<Value>` built once per row, or `Store::insert` extended to take columns |

The last row matters: today `insert_fact` takes `Vec<Value>` per tuple. A
follow-up `Store::insert_batch(&mut self, relation, &FactBatch)` can bulk-load
straight from the packed arrays without materializing per-row `Vec<Value>` at
all. The wire format is designed so that this fast path exists.

Two buffa properties reinforce this analysis:

- **Zero-copy views.** Buffa generates a borrowed `FooView<'a>` alongside
  every owned message type; decoding a view borrows `string`/`bytes` fields
  directly from the input buffer, and connect-rust handlers receive requests
  as views (`ServiceRequest` derefs to one). On the server's read path
  (bulk `InsertFacts`) the wire structs cost *zero* string allocations —
  `Value::String(s.to_string())` is the first and only copy of each string.
  Packed scalar columns decode as slice-backed `RepeatedView`s. Views borrow
  for the duration of the handler call, which is exactly the lifetime of a
  mutation request; anything that must outlive the call (there is nothing in
  the mutation path) would use `to_owned_message()`.
- **Force-inlined packed decode loops.** Buffa's generated packed-varint
  decode loops use a measured fast path (`decode_*_packed` twins) — the
  `numbers`/`times`/`durations` columns decode without per-element call
  overhead.

## 4. Structured queries

Today the server accepts a query *string* (`route("GET", Path, Handler)`) and
re-parses it with `mangle-parse`, then pattern-matches in
`query.rs::filter_tuples`. The proto interface makes the same query an
explicit message — this is the query-language-level expression of "explicit
interfaces":

```protobuf
message QueryRequest {
  string program = 1;

  oneof query {
    // Textual form, for interactive use. Parsed server-side with
    // mangle-parse (same grammar as the CLI).
    string text = 2;
    // Structured form, for programmatic use. No parsing involved.
    StructuredQuery structured = 3;
  }

  // Encoding of result facts in the stream. ROWS sends each FactBatch with
  // all-ANY columns (row-encoded values); COLUMNAR emits typed packed
  // columns.
  ResultEncoding encoding = 4;   // ROW (default) or COLUMNAR
  // Target number of facts per streamed FactBatch (server may deviate).
  uint32 batch_size = 5;         // default e.g. 4096
  // Hard limit on number of result facts; 0 = unlimited.
  uint64 limit = 6;
}

enum ResultEncoding { ROWS = 0; COLUMNAR = 1; }

message StructuredQuery {
  string predicate = 1;
  // One entry per column position. Absent trailing entries = wildcard.
  repeated ArgPattern args = 2;
}

message ArgPattern {
  oneof pattern {
    // Unbound: matches anything (a Mangle variable / `_`).
    Wildcard any = 1;
    // Ground constant at this position.
    Value const = 2;
  }
}

// Own wildcard marker instead of google.protobuf.Empty, so the canonical
// protos stay self-contained (no well-known-type codegen plumbing).
message Wildcard {}
```

The structured form is exactly `query.rs::ParsedQuery` made explicit: no
stringly-typed `QueryArg::StringConst` vs `NameConst` guesswork, and patterns
compose with the canonical `Value` so every Mangle type can be bound,
including names, times, and compounds.

## 5. Service definition

```protobuf
service MangleService {
  // --- Queries (server-streaming) ---
  // Streams result facts of a query against a loaded program, in batches.
  rpc Query(QueryRequest) returns (stream QueryEvent);
  // Stateless evaluation: compile + run a source program, stream results.
  rpc Eval(EvalRequest) returns (stream QueryEvent);

  // --- Mutations (client-streaming for bulk load) ---
  // Insert facts into a program's EDB. Stream single Facts or FactBatches.
  rpc InsertFacts(stream MutationRequest) returns (MutationSummary);
  // Retract facts from a program's EDB.
  rpc RetractFacts(stream MutationRequest) returns (MutationSummary);

  // --- Program management (unary) ---
  rpc ListPrograms(ListProgramsRequest) returns (ListProgramsResponse);
  rpc LoadProgram(LoadProgramRequest) returns (ProgramInfo);
  rpc GetProgram(GetProgramRequest) returns (ProgramInfo);
  rpc DeleteProgram(DeleteProgramRequest) returns (Empty);
  rpc ReloadProgram(ReloadProgramRequest) returns (ProgramInfo);
  rpc ReloadAll(ReloadAllRequest) returns (ListProgramsResponse);
}

// Own empty messages instead of google.protobuf.Empty (see the note on
// Wildcard above).
message Empty {}
message ListProgramsRequest {}
message ReloadAllRequest {}

message EvalRequest {
  string source = 1;         // Mangle program source
  repeated string libs = 2;  // additional sources (eval_source_multi)
  // rest: same options as QueryRequest (query text, encoding, batch, limit)
  string query = 3;
  ResultEncoding encoding = 4;
  uint32 batch_size = 5;
  uint64 limit = 6;
}

// One streamed event of a query.
message QueryEvent {
  oneof event {
    // Progress: a batch of matching facts. `columns` carry the variable
    // names bound by the query (schema of the result rows).
    FactBatch batch = 1;
    // Terminal: query completed successfully.
    QueryStats done = 2;
  }
}

message QueryStats {
  uint64 num_facts = 1;
  // Wall time of query evaluation, microseconds.
  uint64 elapsed_us = 2;
  // Names of the variables bound by the query, in column order.
  repeated string variables = 3;
}

message MutationRequest {
  string program = 1;
  oneof payload {
    Fact fact = 2;         // single fact
    FactBatch batch = 3;   // bulk
  }
}

message MutationSummary {
  uint64 num_inserted = 1;
  uint64 num_retracted = 2;
  uint64 num_skipped = 3;  // e.g. duplicate inserts
}

message ProgramInfo {
  string name = 1;
  repeated string predicates = 2;
  string source = 3;       // only in GetProgram
}

message LoadProgramRequest { string name = 1; string source = 2; }
message GetProgramRequest   { string name = 1; }
message DeleteProgramRequest{ string name = 1; }
message ReloadProgramRequest{ string name = 1; }
message ListProgramsResponse{ repeated ProgramInfo programs = 1; }
```

Notes on the service shape:

- **`Query` is server-streaming, not client-streaming.** The query itself is
  small and known upfront; only results are large. This is the shape that
  unlocks "return 10M facts without buffering them in RAM": the handler pulls
  from the evaluation with a bounded channel and yields `FactBatch`es as
  they fill.
- **`InsertFacts` is client-streaming.** Bulk load is the inverse problem:
  the request is huge, the response is a summary. This also gives natural
  backpressure: connect flow-control stops the client when the server is
  saturated. The server can transactionalize per batch (apply + log to
  `mutations.mg` per `FactBatch`), which bounds replay cost on crash.
- **Errors mid-stream** use Connect's error model: the stream is terminated
  with a status code. Partial results already delivered remain valid — this
  is precisely why each event is a self-contained batch. Query failures
  (compile errors, unknown program) surface before the first batch, so
  clients can treat "no events, error status" atomically.
- **Unary program management** maps 1:1 onto the existing `ProgramStore`
  methods (`load`, `get`, `list`, `remove`, `reload`, `reload_all`).

### Error mapping

The current handlers sniff error strings (`msg.contains("not found")`) to
pick a status code. The Connect interface makes this explicit:

| condition | connect code |
|---|---|
| program not found | `not_found` |
| parse/type error in program or query | `invalid_argument` |
| wrong arity / type mismatch in Fact vs. declared schema | `invalid_argument` |
| batch encoding inconsistent (num_rows vs column lengths) | `invalid_argument` |
| relation does not exist | `not_found` |
| internal error (store failure, IO) | `internal` |

The server should classify errors at the source (typed error enum in
`ProgramStore`) rather than by string matching; the proto interface is the
occasion to do that cleanup.

## 6. Typed overlays: generated `.proto` per Mangle program

This is the layer that delivers the original motivation — "a protobuf
message type corresponds to a fact" — without breaking server genericity.

A new generator (e.g. `mangle-proto`, analogous to `mangle-py`/`mangle-ffi`
glue crates) reads a Mangle program's type declarations and emits a `.proto`
file with one message per predicate, plus conversion functions to/from the
canonical encoding:

```mangle
type Route = route(method: string, path: string, handler: string)
             @[ValidFrom, ValidTo];
```

generates

```protobuf
syntax = "proto3";
package myapp.mangle;
import "mangle/value.proto";

extend google.protobuf.MessageOptions {
  // Binds this message to a Mangle predicate. The server never reads this;
  // it exists for tooling and self-documentation.
  string mangle_predicate = 50000;
}

message Route {
  option (mangle_predicate) = "route/3";
  string method = 1;
  string path = 2;
  string handler = 3;
  // temporal columns become the interval, not extra fields
  mangle.Interval valid_time = 4;
}
```

plus generated Rust/Go/TS (depending on the client) with:

```rust
impl Route {
    fn to_fact(&self) -> mangle::Fact;      // typed -> canonical
    fn from_fact(f: &mangle::Fact) -> Result<Self>; // canonical -> typed
}
```

Type mapping used by the generator:

| Mangle type | proto field |
|---|---|
| `int` | `sint64` |
| `float` | `double` |
| `string` | `string` |
| `name` / namespace | `string` (+ field option noting name-ness, or wrapper `Name` if strictness is wanted) |
| `time` | `sfixed64` |
| `duration` | `sfixed64` |
| `bytes` | `bytes` |
| `bool` | `bool` |
| `list[T]` | `repeated T` |
| `map[K,V]` | `map<K,V>` |
| `struct{...}` | nested `message` |
| pair | `message { T first; T second; }` |
| `/any` or union type | `mangle.Value` (or `oneof` for closed unions) |
| temporal columns | `mangle.Interval valid_time` |

Caveat: `map[K,V]` maps to a proto `map<K,V>` field only when `K` is a valid
proto map key type (integral/string/bool); otherwise the generator emits
`repeated Entry`. Buffa decodes `map` fields to `HashMap`, which loses
insertion order — acceptable for Mangle map *values* (equality semantics are
unchanged), but the generator must not use proto maps where iteration order
is semantically observable.

Why this works without server changes: **every typed message transcodes
losslessly into the canonical encoding** (field *i* ↔ fact arg *i*, nested
messages ↔ `Value` compounds). The client converts, sends canonical
`Fact`/`FactBatch`, the server executes generically, and the client converts
the streamed batches back into typed records. The generated code is the only
piece that knows the schema; both the wire and the server speak the
schema-less canonical form. Clients that prefer to stay dynamic (scripts,
tests, `grpcurl`-style tooling) use the canonical messages directly.

Optional future tightening: clients can register a program's generated
descriptor with the server and ask it to *verify* batches against the
descriptor before applying — but this is an optimization of validation, not a
requirement for correctness.

## 7. Integration with connect-rust and buffa

Runtime and codegen choices:

- **Runtime:** `connectrpc` 0.9 (Tower-based, serves Connect, gRPC and
  gRPC-Web over the same handlers) on **buffa** 0.9, its protobuf runtime.
  connect-rust does not use prost; buffa is a dependency of the runtime
  itself, so using it costs nothing extra.
- **Codegen, two supported workflows** (pick one):
  1. `buf generate` with `protoc-gen-buffa` (message types, `views=true
     json=true`), `protoc-gen-connect-rust` (service stubs,
     `buffa_module=crate::proto`), and `protoc-gen-buffa-packaging`
     (`strategy: all`) — generates checked-in code.
  2. `connectrpc-build` in `build.rs` (`Config::new().files(..).compile()`)
     — unified output, no plugin binaries at build time.

  **Chosen:** workflow 2 for `mangle-proto`, with `protoc` supplied by
  `protoc-bin-vendored` (set as `PROTOC` in `build.rs`) so the build stays
  hermetic — no system protoc, buf, or plugin binaries required.
- Generated code requires: `connectrpc` (features `client` for clients,
  `axum`/`server` for the server), `buffa` and `buffa-types` with `json`,
  `serde`, `serde_json`.
- **Views must be enabled** (`views=true` / default in `connectrpc-build`):
  the generated service stubs rely on buffa's `HasMessageView` impls and
  owned-view wrappers.
- Connect serves over HTTP/1.1, HTTP/2 and HTTP/3 with the same handlers, and
  the same `.proto` is directly usable by gRPC and gRPC-Web clients — the
  explicit-interface goal is met for the whole ecosystem, not just Rust.
- **JSON comes for free**: connect's JSON codec plus buffa's `json` feature
  give the canonical messages a standard proto3-JSON encoding. This eases
  migration (existing JSON clients can be ported to the Connect protocol
  without binary tooling) and enables `curl`-style debugging against a live
  server.
- The existing `axum` + `tokio` stack is kept: `connectrpc`'s `axum` feature
  provides `ConnectRouter::new().add_service(...)` whose
  `into_axum_service()` mounts under `Router::fallback_service`, so the JSON
  endpoints can coexist during migration and be removed (or kept behind a
  flag) afterwards.
- Handler shape: `impl MangleService for MangleServiceImpl` with methods
  taking `ServiceRequest<'_, T>` (deref to the zero-copy view) and returning
  `ServiceResult<T>` / streaming responses; unary program-management RPCs map
  1:1 onto `ProgramStore` methods.
- **Standard services**: mount `connectrpc-health` (gRPC health protocol for
  `grpc_health_probe`, kubelet `grpc:` probes, service meshes) and
  `connectrpc-reflection` (server reflection, so `grpcurl`/`buf curl`/
  Postman can discover and call `MangleService` at runtime — a nice
  complement to the explicit-interfaces goal).
- **Untrusted input**: buffa's `DecodeOptions` (recursion limit, max message
  size, unknown-field limit) should be applied at the RPC entry points for
  `Value`-bearing messages; `Value` nesting depth is bounded by Mangle data,
  but the server should reject pathological frames early.
- Streaming plumbing: each query runs on a blocking task (evaluation is
  CPU-bound and synchronous today) and sends `FactBatch`es through a bounded
  `tokio::sync::mpsc` channel to the connect handler, which yields them as
  stream items. Bounded channel = backpressure; client cancellation (dropped
  stream) propagates by dropping the receiver, which stops the producer.
- All the Rust glue lives in a `mangle-proto` crate: the `.proto` files, the
  buffa/connectrpc generated code, `Value`/`Fact`/`FactBatch` ↔
  `mangle_common::Value` conversions (both row and columnar, and both owned
  and view inputs), and the typed-overlay generator. The server depends on
  it; client crates depend on it without depending on the server.
- **Editions**: the canonical files use `proto3` syntax for maximal
  ecosystem compatibility, but buffa supports editions 2023/2024 natively
  (per-field presence, packed encoding control), so migrating later — e.g.
  to require explicit presence somewhere — is not blocked by the runtime.
- Buffa's custom-type knobs (`string_type`/`bytes_type` in codegen) let
  generated `string`/`bytes` fields use a different owned Rust type; the
  typed overlay could use this to bind `name` columns to an interned name
  type if desired.

## 8. What this fixes relative to the current JSON API

1. `Name` vs `String` distinction survives the wire (currently collapsed).
2. Non-finite floats survive the wire (currently `null`).
3. `Bool` and `Bytes` values are representable (currently stringified).
4. `Time`/`Duration` are wire types, not formatted strings needing re-parse.
5. Error status is a code, not string sniffing.
6. Queries have a structured form; no ad-hoc query-string parser on the
   server hot path for programmatic clients.
7. Bulk insert and large result sets stream with backpressure instead of
   materializing full JSON documents.
8. Typed client bindings per program, generated from the Mangle program's
   own type declarations — one source of truth.

## 9. Prerequisites and implementation plan

Prerequisites in other crates:

- `mangle-common`: add `Value::Bytes(Vec<u8>)` (parity with
  `ast::Const::Bytes`; needed by columnar `BYTES` and by `mangle-parquet`
  round-tripping anyway).
- `mangle-common::Store`: add `insert_batch(&mut self, relation: &str,
  batch: &FactBatch)` (or a columnar input struct) so bulk load can skip
  per-row `Vec<Value>` materialization.
- `mangle-server::store`: replace string-matching error classification with
  a typed error enum.

Steps, in dependency order:

1. `mangle-proto` crate: `value.proto`, `service.proto`, buffa + connectrpc
   generated code (via `buf generate` or `connectrpc-build`), conversions
   to/from `mangle_common::Value` (both row and columnar, owned and view
   inputs), unit tests incl. round-trip property tests over all `Value`
   variants.
2. `mangle-server`: mount `MangleService` alongside existing JSON routes.
   Implement unary program-management RPCs first (trivial mapping to
   `ProgramStore`).
3. Server-streaming `Query`/`Eval` with ROWS encoding; then COLUMNAR
   encoding (needs column-type resolution from the program schema).
4. Client-streaming `InsertFacts`/`RetractFacts` wired to `ProgramStore` +
   `MutationLog` (log per batch).
5. Typed-overlay generator in `mangle-proto` (Mangle type decls → `.proto` +
   conversion code).
6. Deprecate, then remove, the JSON HTTP API. **Decision:** the JSON
   routes are kept — *deprecated, not flagged* — for exactly one release
   alongside the Connect RPC API, and removed in the release after that.
   The deprecated handlers answer with a `Deprecation: true` (RFC 8594)
   and `Sunset` response header and log a deprecation warning, but require
   no flag or config change, so existing clients keep working unchanged
   during the migration window.

Non-goals for v1: compound columns in packed columnar form (use `ANY`),
dictionary encoding for string columns, server-side descriptor verification,
client streaming *and* server streaming for queries ( bidi watch/subscribe
would be a natural later addition, e.g. `Watch(WatchRequest) returns
(stream WatchEvent)` replaying deltas of `scan_delta` — the `Store` trait
already exposes the needed iterators).

## 10. Implementation status

Implemented (steps 1–4 and the deprecation half of step 6):

- **`mangle-proto` crate**: `proto/mangle/{value,service}.proto`, codegen
  via `connectrpc-build` in `build.rs` (hermetic: `protoc` supplied by
  `protoc-bin-vendored`), owned + zero-copy view conversions (row and
  columnar), round-trip unit tests including a wire-codec round-trip.
- **`mangle-server`**: `MangleServiceImpl` (in `src/rpc.rs`) mounted via
  `ConnectRouter::into_axum_service()` as the fallback service; JSON
  routes kept and marked deprecated (`Deprecation`/`Sunset` headers via
  middleware in `src/app.rs`). The crate was restructured into lib + bin
  so integration tests can drive the full stack; `tests/rpc.rs` covers
  both APIs end-to-end (10 tests, including streaming, columnar batches,
  bulk mutations, error codes, and the deprecation headers).

Deliberate v1 simplifications (all compatible with the wire format):

- Column types in outgoing `FactBatch`es are *inferred from the data*
  rather than resolved from the program's declared schema; incoming
  batches are validated structurally. Schema-driven typing (§3) is a
  follow-up.
- `Query`/`Eval` materialize the full result before streaming (chunks
  arrive promptly, but evaluation itself is not incremental). The
  bounded-channel producer design in §7 remains the target.
- `QueryStats.variables` is not populated (query parsing does not expose
  bound variable names yet).
- Errors are classified by message inspection in one helper (`classify`),
  pending the typed error enum in `ProgramStore` (§5).
- `MutationSummary.num_skipped` stays 0: the store does not report
  duplicate inserts.

Remaining steps: schema-driven column typing, `Store::insert_batch` bulk
  fast path, typed-overlay generator (step 5), JSON API removal in the
  release after next (step 6).
