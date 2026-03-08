// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Benchmark comparing three execution modes:
//!
//! 1. **interpreter** — native Rust interpreter (edge mode)
//! 2. **codegen-wasm** — per-program WASM codegen with host calls (server mode)
//! 3. **interp-in-wasm** — full interpreter compiled to WASM, run in wasmtime
//!
//! Uses transitive closure (reachability) on linear graphs of varying sizes.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mangle_ast::Arena;
use mangle_driver::{compile, compile_to_wasm, execute};
use mangle_interpreter::MemStore;
use mangle_vm::{Host, HostVal, Vm};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Graph generation
// ---------------------------------------------------------------------------

/// Build Mangle source for reachability with `n` nodes in a linear chain.
fn reachability_source(n: usize) -> String {
    let mut s = String::new();
    for i in 0..n - 1 {
        s.push_str(&format!("edge({}, {}). ", i, i + 1));
    }
    s.push_str("reachable(Y) :- edge(0, Y). ");
    s.push_str("reachable(Z) :- reachable(Y), edge(Y, Z).");
    s
}

// ---------------------------------------------------------------------------
// Codegen-WASM host (minimal MemHost with delta/merge for fixpoint)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Number(i64),
}

struct WasmMemHost {
    values: Vec<Val>,
    stable: HashMap<i32, Vec<Vec<HostVal>>>,
    delta: HashMap<i32, Vec<Vec<HostVal>>>,
    next_delta: HashMap<i32, Vec<Vec<HostVal>>>,
    iters: HashMap<i32, (i32, usize, bool)>,
    next_iter_id: i32,
    pending_rel: i32,
    pending_tuple: Vec<HostVal>,
    strings: Vec<String>,
    names: Vec<String>,
}

impl WasmMemHost {
    fn new(strings: Vec<String>, names: Vec<String>) -> Self {
        Self {
            values: Vec::new(),
            stable: HashMap::new(),
            delta: HashMap::new(),
            next_delta: HashMap::new(),
            iters: HashMap::new(),
            next_iter_id: 1,
            pending_rel: 0,
            pending_tuple: Vec::new(),
            strings,
            names,
        }
    }

    fn alloc(&mut self, v: Val) -> HostVal {
        let idx = self.values.len() as u32;
        self.values.push(v);
        HostVal(idx)
    }

    fn get_val(&self, hv: HostVal) -> &Val {
        &self.values[hv.0 as usize]
    }

    fn tuples_eq(&self, a: &[HostVal], b: &[HostVal]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b.iter())
                .all(|(x, y)| self.get_val(*x) == self.get_val(*y))
    }

    fn tuple_exists_in(&self, tuple: &[HostVal], set: &[Vec<HostVal>]) -> bool {
        set.iter().any(|t| self.tuples_eq(t, tuple))
    }
}

