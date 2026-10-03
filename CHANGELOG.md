# Changelog

All notable changes in mangle/rust will be documented in this file.

## Unreleased

### 🐛 Bug Fixes

- **Check function argument types** against the interpreter's runtime
  semantics, mirroring mangle-go's `typeOfFn` argument checks: `fn:plus` on
  a /string variable, `fn:sum` over strings in a transform, `fn:mult` on
  /name, `fn:sqrt`/float functions on non-numeric values, `fn:time:*`/
  `fn:duration:*` with wrong argument kinds, `fn:string:replace` argument
  positions — all now compile errors instead of runtime failures. Float
  functions accept /number (coerced); only provably-disjoint types are
  rejected, so unions like `.Union</number, /string>` never false-positive.
  The check runs for every rule, including rules whose head predicate has
  no `Decl`.
- **Fix `/number` etc. conforming to `/name`**: the name-hierarchy shortcut
  in `type_conforms` accepted any type starting with `/`, so all base types
  were (wrongly) subtypes of `/name`.
- **`fn:time:sub` result type** now depends on the second argument:
  (time, time) infers /duration, (time, duration) infers /time (previously
  always /duration).
- **Reject facts containing variables** (`q(X).`, `q(_)`, `q([1, X])`):
  facts are unit clauses with no body to bind variables; they must be
  ground. Previously these compiled and then failed at runtime
  (`Variable not found`) or matched nothing. Matches mangle-go's
  `CheckRule`, which requires head variables to be bound.

### 🚀 Features

- **Binding (safety) analysis for rules**, mirroring mangle-go's
  `Analyzer.CheckRule`. The bounds checker now rejects, at compile time,
  rules that previously failed at runtime (`Variable not found`) or —
  worse — silently produced wrong results:
  - variables in the head (or used anywhere) that are never bound by a
    positive atom, an equality, or a transform `let`;
  - filter built-ins (`:lt`, `:match_prefix`, string predicates, …)
    applied to variables not yet bound ("move the subgoal to the right");
  - `group_by` keys that are not bound variables, or not distinct;
  - head variables that are neither `group_by` keys nor aggregated when
    the rule has a `do fn:group_by` transform;
  - transforms that redefine body variables, or use variables that are
    not in scope (non-reducer `let`s see only group keys and earlier
    transform definitions);
  - `do`-transforms applying anything other than `fn:group_by` (the
    planner silently mis-planned these as group-bys).
- **Bounds inference for built-in functions** (`bound_of_apply_fn`):
  fixed misnamed function arms that never matched (`fn:float_plus` →
  `fn:float:plus` etc., `fn:struct_get` → `fn:struct:get`) and added the
  missing ones: `fn:pair`/`fn:pair:first`/`fn:pair:second`, `fn:map:get`,
  `fn:map:keys`/`fn:map:values`/`fn:struct:values`, `fn:float:minus`,
  the float aggregates (`fn:float:sum`/`max`/`min`), the `*:to_string`
  conversions, and all `fn:time:*`/`fn:duration:*` functions, following
  mangle-go's builtin function-type table. `fn:tuple` is documented as
  acting like identity (one argument), a pair (two) or nested pairs
  (more), matching mangle-go.

## [0.9.2] - 2026-10-03

### 🐛 Bug Fixes

- Add missing `fn:collect_distinct` to planner.

## [0.9.1] - 2026-10-01

### 🚀 Features

- **Negated built-in predicates** (`!:list:member`, `!:match_field`,
  `!:lt`/`:le`/`:gt`/`:ge` incl. `:time:`/`:duration:` variants,
  `!:match_prefix`, `!:string:starts_with`/`ends_with`/`contains`):
  previously these fell through to the generic negation path, which looks
  the predicate up as a never-populated relation — the negation always
  succeeded, silently producing wrong results. The planner now emits a
  `Condition::Not` wrapping the built-in's check mode, requiring all
  variable arguments to be bound by earlier premises (negation cannot
  bind). Negation of built-ins without positive-form support (e.g.
  `!:match_entry`) is a compile error instead of a silent wrong answer.
- **WASM codegen reaches interpreter parity on the physical plan**: every
  `Op` and `Condition` variant now has emission.
  - Plain Datalog negation compiles to a host buffer protocol
    (`negation_begin` / `negation_push` / `negation_end`), mirroring the
    HashJoin pattern.
  - Built-in predicate checks (`:string:*`, `:match_prefix`,
    `:list:member`, `:match_field`) delegate to new host imports mirroring
    the interpreter's `eval_builtin_predicate`.
  - The binding forms of `:match_field` and `:list:member` compile to a
    `field_present` check + `compound_get` extraction and a
    `list_iter_start` iteration, reusing the scan protocol.
  - `Op::GroupBy` writes an aggregate description (key columns + function
    codes) to linear memory and iterates the host-computed groups via the
    existing `scan_aggregate_start` import; hosts implement all nine
    aggregate functions.
  - New interpreter-vs-WASM parity test suite in `mangle-driver` (24
    programs) asserting identical results in both execution modes.

