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

use anyhow::Result;
use mangle_analysis::{LoweringContext, Program};
use mangle_ast as ast;
use mangle_codegen::{Codegen, WasmImportsBackend};
use mangle_vm::{Host, HostVal, Vm};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// Value slab entry
#[derive(Debug, Clone, PartialEq)]
enum Val {
    Number(i64),
}

// A MemHost with delta-merge support for fixpoint iteration
struct MemHost {
    values: Vec<Val>,
    // stable: rel_id -> List of Tuples (as HostVal handles)
    stable: HashMap<i32, Vec<Vec<HostVal>>>,
    // delta: rel_id -> List of Tuples
    delta: HashMap<i32, Vec<Vec<HostVal>>>,
    // next_delta: rel_id -> List of Tuples
    next_delta: HashMap<i32, Vec<Vec<HostVal>>>,

    iters: HashMap<i32, (i32, usize, bool)>, // (rel_id, idx, is_delta)
    next_iter_id: i32,
    // Pending multi-column insert
    pending_rel: i32,
    pending_tuple: Vec<HostVal>,
    // String/name tables
    strings: Vec<String>,
    names: Vec<String>,
}

impl MemHost {
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

    fn hash_name(name: &str) -> i32 {
        let mut hash: u32 = 5381;
        for c in name.bytes() {
            hash = ((hash << 5).wrapping_add(hash)).wrapping_add(c as u32);
        }
        hash as i32
    }

    fn alloc(&mut self, v: Val) -> HostVal {
        let idx = self.values.len() as u32;
        self.values.push(v);
        HostVal(idx)
    }

    fn get_val(&self, hv: HostVal) -> &Val {
        &self.values[hv.0 as usize]
    }

    fn add_fact(&mut self, rel: &str, args: &[i64]) {
        let id = Self::hash_name(rel);
        let hvs: Vec<HostVal> = args.iter().map(|n| self.alloc(Val::Number(*n))).collect();
        // Initial facts go to stable AND delta
        self.stable.entry(id).or_default().push(hvs.clone());
        self.delta.entry(id).or_default().push(hvs);
    }