impl Host for WasmMemHost {
    fn scan_start(&mut self, rel_id: i32) -> i32 {
        let id = self.next_iter_id;
        self.next_iter_id += 1;
        self.iters.insert(id, (rel_id, 0, false));
        id
    }
    fn scan_delta_start(&mut self, rel_id: i32) -> i32 {
        let id = self.next_iter_id;
        self.next_iter_id += 1;
        self.iters.insert(id, (rel_id, 0, true));
        id
    }
    fn scan_next(&mut self, iter_id: i32) -> i32 {
        if let Some((rel_id, idx, is_delta)) = self.iters.get_mut(&iter_id) {
            let rel_id = *rel_id;
            let tuples_opt = if *is_delta {
                self.delta.get(&rel_id)
            } else {
                self.stable.get(&rel_id)
            };
            if let Some(tuples) = tuples_opt {
                if *idx < tuples.len() {
                    let ptr = (iter_id << 16) | (*idx as i32 + 1);
                    *idx += 1;
                    return ptr;
                }
            }
        }
        0
    }
    fn get_col(&mut self, ptr: i32, col_idx: i32) -> HostVal {
        let iter_id = ptr >> 16;
        let tuple_idx = (ptr & 0xFFFF) - 1;
        if let Some((rel_id, _, is_delta)) = self.iters.get(&iter_id) {
            let tuples = if *is_delta {
                self.delta.get(rel_id)
            } else {
                self.stable.get(rel_id)
            };
            if let Some(tuples) = tuples {
                if let Some(row) = tuples.get(tuple_idx as usize) {
                    return row[col_idx as usize];
                }
            }
        }
        HostVal(0)
    }
    fn insert_begin(&mut self, rel_id: i32) {
        self.pending_rel = rel_id;
        self.pending_tuple.clear();
    }
    fn insert_push(&mut self, val: HostVal) {
        self.pending_tuple.push(val);
    }
    fn insert_end(&mut self) {
        let tuple = std::mem::take(&mut self.pending_tuple);
        let rel_id = self.pending_rel;
        if self.stable.get(&rel_id).is_some_and(|v| self.tuple_exists_in(&tuple, v))
            || self.delta.get(&rel_id).is_some_and(|v| self.tuple_exists_in(&tuple, v))
            || self.next_delta.get(&rel_id).is_some_and(|v| self.tuple_exists_in(&tuple, v))
        {
            return;
        }
        self.next_delta.entry(rel_id).or_default().push(tuple);
    }
    fn merge_deltas(&mut self) -> i32 {
        let changed = if !self.next_delta.is_empty() { 1 } else { 0 };
        for (rel, tuples) in self.delta.drain() {
            self.stable.entry(rel).or_default().extend(tuples);
        }
        self.delta = std::mem::take(&mut self.next_delta);
        changed
    }
    fn scan_aggregate_start(&mut self, _rel_id: i32, _desc: Vec<i32>) -> i32 { 0 }
    fn scan_index_start(&mut self, rel_id: i32, col_idx: i32, val: HostVal) -> i32 {
        let target_val = self.get_val(val).clone();
        let mut filtered = Vec::new();
        if let Some(tuples) = self.stable.get(&rel_id) {
            for tuple in tuples {
                if let Some(hv) = tuple.get(col_idx as usize) {
                    if self.get_val(*hv) == &target_val {
                        filtered.push(tuple.clone());
                    }
                }
            }
        }
        let temp_rel_id = -(self.next_iter_id * 1000 + rel_id.abs());
        self.stable.insert(temp_rel_id, filtered);
        let id = self.next_iter_id;
        self.next_iter_id += 1;
        self.iters.insert(id, (temp_rel_id, 0, false));
        id
    }
    fn const_number(&mut self, n: i64) -> HostVal { self.alloc(Val::Number(n)) }
    fn const_float(&mut self, _bits: i64) -> HostVal { HostVal(0) }
    fn const_string(&mut self, _id: i32) -> HostVal { HostVal(0) }
    fn const_name(&mut self, _id: i32) -> HostVal { HostVal(0) }
    fn const_time(&mut self, _nanos: i64) -> HostVal { HostVal(0) }
    fn const_duration(&mut self, _nanos: i64) -> HostVal { HostVal(0) }
    fn val_add(&mut self, a: HostVal, b: HostVal) -> HostVal {
        match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => { let r = Val::Number(a + b); self.alloc(r) }
        }
    }
    fn val_sub(&mut self, a: HostVal, b: HostVal) -> HostVal {
        match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => { let r = Val::Number(a - b); self.alloc(r) }
        }
    }
    fn val_mul(&mut self, a: HostVal, b: HostVal) -> HostVal {
        match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => { let r = Val::Number(a * b); self.alloc(r) }
        }
    }
    fn val_div(&mut self, a: HostVal, b: HostVal) -> HostVal {
        match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) if *b != 0 => { let r = Val::Number(a / b); self.alloc(r) }
            _ => HostVal(0),
        }
    }
    fn val_sqrt(&mut self, _a: HostVal) -> HostVal { HostVal(0) }
    fn val_eq(&mut self, a: HostVal, b: HostVal) -> i32 { (self.get_val(a) == self.get_val(b)) as i32 }
    fn val_neq(&mut self, a: HostVal, b: HostVal) -> i32 { (self.get_val(a) != self.get_val(b)) as i32 }
    fn val_lt(&mut self, a: HostVal, b: HostVal) -> i32 {
        match (self.get_val(a), self.get_val(b)) { (Val::Number(a), Val::Number(b)) => (a < b) as i32 }
    }
    fn val_le(&mut self, a: HostVal, b: HostVal) -> i32 {
        match (self.get_val(a), self.get_val(b)) { (Val::Number(a), Val::Number(b)) => (a <= b) as i32 }
    }
    fn val_gt(&mut self, a: HostVal, b: HostVal) -> i32 {
        match (self.get_val(a), self.get_val(b)) { (Val::Number(a), Val::Number(b)) => (a > b) as i32 }
    }
    fn val_ge(&mut self, a: HostVal, b: HostVal) -> i32 {
        match (self.get_val(a), self.get_val(b)) { (Val::Number(a), Val::Number(b)) => (a >= b) as i32 }
    }
    fn str_concat(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
    fn str_replace(&mut self, _s: HostVal, _o: HostVal, _n: HostVal, _c: HostVal) -> HostVal { HostVal(0) }
    fn val_to_string(&mut self, _val: HostVal) -> HostVal { HostVal(0) }
    fn compound_begin(&mut self, _kind: i32) {}
    fn compound_push(&mut self, _val: HostVal) {}
    fn compound_end(&mut self) -> HostVal { HostVal(0) }
    fn compound_get(&mut self, _c: HostVal, _k: HostVal) -> HostVal { HostVal(0) }
    fn compound_len(&mut self, _c: HostVal) -> HostVal { HostVal(0) }
    fn pair_first(&mut self, _c: HostVal) -> HostVal { HostVal(0) }
    fn pair_second(&mut self, _c: HostVal) -> HostVal { HostVal(0) }
    fn debuglog(&mut self, _val: HostVal) {}
}

