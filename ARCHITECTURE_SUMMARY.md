# Mangle Rust Implementation Architecture Summary

This document describes the architecture of `mangle-rs`, the Rust implementation
of the [Mangle](https://mangle.readthedocs.io/en/latest/) deductive database
language.

## 1. Abstract Syntax Tree (AST) & Parsing

**Crates:** `mangle-ast`, `mangle-parse`

*   **Memory Management:** Bump-pointer allocator (`bumpalo`) for arena-allocated
    AST nodes. All AST types carry a lifetime tied to the arena.
*   **Interning:** Global string `Interner` deduplicates predicate names,
    variable names, and string constants.
*   **Parser:** Recursive-descent parser producing `ast::Unit` (declarations +
    clauses) from source bytes. Supports Mangle syntax including transforms
    (`|> let X = ...`), negation (`!p(X)`), and package/use directives.

## 2. Intermediate Representation (IR)

**Crate:** `mangle-ir`

*   **Design:** Flat, vector-based representation inspired by Carbon's SemIR.
    Instructions are identified by `InstId` indices into a single `Vec<Inst>`.
*   **Logical IR (`Inst`):** Declarative logic — `Rule`, `Atom`, `NegAtom`,
    `Expr`, `Const`, `Variable`, `LetTransform`.
*   **Physical Plan IR (`physical::Op`):** Imperative execution plan — `Iterate`,
    `IterateDelta`, `IndexLookup`, `Filter`, `Insert`, `Aggregate`, `Project`.
    Produced by the `Planner`.

## 3. Analysis & Lowering

**Crate:** `mangle-analysis`

*   **Lowering (`lowering.rs`):** Converts AST into Logical IR.
*   **Planner (`planner.rs`):** Transforms Logical IR rules into Physical Plan
    IR. Generates nested-loop joins, index lookups (when a body atom has bound
    constants), and semi-naive delta plans for recursive predicates.
*   **Stratification (`program.rs`):** Dependency analysis and topological
    ordering of predicates to handle negation correctly. Produces
    `StratifiedProgram` with ordered strata.
*   **Type Checking (`type_check.rs`):** Validates predicate arities and type
    consistency.

## 4. Driver & Orchestration

**Crate:** `mangle-driver`

*   **API:**
    *   `compile(source, arena)` — Parse → rename → stratify → lower to IR.
    *   `execute(ir, stratified, store)` — Plan → interpret (Edge Mode).
    *   `compile_to_wasm(ir, stratified)` — Plan → codegen → `CompiledModule`.
*   **Multi-unit compilation:** `compile_units()` supports multiple source units
    with package/use namespacing.
*   **Fixpoint evaluation:** For recursive strata, runs semi-naive iteration
    (initial rules → merge deltas → delta rules → repeat until no changes).

## 5. Execution Modes

### A. Server Mode (WASM)

**Crates:** `mangle-codegen`, `mangle-vm`

*   **Value representation:** All Mangle values (numbers, floats, strings, names,
    timestamps, durations, compounds) are opaque `externref` handles in WASM.
    Each handle wraps a `HostVal(u32)` index into the host's value slab.

*   **Codegen (`mangle-codegen`):**
    *   Translates Physical Plan IR into WASM bytecode using `wasm-encoder`.
    *   Generates a single `run()` function with the fixpoint loop embedded.
    *   38 host imports organized as:
        *   **Scan/iter control** (0-10): `scan_start`, `scan_next`, `get_col`,
            `insert_begin/push/end`, `scan_delta_start`, `merge_deltas`,
            `debuglog`, `scan_index_start`, `scan_aggregate_start`
        *   **Constants** (11-16): `const_number`, `const_float`, `const_string`,
            `const_name`, `const_time`, `const_duration`
        *   **Arithmetic** (17-21): `val_add/sub/mul/div`, `val_sqrt`
        *   **Comparisons** (22-27): `val_eq/neq/lt/le/gt/ge`
        *   **String ops** (28-30): `str_concat`, `str_replace`, `val_to_string`
        *   **Compound ops** (31-37): `compound_begin/push/end`,
            `compound_get/len`, `pair_first/second`
    *   Returns `CompiledModule { wasm, strings, names }`.

*   **VM (`mangle-vm`):**
    *   Executes compiled WASM via `wasmtime`.
    *   Links all 38 imports using the `Host` trait.
    *   `externref` boxing/unboxing via `extract_hv()` / `make_ref()`.
    *   Pluggable storage: any `Host` impl works (MemHost, CsvHost,
        CompositeHost, SimpleColumnHost).
    *   `CompositeHost`: Routes relations to different sub-hosts with a value
        remapping table for HostVal handles.

### B. Edge Mode (Interpreter)

**Crate:** `mangle-interpreter`

*   **Interpreter:** Directly interprets `physical::Op` operations.
*   **Store trait:** Abstract interface for relation storage with operations for
    scan, scan_delta, scan_index, insert, merge_deltas, retract, clear.
*   **MemStore:** Default in-memory implementation with hash-set-based dedup and
    delta tracking for semi-naive evaluation.
*   **Value enum:** `Number(i64)`, `Float(f64)`, `String(String)`, `Name(String)`,
    `Time(i64)`, `Duration(i64)`, `Compound(CompoundKind, Vec<Value>)`.

## 6. Shared Interfaces

**Crate:** `mangle-common`

Acts as the central interface crate to prevent dependency cycles.

*   **Feature `edge`:**
    *   `Value` enum — typed values used by the interpreter.
    *   `Store` trait — abstract storage for Edge Mode.
*   **Feature `server`:**
    *   `HostVal(u32)` — opaque handle to a value in the host's slab.
    *   `Host` trait — 34-method interface for WASM host callbacks.

## 7. Storage Adapters

*   **`mangle-simplecolumn`:** Reads the SimpleColumn columnar file format.
    Provides `SimpleColumnStore` (Edge) and `SimpleColumnHost` (Server).
*   **`mangle-vm::csv_host`:** CSV-based storage for Server Mode.
*   **`mangle-vm::composite_host`:** Routes relations to different sub-hosts.
*   **`mangle-db`:** Persistent storage layer with durable EDB writes.

## 8. Browser WASM Target

**Crate:** `mangle-wasm`

*   Compiles the edge-mode interpreter to `wasm32-unknown-unknown` for browser use.
*   **Dynamic mode:** `run_mangle(source, facts_json)` — supply program and data at runtime.
*   **Bundled mode:** `run_bundled(facts_json)` — program baked in at compile time via
    `MANGLE_PROGRAM` env var (partial evaluation).
*   `wasm-bindgen` is optional (feature `bindgen`). Without it, raw C-ABI exports
    (`alloc`, `dealloc`, `run_raw`) enable use from wasmtime or other WASM runtimes.
*   Built with: `wasm-pack build --target web crates/mangle-wasm`

## 9. Performance

A criterion benchmark (`mangle-driver/benches/wasm_vs_interpreter.rs`) compares
three execution modes on transitive closure (reachability) over linear graphs:

1.  **Interpreter** — native Rust interpreter (edge mode).
2.  **Codegen-WASM** — per-program WASM codegen with externref host calls (server mode).
3.  **Interp-in-WASM** — full interpreter compiled to WASM, run in wasmtime.

At small-medium sizes (up to ~1000 nodes), interp-in-WASM outperforms
codegen-WASM because it avoids the externref host-call boundary. At 5000 nodes,
codegen-WASM (2.2x native) beats interp-in-WASM (6.3x native), as the
JIT-compiled control flow is more efficient than interpreted IR dispatch inside
WASM.

## 10. Key Data Structures

*   **`InstId`**: Index into the IR instruction vector.
*   **`NameId`**: Interned string reference.
*   **`physical::Op`**: Imperative operation tree (Iterate, Insert, etc.).
*   **`HostVal(u32)`**: Opaque value handle for the WASM/host boundary.
*   **`CompiledModule`**: WASM bytecode + string/name tables.
*   **`StratifiedProgram`**: Ordered strata with extensional/intensional pred sets.
