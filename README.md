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
    Stratification, type checking, AST-to-IR lowering, and query planning
    (nested-loop joins, index lookups, semi-naive delta iteration).

3.  **Intermediate Representation** (`mangle-ir`):
    Flat, indexed representation (logical `Inst` + physical `Op`).

4.  **Driver** (`mangle-driver`):
    Orchestrates the full pipeline. Provides `compile()`, `execute()`, and
    `compile_to_wasm()`.

5.  **Execution**:

    *   **Server Mode** (`mangle-codegen` + `mangle-vm`):
        Generates WASM with 38 host imports covering scan/insert, constants,
        arithmetic, comparisons, string operations, and compound types.
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
| `mangle-simplecolumn` | SimpleColumn file format reader + `Host`/`Store` adapters |
| `mangle-db` | Persistent storage layer |
| `mangle-server` | HTTP server for Mangle queries |
| `mangle-engine` | (Legacy) AST-level interpreter |

## Type Support

Both execution modes support:

*   **Scalars**: integers (`i64`), floats (`f64`), strings, names, timestamps, durations
*   **Compounds**: lists, pairs, maps, structs (constructed via `fn:list`, `fn:pair`, `fn:map`, `fn:struct`)
*   **String operations**: `fn:string:concat`, `fn:string:replace`, `fn:number:to_string`, etc.
*   **Arithmetic**: `fn:plus`, `fn:minus`, `fn:mult`, `fn:div`, `fn:sqrt`
*   **Comparisons**: `=`, `!=`, `<`, `<=`, `>`, `>=` (including cross-type numeric ordering)

## Usage

### Running Tests

```bash
cargo test                              # all tests
cargo test --features csv_storage -p mangle-vm  # CSV storage tests
```

### Benchmarks

A criterion benchmark compares interpreter vs WASM on transitive closure
(reachability) over linear graphs:

```bash
cargo bench --bench wasm_vs_interpreter --features server -p mangle-driver
```

Representative results (Apple Silicon):

| Nodes | Interpreter | WASM | Ratio |
|---|---|---|---|
| 10 | 22 us | 550 us | 25x |
| 100 | 187 us | 2.4 ms | 13x |
| 1000 | 3.9 ms | 22.5 ms | 5.8x |
| 5000 | 63 ms | 132 ms | 2.1x |

The gap narrows with larger workloads as one-off costs (WASM instantiation)
are amortized. The remaining overhead comes from externref host-call boundary
crossing on every value operation.

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