#[derive(Clone)]
struct SharedHost {
    inner: Arc<Mutex<WasmMemHost>>,
}

impl Host for SharedHost {
    fn scan_start(&mut self, rel_id: i32) -> i32 { self.inner.lock().unwrap().scan_start(rel_id) }
    fn scan_delta_start(&mut self, rel_id: i32) -> i32 { self.inner.lock().unwrap().scan_delta_start(rel_id) }
    fn scan_next(&mut self, iter_id: i32) -> i32 { self.inner.lock().unwrap().scan_next(iter_id) }
    fn merge_deltas(&mut self) -> i32 { self.inner.lock().unwrap().merge_deltas() }
    fn scan_aggregate_start(&mut self, rel_id: i32, desc: Vec<i32>) -> i32 { self.inner.lock().unwrap().scan_aggregate_start(rel_id, desc) }
    fn scan_index_start(&mut self, rel_id: i32, col_idx: i32, val: HostVal) -> i32 { self.inner.lock().unwrap().scan_index_start(rel_id, col_idx, val) }
    fn get_col(&mut self, ptr: i32, idx: i32) -> HostVal { self.inner.lock().unwrap().get_col(ptr, idx) }
    fn insert_begin(&mut self, rel_id: i32) { self.inner.lock().unwrap().insert_begin(rel_id) }
    fn insert_push(&mut self, val: HostVal) { self.inner.lock().unwrap().insert_push(val) }
    fn insert_end(&mut self) { self.inner.lock().unwrap().insert_end() }
    fn const_number(&mut self, n: i64) -> HostVal { self.inner.lock().unwrap().const_number(n) }
    fn const_float(&mut self, bits: i64) -> HostVal { self.inner.lock().unwrap().const_float(bits) }
    fn const_string(&mut self, id: i32) -> HostVal { self.inner.lock().unwrap().const_string(id) }
    fn const_name(&mut self, id: i32) -> HostVal { self.inner.lock().unwrap().const_name(id) }
    fn const_time(&mut self, nanos: i64) -> HostVal { self.inner.lock().unwrap().const_time(nanos) }
    fn const_duration(&mut self, nanos: i64) -> HostVal { self.inner.lock().unwrap().const_duration(nanos) }
    fn val_add(&mut self, a: HostVal, b: HostVal) -> HostVal { self.inner.lock().unwrap().val_add(a, b) }
    fn val_sub(&mut self, a: HostVal, b: HostVal) -> HostVal { self.inner.lock().unwrap().val_sub(a, b) }
    fn val_mul(&mut self, a: HostVal, b: HostVal) -> HostVal { self.inner.lock().unwrap().val_mul(a, b) }
    fn val_div(&mut self, a: HostVal, b: HostVal) -> HostVal { self.inner.lock().unwrap().val_div(a, b) }
    fn val_sqrt(&mut self, a: HostVal) -> HostVal { self.inner.lock().unwrap().val_sqrt(a) }
    fn val_eq(&mut self, a: HostVal, b: HostVal) -> i32 { self.inner.lock().unwrap().val_eq(a, b) }
    fn val_neq(&mut self, a: HostVal, b: HostVal) -> i32 { self.inner.lock().unwrap().val_neq(a, b) }
    fn val_lt(&mut self, a: HostVal, b: HostVal) -> i32 { self.inner.lock().unwrap().val_lt(a, b) }
    fn val_le(&mut self, a: HostVal, b: HostVal) -> i32 { self.inner.lock().unwrap().val_le(a, b) }
    fn val_gt(&mut self, a: HostVal, b: HostVal) -> i32 { self.inner.lock().unwrap().val_gt(a, b) }
    fn val_ge(&mut self, a: HostVal, b: HostVal) -> i32 { self.inner.lock().unwrap().val_ge(a, b) }
    fn str_concat(&mut self, a: HostVal, b: HostVal) -> HostVal { self.inner.lock().unwrap().str_concat(a, b) }
    fn str_replace(&mut self, s: HostVal, o: HostVal, n: HostVal, c: HostVal) -> HostVal { self.inner.lock().unwrap().str_replace(s, o, n, c) }
    fn val_to_string(&mut self, val: HostVal) -> HostVal { self.inner.lock().unwrap().val_to_string(val) }
    fn compound_begin(&mut self, kind: i32) { self.inner.lock().unwrap().compound_begin(kind) }
    fn compound_push(&mut self, val: HostVal) { self.inner.lock().unwrap().compound_push(val) }
    fn compound_end(&mut self) -> HostVal { self.inner.lock().unwrap().compound_end() }
    fn compound_get(&mut self, c: HostVal, k: HostVal) -> HostVal { self.inner.lock().unwrap().compound_get(c, k) }
    fn compound_len(&mut self, c: HostVal) -> HostVal { self.inner.lock().unwrap().compound_len(c) }
    fn pair_first(&mut self, c: HostVal) -> HostVal { self.inner.lock().unwrap().pair_first(c) }
    fn pair_second(&mut self, c: HostVal) -> HostVal { self.inner.lock().unwrap().pair_second(c) }
    fn debuglog(&mut self, val: HostVal) { self.inner.lock().unwrap().debuglog(val) }
}