    fn get_facts(&self, rel: &str) -> Vec<Vec<i64>> {
        let id = Self::hash_name(rel);
        let mut all = Vec::new();
        if let Some(tuples) = self.stable.get(&id) {
            for t in tuples {
                let row: Vec<i64> = t
                    .iter()
                    .map(|hv| match self.get_val(*hv) {
                        Val::Number(n) => *n,
                    })
                    .collect();
                if !all.contains(&row) {
                    all.push(row);
                }
            }
        }
        all
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

impl Host for MemHost {
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

        // Dedup: check stable, delta, and next_delta
        if self
            .stable
            .get(&rel_id)
            .is_some_and(|v| self.tuple_exists_in(&tuple, v))
            || self
                .delta
                .get(&rel_id)
                .is_some_and(|v| self.tuple_exists_in(&tuple, v))
            || self
                .next_delta
                .get(&rel_id)
                .is_some_and(|v| self.tuple_exists_in(&tuple, v))
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

    fn scan_aggregate_start(&mut self, _rel_id: i32, _desc: Vec<i32>) -> i32 {
        0
    }
    fn scan_index_start(&mut self, rel_id: i32, col_idx: i32, val: HostVal) -> i32 {
        // Filter scan: only return rows where column col_idx matches val.
        // We build a filtered subset and store it as a virtual relation.
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
        // Store the filtered result in a temporary relation slot.
        // Use negative rel_ids to avoid collisions.
        let temp_rel_id = -(self.next_iter_id * 1000 + rel_id.abs());
        self.stable.insert(temp_rel_id, filtered);
        let id = self.next_iter_id;
        self.next_iter_id += 1;
        self.iters.insert(id, (temp_rel_id, 0, false));
        id
    }

    fn const_number(&mut self, n: i64) -> HostVal {
        self.alloc(Val::Number(n))
    }
    fn const_float(&mut self, _bits: i64) -> HostVal {
        HostVal(0)
    }
    fn const_string(&mut self, _id: i32) -> HostVal {
        HostVal(0)
    }
    fn const_name(&mut self, _id: i32) -> HostVal {
        HostVal(0)
    }
    fn const_time(&mut self, _nanos: i64) -> HostVal {
        HostVal(0)
    }
    fn const_duration(&mut self, _nanos: i64) -> HostVal {
        HostVal(0)
    }
    fn val_add(&mut self, a: HostVal, b: HostVal) -> HostVal {
        let result = match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => Val::Number(a + b),
        };
        self.alloc(result)
    }
    fn val_sub(&mut self, a: HostVal, b: HostVal) -> HostVal {
        let result = match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => Val::Number(a - b),
        };
        self.alloc(result)
    }
    fn val_mul(&mut self, a: HostVal, b: HostVal) -> HostVal {
        let result = match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => Val::Number(a * b),
        };
        self.alloc(result)
    }
    fn val_div(&mut self, a: HostVal, b: HostVal) -> HostVal {
        let result = match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) if *b != 0 => Val::Number(a / b),
            _ => Val::Number(0),
        };
        self.alloc(result)
    }
    fn val_sqrt(&mut self, _a: HostVal) -> HostVal {
        HostVal(0)
    }
    fn val_eq(&mut self, a: HostVal, b: HostVal) -> i32 {
        (self.get_val(a) == self.get_val(b)) as i32
    }
    fn val_neq(&mut self, a: HostVal, b: HostVal) -> i32 {
        (self.get_val(a) != self.get_val(b)) as i32
    }
    fn val_lt(&mut self, a: HostVal, b: HostVal) -> i32 {
        match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => (a < b) as i32,
        }
    }
    fn val_le(&mut self, a: HostVal, b: HostVal) -> i32 {
        match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => (a <= b) as i32,
        }
    }
    fn val_gt(&mut self, a: HostVal, b: HostVal) -> i32 {
        match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => (a > b) as i32,
        }
    }
    fn val_ge(&mut self, a: HostVal, b: HostVal) -> i32 {
        match (self.get_val(a), self.get_val(b)) {
            (Val::Number(a), Val::Number(b)) => (a >= b) as i32,
        }
    }
    fn str_concat(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
    fn str_replace(&mut self, _s: HostVal, _old: HostVal, _new: HostVal, _count: HostVal) -> HostVal { HostVal(0) }
    fn val_to_string(&mut self, _val: HostVal) -> HostVal { HostVal(0) }
    fn compound_begin(&mut self, _kind: i32) {}
    fn compound_push(&mut self, _val: HostVal) {}
    fn compound_end(&mut self) -> HostVal { HostVal(0) }
    fn compound_get(&mut self, _compound: HostVal, _key: HostVal) -> HostVal { HostVal(0) }
    fn compound_len(&mut self, _compound: HostVal) -> HostVal { HostVal(0) }
    fn pair_first(&mut self, _compound: HostVal) -> HostVal { HostVal(0) }
    fn pair_second(&mut self, _compound: HostVal) -> HostVal { HostVal(0) }
    fn debuglog(&mut self, _val: HostVal) {}
}

// Wrapper for thread-safety (Arc<Mutex>)
#[derive(Clone)]
struct SharedMemHost {
    inner: Arc<Mutex<MemHost>>,
}