### 🐛 Bug Fixes

- WASM codegen: `Op::GroupBy`, `fn:map:keys`/`fn:map:values`/
  `fn:struct:values`, and unknown functions previously compiled to
  silently wrong results (empty output, `compound_len`, or null); they now
  fail loudly at codegen time.
- `mangle-vm` test hosts: inserts are deduplicated value-based, matching
  the interpreter's set semantics for relations.
- `CsvHost::scan_aggregate_start` now fails loudly instead of silently
  returning no groups.

### 📖 Documentation

- README: new "Parity TODO" section documenting the remaining gaps towards
  mangle-go (missing `Expr::Call` function families in WASM codegen, the
  broken positive `:match_entry` form, absent built-in predicates and
  functions).

## [0.9.0] - 2026-09-22

### 🚀 Features

- **Connect RPC API for `mangle-server`** (design:
  `crates/mangle-server/RPC_DESIGN.md`): the new `mangle-proto` crate
  defines the canonical, schema-less protobuf encoding of Mangle facts
  (`Value`, row-encoded `Fact`, columnar `FactBatch` with typed packed
  columns) and the `MangleService` interface, generated with
  `connectrpc-build` on the buffa runtime (hermetic build via
  `protoc-bin-vendored`). The server serves it under
  `/mangle.MangleService/` for Connect, gRPC and gRPC-Web clients:
  server-streaming `Query`/`Eval` (row or columnar batches, `batch_size`/
  `limit` control), client-streaming `InsertFacts`/`RetractFacts`, and
  unary program management. The canonical encoding preserves Mangle type
  distinctions the JSON API loses (name vs string, time/duration as
  nanosecond integers, non-finite floats).
- `mangle-server` restructured into lib + bin with end-to-end integration
  tests covering both APIs.

### ⚠️ Deprecations

- **The JSON HTTP API of `mangle-server` (`/query`, `/programs`, `/eval`,
  `/admin/reload-all`) is deprecated.** It coexists with the Connect RPC
  API for one release and will be removed in the release after that.
  Responses carry `Deprecation: true` (RFC 8594) and a `Sunset` header;
  no flags or config changes are required to keep using it meanwhile.

## [0.8.0] - 2026-06-19

### 🚀 Features

- **New `mangle-parquet` crate**: `ParquetEdbSource` reads plain Parquet
  files (single file or a directory of `.parquet` files) as EDB facts,
  with **row-group predicate pushdown** via per-column min/max
  statistics. Supports the same `ColumnPredicate` pushdown protocol as
  `mangle-delta`; pushdown is best-effort and always re-checked in memory.
- **Shared Arrow → Mangle `Value` mapping**: the converter (`convert.rs`)
  moved from `mangle-delta` into `mangle-parquet::convert` (now `pub`),
  so Parquet and Delta Lake data are mapped to Mangle values identically.

### ⚠️ Breaking Changes

- **`mangle-delta` now uses the published `deltalake-core` 0.32** (was a
  path reference to a sibling `delta-rs` checkout). Same Arrow/Parquet 58.

### ⚙️ Miscellaneous Tasks

- `mangle-parquet` and `mangle-delta` are now full workspace members:
  inherit version/edition/metadata from `[workspace.package]`, use
  workspace dependencies, and resolve through the root `Cargo.lock`.
  Kept out of `default-members` so a plain `cargo build` stays light
  (DataFusion only pulled by explicit `-p`).
- Replaced `fxhash` with `rustc-hash`; general dependency updates.
- 12 `mangle-parquet` integration tests (row-group pruning for int and
  string columns, multi-file directory, all-pruned and `Neq` safety).

## [0.7.0] - 2026-04-15

### 🚀 Features

- **Persistent secondary indexes in `mangle-db`**: Every argument position of
  every relation is indexed transactionally. `Store::scan_index` /
  `scan_delta_index` now range-scan redb index tables instead of doing linear
  scans. Enables sub-linear point lookups on disk-backed datasets.
- **`Op::HashJoin` physical operator**: new two-way hash-join op for joins
  whose shared variable is unbound on both sides. Executed directly by the
  interpreter via an in-memory hash table keyed by the `join_keys`
  projection.
- **HashJoin planner fast path**: `Planner::with_hash_join(true)` (seeded
  from `MANGLE_HASHJOIN=1`) makes `plan_join_sequence` emit `Op::HashJoin`
  for eligible 2-premise joins. Off by default — falls through to the
  existing nested-Iterate + IndexLookup path.
- **HashJoin WASM codegen**: five new host imports (`hash_join_begin`,
  `hash_join_push`, `hash_join_commit_build`, `hash_join_probe`,
  `hash_join_end`) and corresponding `Backend` / `Host` trait methods.
  `Codegen::with_hash_join(true)` threads the flag through the internal
  planner. Match iteration reuses the existing `scan_next` + `get_col`
  imports.