// ---------------------------------------------------------------------------
// Interpreter-in-WASM runner (calls mangle-wasm's run_raw via wasmtime)
// ---------------------------------------------------------------------------

struct InterpInWasm {
    engine: wasmtime::Engine,
    module: wasmtime::Module,
}

impl InterpInWasm {
    fn new() -> Self {
        let wasm_bytes = include_bytes!(
            "../../../target/wasm32-unknown-unknown/release/mangle_wasm.wasm"
        );
        let engine = wasmtime::Engine::default();
        let module = wasmtime::Module::new(&engine, wasm_bytes)
            .expect("failed to compile mangle-wasm module");
        Self { engine, module }
    }

    fn run(&self, source: &str, facts_json: &str) {
        let mut store = wasmtime::Store::new(&self.engine, ());
        let instance = wasmtime::Instance::new(&mut store, &self.module, &[])
            .expect("failed to instantiate");

        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("no memory export");
        let alloc_fn = instance
            .get_typed_func::<u32, u32>(&mut store, "alloc")
            .expect("no alloc export");
        let run_raw_fn = instance
            .get_typed_func::<(u32, u32, u32, u32), i64>(&mut store, "run_raw")
            .expect("no run_raw export");

        // Write source string into WASM memory
        let src_bytes = source.as_bytes();
        let src_ptr = alloc_fn
            .call(&mut store, src_bytes.len() as u32)
            .expect("alloc failed");
        memory.data_mut(&mut store)[src_ptr as usize..src_ptr as usize + src_bytes.len()]
            .copy_from_slice(src_bytes);

        // Write facts JSON into WASM memory
        let facts_bytes = facts_json.as_bytes();
        let facts_ptr = alloc_fn
            .call(&mut store, facts_bytes.len() as u32)
            .expect("alloc failed");
        memory.data_mut(&mut store)[facts_ptr as usize..facts_ptr as usize + facts_bytes.len()]
            .copy_from_slice(facts_bytes);

        // Call run_raw
        let result = run_raw_fn
            .call(
                &mut store,
                (
                    src_ptr,
                    src_bytes.len() as u32,
                    facts_ptr,
                    facts_bytes.len() as u32,
                ),
            )
            .expect("run_raw failed");
        assert!(result != 0, "run_raw returned error");
    }
}

