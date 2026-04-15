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

//! End-to-end round trip for `Op::HashJoin` through codegen + WASM VM.
//!
//! Hand-builds the rule `result(X, Y) :- a(X, Z), b(Z, Y).`, lowers it,
//! compiles with `Codegen::with_hash_join(true)` so the planner emits the
//! new op, runs on the VM with a minimal in-process `Host` implementation,
//! and asserts the output matches the expected natural join.

use anyhow::Result;
use mangle_analysis::LoweringContext;
use mangle_ast as ast;
use mangle_codegen::{Codegen, WasmImportsBackend};
use mangle_vm::{Host, HostVal, Vm};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Val {
    Number(i64),
}

/// Backing of an iterator: either a real relation table (scan) or a
/// materialized list of rows (HashJoin match iterator).
enum IterKind {
    Scan(i32),
    Hj(Vec<Vec<HostVal>>),
}

/// Minimal Host for the hash-join test: integer values only, no strings,
/// no compound types, no delta iteration (we merge on every insert).
struct MemHost {
    values: Vec<Val>,
    tables: HashMap<i32, Vec<Vec<HostVal>>>,
    /// Unified iterator namespace: iter_id → (backing, cursor).
    iters: HashMap<i32, (IterKind, usize)>,
    next_iter_id: i32,
    pending_rel: i32,
    pending_tuple: Vec<HostVal>,
    /// HashJoin state: per-join_id build table keyed by the canonical value
    /// form of the join-key projection (so two different HostVal slab slots
    /// holding the same number hash to the same bucket). Values are the
    /// full build-side rows (keys first).
    hj_tables: HashMap<i32, HashMap<Vec<Val>, Vec<Vec<HostVal>>>>,
    hj_buffer: Vec<HostVal>,
}

impl MemHost {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            tables: HashMap::new(),
            iters: HashMap::new(),
            next_iter_id: 1,
            pending_rel: 0,
            pending_tuple: Vec::new(),
            hj_tables: HashMap::new(),
            hj_buffer: Vec::new(),
        }
    }

    fn alloc(&mut self, v: Val) -> HostVal {
        let idx = self.values.len() as u32;
        self.values.push(v);
        HostVal(idx)
    }

    fn vals_equal(&self, a: HostVal, b: HostVal) -> bool {
        self.values[a.0 as usize] == self.values[b.0 as usize]
    }

    fn key_canonical(&self, tuple: &[HostVal]) -> Vec<Val> {
        tuple.iter().map(|h| self.values[h.0 as usize].clone()).collect()
    }

    fn preload(&mut self, rel_id: i32, rows: Vec<Vec<i64>>) {
        for row in rows {
            let host_row: Vec<HostVal> = row
                .into_iter()
                .map(|n| self.alloc(Val::Number(n)))
                .collect();
            self.tables.entry(rel_id).or_default().push(host_row);
        }
    }

    fn dump_rel(&self, rel_id: i32) -> Vec<Vec<i64>> {
        let empty = Vec::new();
        let rows = self.tables.get(&rel_id).unwrap_or(&empty);
        rows.iter()
            .map(|r| {
                r.iter()
                    .map(|h| match &self.values[h.0 as usize] {
                        Val::Number(n) => *n,
                    })
                    .collect()
            })
            .collect()
    }
}

// Pointer returned by `scan_next` encodes (iter_id in high bits, 1-based row
// index in low 16 bits). iter_ids are limited to 16 bits — plenty for tests.
fn make_ptr(iter_id: i32, one_based_row: usize) -> i32 {
    (iter_id << 16) | (one_based_row as i32)
}
fn ptr_iter_id(ptr: i32) -> i32 {
    (ptr >> 16) & 0xFFFF
}
fn ptr_row_idx(ptr: i32) -> Option<usize> {
    let r = ptr & 0xFFFF;
    if r == 0 { None } else { Some((r - 1) as usize) }
}