macro_rules! delegate_host {
    () => {
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
        fn str_replace(&mut self, s: HostVal, old: HostVal, new: HostVal, count: HostVal) -> HostVal { self.inner.lock().unwrap().str_replace(s, old, new, count) }
        fn val_to_string(&mut self, val: HostVal) -> HostVal { self.inner.lock().unwrap().val_to_string(val) }
        fn compound_begin(&mut self, kind: i32) { self.inner.lock().unwrap().compound_begin(kind) }
        fn compound_push(&mut self, val: HostVal) { self.inner.lock().unwrap().compound_push(val) }
        fn compound_end(&mut self) -> HostVal { self.inner.lock().unwrap().compound_end() }
        fn compound_get(&mut self, compound: HostVal, key: HostVal) -> HostVal { self.inner.lock().unwrap().compound_get(compound, key) }
        fn compound_len(&mut self, compound: HostVal) -> HostVal { self.inner.lock().unwrap().compound_len(compound) }
        fn pair_first(&mut self, compound: HostVal) -> HostVal { self.inner.lock().unwrap().pair_first(compound) }
        fn pair_second(&mut self, compound: HostVal) -> HostVal { self.inner.lock().unwrap().pair_second(compound) }
        fn debuglog(&mut self, val: HostVal) { self.inner.lock().unwrap().debuglog(val) }
    };
}

impl Host for SharedMemHost {
    delegate_host!();
}

#[test]
fn test_reachability_arity1() -> Result<()> {
    // Problem: Reachable nodes from node 1.
    // edge(1, 2). edge(2, 3). edge(3, 4).
    // reachable(Y) :- edge(1, Y).
    // reachable(Z) :- reachable(Y), edge(Y, Z).

    let arena = ast::Arena::new_with_global_interner();
    let edge = arena.predicate_sym("edge", Some(2));
    let reachable = arena.predicate_sym("reachable", Some(1));

    let _x = arena.variable("X");
    let y = arena.variable("Y");
    let z = arena.variable("Z");

    let c1 = arena.const_(ast::Const::Number(1));

    // Rule 1: reachable(Y) :- edge(1, Y).
    let rule1 = ast::Clause {
        head: arena.atom(reachable, &[y]),
        premises: arena
            .alloc_slice_copy(&[arena.alloc(ast::Term::Atom(arena.atom(edge, &[c1, y])))]),
        transform: &[],
    };

    // Rule 2: reachable(Z) :- reachable(Y), edge(Y, Z).
    let rule2 = ast::Clause {
        head: arena.atom(reachable, &[z]),
        premises: arena.alloc_slice_copy(&[
            arena.alloc(ast::Term::Atom(arena.atom(reachable, &[y]))),
            arena.alloc(ast::Term::Atom(arena.atom(edge, &[y, z]))),
        ]),
        transform: &[],
    };

    let unit = ast::Unit {
        decls: &[],
        clauses: arena.alloc_slice_copy(&[&rule1, &rule2]),
    };

    // Stratify
    let mut program = Program::new(&arena);
    for clause in unit.clauses {
        program.add_clause(&arena, clause);
    }
    let stratified = program.stratify().expect("stratification failed");

    let ctx = LoweringContext::new(&arena);
    let mut ir = ctx.lower_unit(&unit);

    let mut codegen = Codegen::new_with_stratified(&mut ir, &stratified, WasmImportsBackend);
    let compiled = codegen.generate();

    let mut host = MemHost::new(compiled.strings.clone(), compiled.names.clone());
    host.add_fact("edge", &[1, 2]);
    host.add_fact("edge", &[2, 3]);
    host.add_fact("edge", &[3, 4]);
    host.add_fact("edge", &[4, 5]);

    let shared_host = SharedMemHost {
        inner: Arc::new(Mutex::new(host)),
    };
    let vm = Vm::new()?;

    // The WASM now contains the fixpoint loop internally!
    vm.execute(
        &compiled.wasm,
        shared_host.clone(),
        compiled.strings,
        compiled.names,
    )?;

    let final_host = shared_host.inner.lock().unwrap();
    let facts = final_host.get_facts("reachable");

    let mut values: Vec<i64> = facts.iter().map(|v| v[0]).collect();
    values.sort();

    assert_eq!(values, vec![2, 3, 4, 5]);

    Ok(())
}