- **Compressed SimpleColumn input**: `mangle-simplecolumn` now
  transparently decompresses gzip (`1F 8B`) and zstd (`28 B5 2F FD`)
  streams at read time via magic-byte sniffing. Gated behind optional
  crate features `gzip` (pulls `flate2`) and `zstd` (pulls `ruzstd`),
  both on by default. Pure-Rust implementations — no C toolchain
  required.

### ⚠️ Breaking Changes

- **`mangle-db` on-disk format**: tuples are now serialized with
  [postcard] instead of JSON, and the per-database `__format__` redb table
  carries a format version. Opening a database created by 0.6.0 fails with a
  clear error — **recreate the database** after upgrading. The switch
  shrinks on-disk size substantially (a compact variant-tagged encoding,
  no string field names) and fixes `Value::Compound` persistence, which
  previously serialized as `null`. `Value::Float` NaN bit patterns now
  round-trip exactly.
- **`mangle-common::Host` trait**: five new methods for the HashJoin
  protocol (`hash_join_begin`, `hash_join_push`, `hash_join_commit_build`,
  `hash_join_probe`, `hash_join_end`). Default impls `unimplemented!()`, so
  existing implementations compile unchanged; they only trip if a program
  compiled with HashJoin enabled is run against a Host that hasn't opted
  in.
- **Index-backed dedup in `DiskStore::insert`**: replaces the previous
  full-tier scan. Insert throughput on non-trivially-sized relations
  improves, but behavior on zero-arity relations now falls back to the
  scan path.

### ⚙️ Miscellaneous Tasks

- New `postcard` dependency in `mangle-db`.
- In-RAM `stable_indexes` / `delta_indexes` HashMaps removed from
  `DiskStore` — all index state lives in redb.
- 19 new mangle-db tests (8 index, 11 roundtrip / open-time validation),
  7 interpreter HashJoin tests, 2 planner emission tests, 1 end-to-end
  WASM round-trip test.

[postcard]: https://github.com/jamesmunns/postcard

## [0.6.0] - 2026-03-14

### ⚠️ Breaking Changes

- **`Value::Name` variant**: The `Value` enum now has a dedicated `Name(String)`
  variant for Mangle name constants (e.g. `/foo/bar`). Previously names were
  collapsed into `Value::String` at runtime. Any code matching on `Value` will
  need to handle the new variant. Built-in predicates (`:match_prefix`) and
  functions (`fn:time:trunc`, `fn:name:to_string`) now expect `Value::Name`
  instead of `Value::String` for name arguments.
- **Disk storage format**: `disk_store` serialization changed — names are now
  encoded as `{"__name__": "..."}` JSON objects. Existing databases created with
  0.5.0 will deserialize name values as `Value::String` instead of `Value::Name`.

## [0.5.0] - 2026-03-09

### 🚀 Features

- **Temporal facts**: Support for facts with validity intervals `@[start, end]`,
  implemented via synthetic columns approach matching the Go reference implementation.
  Includes interval coalescing after fixpoint convergence.
- **Float support**: IEEE 754 floating-point values across IR, interpreter, codegen, and server.
- **Comparison operators**: Support for `<`, `<=` in planner with cross-type numeric ordering
  (Duration/Number, Time/Number comparisons).
- **Built-in functions and predicates**: `fn:time:sub`, duration/time comparisons,
  and other built-in operations.
- **Time and duration types**: Full time/duration support with Go-compatible formatting
  (compound duration forms, RFC3339 timestamps, Howard Hinnant's civil date algorithm).
- **Negative number literals**: Parser and IR support for negative numeric constants.
- **mangle-wasm crate**: Browser-targeted interpreter compiled to WebAssembly.
- **Externref-based WASM value passing**: All Mangle values represented as `externref`
  in WASM with host-maintained value slab, string and compound type support.

### ⏱️ Performance

- Interpreter-in-WASM benchmark suite for three-way comparison
  (native, wasmtime, browser).

### 🐛 Bug Fixes

- Fix stratification to handle TemporalAtom variant in dependency graph.
- Fix cross-type Duration/Number and Time/Number comparison ordering.
- Fix coalescing to run after fixpoint convergence (not during semi-naive loop).
- Fix EDB/IDB classification for temporal atom predicates.

### ⚙️ Miscellaneous Tasks

- Bump wasmtime dependency to v41.
- Add configuration file support (`config.mg`) and durable EDB writes.
- Comprehensive test coverage for temporal facts (25 new tests across parser,
  interpreter, and driver).

## [0.4.0]

### 🚀 Features

- change of architecture, WASM execution
- seminaive evaluation, aggregation, indexed lookup

### 🐛 Bug Fixes

- add missing parser code

### ⚙️ Miscellaneous Tasks

- change AST, use interning (changes API)

## [0.1.1] - 2024-07-24

### 🚀 Features

- Add forgotten Display implementation for `mangle_ast::Term`

### 🐛 Bug Fixes

- Fix `repository` field in Cargo.toml (main reason for this release),
  pointed out in #34 (thanks!).

### ⚙️ Miscellaneous Tasks

- Add dependency on 'googletest' crate.

## [0.1.0] - 2024-06-11

- Initial set up.