// ---------------------------------------------------------------------------
// Benchmark functions
// ---------------------------------------------------------------------------

/// Interpreter path: parse → compile → execute via interpreter.
fn bench_interpreter(source: &str) {
    let arena = Arena::new_with_global_interner();
    let (mut ir, stratified) = compile(source, &arena).expect("compile failed");
    let store = Box::new(MemStore::new());
    let _interpreter = execute(&mut ir, &stratified, store).expect("execute failed");
}

/// Codegen-WASM path: pre-compiled WASM, benchmark instantiation + execution.
fn bench_codegen_wasm(vm: &Vm, wasm: &[u8], strings: &[String], names: &[String]) {
    let host = WasmMemHost::new(strings.to_vec(), names.to_vec());
    let shared = SharedHost {
        inner: Arc::new(Mutex::new(host)),
    };
    vm.execute(wasm, shared, strings.to_vec(), names.to_vec())
        .expect("wasm execute failed");
}

fn reachability_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("reachability");

    // Pre-load the interpreter-in-WASM module (shared across sizes)
    let interp_wasm = InterpInWasm::new();

    for &n in &[10, 50, 100, 500, 1000, 5000] {
        let source = reachability_source(n);

        // 1. Native interpreter
        group.bench_with_input(
            BenchmarkId::new("interpreter", n),
            &source,
            |b, source| {
                b.iter(|| bench_interpreter(source));
            },
        );

        // 2. Codegen WASM (server mode)
        let arena = Arena::new_with_global_interner();
        let (mut ir, stratified) = compile(&source, &arena).expect("compile failed");
        let compiled = compile_to_wasm(&mut ir, &stratified);
        let vm = Vm::new().expect("vm creation failed");

        group.bench_with_input(BenchmarkId::new("codegen-wasm", n), &n, |b, _n| {
            b.iter(|| {
                bench_codegen_wasm(&vm, &compiled.wasm, &compiled.strings, &compiled.names)
            });
        });

        // 3. Interpreter-in-WASM
        group.bench_with_input(
            BenchmarkId::new("interp-in-wasm", n),
            &source,
            |b, source| {
                b.iter(|| interp_wasm.run(source, "{}"));
            },
        );
    }

    group.finish();
}

criterion_group!(benches, reachability_benchmark);
criterion_main!(benches);
