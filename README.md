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
| `mangle-wasm` | Browser WASM target (interpreter compiled to `wasm32-unknown-unknown`) |
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
