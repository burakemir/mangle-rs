# Changelog

All notable changes in mangle/rust will be documented in this file.

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
