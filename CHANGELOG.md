# Changelog

All notable changes in mangle/rust will be documented in this file.

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
