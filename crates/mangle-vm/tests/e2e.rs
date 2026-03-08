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

#[cfg(feature = "csv_storage")]
use mangle_simplecolumn::host::SimpleColumnHost;
#[cfg(feature = "csv_storage")]
use mangle_vm::csv_host::CsvHost;

#[cfg(feature = "csv_storage")]
#[test]
fn test_e2e_composite_storage() -> Result<()> {
    use anyhow::Result;
    use mangle_analysis::LoweringContext;
    use mangle_ast as ast;
    use mangle_codegen::{Codegen, WasmImportsBackend};
    use mangle_vm::composite_host::CompositeHost;
    use mangle_vm::{Host, HostVal, Vm};
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tempfile::NamedTempFile;

    // 1. CSV for 'p' (10, 30)
    let mut file_p = NamedTempFile::new()?;
    writeln!(file_p, "10")?;
    writeln!(file_p, "30")?;
    let path_p = file_p.path().to_path_buf();

    // 2. SimpleColumn for 'q' (10, 20)
    let mut file_q = NamedTempFile::new()?;
    writeln!(file_q, "1")?;
    writeln!(file_q, "q 1 2")?;
    writeln!(file_q, "10")?;
    writeln!(file_q, "20")?;
    let path_q = file_q.path().to_path_buf();

    // 3. Setup Sub-Hosts
    let mut csv_host = CsvHost::new();
    csv_host.add_file("p", path_p);

    let mut sc_host = SimpleColumnHost::new();
    sc_host.load_file("q", &path_q)?;

    // 4. Setup Composite Host
    let mut comp_host = CompositeHost::new();
    let h_csv = comp_host.add_host(Box::new(csv_host));
    let h_sc = comp_host.add_host(Box::new(sc_host));

    comp_host.route_relation("p", h_csv);
    comp_host.route_relation("q", h_sc);

    // 5. Compile Program: r(X) :- p(X), q(X).
    let arena = ast::Arena::new_with_global_interner();
    let p = arena.predicate_sym("p", Some(1));
    let q = arena.predicate_sym("q", Some(1));
    let r = arena.predicate_sym("r", Some(1));
    let x = arena.variable("X");

    let clause = ast::Clause {
        head: arena.atom(r, &[x]),
        premises: arena.alloc_slice_copy(&[
            arena.alloc(ast::Term::Atom(arena.atom(p, &[x]))),
            arena.alloc(ast::Term::Atom(arena.atom(q, &[x]))),
        ]),
        transform: &[],
    };
    let unit = ast::Unit {
        decls: &[],
        clauses: arena.alloc_slice_copy(&[&clause]),
    };

    let ctx = LoweringContext::new(&arena);
    let mut ir = ctx.lower_unit(&unit);

    let mut codegen = Codegen::new(&mut ir, WasmImportsBackend);
    let compiled = codegen.generate();

    // 6. Execute via Shared Wrapper
    #[derive(Clone)]
    struct SharedHost<H>(Arc<Mutex<H>>);
    impl<H: Host> Host for SharedHost<H> {
        fn scan_start(&mut self, id: i32) -> i32 { self.0.lock().unwrap().scan_start(id) }
        fn scan_delta_start(&mut self, id: i32) -> i32 { self.0.lock().unwrap().scan_delta_start(id) }
        fn scan_next(&mut self, id: i32) -> i32 { self.0.lock().unwrap().scan_next(id) }
        fn merge_deltas(&mut self) -> i32 { self.0.lock().unwrap().merge_deltas() }
        fn scan_aggregate_start(&mut self, id: i32, desc: Vec<i32>) -> i32 { self.0.lock().unwrap().scan_aggregate_start(id, desc) }
        fn scan_index_start(&mut self, id: i32, col: i32, val: HostVal) -> i32 { self.0.lock().unwrap().scan_index_start(id, col, val) }
        fn get_col(&mut self, p: i32, i: i32) -> HostVal { self.0.lock().unwrap().get_col(p, i) }
        fn insert_begin(&mut self, id: i32) { self.0.lock().unwrap().insert_begin(id) }
        fn insert_push(&mut self, v: HostVal) { self.0.lock().unwrap().insert_push(v) }
        fn insert_end(&mut self) { self.0.lock().unwrap().insert_end() }
        fn const_number(&mut self, n: i64) -> HostVal { self.0.lock().unwrap().const_number(n) }
        fn const_float(&mut self, b: i64) -> HostVal { self.0.lock().unwrap().const_float(b) }
        fn const_string(&mut self, id: i32) -> HostVal { self.0.lock().unwrap().const_string(id) }
        fn const_name(&mut self, id: i32) -> HostVal { self.0.lock().unwrap().const_name(id) }
        fn const_time(&mut self, n: i64) -> HostVal { self.0.lock().unwrap().const_time(n) }
        fn const_duration(&mut self, n: i64) -> HostVal { self.0.lock().unwrap().const_duration(n) }
        fn val_add(&mut self, a: HostVal, b: HostVal) -> HostVal { self.0.lock().unwrap().val_add(a, b) }
        fn val_sub(&mut self, a: HostVal, b: HostVal) -> HostVal { self.0.lock().unwrap().val_sub(a, b) }
        fn val_mul(&mut self, a: HostVal, b: HostVal) -> HostVal { self.0.lock().unwrap().val_mul(a, b) }
        fn val_div(&mut self, a: HostVal, b: HostVal) -> HostVal { self.0.lock().unwrap().val_div(a, b) }
        fn val_sqrt(&mut self, a: HostVal) -> HostVal { self.0.lock().unwrap().val_sqrt(a) }
        fn val_eq(&mut self, a: HostVal, b: HostVal) -> i32 { self.0.lock().unwrap().val_eq(a, b) }
        fn val_neq(&mut self, a: HostVal, b: HostVal) -> i32 { self.0.lock().unwrap().val_neq(a, b) }
        fn val_lt(&mut self, a: HostVal, b: HostVal) -> i32 { self.0.lock().unwrap().val_lt(a, b) }
        fn val_le(&mut self, a: HostVal, b: HostVal) -> i32 { self.0.lock().unwrap().val_le(a, b) }
        fn val_gt(&mut self, a: HostVal, b: HostVal) -> i32 { self.0.lock().unwrap().val_gt(a, b) }
        fn val_ge(&mut self, a: HostVal, b: HostVal) -> i32 { self.0.lock().unwrap().val_ge(a, b) }
        fn str_concat(&mut self, a: HostVal, b: HostVal) -> HostVal { self.0.lock().unwrap().str_concat(a, b) }
        fn str_replace(&mut self, s: HostVal, o: HostVal, n: HostVal, c: HostVal) -> HostVal { self.0.lock().unwrap().str_replace(s, o, n, c) }
        fn val_to_string(&mut self, v: HostVal) -> HostVal { self.0.lock().unwrap().val_to_string(v) }
        fn compound_begin(&mut self, k: i32) { self.0.lock().unwrap().compound_begin(k) }
        fn compound_push(&mut self, v: HostVal) { self.0.lock().unwrap().compound_push(v) }
        fn compound_end(&mut self) -> HostVal { self.0.lock().unwrap().compound_end() }
        fn compound_get(&mut self, c: HostVal, k: HostVal) -> HostVal { self.0.lock().unwrap().compound_get(c, k) }
        fn compound_len(&mut self, c: HostVal) -> HostVal { self.0.lock().unwrap().compound_len(c) }
        fn pair_first(&mut self, c: HostVal) -> HostVal { self.0.lock().unwrap().pair_first(c) }
        fn pair_second(&mut self, c: HostVal) -> HostVal { self.0.lock().unwrap().pair_second(c) }
        fn debuglog(&mut self, v: HostVal) { self.0.lock().unwrap().debuglog(v) }
    }

    let shared_host = SharedHost(Arc::new(Mutex::new(comp_host)));
    let vm = Vm::new()?;
    vm.execute(
        &compiled.wasm,
        shared_host,
        compiled.strings,
        compiled.names,
    )?;

    Ok(())
}
