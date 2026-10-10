# Mangle (Rust)

[Mangle](https://mangle.readthedocs.io/en/latest/) is a language for
deductive database programming based on Datalog.

This is the Rust implementation of Mangle, featuring a modern compiler pipeline
with two execution modes:

1.  **Server Mode**: Compiles to WebAssembly (WASM) and executes via
    [wasmtime](https://wasmtime.dev/). All values are represented as opaque
    `externref` handles passed between WASM and a pluggable host.
2.  **Edge Mode**: Uses a pure Rust interpreter for lightweight, self-contained
    execution.

Both modes share the same front-end (parsing, analysis, IR, planning) and
diverge only at the final execution stage.

## Architecture

```
Source ──> Parser ──> AST ──> Analysis ──> IR ──> Planner ──> Physical Plan
                                                                  │
                                          ┌───────────────────────┤
                                          ▼                       ▼
                                    Interpreter             Codegen (WASM)
                                    (Edge Mode)                   │
                                          │                       ▼
                                          ▼                  VM (wasmtime)
                                       MemStore             (Server Mode)
                                                                  │
                                                                  ▼
                                                           Host trait impl
                                                        (MemHost, CSV, etc.)
```

### Pipeline Stages

1.  **Parsing & AST** (`mangle-ast`, `mangle-parse`):
    Arena-allocated AST with interned identifiers.

2.  **Analysis & Lowering** (`mangle-analysis`):
    Stratification, binding (safety) analysis, bounds checking (type
    inference with declared bounds, function arity and argument-type
    checking), AST-to-IR lowering, and query planning (nested-loop joins,
    index lookups, semi-naive delta iteration).

3.  **Intermediate Representation** (`mangle-ir`):
    Flat, indexed representation (logical `Inst` + physical `Op`).

4.  **Driver** (`mangle-driver`):
    Orchestrates the full pipeline. Provides `compile()`, `execute()`, and
    `compile_to_wasm()`.

5.  **Execution**:

    *   **Server Mode** (`mangle-codegen` + `mangle-vm`):
        Generates WASM with 54 host imports covering scan/insert, constants,
        arithmetic, comparisons, string operations, compound types, negation
        checks, and aggregations.
        Values cross the WASM boundary as `externref` handles backed by an
        in-host value slab (`HostVal(u32)`). The `Host` trait abstracts
        storage, enabling pluggable backends (in-memory, CSV, composite).

    *   **Edge Mode** (`mangle-interpreter`):
        Directly interprets physical plan operations against a `Store` trait
        implementation (default: `MemStore`).

## Crates

| Crate | Description |
|---|---|
| `mangle-ast` | Arena-allocated Abstract Syntax Tree |
| `mangle-parse` | Recursive-descent parser |
| `mangle-ir` | Flat indexed IR (logical + physical plan) |
| `mangle-analysis` | Lowering, type checking, stratification, query planning |
| `mangle-driver` | Pipeline orchestration and high-level API |
| `mangle-codegen` | WASM code generation backend |
| `mangle-vm` | Wasmtime-based WASM runtime with `Host` trait |
| `mangle-interpreter` | Pure Rust interpreter with `Store` trait |
| `mangle-common` | Shared types (`Value`, `Store`, `Host`, `HostVal`) |
| `mangle-wasm` | Browser WASM target (interpreter compiled to `wasm32-unknown-unknown`) |
| `mangle-simplecolumn` | SimpleColumn file format reader + `Host`/`Store` adapters |
| `mangle-db` | Persistent storage layer |
| `mangle-delta` | Delta Lake EDB source (`EdbSource` impl with predicate pushdown) |
| `mangle-parquet` | Plain Parquet EDB source (row-group pruning) |
| `mangle-server` | HTTP server for Mangle queries |
| `mangle-proto` | Protobuf / Connect RPC interface (`MangleService`) |
| `mangle-ffi` | Stable C ABI over the engine (see `include/mangle.h`) |
| `mangle-engine` | (Legacy) AST-level interpreter |

## Type Support

Both execution modes support:

*   **Scalars**: integers (`i64`), floats (`f64`), strings, names, timestamps, durations
*   **Compounds**: lists, pairs, maps, structs (constructed via `fn:list`, `fn:pair`, `fn:map`, `fn:struct`)
*   **String operations**: `fn:string:concat`, `fn:string:replace`, `fn:number:to_string`, etc.
*   **Arithmetic**: `fn:plus`, `fn:minus`, `fn:mult`, `fn:div`, `fn:sqrt`
*   **Time & durations**: `fn:time:*` (`year`, `month`, `format`, `parse_rfc3339`, `trunc`, `add`, `sub`, ...) and `fn:duration:*` (`from_hours`, `from_seconds`, `hours`, `nanos`, ...) — interpreter only, see Parity TODO
*   **Comparisons**: `=`, `!=`, `<`, `<=`, `>`, `>=` (including cross-type numeric ordering)

## Parity TODO

The long-term goal is parity with
[mangle-go](https://codeberg.org/TauCeti/mangle-go). Status and remaining gaps:

### WASM codegen vs. interpreter

Every physical-plan operation (`Op`, `Condition`) now has WASM emission and
is covered by the interpreter-parity test suite in `mangle-driver`. The
remaining gap is expression functions (`Expr::Call`) that the interpreter
evaluates but WASM codegen rejects with a panic:

*   `fn:list:append`
*   `fn:map:keys`, `fn:map:values`, `fn:struct:values`
*   all `fn:duration:*` functions (`from_hours`, `from_seconds`, `hours`,
    `nanos`, `add`, `mult`, `parse`, ...)
*   all `fn:time:*` functions (`year`, `month`, `format`, `parse_rfc3339`,
    `trunc`, `add`, `sub`, ...); `fn:time:now` additionally needs a decision
    on non-determinism in compiled modules

### Interpreter vs. mangle-go

*   `:match_entry` (map matching): the positive form is silently broken —
    the planner has no arm for it and falls through to a relation lookup on
    a relation that never exists, so rules using it return no rows. Its
    negation is a loud compile error. Needs a dedicated `Op` like
    `MatchField`/`IterateList`.
*   Missing built-in predicates: `:string:matches` (RE2), `:filter`,
    `:match_pair`, `:match_cons`, `:match_nil`, `:within_distance`,
    `:float:lt`/`:le`/`:gt`/`:ge`, and the interval-algebra predicates
    (`:interval:before`, `:interval:after`, ...).
*   Missing functions: `fn:mod` and several others present in mangle-go's
    `builtin` package.

### Static analysis vs. mangle-go

The bounds checker (`mangle-analysis`) now covers the core of mangle-go's
`analysis` package: binding (safety) analysis, function arity and
argument-type checking, declared-bounds checking with feasible-alternatives
inference, filter-predicate typing, and empty-meet detection. Remaining
known gaps:

*   **Duplicate declarations** are not rejected (the last `Decl` wins);
    mangle-go errors (its issue #25).
*   **Undefined predicates** in rule bodies are not rejected — deliberate,
    since this implementation supports loading EDB facts at runtime
    without a declaration.
*   **Mode declarations** (`descr [ mode('+', '-') ]`) are not implemented:
    all non-builtin atom positions bind.
*   **Option types**: mangle-go types `fn:list:get` as `.Option<T>`;
    this implementation infers the bare element type `T` — a deliberate
    divergence until option types exist here.

## Try it in a browser

Build the interpreter to WebAssembly and open a small playground page:

```bash
scripts/playground.sh            # serves on http://localhost:8000
```

Requires [`wasm-pack`](https://rustwasm.github.io/wasm-pack/installer/) and
`python3`. See `crates/mangle-wasm/README.md` for the underlying JS API.

## Usage

### Running Tests

```bash
cargo test                              # all tests
cargo test --features csv_storage -p mangle-vm  # CSV storage tests
```

> **Note:** `mangle-py` (the PyO3 Python bindings) is excluded from the
> workspace `default-members`, so root-level `cargo build` / `cargo test` skip
> it. With the `extension-module` feature enabled, Python symbols are resolved
> at load time, which only links when cargo runs from the crate directory (it
> uses `crates/mangle-py/.cargo/config.toml`) or via `maturin`. Build it with:
>
> ```bash
> cd crates/mangle-py && cargo build    # or: maturin develop
> ```

### Benchmarks

A criterion benchmark compares three execution modes on transitive closure
(reachability) over linear graphs:

```bash
cargo bench --bench wasm_vs_interpreter --features server -p mangle-driver
```

Representative results (Apple Silicon):

| Nodes | Interpreter | Codegen-WASM | Interp-in-WASM |
|---|---|---|---|
| 10 | 22 µs | 571 µs (26x) | 66 µs (3x) |
| 100 | 188 µs | 2.4 ms (13x) | 555 µs (3x) |
| 1000 | 3.7 ms | 22.3 ms (6x) | 17.9 ms (4.8x) |
| 5000 | 59 ms | 133 ms (2.2x) | 371 ms (6.3x) |

*   **Codegen-WASM** (server mode): High per-invocation cost from externref
    host-call boundary crossing, but the JIT-compiled control flow scales well.
*   **Interp-in-WASM**: The full interpreter compiled to `wasm32-unknown-unknown`
    and run via wasmtime. Low overhead at small sizes (no host calls), but at
    scale the interpreted dispatch inside WASM becomes the bottleneck.

### Example: Edge Mode

```rust
use mangle_ast::Arena;
use mangle_driver::{compile, execute};
use mangle_interpreter::MemStore;

let arena = Arena::new_with_global_interner();
let source = "p(1). q(X) :- p(X).";

let (mut ir, stratified) = compile(source, &arena)?;
let store = Box::new(MemStore::new());
let interpreter = execute(&mut ir, &stratified, store)?;

for fact in interpreter.store().scan("q")? {
    println!("{:?}", fact);
}
```

### Example: Server Mode (WASM)

```rust
use mangle_ast::Arena;
use mangle_driver::{compile, compile_to_wasm};
use mangle_vm::Vm;

let arena = Arena::new_with_global_interner();
let source = "p(1). q(X) :- p(X).";

let (mut ir, stratified) = compile(source, &arena)?;
let compiled = compile_to_wasm(&mut ir, &stratified);

let vm = Vm::new()?;
vm.execute(&compiled.wasm, my_host, compiled.strings, compiled.names)?;
```