impl Host for MemHost {
    fn scan_start(&mut self, rel_id: i32) -> i32 {
        let id = self.next_iter_id;
        self.next_iter_id += 1;
        self.iters.insert(id, (IterKind::Scan(rel_id), 0));
        id
    }
    fn scan_delta_start(&mut self, rel_id: i32) -> i32 {
        // This test doesn't use delta scans; treat as a normal scan.
        self.scan_start(rel_id)
    }
    fn scan_next(&mut self, iter_id: i32) -> i32 {
        if let Some((kind, cursor)) = self.iters.get_mut(&iter_id) {
            let len = match kind {
                IterKind::Scan(rel) => self.tables.get(rel).map(|t| t.len()).unwrap_or(0),
                IterKind::Hj(rows) => rows.len(),
            };
            if *cursor < len {
                *cursor += 1;
                return make_ptr(iter_id, *cursor);
            }
        }
        0
    }
    fn get_col(&mut self, ptr: i32, col: i32) -> HostVal {
        let Some(row_idx) = ptr_row_idx(ptr) else {
            return HostVal(0);
        };
        let iter_id = ptr_iter_id(ptr);
        let Some((kind, _)) = self.iters.get(&iter_id) else {
            return HostVal(0);
        };
        let row_opt: Option<&Vec<HostVal>> = match kind {
            IterKind::Scan(rel) => self.tables.get(rel).and_then(|t| t.get(row_idx)),
            IterKind::Hj(rows) => rows.get(row_idx),
        };
        row_opt
            .and_then(|r| r.get(col as usize).copied())
            .unwrap_or(HostVal(0))
    }
    fn merge_deltas(&mut self) -> i32 {
        0
    }
    fn scan_aggregate_start(&mut self, _rel_id: i32, _desc: Vec<i32>) -> i32 {
        0
    }
    fn scan_index_start(&mut self, _rel_id: i32, _col: i32, _val: HostVal) -> i32 {
        0
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
        let already = self
            .tables
            .get(&rel_id)
            .map(|rows| {
                rows.iter().any(|existing| {
                    existing.len() == tuple.len()
                        && existing
                            .iter()
                            .zip(tuple.iter())
                            .all(|(a, b)| self.vals_equal(*a, *b))
                })
            })
            .unwrap_or(false);
        if !already {
            self.tables.entry(rel_id).or_default().push(tuple);
        }
    }
    fn const_number(&mut self, n: i64) -> HostVal {
        self.alloc(Val::Number(n))
    }
    fn const_float(&mut self, _: i64) -> HostVal {
        HostVal(0)
    }
    fn const_string(&mut self, _: i32) -> HostVal {
        HostVal(0)
    }
    fn const_name(&mut self, _: i32) -> HostVal {
        HostVal(0)
    }
    fn const_time(&mut self, _: i64) -> HostVal {
        HostVal(0)
    }
    fn const_duration(&mut self, _: i64) -> HostVal {
        HostVal(0)
    }
    fn val_add(&mut self, _: HostVal, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn val_sub(&mut self, _: HostVal, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn val_mul(&mut self, _: HostVal, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn val_div(&mut self, _: HostVal, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn val_sqrt(&mut self, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn val_eq(&mut self, a: HostVal, b: HostVal) -> i32 {
        if self.vals_equal(a, b) { 1 } else { 0 }
    }
    fn val_neq(&mut self, a: HostVal, b: HostVal) -> i32 {
        if self.vals_equal(a, b) { 0 } else { 1 }
    }
    fn val_lt(&mut self, _: HostVal, _: HostVal) -> i32 {
        0
    }
    fn val_le(&mut self, _: HostVal, _: HostVal) -> i32 {
        0
    }
    fn val_gt(&mut self, _: HostVal, _: HostVal) -> i32 {
        0
    }
    fn val_ge(&mut self, _: HostVal, _: HostVal) -> i32 {
        0
    }
    fn str_concat(&mut self, _: HostVal, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn str_replace(&mut self, _: HostVal, _: HostVal, _: HostVal, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn val_to_string(&mut self, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn compound_begin(&mut self, _: i32) {}
    fn compound_push(&mut self, _: HostVal) {}
    fn compound_end(&mut self) -> HostVal {
        HostVal(0)
    }
    fn compound_get(&mut self, _: HostVal, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn compound_len(&mut self, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn pair_first(&mut self, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn pair_second(&mut self, _: HostVal) -> HostVal {
        HostVal(0)
    }
    fn debuglog(&mut self, _: HostVal) {}

    fn hash_join_begin(&mut self, join_id: i32) {
        self.hj_tables.insert(join_id, HashMap::new());
        self.hj_buffer.clear();
    }
    fn hash_join_push(&mut self, val: HostVal) {
        self.hj_buffer.push(val);
    }
    fn hash_join_commit_build(&mut self, join_id: i32, n_keys: i32) {
        let row = std::mem::take(&mut self.hj_buffer);
        let n_keys = n_keys as usize;
        let key = self.key_canonical(&row[..n_keys]);
        self.hj_tables
            .entry(join_id)
            .or_default()
            .entry(key)
            .or_default()
            .push(row);
    }
    fn hash_join_probe(&mut self, join_id: i32) -> i32 {
        let key_buf = std::mem::take(&mut self.hj_buffer);
        let key = self.key_canonical(&key_buf);
        let matches = self
            .hj_tables
            .get(&join_id)
            .and_then(|t| t.get(&key))
            .cloned()
            .unwrap_or_default();
        let id = self.next_iter_id;
        self.next_iter_id += 1;
        self.iters.insert(id, (IterKind::Hj(matches), 0));
        id
    }
    fn hash_join_end(&mut self, join_id: i32) {
        self.hj_tables.remove(&join_id);
    }
}

#[derive(Clone)]
struct SharedHost(Arc<Mutex<MemHost>>);

impl Host for SharedHost {
    fn scan_start(&mut self, id: i32) -> i32 {
        self.0.lock().unwrap().scan_start(id)
    }
    fn scan_delta_start(&mut self, id: i32) -> i32 {
        self.0.lock().unwrap().scan_delta_start(id)
    }
    fn scan_next(&mut self, id: i32) -> i32 {
        self.0.lock().unwrap().scan_next(id)
    }
    fn merge_deltas(&mut self) -> i32 {
        self.0.lock().unwrap().merge_deltas()
    }
    fn scan_aggregate_start(&mut self, id: i32, desc: Vec<i32>) -> i32 {
        self.0.lock().unwrap().scan_aggregate_start(id, desc)
    }
    fn scan_index_start(&mut self, id: i32, col: i32, val: HostVal) -> i32 {
        self.0.lock().unwrap().scan_index_start(id, col, val)
    }
    fn get_col(&mut self, p: i32, i: i32) -> HostVal {
        self.0.lock().unwrap().get_col(p, i)
    }
    fn insert_begin(&mut self, id: i32) {
        self.0.lock().unwrap().insert_begin(id)
    }
    fn insert_push(&mut self, v: HostVal) {
        self.0.lock().unwrap().insert_push(v)
    }
    fn insert_end(&mut self) {
        self.0.lock().unwrap().insert_end()
    }
    fn const_number(&mut self, n: i64) -> HostVal {
        self.0.lock().unwrap().const_number(n)
    }
    fn const_float(&mut self, b: i64) -> HostVal {
        self.0.lock().unwrap().const_float(b)
    }
    fn const_string(&mut self, id: i32) -> HostVal {
        self.0.lock().unwrap().const_string(id)
    }
    fn const_name(&mut self, id: i32) -> HostVal {
        self.0.lock().unwrap().const_name(id)
    }
    fn const_time(&mut self, n: i64) -> HostVal {
        self.0.lock().unwrap().const_time(n)
    }
    fn const_duration(&mut self, n: i64) -> HostVal {
        self.0.lock().unwrap().const_duration(n)
    }
    fn val_add(&mut self, a: HostVal, b: HostVal) -> HostVal {
        self.0.lock().unwrap().val_add(a, b)
    }
    fn val_sub(&mut self, a: HostVal, b: HostVal) -> HostVal {
        self.0.lock().unwrap().val_sub(a, b)
    }
    fn val_mul(&mut self, a: HostVal, b: HostVal) -> HostVal {
        self.0.lock().unwrap().val_mul(a, b)
    }
    fn val_div(&mut self, a: HostVal, b: HostVal) -> HostVal {
        self.0.lock().unwrap().val_div(a, b)
    }
    fn val_sqrt(&mut self, a: HostVal) -> HostVal {
        self.0.lock().unwrap().val_sqrt(a)
    }
    fn val_eq(&mut self, a: HostVal, b: HostVal) -> i32 {
        self.0.lock().unwrap().val_eq(a, b)
    }
    fn val_neq(&mut self, a: HostVal, b: HostVal) -> i32 {
        self.0.lock().unwrap().val_neq(a, b)
    }
    fn val_lt(&mut self, a: HostVal, b: HostVal) -> i32 {
        self.0.lock().unwrap().val_lt(a, b)
    }
    fn val_le(&mut self, a: HostVal, b: HostVal) -> i32 {
        self.0.lock().unwrap().val_le(a, b)
    }
    fn val_gt(&mut self, a: HostVal, b: HostVal) -> i32 {
        self.0.lock().unwrap().val_gt(a, b)
    }
    fn val_ge(&mut self, a: HostVal, b: HostVal) -> i32 {
        self.0.lock().unwrap().val_ge(a, b)
    }
    fn str_concat(&mut self, a: HostVal, b: HostVal) -> HostVal {
        self.0.lock().unwrap().str_concat(a, b)
    }
    fn str_replace(&mut self, s: HostVal, o: HostVal, n: HostVal, c: HostVal) -> HostVal {
        self.0.lock().unwrap().str_replace(s, o, n, c)
    }
    fn val_to_string(&mut self, v: HostVal) -> HostVal {
        self.0.lock().unwrap().val_to_string(v)
    }
    fn compound_begin(&mut self, k: i32) {
        self.0.lock().unwrap().compound_begin(k)
    }
    fn compound_push(&mut self, v: HostVal) {
        self.0.lock().unwrap().compound_push(v)
    }
    fn compound_end(&mut self) -> HostVal {
        self.0.lock().unwrap().compound_end()
    }
    fn compound_get(&mut self, c: HostVal, k: HostVal) -> HostVal {
        self.0.lock().unwrap().compound_get(c, k)
    }
    fn compound_len(&mut self, c: HostVal) -> HostVal {
        self.0.lock().unwrap().compound_len(c)
    }
    fn pair_first(&mut self, c: HostVal) -> HostVal {
        self.0.lock().unwrap().pair_first(c)
    }
    fn pair_second(&mut self, c: HostVal) -> HostVal {
        self.0.lock().unwrap().pair_second(c)
    }
    fn debuglog(&mut self, v: HostVal) {
        self.0.lock().unwrap().debuglog(v)
    }
    fn hash_join_begin(&mut self, id: i32) {
        self.0.lock().unwrap().hash_join_begin(id)
    }
    fn hash_join_push(&mut self, v: HostVal) {
        self.0.lock().unwrap().hash_join_push(v)
    }
    fn hash_join_commit_build(&mut self, id: i32, n: i32) {
        self.0.lock().unwrap().hash_join_commit_build(id, n)
    }
    fn hash_join_probe(&mut self, id: i32) -> i32 {
        self.0.lock().unwrap().hash_join_probe(id)
    }
    fn hash_join_end(&mut self, id: i32) {
        self.0.lock().unwrap().hash_join_end(id)
    }
}

fn djb2_hash(name: &str) -> u32 {
    let mut hash: u32 = 5381;
    for c in name.bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(c as u32);
    }
    hash
}

#[test]
fn test_hash_join_wasm_round_trip() -> Result<()> {
    // `result(X, Y) :- a(X, Z), b(Z, Y).`
    let arena = ast::Arena::new_with_global_interner();
    let result_pred = arena.predicate_sym("result", Some(2));
    let a = arena.predicate_sym("a", Some(2));
    let b = arena.predicate_sym("b", Some(2));
    let x = arena.variable("X");
    let y = arena.variable("Y");
    let z = arena.variable("Z");

    let clause = ast::Clause {
        head: arena.atom(result_pred, &[x, y]),
        head_time: None,
        premises: arena.alloc_slice_copy(&[
            arena.alloc(ast::Term::Atom(arena.atom(a, &[x, z]))),
            arena.alloc(ast::Term::Atom(arena.atom(b, &[z, y]))),
        ]),
        transform: &[],
    };
    let unit = ast::Unit {
        decls: &[],
        clauses: arena.alloc_slice_copy(&[&clause]),
    };
    let ctx = LoweringContext::new(&arena);
    let mut ir = ctx.lower_unit(&unit);

    let mut codegen = Codegen::new(&mut ir, WasmImportsBackend).with_hash_join(true);
    let compiled = codegen.generate();

    let mut host = MemHost::new();
    // Relation ids are djb2 hashes of the predicate names.
    let a_id = djb2_hash("a") as i32;
    let b_id = djb2_hash("b") as i32;
    let result_id = djb2_hash("result") as i32;

    host.preload(a_id, vec![vec![1, 10], vec![2, 20], vec![3, 10]]);
    host.preload(
        b_id,
        vec![vec![10, 100], vec![10, 101], vec![20, 200], vec![30, 300]],
    );

    let shared = SharedHost(Arc::new(Mutex::new(host)));
    let vm = Vm::new()?;
    vm.execute(
        &compiled.wasm,
        shared.clone(),
        compiled.strings,
        compiled.names,
    )?;

    let host = shared.0.lock().unwrap();
    let got = host.dump_rel(result_id);
    let mut got_sorted = got.clone();
    got_sorted.sort();

    // Expected natural join: (1,10)⋈(10,100/101) (2,20)⋈(20,200) (3,10)⋈(10,100/101)
    let expected = vec![
        vec![1, 100],
        vec![1, 101],
        vec![2, 200],
        vec![3, 100],
        vec![3, 101],
    ];
    assert_eq!(got_sorted, expected, "got: {got:?}");
    Ok(())
}
